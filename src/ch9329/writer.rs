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
use tokio::sync::mpsc;

use super::{paste, protocol};

const BAUD_RATE: u32 = 9600;
const OPEN_TIMEOUT: Duration = Duration::from_millis(500);
const KEY_HOLD_DELAY: Duration = Duration::from_millis(20);

#[derive(Debug, Clone)]
pub enum SerialCommand {
    /// A full keyboard HID report: the modifier bitmask plus up to 6
    /// simultaneously-held HID usage codes. Callers (the WebTransport
    /// session's per-connection key-state tracker) are responsible for
    /// sending the all-zero report on key-up.
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
    port: Box<dyn SerialPort>,
}

impl SerialWriter {
    pub fn new(port: Box<dyn SerialPort>) -> Self {
        Self { port }
    }

    fn write_packet(&mut self, packet: &[u8]) -> Result<()> {
        self.port.write_all(packet).context("writing to CH9329 serial port")
    }

    fn handle(&mut self, cmd: SerialCommand) -> Result<()> {
        match cmd {
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
            if let Err(err) = self.handle(cmd) {
                tracing::error!(%err, "failed to write CH9329 command");
            }
        }
    }
}
