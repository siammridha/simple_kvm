//! Owns the single open connection to the CH9329's serial port, turns
//! input commands into CH9329 reports, and serializes all writes onto it.
//! Runs as a dedicated blocking loop (driven by `run`), fed by `Hid`'s
//! queue so every input source shares one writer — and, with it, one
//! keyboard state.
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
//! callers of this module — see `InputCommand::PointerButtons`.

use std::time::Duration;

use anyhow::{Context, Result};
use serialport::SerialPort;
use tokio::sync::mpsc;

use super::keyboard::Keyboard;
use super::{paste, protocol, InputCommand};
use crate::device::Ch9329Device;

const KEY_HOLD_DELAY: Duration = Duration::from_millis(20);
/// Above this, `handle` logs at `warn` instead of `debug` — visible in the
/// log at the default level, so a slow CH9329 write shows up without
/// needing `RUST_LOG=debug` turned on first.
const SLOW_COMMAND_THRESHOLD: Duration = Duration::from_millis(50);

/// What travels down `Hid`'s queue: either something the peer did, or the
/// module's own housekeeping.
#[derive(Debug, Clone)]
pub(super) enum Command {
    Input(InputCommand),
}

impl Command {
    fn kind(&self) -> &'static str {
        match self {
            Command::Input(input) => input.kind(),
        }
    }
}

/// What one command turns into on the wire. Keeping this separate from
/// writing it is what lets keyboard state be updated even when there's no
/// port to write to (see `handle`).
enum Encoded {
    Packet(Vec<u8>),
    /// Many packets with a hold delay between them — `type_text` needs the
    /// port itself, so it can't be pre-encoded to bytes here.
    Text(String),
    Nothing,
}

pub struct SerialWriter {
    device: Ch9329Device,
    port: Option<Box<dyn SerialPort>>,
    /// The one keyboard the CH9329 presents to the target — see
    /// `keyboard::Keyboard`.
    keyboard: Keyboard,
}

impl SerialWriter {
    pub(super) fn new(device: Ch9329Device) -> Self {
        Self { device, port: None, keyboard: Keyboard::default() }
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

    /// Turns one command into what should go out on the wire, updating
    /// keyboard state on the way. Runs before `handle` checks the port, so
    /// keys held and released while the CH9329 is unplugged still cancel
    /// out instead of coming back stuck on reconnect.
    fn encode(&mut self, cmd: Command) -> Encoded {
        match cmd {
            Command::Input(InputCommand::Key { code, pressed }) => match self.keyboard.apply(&code, pressed) {
                Some((modifiers, keys)) => Encoded::Packet(protocol::keyboard_report(modifiers, keys)),
                // A code with no HID mapping: nothing to press.
                None => Encoded::Nothing,
            },
            Command::Input(InputCommand::PointerMoveAbsolute { x_frac, y_frac }) => {
                Encoded::Packet(protocol::mouse_absolute(0, x_frac, y_frac, 0))
            }
            Command::Input(InputCommand::PointerMoveRelative { buttons, dx, dy, wheel }) => {
                Encoded::Packet(protocol::mouse_relative(buttons, dx, dy, wheel))
            }
            // Buttons and wheel ride a zero-delta relative report whatever
            // the mouse mode is, because the chip's absolute report drops
            // them (see module docs).
            Command::Input(InputCommand::PointerButtons { buttons, wheel }) => {
                Encoded::Packet(protocol::mouse_relative(buttons, 0, 0, wheel))
            }
            Command::Input(InputCommand::PasteText(text)) => Encoded::Text(text),
        }
    }

    fn handle(&mut self, cmd: Command) {
        let start = std::time::Instant::now();
        let kind = cmd.kind();
        let encoded = self.encode(cmd);
        self.sync_connection_state();
        if self.port.is_none() {
            return;
        }
        let result = match encoded {
            Encoded::Packet(packet) => self.write_packet(&packet),
            Encoded::Text(text) => self.type_text(&text),
            Encoded::Nothing => Ok(()),
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
    pub(super) fn run(mut self, mut commands: mpsc::Receiver<Command>) {
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
        commands_tx.send(Command::Input(InputCommand::PointerButtons { buttons: 1, wheel: 0 })).await.unwrap();
        drop(commands_tx);

        tokio::time::timeout(Duration::from_secs(5), worker).await.expect("the worker must end once its queue closes").expect("the worker must not panic");
    }

    /// Encoding happens before the port check, so a key pressed and
    /// released while the CH9329 is absent doesn't leave a stuck key
    /// behind for whenever it comes back.
    #[tokio::test]
    async fn key_state_is_kept_up_to_date_with_no_port_open() {
        let mut writer = SerialWriter::new(Ch9329Device::spawn_at("/nonexistent-simple-kvm-test-ch9329"));

        let down = writer.encode(Command::Input(InputCommand::Key { code: "KeyA".into(), pressed: true }));
        let up = writer.encode(Command::Input(InputCommand::Key { code: "KeyA".into(), pressed: false }));

        assert!(matches!(down, Encoded::Packet(p) if p == protocol::keyboard_report(0, [0x04, 0, 0, 0, 0, 0])));
        assert!(matches!(up, Encoded::Packet(p) if p == protocol::keyboard_report(0, [0; 6])));
    }
}
