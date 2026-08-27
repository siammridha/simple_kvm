//! Generic device-presence/capability-tracking core, mirroring
//! `navigator.mediaDevices` generalizing across camera, mic, and speaker
//! through one interface rather than a separate hand-rolled presence
//! module per physical device kind (see `docs/capture-redesign-ideas.md`,
//! "Decided: one generic device module, not one-off per device kind").
//! The capture card (`capture::driver::CaptureDriver`) and the CH9329
//! (`ch9329::device::Ch9329Driver`) each plug in their own `DeviceDriver`
//! impl for *how* to probe/open; this module owns everything
//! device-kind-independent: presence detection, event dispatch (via
//! `crate::event`), and encapsulating the raw device path.

use std::fmt;
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::event::{EventEmitter, Subscription};
use crate::uevent;

/// Backoff-free fallback poll interval, used only when the kernel uevent
/// listener itself failed to open (see `uevent::UeventListener::open`) -
/// mirrors `capture::DEVICE_POLL_INTERVAL`'s role for the same case.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// What differs between device kinds - probing and opening. Everything
/// else (presence detection, event dispatch, path encapsulation) is
/// shared by the generic `Device<D>` core below.
pub trait DeviceDriver: Send + Sync + 'static {
    type Info: Clone + Send + 'static;
    type Settings: Send + 'static;
    type Open: Send + 'static;

    /// Probes `device_path` for its capabilities. Never errors the caller -
    /// any failure (no such device, wrong kind, an ioctl error) is reported
    /// as `None`, the same contract `CaptureDriver::probe` follows.
    fn probe(device_path: &str) -> Option<Self::Info>;

    /// Opens `device_path` for actual use with the given settings.
    fn open(device_path: &str, settings: &Self::Settings) -> Result<Self::Open, OpenError>;
}

#[derive(Debug)]
pub struct OpenError(pub String);

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for OpenError {}

/// Whether a device is currently plugged in and, once probed, what it
/// reports about its own capabilities. `Present(None)` covers the
/// boot-time "already present, deliberately not probed" case (see
/// `PresenceState::observe`) - it persists until a genuine absent->present
/// transition actually probes the device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceStatus<Info> {
    Absent,
    Present(Option<Info>),
}

struct DeviceInner<D: DeviceDriver> {
    device_path: String,
    events: Arc<EventEmitter<DeviceStatus<D::Info>>>,
    current: Mutex<DeviceStatus<D::Info>>,
}

/// One generic presence/capability-tracking core, parameterized by a
/// `DeviceDriver`. Mirrors `navigator.mediaDevices`: subscribe for
/// presence/capability changes (`add_event_listener`, the
/// `ondevicechange` equivalent), or ask for a live handle
/// (`open`, the `getUserMedia` equivalent - fails immediately if the
/// device isn't currently present).
pub struct Device<D: DeviceDriver> {
    inner: Arc<DeviceInner<D>>,
}

/// Manual impl (rather than `#[derive(Clone)]`, which would otherwise
/// require `D: Clone` itself - no `DeviceDriver` impl needs that) - every
/// field this actually clones is already `Arc`-backed. Lets a caller (e.g.
/// `main.rs`) hold two independent handles to the same underlying presence
/// task/device path - one to build a `CaptureEngine` from, one to
/// subscribe to directly for `DeviceState` publishing - without the
/// presence task itself being duplicated.
impl<D: DeviceDriver> Clone for Device<D> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

impl<D: DeviceDriver> Device<D> {
    /// Starts the background presence task and returns a handle to it.
    /// `uevent_subsystem` is the kernel subsystem name to listen for on
    /// this device's uevent stream (e.g. `"video4linux"` for the capture
    /// card, `"tty"` for the CH9329 - see `uevent::UeventListener::
    /// wait_for_subsystem`); it isn't part of `DeviceDriver` itself since
    /// it names where the device lives, not how to probe/open it, the
    /// same kind of thing `device_path` already is.
    pub fn spawn(device_path: impl Into<String>, uevent_subsystem: impl Into<String>) -> Self {
        let device_path = device_path.into();
        let inner = Arc::new(DeviceInner { device_path, events: Arc::new(EventEmitter::new()), current: Mutex::new(DeviceStatus::Absent) });

        let task_inner = Arc::clone(&inner);
        tokio::spawn(run_presence_task::<D>(task_inner, uevent_subsystem.into()));

        Self { inner }
    }

    /// Mirrors `addEventListener('devicechange', cb)`.
    pub fn add_event_listener<F, Fut>(&self, callback: F) -> Subscription<DeviceStatus<D::Info>>
    where
        F: Fn(DeviceStatus<D::Info>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.inner.events.add_event_listener(callback)
    }

    /// Mirrors `getUserMedia()` rejecting when no matching device exists:
    /// fails immediately if the device isn't currently present, otherwise
    /// delegates straight to `D::open`. Never touches the device path
    /// itself - the path stays private to this module (see the module
    /// doc comment).
    pub fn open(&self, settings: &D::Settings) -> Result<D::Open, OpenError> {
        match &*self.inner.current.lock().unwrap() {
            DeviceStatus::Present(_) => D::open(&self.inner.device_path, settings),
            DeviceStatus::Absent => Err(OpenError("device is not currently present".to_string())),
        }
    }

    /// Test-only: builds a `Device` in a given status without spawning
    /// the real presence task, so `open`'s present/absent gating can be
    /// tested without real hardware, a real device path, or real timing.
    #[cfg(test)]
    fn from_status(device_path: impl Into<String>, status: DeviceStatus<D::Info>) -> Self {
        Self { inner: Arc::new(DeviceInner { device_path: device_path.into(), events: Arc::new(EventEmitter::new()), current: Mutex::new(status) }) }
    }
}

/// Runs forever, dispatching a `DeviceStatus` change through `inner.events`
/// each time `PresenceState::observe` reports one. Owns the only I/O in
/// this module - the filesystem presence check and the uevent wait - so
/// that `PresenceState` itself stays pure and unit-testable without either
/// (see `PresenceState`'s doc comment).
async fn run_presence_task<D: DeviceDriver>(inner: Arc<DeviceInner<D>>, uevent_subsystem: String) {
    let mut state = PresenceState::<D>::new();
    let mut uevents = match uevent::UeventListener::open() {
        Ok(listener) => Some(listener),
        Err(err) => {
            tracing::warn!(%err, device_path = %inner.device_path, "failed to open kernel uevent listener, this device's reconnects will only be noticed on the poll interval");
            None
        }
    };

    loop {
        let device_present = Path::new(&inner.device_path).exists();
        if let Some(status) = state.observe(&inner.device_path, device_present) {
            *inner.current.lock().unwrap() = status.clone();
            inner.events.dispatch(status);
        }

        match &mut uevents {
            Some(listener) => listener.wait_for_subsystem(&uevent_subsystem).await,
            None => tokio::time::sleep(DEVICE_POLL_INTERVAL).await,
        }
    }
}

/// The present/absent/first-check-skips-probe decision logic, factored out
/// of `run_presence_task` so it's testable against a fake `DeviceDriver`
/// with no real filesystem path or uevent socket involved - `observe`
/// takes "is the device present right now" as a plain `bool` from its
/// caller rather than checking `Path::exists` itself. Moved and
/// generalized from the `device_present`/`known_present`/`first_check`
/// variables and loop structure the capture card's presence handling used
/// before this module existed, not reinvented.
struct PresenceState<D: DeviceDriver> {
    known_present: bool,
    first_check: bool,
    current_info: Option<D::Info>,
}

impl<D: DeviceDriver> PresenceState<D> {
    fn new() -> Self {
        Self { known_present: false, first_check: true, current_info: None }
    }

    /// Returns the new status to publish if this call caused a
    /// transition, or `None` if presence didn't change. The very first
    /// call ever made, if `device_present` is already `true`, is the
    /// boot-crash-risk moment (real hardware, right after USB enumeration
    /// finishes at startup) and deliberately never calls `D::probe`;
    /// every later genuine absent->present transition does.
    fn observe(&mut self, device_path: &str, device_present: bool) -> Option<DeviceStatus<D::Info>> {
        let skip_probe_this_transition = self.first_check && device_present;
        self.first_check = false;

        if !device_present {
            if self.known_present {
                self.known_present = false;
                self.current_info = None;
                return Some(DeviceStatus::Absent);
            }
            return None;
        }

        if self.known_present {
            return None;
        }

        self.known_present = true;
        if !skip_probe_this_transition {
            self.current_info = D::probe(device_path);
        }
        Some(DeviceStatus::Present(self.current_info.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration as StdDuration;
    use tokio::sync::mpsc;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct FakeInfo(u32);

    #[test]
    fn boot_time_already_present_does_not_probe() {
        struct Fake;
        static PROBE_CALLS: AtomicUsize = AtomicUsize::new(0);
        impl DeviceDriver for Fake {
            type Info = FakeInfo;
            type Settings = ();
            type Open = ();
            fn probe(_device_path: &str) -> Option<Self::Info> {
                PROBE_CALLS.fetch_add(1, Ordering::SeqCst);
                Some(FakeInfo(1))
            }
            fn open(_device_path: &str, _settings: &Self::Settings) -> Result<Self::Open, OpenError> {
                Ok(())
            }
        }

        let mut state = PresenceState::<Fake>::new();
        let status = state.observe("dummy", true);

        assert_eq!(PROBE_CALLS.load(Ordering::SeqCst), 0, "the very first check must never probe when already present");
        assert!(matches!(status, Some(DeviceStatus::Present(None))));
    }

    #[test]
    fn genuine_absent_to_present_transition_probes() {
        struct Fake;
        static PROBE_CALLS: AtomicUsize = AtomicUsize::new(0);
        impl DeviceDriver for Fake {
            type Info = FakeInfo;
            type Settings = ();
            type Open = ();
            fn probe(_device_path: &str) -> Option<Self::Info> {
                PROBE_CALLS.fetch_add(1, Ordering::SeqCst);
                Some(FakeInfo(2))
            }
            fn open(_device_path: &str, _settings: &Self::Settings) -> Result<Self::Open, OpenError> {
                Ok(())
            }
        }

        let mut state = PresenceState::<Fake>::new();
        let first = state.observe("dummy", false);
        assert!(first.is_none(), "starting absent is not itself a transition");
        assert_eq!(PROBE_CALLS.load(Ordering::SeqCst), 0);

        let second = state.observe("dummy", true);
        assert_eq!(PROBE_CALLS.load(Ordering::SeqCst), 1, "a genuine absent->present transition must probe");
        assert!(matches!(second, Some(DeviceStatus::Present(Some(FakeInfo(2))))));
    }

    #[tokio::test]
    async fn device_status_change_dispatches_to_subscribers() {
        struct Fake;
        impl DeviceDriver for Fake {
            type Info = FakeInfo;
            type Settings = ();
            type Open = ();
            fn probe(_device_path: &str) -> Option<Self::Info> {
                Some(FakeInfo(9))
            }
            fn open(_device_path: &str, _settings: &Self::Settings) -> Result<Self::Open, OpenError> {
                Ok(())
            }
        }

        let events = Arc::new(EventEmitter::<DeviceStatus<FakeInfo>>::new());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _sub = events.add_event_listener(move |status| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(status);
            }
        });

        let mut state = PresenceState::<Fake>::new();
        let status = state.observe("dummy", true).expect("boot-time present is a transition");
        events.dispatch(status);

        let received = tokio::time::timeout(StdDuration::from_secs(1), rx.recv()).await.expect("subscriber should have been notified").expect("channel should still be open");
        assert!(matches!(received, DeviceStatus::Present(None)));
    }

    #[test]
    fn open_fails_immediately_when_not_present() {
        struct Fake;
        impl DeviceDriver for Fake {
            type Info = FakeInfo;
            type Settings = ();
            type Open = ();
            fn probe(_device_path: &str) -> Option<Self::Info> {
                None
            }
            fn open(_device_path: &str, _settings: &Self::Settings) -> Result<Self::Open, OpenError> {
                panic!("open must not be called when the device isn't present");
            }
        }

        let device = Device::<Fake>::from_status("dummy", DeviceStatus::Absent);
        assert!(device.open(&()).is_err());
    }

    #[test]
    fn open_delegates_to_driver_when_present() {
        struct Fake;
        impl DeviceDriver for Fake {
            type Info = FakeInfo;
            type Settings = ();
            type Open = &'static str;
            fn probe(_device_path: &str) -> Option<Self::Info> {
                Some(FakeInfo(1))
            }
            fn open(_device_path: &str, _settings: &Self::Settings) -> Result<Self::Open, OpenError> {
                Ok("opened")
            }
        }

        let device = Device::<Fake>::from_status("dummy", DeviceStatus::Present(Some(FakeInfo(1))));
        assert_eq!(device.open(&()).unwrap(), "opened");
    }
}
