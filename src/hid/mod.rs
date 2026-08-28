//! The CH9329 keyboard/mouse bridge, as one owned object (`Hid`).
//!
//! `Hid` holds the `Ch9329Device`, opens its serial channel through the
//! Device API (`Device::open` - the only open path in this module), owns
//! the command queue, and spawns its own drain worker. Its whole public
//! surface is `send` plus `add_event_listener`: no channel sender, no
//! serial handle and no device path leaves it (`ARCHITECTURE.md` §3.3).
//!
//! Lifetimes follow ownership: `Hid` holds the only strong queue sender,
//! so dropping it closes the queue, which ends the blocking drain worker,
//! which ends the task awaiting it.

pub mod keymap;
mod paste;
mod protocol;
mod writer;

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::device::{Ch9329Device, DeviceStatus, EventEmitter, Subscription};

pub use writer::SerialCommand;

/// Deep enough that a burst of mouse-move reports never blocks the WebRTC
/// session task, and shallow enough that a stalled CH9329 doesn't build up
/// seconds of stale input to replay.
const COMMAND_QUEUE_CAPACITY: usize = 256;

/// How long to wait before first opening the CH9329, giving its USB
/// enumeration time to settle - the same crash-avoidance reasoning as the
/// capture card's boot delay (see `deploy/install.sh`). Commands sent
/// during the wait queue up rather than being lost, so this never holds up
/// the HTTP page starting.
const OPEN_DELAY_ENV_VAR: &str = "SERIAL_OPEN_DELAY_SECS";
const DEFAULT_OPEN_DELAY_SECS: u64 = 30;

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
    commands: mpsc::Sender<SerialCommand>,
    /// This module's own `devicechange` emitter rather than the device's:
    /// subscribers exist from startup, while the `Ch9329Device` itself
    /// isn't spawned until the settle delay has passed.
    events: Arc<EventEmitter<DeviceStatus<()>>>,
}

impl Hid {
    pub fn spawn() -> Arc<Self> {
        Self::spawn_with_delay(Duration::from_secs(configured_open_delay_secs()))
    }

    /// Test-only: a `Hid` whose settle delay outlasts any test, so no
    /// `Ch9329Device` is ever spawned and no port is ever opened - the
    /// queue still accepts commands, exactly as it does at startup.
    #[cfg(test)]
    pub fn spawn_for_test() -> Arc<Self> {
        Self::spawn_with_delay(Duration::from_secs(3600))
    }

    fn spawn_with_delay(open_delay: Duration) -> Arc<Self> {
        let (commands_tx, commands_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let events = Arc::new(EventEmitter::new());

        tokio::spawn(open_after_delay(open_delay, commands_tx.downgrade(), commands_rx, Arc::clone(&events)));

        Arc::new(Self { commands: commands_tx, events })
    }

    /// Queues `command` for the CH9329. Commands reach the hardware in the
    /// order they were submitted, and queue rather than fail while the
    /// port isn't open yet.
    pub async fn send(&self, command: SerialCommand) -> Result<(), QueueClosed> {
        self.commands.send(command).await.map_err(|_| QueueClosed)
    }

    /// Mirrors `addEventListener('devicechange', cb)` for the CH9329 -
    /// forwards presence exactly as `Device<Ch9329Driver>` reports it,
    /// the HID counterpart of `CaptureEngine::add_event_listener`.
    pub fn add_event_listener<F, Fut>(&self, callback: F) -> Subscription<DeviceStatus<()>>
    where
        F: Fn(DeviceStatus<()>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.events.add_event_listener(callback)
    }
}

fn configured_open_delay_secs() -> u64 {
    std::env::var(OPEN_DELAY_ENV_VAR).ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_OPEN_DELAY_SECS)
}

/// Waits out the settle delay, then starts presence detection and the
/// drain worker. Holds only a `WeakSender`, so a `Hid` dropped either
/// during the delay or while the worker is running still closes the
/// queue and ends everything here.
///
/// Presence detection itself is harmless (a filesystem check plus a kernel
/// uevent listener, no device I/O), but it's still started only after the
/// delay so the browser learns about CH9329 connectivity at the same point
/// in time it always has. The worker re-checks presence before every
/// command, so there's nothing to decide up front here.
async fn open_after_delay(
    open_delay: Duration,
    commands_tx: mpsc::WeakSender<SerialCommand>,
    commands_rx: mpsc::Receiver<SerialCommand>,
    events: Arc<EventEmitter<DeviceStatus<()>>>,
) {
    if !open_delay.is_zero() {
        tracing::info!(seconds = open_delay.as_secs(), "waiting before opening CH9329 serial port");
        tokio::time::sleep(open_delay).await;
    }
    if commands_tx.upgrade().is_none() {
        return; // dropped during the delay - nothing left to serve
    }

    let device = Ch9329Device::spawn();
    let _presence_sub = device.add_event_listener(move |status| {
        let events = Arc::clone(&events);
        let commands_tx = commands_tx.clone();
        async move {
            events.dispatch(status);
            // Prompts the worker to open or drop its port right away,
            // rather than only on the next real keystroke or click.
            if let Some(commands_tx) = commands_tx.upgrade() {
                let _ = commands_tx.send(SerialCommand::CheckConnection).await;
            }
        }
    });

    let writer = writer::SerialWriter::new(device);
    let _ = tokio::task::spawn_blocking(move || writer.run(commands_rx)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn commands_queue_while_the_port_is_not_open_yet() {
        // Long enough that the settle delay is still running throughout.
        let hid = Hid::spawn_with_delay(Duration::from_secs(600));

        for _ in 0..8 {
            hid.send(SerialCommand::MouseButtons { buttons: 0, wheel: 0 }).await.expect("commands must queue, not fail, before the port opens");
        }
    }
}
