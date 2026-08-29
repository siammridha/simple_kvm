//! Generic device-presence/capability-tracking core, mirroring
//! `navigator.mediaDevices` generalizing across camera, mic, and speaker
//! through one interface rather than a separate hand-rolled presence
//! module per physical device kind (see `docs/capture-redesign-ideas.md`,
//! "Decided: one generic device module, not one-off per device kind").
//! The capture card (`capture_driver`) and the CH9329 (`ch9329_driver`)
//! each plug in their own `DeviceDriver` impl for *how* to probe/open;
//! this module owns everything device-kind-independent: presence
//! detection, event dispatch (via `event`), and encapsulating the raw
//! device path.
//!
//! The drivers live here, not in the modules that consume them, because
//! `probe`/`open` are the only calls that touch a device path or do a raw
//! OS open. What they hand back is an already-open OS handle; no handle
//! ever exposes the path it came from (see `ARCHITECTURE.md` I3).

mod capture_driver;
mod ch9329_driver;
mod event;
mod uevent;

use std::fmt;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// The emitter/subscription pair is owned here because `Device`'s
/// `devicechange` events are its original reason to exist, but every
/// `add_event_listener` in the codebase returns a `Subscription`, so
/// `capture` and `rtc` have to be able to name it. Re-exporting rather
/// than duplicating keeps one implementation and adds no dependency edge:
/// both already depend on `device`. `StateEmitter` comes along for the
/// same reason - `capture` needs it for a `CaptureStream`'s `ended`.
pub use event::{EventEmitter, StateEmitter, Subscription};

/// The driver types themselves stay unexported - a device kind's whole
/// public surface is its `Device<D>` alias, the handle its `open` hands
/// back, and (for the capture card) the types its driver produces.
pub use capture_driver::{CaptureDevice, CaptureHandle, CaptureSettings, Resolution, SupportedFormat};
pub use ch9329_driver::Ch9329Device;

/// How many times to retry opening the kernel uevent listener before
/// giving up on tracking a device's presence altogether. There is no
/// fallback poll of the device path: presence is learned exclusively from
/// uevents, so a listener that still won't open after retrying really
/// can't be worked around - see `ARCHITECTURE.md`'s "no fallback polling
/// for device presence".
const UEVENT_LISTENER_OPEN_ATTEMPTS: u32 = 5;

/// Delay between attempts to open the kernel uevent listener.
const UEVENT_LISTENER_RETRY_DELAY: Duration = Duration::from_secs(1);

/// How long to wait after presence is first detected before actually
/// probing the device - real hardware (the capture card and the CH9329
/// alike) has crashed when opened/probed too soon after USB enumeration,
/// including right at boot if the device was already plugged in. Applies
/// uniformly to every `DeviceDriver`, and to every absent->present
/// transition, not just the very first one: presence itself is reported
/// (logged) the instant it's seen, since noticing is harmless; only the
/// probe - which does touch the hardware - waits.
const DETECT_TO_PROBE_DELAY: Duration = Duration::from_secs(5);

/// What differs between device kinds - where the device lives, and how to
/// probe and open it. Everything else (presence detection, event
/// dispatch, path encapsulation) is shared by the generic `Device<D>`
/// core below.
///
/// Where the device lives is stated here rather than passed to `spawn`
/// because it describes the device *kind*, not one caller's wish: the
/// CH9329 always appears under `tty`, the capture card always under
/// `video4linux`, and each kind has exactly one environment variable
/// naming its path. Keeping all three here is what lets `spawn` take no
/// arguments at all, so no caller ever holds a device path
/// (`ARCHITECTURE.md` I2/I3).
pub trait DeviceDriver: Send + Sync + 'static {
    type Info: Clone + std::fmt::Debug + Send + 'static;
    type Settings: Send + 'static;
    type Open: Send + 'static;

    /// Kernel subsystem name this device kind appears under on the uevent
    /// stream (see `uevent::UeventListener::wait_for_subsystem`).
    const UEVENT_SUBSYSTEM: &'static str;

    /// Environment variable naming this device's path, and the path used
    /// when it isn't set.
    const PATH_ENV_VAR: &'static str;
    const DEFAULT_PATH: &'static str;

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
/// reports about its own capabilities. `Present(None)` means the device
/// was found but `D::probe` itself failed (no such device, wrong kind, an
/// ioctl error) - the same "never errors the caller" contract `probe`
/// documents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceStatus<Info> {
    Absent,
    Present(Option<Info>),
}

struct DeviceInner<D: DeviceDriver> {
    device_path: String,
    /// A `StateEmitter`, not a plain `EventEmitter`: the presence task is
    /// already running by the time anyone can call `add_event_listener`
    /// (`spawn` starts it), so a subscriber can arrive after the first
    /// status was published and must still be told what it is. It doubles
    /// as this device's current-status store, so `is_present`/`open` can
    /// never disagree with what subscribers were last told.
    events: Arc<StateEmitter<DeviceStatus<D::Info>>>,
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
/// task/device path - one to build a `CaptureCard` from, one to
/// subscribe to directly for `DeviceState` publishing - without the
/// presence task itself being duplicated.
impl<D: DeviceDriver> Clone for Device<D> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

impl<D: DeviceDriver> Device<D> {
    /// Starts the background presence task and returns a handle to it.
    /// Takes nothing: the path comes from this device kind's own
    /// environment variable (`D::PATH_ENV_VAR`, falling back to
    /// `D::DEFAULT_PATH`) and the uevent subsystem from `D` too, so the
    /// path is read here and never travels through a caller.
    pub fn spawn() -> Self {
        Self::spawn_at_path(std::env::var(D::PATH_ENV_VAR).unwrap_or_else(|_| D::DEFAULT_PATH.to_string()), DETECT_TO_PROBE_DELAY)
    }

    /// `spawn` against an explicit path. Test-only: the whole point of
    /// `spawn` is that no caller supplies a path, but a test needs to
    /// point a device at a temp file (present) or at a path that will
    /// never exist (absent) without mutating process-wide environment.
    #[cfg(test)]
    pub fn spawn_at(device_path: impl Into<String>) -> Self {
        Self::spawn_at_path(device_path, DETECT_TO_PROBE_DELAY)
    }

    /// `spawn_at` with no detect-to-probe delay. Test-only: for a test
    /// that needs a fast, deterministic `Present` dispatch against a real
    /// (fake) path and isn't testing the delay itself - the delay is
    /// covered on its own by `PresenceState`'s tests.
    #[cfg(test)]
    pub fn spawn_at_immediate(device_path: impl Into<String>) -> Self {
        Self::spawn_at_path(device_path, Duration::ZERO)
    }

    fn spawn_at_path(device_path: impl Into<String>, probe_delay: Duration) -> Self {
        let device_path = device_path.into();
        let inner = Arc::new(DeviceInner { device_path, events: Arc::new(StateEmitter::new()) });

        let task_inner = Arc::clone(&inner);
        tokio::spawn(run_presence_task::<D>(task_inner, probe_delay));

        Self { inner }
    }

    /// Mirrors `addEventListener('devicechange', cb)`. A listener that
    /// subscribes after the presence task has already published a status
    /// is called once with that status straight away, so it can't be left
    /// waiting for a transition that already happened (see
    /// `StateEmitter`).
    pub fn add_event_listener<F, Fut>(&self, callback: F) -> Subscription<DeviceStatus<D::Info>>
    where
        F: Fn(DeviceStatus<D::Info>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.inner.events.add_event_listener(callback)
    }

    /// Mirrors `getUserMedia()` rejecting when no matching device exists:
    /// fails immediately if the device isn't currently present, otherwise
    /// delegates straight to `D::open`. Rechecks the actual device path,
    /// not just the last-published status, right before opening - closes
    /// the race where the device disappeared after the last presence
    /// event but before this call (the presence task only notices on the
    /// next uevent wake-up, which could be later). Never exposes the
    /// device path itself - the path stays private to this module (see
    /// the module doc comment).
    pub fn open(&self, settings: &D::Settings) -> Result<D::Open, OpenError> {
        if self.is_present() && Path::new(&self.inner.device_path).exists() {
            D::open(&self.inner.device_path, settings)
        } else {
            Err(OpenError("device is not currently present".to_string()))
        }
    }

    /// Whether the device is plugged in right now - exactly the gate
    /// `open` applies, exposed on its own so a caller can reject early
    /// (the way `getUserMedia` rejects with no matching device) without
    /// performing a real open. A real open is neither free nor repeatable:
    /// opening the capture card negotiates a format, which only works
    /// while nothing else is already streaming from it, so `capture`
    /// checks presence per consumer but opens once per encode pass.
    pub fn is_present(&self) -> bool {
        matches!(self.inner.events.latest(), Some(DeviceStatus::Present(_)))
    }

    /// The full status last published, mirroring `is_present`'s "ask
    /// without subscribing" contract but carrying the probed capabilities
    /// too - lets a caller (e.g. `rtc`, computing device state for a
    /// session that's only just subscribing) read what's currently known
    /// without waiting for the next transition or paying for a fresh
    /// probe.
    pub fn latest_status(&self) -> Option<DeviceStatus<D::Info>> {
        self.inner.events.latest()
    }

    /// Test-only: builds a `Device` in a given status without spawning
    /// the real presence task, so `open`'s present/absent gating can be
    /// tested without real hardware, a real device path, or real timing.
    #[cfg(test)]
    fn from_status(device_path: impl Into<String>, status: DeviceStatus<D::Info>) -> Self {
        let events = Arc::new(StateEmitter::new());
        // No listener exists yet, so this only records the status - it
        // spawns nothing and needs no runtime.
        events.dispatch(status);
        Self { inner: Arc::new(DeviceInner { device_path: device_path.into(), events }) }
    }
}

/// Runs forever, dispatching a `DeviceStatus` change through `inner.events`
/// each time `PresenceState::observe` reports one. Owns the only I/O in
/// this module - the filesystem presence check, the detect-to-probe delay,
/// the probe call itself, and the uevent wait - so that `PresenceState`
/// itself stays pure and unit-testable without any of that (see
/// `PresenceState`'s doc comment).
async fn run_presence_task<D: DeviceDriver>(inner: Arc<DeviceInner<D>>, probe_delay: Duration) {
    let mut state = PresenceState::new();
    let Some(mut uevents) = open_uevent_listener_with_retries(&inner.device_path).await else {
        return;
    };

    loop {
        let device_present = Path::new(&inner.device_path).exists();
        match state.observe(device_present) {
            Some(PresenceTransition::Lost) => {
                tracing::info!(device_path = %inner.device_path, "device disconnected");
                inner.events.dispatch(DeviceStatus::Absent);
            }
            Some(PresenceTransition::Detected) => {
                tracing::info!(device_path = %inner.device_path, "device connected");
                if !probe_delay.is_zero() {
                    tokio::time::sleep(probe_delay).await;
                }
                if Path::new(&inner.device_path).exists() {
                    tracing::debug!(device_path = %inner.device_path, "probing device");
                    let info = D::probe(&inner.device_path);
                    tracing::info!(device_path = %inner.device_path, ?info, "device probed");
                    inner.events.dispatch(DeviceStatus::Present(info));
                } else {
                    tracing::info!(device_path = %inner.device_path, "device disappeared during the detect-to-probe delay, not probing");
                    state.reset();
                }
            }
            None => {}
        }

        uevents.wait_for_subsystem(D::UEVENT_SUBSYSTEM).await;
    }
}

/// Tries to open the kernel uevent listener up to
/// `UEVENT_LISTENER_OPEN_ATTEMPTS` times, waiting
/// `UEVENT_LISTENER_RETRY_DELAY` between attempts. Returns `None` once
/// every attempt has failed, meaning the caller must give up on tracking
/// this device's presence rather than fall back to polling its path.
async fn open_uevent_listener_with_retries(device_path: &str) -> Option<uevent::UeventListener> {
    for attempt in 1..=UEVENT_LISTENER_OPEN_ATTEMPTS {
        match uevent::UeventListener::open() {
            Ok(listener) => return Some(listener),
            Err(err) => {
                tracing::warn!(%err, device_path = %device_path, attempt, max_attempts = UEVENT_LISTENER_OPEN_ATTEMPTS, "failed to open kernel uevent listener");
                if attempt < UEVENT_LISTENER_OPEN_ATTEMPTS {
                    tokio::time::sleep(UEVENT_LISTENER_RETRY_DELAY).await;
                }
            }
        }
    }
    tracing::error!(device_path = %device_path, attempts = UEVENT_LISTENER_OPEN_ATTEMPTS, "giving up opening kernel uevent listener; this device's presence will no longer be tracked");
    None
}

/// A genuine presence transition, with the actual probing left to the
/// caller (see `run_presence_task`) - `Detected` fires before the
/// detect-to-probe delay, `Lost` needs no delay at all.
#[derive(Debug, PartialEq, Eq)]
enum PresenceTransition {
    Detected,
    Lost,
}

/// The present/absent transition-detection logic, factored out of
/// `run_presence_task` so it's testable with no real filesystem path,
/// uevent socket, or timer involved - `observe` takes "is the device
/// present right now" as a plain `bool` from its caller rather than
/// checking `Path::exists` itself, and reports only *that a transition
/// happened*, never touching `D::probe`. Moved and generalized from the
/// `device_present`/`known_present` variables and loop structure the
/// capture card's presence handling used before this module existed, not
/// reinvented.
struct PresenceState {
    known_present: bool,
}

impl PresenceState {
    fn new() -> Self {
        Self { known_present: false }
    }

    /// Returns the transition this call caused, or `None` if presence
    /// didn't change. Every absent->present transition is reported the
    /// same way, including the very first call ever made if the device is
    /// already present then - the caller decides how long to wait before
    /// actually probing.
    fn observe(&mut self, device_present: bool) -> Option<PresenceTransition> {
        if !device_present {
            if self.known_present {
                self.known_present = false;
                return Some(PresenceTransition::Lost);
            }
            return None;
        }

        if self.known_present {
            return None;
        }

        self.known_present = true;
        Some(PresenceTransition::Detected)
    }

    /// Drops back to "not present" without producing a `Lost` transition -
    /// for when a caller saw `Detected` but then found the device gone
    /// before finishing whatever that transition triggers (e.g. probing),
    /// so nothing was ever announced and there is nothing to take back.
    /// Leaves the next genuine appearance to be reported as a fresh
    /// `Detected`, rather than being silently swallowed because this state
    /// still thought the device was present.
    fn reset(&mut self) {
        self.known_present = false;
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
    fn boot_time_already_present_is_still_a_detected_transition() {
        let mut state = PresenceState::new();
        assert_eq!(state.observe(true), Some(PresenceTransition::Detected), "the very first check, if already present, is still a transition - the caller decides how long to wait before probing");
        assert_eq!(state.observe(true), None, "no change while still present");
    }

    #[test]
    fn genuine_absent_to_present_transition_is_detected() {
        let mut state = PresenceState::new();
        assert_eq!(state.observe(false), None, "starting absent is not itself a transition");
        assert_eq!(state.observe(true), Some(PresenceTransition::Detected));
    }

    #[test]
    fn present_to_absent_transition_is_lost() {
        let mut state = PresenceState::new();
        state.observe(true);
        assert_eq!(state.observe(false), Some(PresenceTransition::Lost));
        assert_eq!(state.observe(false), None, "no change while still absent");
    }

    #[test]
    fn reset_lets_the_next_appearance_be_detected_again() {
        let mut state = PresenceState::new();
        state.observe(true);
        state.reset();
        assert_eq!(state.observe(true), Some(PresenceTransition::Detected), "reset must clear the known-present flag so a later appearance isn't swallowed");
    }

    #[tokio::test]
    async fn probe_is_deferred_until_the_detect_to_probe_delay_elapses() {
        struct Fake;
        static PROBE_CALLS: AtomicUsize = AtomicUsize::new(0);
        impl DeviceDriver for Fake {
            const UEVENT_SUBSYSTEM: &'static str = "fake";
            const PATH_ENV_VAR: &'static str = "SIMPLE_KVM_FAKE_DEVICE_PROBE_DELAY";
            const DEFAULT_PATH: &'static str = "/nonexistent/simple-kvm-fake-device";
            type Info = FakeInfo;
            type Settings = ();
            type Open = ();
            fn probe(_device_path: &str) -> Option<Self::Info> {
                PROBE_CALLS.fetch_add(1, Ordering::SeqCst);
                Some(FakeInfo(7))
            }
            fn open(_device_path: &str, _settings: &Self::Settings) -> Result<Self::Open, OpenError> {
                Ok(())
            }
        }

        let tmp = TempDevicePath::new();
        let probe_delay = StdDuration::from_millis(200);
        let device = Device::<Fake>::spawn_at_path(tmp.as_str().to_string(), probe_delay);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _sub = device.add_event_listener(move |status| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(status);
            }
        });

        tokio::time::sleep(probe_delay / 2).await;
        assert_eq!(PROBE_CALLS.load(Ordering::SeqCst), 0, "still inside the delay window - must not have probed yet");

        let received = tokio::time::timeout(StdDuration::from_secs(2), rx.recv()).await.expect("subscriber should have been notified once the delay elapses").expect("channel should still be open");
        assert!(matches!(received, DeviceStatus::Present(Some(FakeInfo(7)))));
        assert_eq!(PROBE_CALLS.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn probe_is_skipped_when_the_device_disappears_during_the_detect_to_probe_delay() {
        struct Fake;
        static PROBE_CALLS: AtomicUsize = AtomicUsize::new(0);
        impl DeviceDriver for Fake {
            const UEVENT_SUBSYSTEM: &'static str = "fake";
            const PATH_ENV_VAR: &'static str = "SIMPLE_KVM_FAKE_DEVICE_VANISHES_DURING_DELAY";
            const DEFAULT_PATH: &'static str = "/nonexistent/simple-kvm-fake-device";
            type Info = FakeInfo;
            type Settings = ();
            type Open = ();
            fn probe(_device_path: &str) -> Option<Self::Info> {
                PROBE_CALLS.fetch_add(1, Ordering::SeqCst);
                Some(FakeInfo(9))
            }
            fn open(_device_path: &str, _settings: &Self::Settings) -> Result<Self::Open, OpenError> {
                Ok(())
            }
        }

        let tmp = TempDevicePath::new();
        let probe_delay = StdDuration::from_millis(150);
        let device = Device::<Fake>::spawn_at_path(tmp.as_str().to_string(), probe_delay);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _sub = device.add_event_listener(move |status| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(status);
            }
        });

        // Remove the device partway through the delay, before the probe
        // would otherwise fire.
        tokio::time::sleep(probe_delay / 3).await;
        drop(tmp);

        // Give the delay time to fully elapse and the task time to make its
        // decision.
        tokio::time::sleep(probe_delay).await;

        assert_eq!(PROBE_CALLS.load(Ordering::SeqCst), 0, "must not probe a device that disappeared during the delay");
        assert!(rx.try_recv().is_err(), "must not announce Present for a device that was never actually there when the delay elapsed");
    }

    struct TempDevicePath(std::path::PathBuf);

    impl TempDevicePath {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("simple-kvm-device-test-{}", unique_suffix()));
            std::fs::write(&path, b"not a real device").unwrap();
            Self(path)
        }

        fn as_str(&self) -> &str {
            self.0.to_str().unwrap()
        }
    }

    impl Drop for TempDevicePath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn unique_suffix() -> u128 {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        now ^ (counter as u128)
    }

    #[test]
    fn open_fails_immediately_when_not_present() {
        struct Fake;
        impl DeviceDriver for Fake {
            const UEVENT_SUBSYSTEM: &'static str = "fake";
            const PATH_ENV_VAR: &'static str = "SIMPLE_KVM_FAKE_DEVICE";
            const DEFAULT_PATH: &'static str = "/nonexistent/simple-kvm-fake-device";
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
            const UEVENT_SUBSYSTEM: &'static str = "fake";
            const PATH_ENV_VAR: &'static str = "SIMPLE_KVM_FAKE_DEVICE";
            const DEFAULT_PATH: &'static str = "/nonexistent/simple-kvm-fake-device";
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

        // `open`'s fresh recheck needs a path that actually exists, unlike
        // the other `from_status` tests here that only exercise the
        // cached-status gate.
        let tmp = TempDevicePath::new();
        let device = Device::<Fake>::from_status(tmp.as_str(), DeviceStatus::Present(Some(FakeInfo(1))));
        assert_eq!(device.open(&()).unwrap(), "opened");
    }

    #[test]
    fn open_fails_when_cached_present_but_the_path_no_longer_exists() {
        struct Fake;
        impl DeviceDriver for Fake {
            const UEVENT_SUBSYSTEM: &'static str = "fake";
            const PATH_ENV_VAR: &'static str = "SIMPLE_KVM_FAKE_DEVICE";
            const DEFAULT_PATH: &'static str = "/nonexistent/simple-kvm-fake-device";
            type Info = FakeInfo;
            type Settings = ();
            type Open = ();
            fn probe(_device_path: &str) -> Option<Self::Info> {
                Some(FakeInfo(1))
            }
            fn open(_device_path: &str, _settings: &Self::Settings) -> Result<Self::Open, OpenError> {
                panic!("open must not be called when the device path no longer exists, even if the last-known status said present");
            }
        }

        // The cached status says "present", but the path itself was never
        // created, so the fresh recheck inside `open` must catch this and
        // fail rather than trust the stale status alone.
        let device = Device::<Fake>::from_status("/nonexistent/simple-kvm-open-recheck-test", DeviceStatus::Present(Some(FakeInfo(1))));
        assert!(device.open(&()).is_err());
    }

    #[tokio::test]
    async fn a_late_subscriber_is_told_the_status_it_missed() {
        struct Fake;
        impl DeviceDriver for Fake {
            const UEVENT_SUBSYSTEM: &'static str = "fake";
            const PATH_ENV_VAR: &'static str = "SIMPLE_KVM_FAKE_DEVICE";
            const DEFAULT_PATH: &'static str = "/nonexistent/simple-kvm-fake-device";
            type Info = FakeInfo;
            type Settings = ();
            type Open = ();
            fn probe(_device_path: &str) -> Option<Self::Info> {
                Some(FakeInfo(4))
            }
            fn open(_device_path: &str, _settings: &Self::Settings) -> Result<Self::Open, OpenError> {
                Ok(())
            }
        }

        // `spawn` starts the presence task before its caller can possibly
        // subscribe, so the first status can be published with nobody
        // listening - a subscriber arriving afterwards must still be told.
        let device = Device::<Fake>::from_status("dummy", DeviceStatus::Present(Some(FakeInfo(4))));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _sub = device.add_event_listener(move |status| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(status);
            }
        });

        let received = tokio::time::timeout(StdDuration::from_secs(1), rx.recv()).await.expect("a listener subscribing after the first status must still be told").expect("channel should still be open");
        assert_eq!(received, DeviceStatus::Present(Some(FakeInfo(4))));
    }

    #[test]
    fn is_present_reports_the_same_gate_open_applies() {
        struct Fake;
        impl DeviceDriver for Fake {
            const UEVENT_SUBSYSTEM: &'static str = "fake";
            const PATH_ENV_VAR: &'static str = "SIMPLE_KVM_FAKE_DEVICE";
            const DEFAULT_PATH: &'static str = "/nonexistent/simple-kvm-fake-device";
            type Info = FakeInfo;
            type Settings = ();
            type Open = ();
            fn probe(_device_path: &str) -> Option<Self::Info> {
                None
            }
            fn open(_device_path: &str, _settings: &Self::Settings) -> Result<Self::Open, OpenError> {
                Ok(())
            }
        }

        assert!(!Device::<Fake>::from_status("dummy", DeviceStatus::Absent).is_present());
        // The boot-time "present but deliberately not probed" case still
        // counts as present, same as `open` treats it.
        assert!(Device::<Fake>::from_status("dummy", DeviceStatus::Present(None)).is_present());
    }
}
