//! The CH9329 keyboard/mouse bridge, as one owned object (`Hid`).
//!
//! `Hid` holds the `Ch9329Device`, opens its serial channel through the
//! Device API (`Device::open` - the only open path in this module), owns
//! the command queue, and spawns its own drain worker. Callers describe
//! what the peer did in input terms (`InputCommand`); everything below —
//! the keymap, which keys are held, the report shapes, the wire framing —
//! is this module's business. No channel sender, no serial handle and no
//! device path leaves it (`ARCHITECTURE.md` §3.3).
//!
//! Lifetimes follow ownership: `Hid` holds the only strong queue sender,
//! so dropping it closes the queue, which ends the blocking drain worker,
//! which ends the task awaiting it.
//!
//! No settle delay lives here any more - `Hid::spawn` holds its
//! `Ch9329Device` from the start, the same way `CaptureCard` holds its
//! `CaptureDevice`. The boot-crash mitigation that used to be this
//! module's own flat wait is now `device`'s detect-to-probe delay,
//! applied generically to every device kind.

mod keyboard;
mod keymap;
mod paste;
mod protocol;
mod writer;

use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::device::{Ch9329Device, DeviceStatus, EventEmitter, Subscription};

use writer::Command;

/// Deep enough that a burst of mouse-move reports never blocks the WebRTC
/// session task, and shallow enough that a stalled CH9329 doesn't build up
/// seconds of stale input to replay.
const COMMAND_QUEUE_CAPACITY: usize = 256;

/// Every run starts here; mouse mode is never read from or written to disk.
const DEFAULT_MOUSE_MODE: MouseMode = MouseMode::Absolute;

/// What the peer did, in input terms. Callers never name a usage code, a
/// modifier bit or a report shape — translating this into CH9329 reports
/// is `writer`'s job.
#[derive(Debug, Clone)]
pub enum InputCommand {
    /// One physical key going down (`pressed`) or up, named by its browser
    /// `KeyboardEvent.code` (e.g. `"KeyA"`). Codes with no HID mapping are
    /// ignored. Which keys are currently held is tracked here, not by the
    /// caller, so nothing outside has to send a matching release.
    Key { code: String, pressed: bool },
    /// The pointer moved to a position, as a fraction of the video frame
    /// (0.0 = left/top edge, 1.0 = right/bottom).
    PointerMoveAbsolute { x_frac: f32, y_frac: f32 },
    /// The pointer moved by a delta, with the buttons held and any wheel
    /// movement at the same moment.
    PointerMoveRelative { buttons: u8, dx: i8, dy: i8, wheel: i8 },
    /// The buttons held and/or the wheel moved, with no pointer movement.
    PointerButtons { buttons: u8, wheel: i8 },
    /// Type `text` out as a sequence of keystrokes (US QWERTY only).
    PasteText(String),
}

impl InputCommand {
    /// A short label for logging — the caller's queue-time log and this
    /// module's write-time log use the same names for the same event.
    pub fn kind(&self) -> &'static str {
        match self {
            InputCommand::Key { .. } => "key",
            InputCommand::PointerMoveAbsolute { .. } => "pointer_move_absolute",
            InputCommand::PointerMoveRelative { .. } => "pointer_move_relative",
            InputCommand::PointerButtons { .. } => "pointer_buttons",
            InputCommand::PasteText(_) => "paste_text",
        }
    }
}

/// Which pointer report shape is in use. Owned here because it decides
/// what gets written to the chip, held in memory only for the life of the
/// process. The page picks which datagram it sends from this, so the
/// translation in `writer` doesn't currently branch on it — a click or
/// scroll goes out as a zero-delta relative report either way, since the
/// chip's absolute report carries position only (see `writer`'s docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseMode {
    Absolute,
    Relative,
}

/// The queue is gone, which only happens once the `Hid` that owns it has
/// been dropped - so the command was not, and never will be, delivered.
#[derive(Debug)]
pub struct QueueClosed;

impl fmt::Display for QueueClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the HID command queue is closed")
    }
}

impl std::error::Error for QueueClosed {}

pub struct Hid {
    /// The only strong sender - see the module docs on lifetimes.
    commands: mpsc::Sender<Command>,
    /// `is_present`/`add_event_listener` delegate straight to this -
    /// there's no settle delay of this module's own any more, so the
    /// `Ch9329Device` exists for `Hid`'s whole lifetime (the boot-crash
    /// mitigation now lives once, generically, in `device`'s own
    /// detect-to-probe delay).
    device: Ch9329Device,
    /// Kept alive only so its callback keeps firing - it prompts the
    /// worker to open or drop its port the instant presence changes,
    /// rather than waiting for the next command. Dropped, like everything
    /// else here, when `Hid` is.
    _presence_forward: Subscription<DeviceStatus<()>>,
    /// Read and written from both async tasks and the page's control
    /// channel, but never held across an `.await` - a plain `std` mutex
    /// rather than tokio's.
    mouse_mode: Mutex<MouseMode>,
    mouse_mode_events: Arc<EventEmitter<MouseMode>>,
}

impl Hid {
    pub fn spawn() -> Arc<Self> {
        Self::new(Ch9329Device::spawn())
    }

    /// Test-only: a `Hid` whose `Ch9329Device` points at a path that will
    /// never exist, so no port is ever opened - the queue still accepts
    /// commands, exactly as it does before the real chip is found.
    #[cfg(test)]
    pub fn spawn_for_test() -> Arc<Self> {
        Self::new(Ch9329Device::spawn_at("/nonexistent-simple-kvm-test-ch9329"))
    }

    fn new(device: Ch9329Device) -> Arc<Self> {
        let (commands_tx, commands_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);

        let forward_tx = commands_tx.downgrade();
        let presence_forward = device.add_event_listener(move |_status| {
            let forward_tx = forward_tx.clone();
            async move {
                // Prompts the worker to open or drop its port right away,
                // rather than only on the next real keystroke or click.
                if let Some(commands_tx) = forward_tx.upgrade() {
                    let _ = commands_tx.send(Command::CheckConnection).await;
                }
            }
        });

        let writer = writer::SerialWriter::new(device.clone());
        tokio::spawn(async move {
            let _ = tokio::task::spawn_blocking(move || writer.run(commands_rx)).await;
        });

        Arc::new(Self { commands: commands_tx, device, _presence_forward: presence_forward, mouse_mode: Mutex::new(DEFAULT_MOUSE_MODE), mouse_mode_events: Arc::new(EventEmitter::new()) })
    }

    /// Queues `command` for the CH9329. Commands reach the hardware in the
    /// order they were submitted, and queue rather than fail while the
    /// port isn't open yet.
    pub async fn send(&self, command: InputCommand) -> Result<(), QueueClosed> {
        self.commands.send(Command::Input(command)).await.map_err(|_| QueueClosed)
    }

    /// Whether the CH9329 is plugged in right now. The read counterpart of
    /// `add_event_listener`: presence events only fire on a transition, so
    /// a subscriber that starts after the chip was already found needs this
    /// to learn where it's starting from.
    pub fn is_present(&self) -> bool {
        self.device.is_present()
    }

    pub fn mouse_mode(&self) -> MouseMode {
        *self.mouse_mode.lock().unwrap()
    }

    /// Changes the pointer report shape in use and tells every listener,
    /// so a tab that's already open follows the change instead of being
    /// stuck on what it saw when it connected.
    pub fn set_mouse_mode(&self, mode: MouseMode) {
        *self.mouse_mode.lock().unwrap() = mode;
        self.mouse_mode_events.dispatch(mode);
    }

    /// Mirrors `addEventListener('change', cb)` for the mouse mode above.
    pub fn add_mouse_mode_listener<F, Fut>(&self, callback: F) -> Subscription<MouseMode>
    where
        F: Fn(MouseMode) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.mouse_mode_events.add_event_listener(callback)
    }

    /// Mirrors `addEventListener('devicechange', cb)` for the CH9329 -
    /// forwards presence exactly as `Device<Ch9329Driver>` reports it. The
    /// HID counterpart of subscribing directly on a `CaptureDevice` handle
    /// (see `CaptureCard::device`, `rtc::session`).
    pub fn add_event_listener<F, Fut>(&self, callback: F) -> Subscription<DeviceStatus<()>>
    where
        F: Fn(DeviceStatus<()>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.device.add_event_listener(callback)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn commands_queue_while_the_port_is_not_open_yet() {
        let hid = Hid::spawn_for_test();

        for _ in 0..8 {
            hid.send(InputCommand::PointerButtons { buttons: 0, wheel: 0 }).await.expect("commands must queue, not fail, before the port opens");
        }
    }

    #[tokio::test]
    async fn changing_mouse_mode_updates_the_current_value_and_tells_listeners() {
        let hid = Hid::spawn_for_test();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _sub = hid.add_mouse_mode_listener(move |mode| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(mode);
            }
        });

        assert_eq!(hid.mouse_mode(), MouseMode::Absolute);
        hid.set_mouse_mode(MouseMode::Relative);

        assert_eq!(hid.mouse_mode(), MouseMode::Relative);
        let seen = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.expect("the listener must fire").expect("the channel is still open");
        assert_eq!(seen, MouseMode::Relative);
    }
}
