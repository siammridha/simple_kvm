//! Per-connection handling: one task juggling three jobs over
//! `tokio::select!` — sending video frames as they arrive on the shared
//! `video_bus`, translating input datagrams into `SerialCommand`s, and
//! reading control-stream JSON (dropdown changes, paste). Settings
//! changes only touch the shared `watch<Settings>` — the connection
//! itself is never disturbed, so switching video/resolution/mouse mode
//! never drops the session.

use anyhow::Result;
use std::collections::HashSet;
use tokio::sync::{mpsc, watch};
use wtransport::{Connection, RecvStream};

use crate::capture::v4l2::Resolution;
use crate::ch9329::keymap::{self, KeyCode};
use crate::ch9329::writer::SerialCommand;
use crate::config::{CaptureSettings, MouseMode, VideoMode};
use crate::video_bus::{self, FrameKind};

use super::protocol::{ControlMessage, InputEvent, MouseModeWire, VideoModeWire};

pub struct SessionContext {
    pub video_bus: video_bus::Receiver,
    pub serial_tx: mpsc::Sender<SerialCommand>,
    pub capture_settings_tx: watch::Sender<CaptureSettings>,
    pub mouse_mode_tx: watch::Sender<MouseMode>,
}

#[derive(Default)]
struct KeyboardState {
    held: HashSet<String>,
}

impl KeyboardState {
    /// Updates held-key state and returns the full report to send, unless
    /// `code` isn't a key we recognize.
    fn apply(&mut self, code: &str, pressed: bool) -> Option<(u8, [u8; 6])> {
        keymap::lookup(code)?;
        if pressed {
            self.held.insert(code.to_string());
        } else {
            self.held.remove(code);
        }

        let mut modifiers = 0u8;
        let mut keys = [0u8; 6];
        let mut slot = 0;
        for held_code in &self.held {
            match keymap::lookup(held_code) {
                Some(KeyCode::Modifier(bit)) => modifiers |= bit,
                Some(KeyCode::Usage(usage)) if slot < keys.len() => {
                    keys[slot] = usage;
                    slot += 1;
                }
                _ => {}
            }
        }
        Some((modifiers, keys))
    }
}

pub async fn handle(connection: Connection, ctx: SessionContext) -> Result<()> {
    let mut video_rx = ctx.video_bus.clone();
    let mut keyboard = KeyboardState::default();
    let mut control: Option<(wtransport::SendStream, RecvStream)> = None;
    let mut control_buf: Vec<u8> = Vec::new();
    // Set once the video_bus sender is gone for good (e.g. no capture
    // device at all, so the capture task exits immediately). Keyboard and
    // mouse must keep working either way, so this only disables the video
    // branch — it must never end the session.
    let mut video_closed = false;

    loop {
        tokio::select! {
            changed = video_rx.changed(), if !video_closed => {
                match changed {
                    Ok(()) => {
                        let frame = video_rx.borrow_and_update().clone();
                        if let Some(frame) = frame
                            && let Err(err) = send_frame(&connection, frame.kind, &frame.data).await
                        {
                            tracing::debug!(%err, "failed to send video frame, dropping it");
                        }
                    }
                    Err(_) => video_closed = true,
                }
            }
            datagram = connection.receive_datagram() => {
                let datagram = datagram?;
                if let Some(event) = InputEvent::parse(&datagram.payload()) {
                    handle_input_event(event, &mut keyboard, &ctx).await;
                }
            }
            bi = connection.accept_bi(), if control.is_none() => {
                control = Some(bi?);
            }
            line = read_control_line(control.as_mut().map(|(_, recv)| recv), &mut control_buf), if control.is_some() => {
                match line? {
                    Some(line) => {
                        match serde_json::from_str::<ControlMessage>(&line) {
                            Ok(msg) => handle_control_message(msg, &ctx),
                            Err(err) => tracing::debug!(%err, "ignoring malformed control message"),
                        }
                    }
                    None => control = None,
                }
            }
        }
    }
}

async fn send_frame(connection: &Connection, kind: FrameKind, data: &[u8]) -> Result<()> {
    let mut stream = connection.open_uni().await?.await?;
    let kind_byte: u8 = match kind {
        FrameKind::Mjpeg => 0,
        FrameKind::H264 => 1,
    };
    stream.write_all(&[kind_byte]).await?;
    stream.write_all(data).await?;
    stream.finish().await?;
    Ok(())
}

async fn handle_input_event(event: InputEvent, keyboard: &mut KeyboardState, ctx: &SessionContext) {
    let cmd = match event {
        InputEvent::KeyEvent { pressed, code } => {
            let Some((modifiers, keys)) = keyboard.apply(&code, pressed) else {
                return;
            };
            SerialCommand::KeyReport { modifiers, keys }
        }
        InputEvent::MouseAbsoluteMove { x_frac, y_frac } => SerialCommand::MouseAbsoluteMove { x_frac, y_frac },
        InputEvent::MouseRelativeMove { buttons, dx, dy, wheel } => SerialCommand::MouseRelativeMove { buttons, dx, dy, wheel },
        InputEvent::MouseButtons { buttons, wheel } => SerialCommand::MouseButtons { buttons, wheel },
    };
    let _ = ctx.serial_tx.send(cmd).await;
}

fn handle_control_message(msg: ControlMessage, ctx: &SessionContext) {
    match msg {
        ControlMessage::SetVideoMode { mode } => ctx.capture_settings_tx.send_modify(|s| {
            s.video_mode = match mode {
                VideoModeWire::Mjpeg => VideoMode::Mjpeg,
                VideoModeWire::H264 => VideoMode::H264,
            };
        }),
        ControlMessage::SetResolution { width, height } => {
            ctx.capture_settings_tx.send_modify(|s| s.resolution = Resolution { width, height });
        }
        // The server doesn't need mouse mode to translate input events —
        // that's purely which datagram variant the client sends (see
        // `InputEvent`) — but it's tracked here anyway so it can be
        // persisted and reported back as the default on the next page
        // load (see `settings_store`).
        ControlMessage::SetMouseMode { mode } => {
            ctx.mouse_mode_tx.send_replace(match mode {
                MouseModeWire::Absolute => MouseMode::Absolute,
                MouseModeWire::Relative => MouseMode::Relative,
            });
        }
        ControlMessage::Paste { text } => {
            let tx = ctx.serial_tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(SerialCommand::PasteText(text)).await;
            });
        }
    }
}

/// No legitimate control message (settings change or paste) needs to be
/// anywhere near this large. Caps how much a connection with no
/// authentication (see README) can make the server buffer by simply never
/// sending a newline.
const MAX_CONTROL_LINE_BYTES: usize = 64 * 1024;

/// Reads one JSON-line from the control stream, buffering partial reads.
/// Returns `Ok(None)` on a `None` `recv` (no control stream open yet —
/// this branch is only ever polled when `control.is_some()`, so `recv`
/// being `None` here can't actually happen) or on stream EOF.
async fn read_control_line(recv: Option<&mut RecvStream>, buf: &mut Vec<u8>) -> Result<Option<String>> {
    let Some(recv) = recv else {
        return std::future::pending().await;
    };
    loop {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            return Ok(Some(String::from_utf8_lossy(&line[..line.len() - 1]).into_owned()));
        }
        if buf.len() >= MAX_CONTROL_LINE_BYTES {
            anyhow::bail!("control message exceeded {MAX_CONTROL_LINE_BYTES} bytes with no newline");
        }
        let mut chunk = [0u8; 4096];
        match recv.read(&mut chunk).await? {
            Some(n) => buf.extend_from_slice(&chunk[..n]),
            None => return Ok(None),
        }
    }
}
