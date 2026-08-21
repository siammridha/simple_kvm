//! Owns the single open connection to the CH9329 over `/dev/ttyUSB*` and
//! serializes all writes onto it. Runs as a dedicated blocking loop (driven
//! by `run`), fed by a channel so every input source shares one writer.
//!
//! Absolute mouse mode on this hardware only conveys X/Y — confirmed by an
//! end-to-end loopback test against the real chip (see the plan doc).
//! Buttons and wheel are silently dropped by the chip's absolute HID
//! report, but work correctly in its relative report. So "absolute mode"
//! here means: position via `mouse_absolute`, clicks/scroll via a
//! `mouse_relative` report with `dx = dy = 0`. This is invisible to
//! callers of this module — see `SerialCommand::MouseButtons`.

use std::time::Duration;

use anyhow::{Context, Result};
use serialport::SerialPort;
use tokio::sync::{mpsc, watch};

use crate::uevent::UeventListener;

use super::{paste, protocol};

const BAUD_RATE: u32 = 9600;
const OPEN_TIMEOUT: Duration = Duration::from_millis(500);
const KEY_HOLD_DELAY: Duration = Duration::from_millis(20);
/// Above this, `handle` logs at `warn` instead of `debug` — visible in the
/// log at the default level, so a slow CH9329 write shows up without
/// needing `RUST_LOG=debug` turned on first.
const SLOW_COMMAND_THRESHOLD: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub enum SerialCommand {
    /// A full keyboard HID report: the modifier bitmask plus up to 6
    /// simultaneously-held HID usage codes. Callers (the WebRTC session's
    /// per-connection key-state tracker) are responsible for sending the
    /// all-zero report on key-up.
    KeyReport { modifiers: u8, keys: [u8; 6] },
    /// Absolute cursor position, as a fraction of the video frame.
    MouseAbsoluteMove { x_frac: f32, y_frac: f32 },
    /// Click/scroll state to apply without moving the cursor — the vehicle
    /// for making clicks and wheel scroll work while in absolute mode
    /// (see module docs).
    MouseButtons { buttons: u8, wheel: i8 },
    /// A full relative-mode report: move plus click/scroll state together.
    MouseRelativeMove { buttons: u8, dx: i8, dy: i8, wheel: i8 },
    /// Types `text` out as a sequence of keystrokes (US QWERTY only).
    PasteText(String),
    /// Sent by `watch_connection` whenever the kernel reports a `tty`
    /// device change — carries no data of its own, just prompts `handle`
    /// to re-run `sync_connection_state` so a reconnect is noticed
    /// immediately instead of waiting for the next real keystroke or
    /// click.
    CheckConnection,
}

impl SerialCommand {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            SerialCommand::KeyReport { .. } => "key_report",
            SerialCommand::MouseAbsoluteMove { .. } => "mouse_absolute_move",
            SerialCommand::MouseButtons { .. } => "mouse_buttons",
            SerialCommand::MouseRelativeMove { .. } => "mouse_relative_move",
            SerialCommand::PasteText(_) => "paste_text",
            SerialCommand::CheckConnection => "check_connection",
        }
    }
}

/// Opens the CH9329's serial port. Returns `Ok(None)` (not an error) if no
/// device is present at `path` — callers should run in a soft
/// "no CH9329 attached" state rather than failing to start.
pub fn open(path: &str) -> Result<Option<Box<dyn SerialPort>>> {
    match serialport::new(path, BAUD_RATE).timeout(OPEN_TIMEOUT).open() {
        Ok(port) => Ok(Some(port)),
        Err(err)
            if matches!(
                err.kind(),
                serialport::ErrorKind::NoDevice | serialport::ErrorKind::Io(std::io::ErrorKind::NotFound)
            ) =>
        {
            Ok(None)
        }
        Err(err) => Err(err).with_context(|| format!("opening CH9329 serial port at {path}")),
    }
}

pub struct SerialWriter {
    path: String,
    port: Option<Box<dyn SerialPort>>,
    connected_tx: watch::Sender<bool>,
}

impl SerialWriter {
    /// `connected_tx` lets the WebRTC session layer know whether the
    /// CH9329 is plugged in right now, the HID counterpart of the capture
    /// card's `DeviceState` — see `rtc::session`.
    pub fn new(path: String, connected_tx: watch::Sender<bool>) -> Self {
        Self { path, port: None, connected_tx }
    }

    /// Checks whether the CH9329 is plugged in right now, and opens or
    /// drops `self.port` to match — so a write is only attempted while the
    /// device is actually present, and a stale handle from a device that
    /// vanished mid-session gets dropped instead of erroring on every
    /// command after it.
    fn sync_connection_state(&mut self) {
        let present = std::path::Path::new(&self.path).exists();
        match (&self.port, present) {
            (None, true) => match open(&self.path) {
                Ok(Some(port)) => {
                    tracing::info!(path = %self.path, "CH9329 connected");
                    self.port = Some(port);
                    let _ = self.connected_tx.send(true);
                }
                Ok(None) => {} // vanished again between the exists() check and opening it
                Err(err) => tracing::error!(%err, path = %self.path, "failed to open CH9329 serial port"),
            },
            (Some(_), false) => {
                tracing::warn!(path = %self.path, "CH9329 disconnected, pausing writes until it reconnects");
                self.port = None;
                let _ = self.connected_tx.send(false);
            }
            _ => {}
        }
    }

    fn write_packet(&mut self, packet: &[u8]) -> Result<()> {
        let port = self.port.as_mut().context("no CH9329 connected")?;
        port.write_all(packet).context("writing to CH9329 serial port")
    }

    fn handle(&mut self, cmd: SerialCommand) {
        let start = std::time::Instant::now();
        let kind = cmd.kind();
        self.sync_connection_state();
        if self.port.is_none() {
            return;
        }
        let result = match cmd {
            SerialCommand::KeyReport { modifiers, keys } => {
                self.write_packet(&protocol::keyboard_report(modifiers, keys))
            }
            SerialCommand::MouseAbsoluteMove { x_frac, y_frac } => {
                self.write_packet(&protocol::mouse_absolute(0, x_frac, y_frac, 0))
            }
            SerialCommand::MouseButtons { buttons, wheel } => {
                self.write_packet(&protocol::mouse_relative(buttons, 0, 0, wheel))
            }
            SerialCommand::MouseRelativeMove { buttons, dx, dy, wheel } => {
                self.write_packet(&protocol::mouse_relative(buttons, dx, dy, wheel))
            }
            SerialCommand::PasteText(text) => self.type_text(&text),
            SerialCommand::CheckConnection => Ok(()), // sync_connection_state() above already did the work
        };
        if let Err(err) = result {
            tracing::error!(%err, "failed to write CH9329 command, dropping the connection until it reconnects");
            self.port = None;
        }
        let elapsed = start.elapsed();
        if elapsed > SLOW_COMMAND_THRESHOLD {
            tracing::warn!(kind, elapsed_ms = elapsed.as_millis(), "CH9329 command took longer than expected to write");
        } else {
            tracing::debug!(kind, elapsed_ms = elapsed.as_millis(), "wrote CH9329 command");
        }
    }

    fn type_text(&mut self, text: &str) -> Result<()> {
        for keystroke in paste::encode(text) {
            let modifiers = if keystroke.shift { protocol::modifier::LEFT_SHIFT } else { 0 };
            self.write_packet(&protocol::keyboard_report(modifiers, [keystroke.usage, 0, 0, 0, 0, 0]))?;
            std::thread::sleep(KEY_HOLD_DELAY);
            self.write_packet(&protocol::keyboard_report(0, [0; 6]))?;
            std::thread::sleep(KEY_HOLD_DELAY);
        }
        Ok(())
    }

    /// Blocking consumer loop — run this on a dedicated thread
    /// (`tokio::task::spawn_blocking`), never on an async task directly.
    pub fn run(mut self, mut commands: mpsc::Receiver<SerialCommand>) {
        while let Some(cmd) = commands.blocking_recv() {
            self.handle(cmd);
        }
    }
}

/// Runs forever, prompting `SerialWriter::run` (via `commands`) to re-check
/// whether the CH9329 is plugged in as soon as the kernel reports a `tty`
/// device change — the same immediate-detection treatment
/// `capture::CaptureManager` gives the capture card, applied here to the
/// CH9329/CH340 side. If the uevent listener can't be opened, reconnects
/// are only noticed on the next real keystroke or click, since there's no
/// polling fallback.
pub async fn watch_connection(commands: mpsc::Sender<SerialCommand>) {
    let mut listener = match UeventListener::open() {
        Ok(listener) => listener,
        Err(err) => {
            tracing::warn!(%err, "failed to open kernel uevent listener, CH9329 reconnects won't be noticed until the next command");
            return;
        }
    };
    loop {
        listener.wait_for_subsystem("tty").await;
        if commands.send(SerialCommand::CheckConnection).await.is_err() {
            return; // writer loop exited, nothing left to watch for
        }
    }
}
