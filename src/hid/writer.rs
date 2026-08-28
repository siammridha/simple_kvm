//! Owns the single open connection to the CH9329's serial port and
//! serializes all writes onto it. Runs as a dedicated blocking loop (driven
//! by `run`), fed by a channel so every input source shares one writer.
//!
//! It still opens that port itself, from a path handed in by the
//! composition root, rather than through `device::Ch9329Device` - #016
//! rewires it.
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
fn open(path: &str) -> Result<Option<Box<dyn SerialPort>>> {
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
    present_rx: watch::Receiver<bool>,
}

impl SerialWriter {
    /// `present_rx` reports whether the CH9329 is plugged in right now,
    /// sourced from `Ch9329Device`'s shared presence detection (see
    /// `device::ch9329_driver`) rather than this struct checking `Path::exists`
    /// itself — the same channel the WebRTC session layer reads from to
    /// tell the browser (the HID counterpart of the capture card's
    /// `DeviceState` — see `rtc::session`), so both sides agree on presence
    /// by construction.
    pub fn new(path: String, present_rx: watch::Receiver<bool>) -> Self {
        Self { path, port: None, present_rx }
    }

    /// Opens or drops `self.port` to match the shared presence signal — so
    /// a write is only attempted while the device is actually present, and
    /// a stale handle from a device that vanished mid-session gets dropped
    /// instead of erroring on every command after it.
    fn sync_connection_state(&mut self) {
        let present = *self.present_rx.borrow();
        match (&self.port, present) {
            (None, true) => match open(&self.path) {
                Ok(Some(port)) => {
                    tracing::info!(path = %self.path, "CH9329 connected");
                    self.port = Some(port);
                }
                Ok(None) => {} // vanished again between the presence signal and opening it
                Err(err) => tracing::error!(%err, path = %self.path, "failed to open CH9329 serial port"),
            },
            (Some(_), false) => {
                tracing::warn!(path = %self.path, "CH9329 disconnected, pausing writes until it reconnects");
                self.port = None;
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
/// connection state (open or drop its port) as soon as `present_rx`
/// reports a presence change — so a reconnect is noticed immediately
/// instead of waiting for the next real keystroke or click. `present_rx`
/// is sourced from `Ch9329Device`'s shared presence detection (see
/// `device::ch9329_driver`), which already does the kernel `tty` uevent
/// listening this function used to do itself — the same immediate-
/// detection treatment `device::CaptureDevice` gives the capture card,
/// both built on the generic `device::Device<D>` core instead of each
/// reimplementing it.
pub async fn watch_connection(mut present_rx: watch::Receiver<bool>, commands: mpsc::Sender<SerialCommand>) {
    loop {
        if present_rx.changed().await.is_err() {
            return; // presence sender dropped, nothing left to watch for
        }
        if commands.send(SerialCommand::CheckConnection).await.is_err() {
            return; // writer loop exited, nothing left to watch for
        }
    }
}
