//! Per-connection handling: one task juggling three jobs over
//! `tokio::select!` — sending video frames as they arrive on the shared
//! `video_bus`, translating input datagrams into `SerialCommand`s, and
//! reading control-stream JSON (the Save button's settings update, paste).
//! Settings changes only touch the shared `watch<Settings>` and the
//! settings file — the connection itself is never disturbed, so applying
//! new settings never drops the session.

use anyhow::Result;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use wtransport::{Connection, RecvStream};

use crate::capture::v4l2::Resolution;
use crate::ch9329::keymap::{self, KeyCode};
use crate::ch9329::writer::SerialCommand;
use crate::config::{CaptureSettings, DeviceState, MouseMode, PersistedSettings, VideoMode};
use crate::settings_store;
use crate::video_bus::{self, FrameKind};

use super::protocol::{ControlMessage, InputEvent, MouseModeWire, ServerMessage, VideoModeWire};

pub struct SessionContext {
    pub video_bus: video_bus::Receiver,
    pub serial_tx: mpsc::Sender<SerialCommand>,
    pub capture_settings_tx: watch::Sender<CaptureSettings>,
    pub capture_settings_rx: watch::Receiver<CaptureSettings>,
    pub mouse_mode_tx: watch::Sender<MouseMode>,
    pub mouse_mode_rx: watch::Receiver<MouseMode>,
    pub device_state_rx: watch::Receiver<DeviceState>,
    pub hid_connected_rx: watch::Receiver<bool>,
    pub settings_path: PathBuf,
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
    let mut device_state_rx = ctx.device_state_rx.clone();
    let mut capture_settings_rx = ctx.capture_settings_rx.clone();
    let mut mouse_mode_rx = ctx.mouse_mode_rx.clone();
    let mut hid_connected_rx = ctx.hid_connected_rx.clone();
    let mut keyboard = KeyboardState::default();
    let mut control: Option<(wtransport::SendStream, RecvStream)> = None;
    let mut control_buf: Vec<u8> = Vec::new();
    // Set once the video_bus sender is gone for good (only happens if the
    // capture task itself is torn down, e.g. process shutdown - it no
    // longer exits just because the card is absent or unplugged, see
    // `CaptureManager::run`). Keyboard and mouse must keep working either
    // way, so this only disables the video branch — it must never end the
    // session.
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
                let (mut send, recv) = bi?;
                // `changed()` on a freshly subscribed watch::Receiver only
                // fires for a change that happens *after* the subscribe -
                // it won't fire for state that was already current when
                // this tab connected. So the current state has to be
                // pushed explicitly here, once, rather than relying on the
                // `changed()` arms below (which still handle every update
                // from this point on).
                let push_result = push_initial_state(&mut send, &mut device_state_rx, &mut hid_connected_rx, &mut capture_settings_rx, &mut mouse_mode_rx).await;
                if push_result.is_ok() {
                    control = Some((send, recv));
                }
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
            changed = device_state_rx.changed(), if control.is_some() => {
                if changed.is_ok() {
                    let state = device_state_rx.borrow_and_update().clone();
                    if let Some((send, _)) = control.as_mut() {
                        let msg = ServerMessage::DeviceState(state);
                        if send_server_message(send, &msg).await.is_err() {
                            control = None;
                        }
                    }
                }
            }
            changed = capture_settings_rx.changed(), if control.is_some() => {
                if changed.is_ok() {
                    let capture = *capture_settings_rx.borrow_and_update();
                    if let Some((send, _)) = control.as_mut() {
                        let msg = ServerMessage::Settings { capture, mouse_mode: *mouse_mode_rx.borrow() };
                        if send_server_message(send, &msg).await.is_err() {
                            control = None;
                        }
                    }
                }
            }
            changed = mouse_mode_rx.changed(), if control.is_some() => {
                if changed.is_ok() {
                    let mouse_mode = *mouse_mode_rx.borrow_and_update();
                    if let Some((send, _)) = control.as_mut() {
                        let msg = ServerMessage::Settings { capture: *capture_settings_rx.borrow(), mouse_mode };
                        if send_server_message(send, &msg).await.is_err() {
                            control = None;
                        }
                    }
                }
            }
            changed = hid_connected_rx.changed(), if control.is_some() => {
                if changed.is_ok() {
                    let available = *hid_connected_rx.borrow_and_update();
                    if let Some((send, _)) = control.as_mut() {
                        let msg = ServerMessage::HidState { available };
                        if send_server_message(send, &msg).await.is_err() {
                            control = None;
                        }
                    }
                }
            }
        }
    }
}

/// Pushes the full current state (device availability, HID connectivity,
/// settings) down a just-opened control stream. Needed because `changed()`
/// on a receiver only fires for changes from here on — it doesn't tell a
/// freshly connected tab about state that was already current before it
/// connected (see the call site in `handle`).
async fn push_initial_state(
    send: &mut wtransport::SendStream,
    device_state_rx: &mut watch::Receiver<DeviceState>,
    hid_connected_rx: &mut watch::Receiver<bool>,
    capture_settings_rx: &mut watch::Receiver<CaptureSettings>,
    mouse_mode_rx: &mut watch::Receiver<MouseMode>,
) -> Result<()> {
    let device_state = device_state_rx.borrow_and_update().clone();
    send_server_message(send, &ServerMessage::DeviceState(device_state)).await?;
    let hid_available = *hid_connected_rx.borrow_and_update();
    send_server_message(send, &ServerMessage::HidState { available: hid_available }).await?;
    let capture = *capture_settings_rx.borrow_and_update();
    let mouse_mode = *mouse_mode_rx.borrow_and_update();
    send_server_message(send, &ServerMessage::Settings { capture, mouse_mode }).await?;
    Ok(())
}

async fn send_server_message(send: &mut wtransport::SendStream, msg: &ServerMessage) -> Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    send.write_all(line.as_bytes()).await?;
    Ok(())
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

/// Above this, the queue-time log below fires at `warn` instead of `debug`
/// — a slow enqueue means the CH9329 writer task is falling behind, which
/// is exactly what shows up as "typing/clicking lags".
const SLOW_ENQUEUE_THRESHOLD: Duration = Duration::from_millis(50);

async fn handle_input_event(event: InputEvent, keyboard: &mut KeyboardState, ctx: &SessionContext) {
    let start = Instant::now();
    let cmd = match event {
        InputEvent::KeyEvent { pressed, code } => {
            let Some((modifiers, keys)) = keyboard.apply(&code, pressed) else {
                return;
            };
            tracing::debug!(code = %code, pressed, "typing: key event received from browser");
            SerialCommand::KeyReport { modifiers, keys }
        }
        InputEvent::MouseAbsoluteMove { x_frac, y_frac } => SerialCommand::MouseAbsoluteMove { x_frac, y_frac },
        InputEvent::MouseRelativeMove { buttons, dx, dy, wheel } => SerialCommand::MouseRelativeMove { buttons, dx, dy, wheel },
        InputEvent::MouseButtons { buttons, wheel } => SerialCommand::MouseButtons { buttons, wheel },
    };
    let kind = cmd.kind();
    let sent = ctx.serial_tx.send(cmd).await.is_ok();
    let elapsed = start.elapsed();
    if elapsed > SLOW_ENQUEUE_THRESHOLD {
        tracing::warn!(kind, sent, elapsed_ms = elapsed.as_millis(), "input event took longer than expected to queue for CH9329");
    } else {
        tracing::debug!(kind, sent, elapsed_ms = elapsed.as_millis(), "queued input event for CH9329");
    }
}

fn handle_control_message(msg: ControlMessage, ctx: &SessionContext) {
    match msg {
        // Applies and persists whichever half the page included, sent when
        // the page's Save button is clicked (see `assets/web/app.js`) -
        // dropdowns no longer apply live. `capture` is only present if the
        // page saw a capture card connected; `mouse_mode` only if it saw
        // the CH9329 connected - there's nothing meaningful to save for a
        // device that isn't there, so that half is just left as it was.
        ControlMessage::UpdateSettings { capture, mouse_mode } => {
            let had_update = capture.is_some() || mouse_mode.is_some();
            if let Some(capture) = capture {
                ctx.capture_settings_tx.send_modify(|s| {
                    s.video_mode = match capture.video_mode {
                        VideoModeWire::Mjpeg => VideoMode::Mjpeg,
                        VideoModeWire::H264 => VideoMode::H264,
                    };
                    s.resolution = Resolution { width: capture.width, height: capture.height };
                    s.fps = capture.fps;
                });
            }
            // The server doesn't need mouse mode to translate input events —
            // that's purely which datagram variant the client sends (see
            // `InputEvent`) — but it's tracked here anyway so it can be
            // persisted and reported back as the default on the next page
            // load.
            if let Some(mouse_mode) = mouse_mode {
                ctx.mouse_mode_tx.send_replace(match mouse_mode {
                    MouseModeWire::Absolute => MouseMode::Absolute,
                    MouseModeWire::Relative => MouseMode::Relative,
                });
            }

            if had_update {
                let settings = PersistedSettings { capture: *ctx.capture_settings_rx.borrow(), mouse_mode: *ctx.mouse_mode_rx.borrow() };
                let path = ctx.settings_path.clone();
                tokio::spawn(async move {
                    if let Err(err) = tokio::task::spawn_blocking(move || settings_store::save(&path, settings)).await {
                        tracing::error!(%err, "settings save task panicked");
                    }
                });
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::v4l2::Resolution;
    use crate::config::VideoMode;
    use crate::webtransport::protocol::CaptureSettingsWire;

    fn test_ctx(settings_path: PathBuf) -> SessionContext {
        let (_video_tx, video_rx) = video_bus::channel();
        let (serial_tx, _serial_rx) = mpsc::channel(1);
        let (capture_settings_tx, capture_settings_rx) =
            watch::channel(CaptureSettings { video_mode: VideoMode::Mjpeg, resolution: Resolution { width: 1280, height: 720 }, fps: 5 });
        let (mouse_mode_tx, mouse_mode_rx) = watch::channel(MouseMode::Absolute);
        let (_device_state_tx, device_state_rx) = watch::channel(DeviceState::default());
        let (_hid_connected_tx, hid_connected_rx) = watch::channel(false);
        SessionContext {
            video_bus: video_rx,
            serial_tx,
            capture_settings_tx,
            capture_settings_rx,
            mouse_mode_tx,
            mouse_mode_rx,
            device_state_rx,
            hid_connected_rx,
            settings_path,
        }
    }

    fn temp_settings_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("simple_kvm_session_test_{}_{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("settings.json")
    }

    #[tokio::test]
    async fn update_settings_with_capture_only_leaves_mouse_mode_untouched() {
        let path = temp_settings_path("capture_only");
        let ctx = test_ctx(path.clone());

        handle_control_message(
            ControlMessage::UpdateSettings {
                capture: Some(CaptureSettingsWire { video_mode: VideoModeWire::H264, width: 1920, height: 1080, fps: 25 }),
                mouse_mode: None,
            },
            &ctx,
        );

        assert_eq!(*ctx.capture_settings_rx.borrow(), CaptureSettings { video_mode: VideoMode::H264, resolution: Resolution { width: 1920, height: 1080 }, fps: 25 });
        assert_eq!(*ctx.mouse_mode_rx.borrow(), MouseMode::Absolute);

        tokio::time::sleep(Duration::from_millis(50)).await;
        let saved = settings_store::load(&path).expect("save spawned by handle_control_message should have run");
        assert_eq!(saved.capture.fps, 25);
        assert_eq!(saved.mouse_mode, MouseMode::Absolute);

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn update_settings_with_mouse_mode_only_leaves_capture_untouched() {
        let path = temp_settings_path("mouse_mode_only");
        let ctx = test_ctx(path.clone());

        handle_control_message(ControlMessage::UpdateSettings { capture: None, mouse_mode: Some(MouseModeWire::Relative) }, &ctx);

        assert_eq!(*ctx.capture_settings_rx.borrow(), CaptureSettings { video_mode: VideoMode::Mjpeg, resolution: Resolution { width: 1280, height: 720 }, fps: 5 });
        assert_eq!(*ctx.mouse_mode_rx.borrow(), MouseMode::Relative);

        tokio::time::sleep(Duration::from_millis(50)).await;
        let saved = settings_store::load(&path).expect("save spawned by handle_control_message should have run");
        assert_eq!(saved.capture.fps, 5);
        assert_eq!(saved.mouse_mode, MouseMode::Relative);

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn update_settings_with_neither_field_does_not_write_a_settings_file() {
        let path = temp_settings_path("neither");
        let ctx = test_ctx(path.clone());

        handle_control_message(ControlMessage::UpdateSettings { capture: None, mouse_mode: None }, &ctx);

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(settings_store::load(&path).is_none(), "no fields present means nothing should be persisted");

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
