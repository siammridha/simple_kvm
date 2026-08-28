//! Owns the single open connection to the CH9329's serial port and
//! serializes all writes onto it. Runs as a dedicated blocking loop (driven
//! by `run`), fed by `Hid`'s queue so every input source shares one writer.
//!
//! The port is opened through `device::Ch9329Device` (`Device::open`), the
//! module's only open path - this writer never sees or holds the device
//! path (`ARCHITECTURE.md` I3).
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
use tokio::sync::mpsc;

use super::{paste, protocol};
use crate::device::Ch9329Device;

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
    /// Sent by `Hid`'s presence listener whenever the CH9329 appears or
    /// disappears — carries no data of its own, just prompts `handle` to
    /// re-run `sync_connection_state` so a reconnect is noticed
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

pub struct SerialWriter {
    device: Ch9329Device,
    port: Option<Box<dyn SerialPort>>,
}

impl SerialWriter {
    pub(super) fn new(device: Ch9329Device) -> Self {
        Self { device, port: None }
    }

    /// Opens or drops `self.port` to match the device's presence — so a
    /// write is only attempted while the device is actually present, and a
    /// stale handle from a device that vanished mid-session gets dropped
    /// instead of erroring on every command after it. Asking `is_present`
    /// first keeps the common unplugged case quiet, rather than logging a
    /// failed open per command.
    fn sync_connection_state(&mut self) {
        match (&self.port, self.device.is_present()) {
            (None, true) => match self.device.open(&()) {
                Ok(port) => {
                    tracing::info!("CH9329 connected");
                    self.port = Some(port);
                }
                // Includes vanishing again between the presence check and
                // the open itself.
                Err(err) => tracing::warn!(%err, "failed to open CH9329 serial port"),
            },
            (Some(_), false) => {
                tracing::warn!("CH9329 disconnected, pausing writes until it reconnects");
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
    /// Returns once the queue closes, which happens when the `Hid` that
    /// owns it is dropped.
    pub(super) fn run(mut self, mut commands: mpsc::Receiver<SerialCommand>) {
        while let Some(cmd) = commands.blocking_recv() {
            self.handle(cmd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole drop-ends-the-worker chain, minus the hardware: closing
    /// the queue must end `run`, which is what lets `Hid`'s destructor
    /// stop its worker.
    #[tokio::test]
    async fn the_worker_ends_when_its_queue_closes() {
        let device = Ch9329Device::spawn_at("/nonexistent-simple-kvm-test-ch9329");
        let (commands_tx, commands_rx) = mpsc::channel(4);
        let worker = tokio::task::spawn_blocking(move || SerialWriter::new(device).run(commands_rx));

        // Queued while the device is absent: handled (as no-ops) in order,
        // then the closed queue ends the loop.
        commands_tx.send(SerialCommand::MouseButtons { buttons: 1, wheel: 0 }).await.unwrap();
        commands_tx.send(SerialCommand::CheckConnection).await.unwrap();
        drop(commands_tx);

        tokio::time::timeout(Duration::from_secs(5), worker).await.expect("the worker must end once its queue closes").expect("the worker must not panic");
    }
}
