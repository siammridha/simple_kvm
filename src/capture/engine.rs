//! `CaptureCard`/`CaptureStream` - the `getUserMedia`/`MediaStreamTrack`
//! equivalent described in `docs/capture-redesign-ideas.md`, built on top
//! of `Device<CaptureDriver>` (see `device::capture_driver`). Wired into
//! the real session layer by `rtc::session::handle` (issue #006): a session asks
//! `request_stream()` for a live stream once its connection is stable, and
//! subscribes to the returned `CaptureStream`'s `ended` event to know when
//! to drop it. That subscription can arrive after the stream has already
//! ended - the session adds and negotiates a video track in between - so
//! `ended` is published through a `StateEmitter` (issue #023), not a
//! plain edge-triggered one.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use tokio::sync::Mutex as AsyncMutex;

use crate::capture::video_bus::{self, FrameEnvelope};
use crate::capture::{device_state_for, DeviceState};
use crate::device::{CaptureDevice, CaptureSettings, DeviceStatus, EventEmitter, Resolution, StateEmitter, Subscription, SupportedFormat};

/// Startup default, falling back to the device's first reported
/// resolution/frame-rate combination if this specific one isn't
/// supported - see `default_settings`.
const DEFAULT_RESOLUTION: Resolution = Resolution { width: 1280, height: 720 };
const DEFAULT_FPS: u32 = 5;

/// Mirrors `getUserMedia()` rejecting when no matching device exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoDevice;

pub struct CaptureCard {
    shared: Arc<Shared>,
}

struct Shared {
    device: CaptureDevice,
    video_bus_tx: video_bus::Sender,
    video_bus_rx: video_bus::Receiver,
    force_keyframe: Arc<AtomicBool>,
    state: Mutex<State>,
    /// Fires whenever the applied `CaptureSettings` actually move, so
    /// every already-open tab sees a `Save` from another tab without a
    /// reload (see `rtc::session::handle`).
    settings_changed: Arc<EventEmitter<CaptureSettings>>,
    /// Fires whenever the UI-facing `DeviceState` moves - either half of
    /// what it's computed from (the card's capabilities, the applied
    /// settings) can move it.
    device_state_changed: Arc<EventEmitter<DeviceState>>,
    /// Keeps `Shared`'s `format`/settings cache current - see
    /// `CaptureCard::new`. Held only for its lifetime effect; never read
    /// directly. A `OnceLock` because the subscription can only be made
    /// once this `Shared` is already inside its `Arc` (see
    /// `CaptureCard::new`), and is then never replaced.
    device_status_sub: OnceLock<Subscription<DeviceStatus<SupportedFormat>>>,
}

struct State {
    settings: CaptureSettings,
    /// True until someone applies settings by hand (`update_settings`).
    /// While it holds, the startup defaults are still provisional: the
    /// card hasn't necessarily reported its capabilities yet, and once it
    /// does they're recomputed against what it actually supports.
    settings_are_defaults: bool,
    format: Option<SupportedFormat>,
    /// Last published `DeviceState`, kept so a recompute that lands on the
    /// same value dispatches nothing.
    device_state: DeviceState,
    live: LiveCount,
    pass: Option<PassHandle>,
    /// Set by `update_settings` when it stops a running pass: the
    /// replacement pass can only start once the old one has actually let
    /// go of the card, so the pass's own supervisor starts it (see
    /// `start_pass`), not `update_settings`.
    restart_pass: bool,
    /// One entry per currently-live `CaptureStream` created against the
    /// pass that's running (or was, if this is left over from a pass that
    /// just ended) - `Weak` so a stream that's already been dropped
    /// doesn't keep its `StateEmitter` alive just by being listed here.
    /// Drained and dispatched to, then cleared, exactly once whenever a
    /// pass ends - whether that's this Vec's generation or a later one -
    /// so a given stream's `ended` can only ever be fired for the one
    /// pass generation it was registered against.
    ended_emitters: Vec<Weak<StateEmitter<()>>>,
}

struct PassHandle {
    stop: Arc<AtomicBool>,
}

/// The start/stop-trigger decision logic, factored out for direct unit
/// testing without spawning any real task or touching real hardware -
/// mirrors `device.rs`'s `PresenceState` pattern. Deliberately keyed off
/// "is a pass currently running" rather than a raw 0->1/1->0 transition:
/// a pass can end (device loss, unrecoverable error) while streams are
/// still live (held by their consumer, `ended` fired but not yet
/// dropped) - a later `request_stream()` must still be able to start a
/// fresh pass even though the live count never dropped back to zero in
/// between.
struct LiveCount {
    count: usize,
    pass_running: bool,
}

impl LiveCount {
    fn new() -> Self {
        Self { count: 0, pass_running: false }
    }

    /// Returns `true` if the caller should start a pass now.
    fn increment(&mut self) -> bool {
        self.count += 1;
        if self.pass_running {
            false
        } else {
            self.pass_running = true;
            true
        }
    }

    /// Returns `true` if the caller should stop the running pass now
    /// (the live count has genuinely reached zero).
    fn decrement(&mut self) -> bool {
        self.count -= 1;
        if self.count == 0 && self.pass_running {
            self.pass_running = false;
            true
        } else {
            false
        }
    }

    /// The running pass ended on its own (not via a deliberate stop) -
    /// record that no pass is running any more, independent of whatever
    /// the live count currently is.
    fn mark_pass_stopped(&mut self) {
        self.pass_running = false;
    }
}

impl CaptureCard {
    pub fn new(device: CaptureDevice) -> Self {
        let (video_bus_tx, video_bus_rx) = video_bus::channel();
        let state = Mutex::new(State {
            settings: default_settings(None),
            settings_are_defaults: true,
            format: None,
            device_state: DeviceState::default(),
            live: LiveCount::new(),
            pass: None,
            restart_pass: false,
            ended_emitters: Vec::new(),
        });

        let shared = Arc::new(Shared {
            device,
            video_bus_tx,
            video_bus_rx,
            force_keyframe: Arc::new(AtomicBool::new(false)),
            state,
            settings_changed: Arc::new(EventEmitter::new()),
            device_state_changed: Arc::new(EventEmitter::new()),
            device_status_sub: OnceLock::new(),
        });

        // Subscribed strictly after `shared` exists, not from inside an
        // `Arc::new_cyclic`: the device replays a status it already
        // published to a listener that subscribes late (see
        // `device::StateEmitter`), and that replay can be running on
        // another worker thread the instant this call returns. A listener
        // built against a `Weak<Shared>` that isn't inhabited yet would
        // fail to upgrade and drop exactly the status this is here to
        // catch.
        let weak_shared = Arc::downgrade(&shared);
        let subscription = shared.device.add_event_listener(move |status| {
            let weak_shared = weak_shared.clone();
            async move {
                let Some(shared) = weak_shared.upgrade() else {
                    return;
                };
                let (new_settings, new_device_state) = {
                    let mut state = shared.state.lock().unwrap();
                    // `Present(None)` means `CaptureDriver::probe` itself
                    // failed (see `device::DeviceStatus`'s doc comment) -
                    // `format` is left exactly as it was, so `DeviceState`
                    // stays whatever it last knew rather than being wiped
                    // by a probe that told us nothing new.
                    match status {
                        DeviceStatus::Present(Some(info)) => state.format = Some(info),
                        DeviceStatus::Present(None) => {}
                        DeviceStatus::Absent => state.format = None,
                    }
                    // The startup defaults were picked before the card
                    // had said anything about itself; now that it has,
                    // fall back to a combination it actually reports -
                    // but never overwrite settings a person chose.
                    let new_settings = if state.settings_are_defaults {
                        let defaults = default_settings(state.format.as_ref());
                        (defaults != state.settings).then(|| {
                            state.settings = defaults;
                            defaults
                        })
                    } else {
                        None
                    };
                    (new_settings, refresh_device_state(&mut state))
                };
                if let Some(settings) = new_settings {
                    shared.settings_changed.dispatch(settings);
                }
                if let Some(device_state) = new_device_state {
                    shared.device_state_changed.dispatch(device_state);
                }
            }
        });
        let _ = shared.device_status_sub.set(subscription);

        Self { shared }
    }

    /// The capture settings currently applied - held in memory for the
    /// life of the process and never read from or written to disk.
    pub fn settings(&self) -> CaptureSettings {
        self.shared.state.lock().unwrap().settings
    }

    /// Applies new capture settings (in memory only) and, if an encode
    /// pass is running, restarts it so the new resolution and frame rate
    /// actually reach the card - the pass negotiates the format at open
    /// time, so there's no way to change it under a running one.
    ///
    /// Always fires `settings_changed`, even for a no-op save, so the tab
    /// that saved gets the same echo back as every other open tab.
    pub fn update_settings(&self, settings: CaptureSettings) {
        let new_device_state = {
            let mut state = self.shared.state.lock().unwrap();
            let moved = state.settings != settings;
            state.settings = settings;
            state.settings_are_defaults = false;
            if moved
                && let Some(stop) = state.pass.as_ref().map(|pass| Arc::clone(&pass.stop))
            {
                stop.store(true, Ordering::Relaxed);
                state.restart_pass = true;
            }
            refresh_device_state(&mut state)
        };
        self.shared.settings_changed.dispatch(settings);
        if let Some(device_state) = new_device_state {
            self.shared.device_state_changed.dispatch(device_state);
        }
    }

    /// Mirrors `addEventListener('change', cb)` for the applied settings.
    pub fn add_settings_listener<F, Fut>(&self, callback: F) -> Subscription<CaptureSettings>
    where
        F: Fn(CaptureSettings) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.shared.settings_changed.add_event_listener(callback)
    }

    /// The UI-facing state of the card: whether it's usable right now,
    /// what it supports, and which combination is selected. Computed here
    /// because this is the only place holding both the card's probed
    /// capabilities and the applied settings.
    pub fn device_state(&self) -> DeviceState {
        self.shared.state.lock().unwrap().device_state.clone()
    }

    /// Mirrors `addEventListener('change', cb)` for `device_state`.
    pub fn add_device_state_listener<F, Fut>(&self, callback: F) -> Subscription<DeviceState>
    where
        F: Fn(DeviceState) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.shared.device_state_changed.add_event_listener(callback)
    }

    /// Mirrors `getUserMedia()`. Fails immediately (never hangs) if the
    /// device isn't currently present; otherwise (re)uses the shared
    /// encode pass, starting it if it isn't already running, and hands
    /// back a new per-consumer `CaptureStream`. Takes no settings: the
    /// card owns them, and a pass is shared by every consumer, so there
    /// is only ever one set in play (see `update_settings`).
    ///
    /// Presence is checked per consumer, but the device is opened only by
    /// the pass this may start (see `start_pass`): a V4L2 device can only
    /// have its format negotiated by one holder at a time, so a second
    /// consumer joining a running pass must not open it a second time.
    pub async fn request_stream(&self) -> Result<CaptureStream, NoDevice> {
        if !self.shared.device.is_present() {
            return Err(NoDevice);
        }

        // A `StateEmitter`: the caller gets this stream back before it can
        // subscribe, and does await-heavy work (a WebRTC session adds and
        // negotiates a video track) in between, so a pass that fails fast
        // ends the stream while nobody is listening yet. Latching it means
        // that subscriber is still told, instead of being stranded with a
        // track for a pass that is already dead (issue #023).
        let ended = Arc::new(StateEmitter::new());
        {
            let mut state = self.shared.state.lock().unwrap();
            let should_start = state.live.increment();
            if should_start {
                start_pass(&self.shared, &mut state);
            }
            state.ended_emitters.push(Arc::downgrade(&ended));
        }

        Ok(CaptureStream { inner: Arc::new(StreamInner { frames: AsyncMutex::new(self.shared.video_bus_rx.clone()), ended, _live: LiveMarker { shared: Arc::clone(&self.shared) } }) })
    }

    /// Mirrors `navigator.mediaDevices.ondevicechange` for the specific
    /// device this card wraps - forwards presence/capability transitions
    /// exactly as `Device<CaptureDriver>` reports them. What
    /// `rtc::session::handle` subscribes to in order to retry
    /// `request_stream()` once a previously-unavailable device becomes
    /// present again, without needing a new browser connection - matches
    /// `request_stream`'s own presence-only gating (`Device::is_present`
    /// is true for `DeviceStatus::Present(_)` regardless of whether
    /// probing succeeded), rather than the stricter "successfully probed"
    /// signal `DeviceState` carries for the UI.
    pub fn add_event_listener<F, Fut>(&self, callback: F) -> Subscription<DeviceStatus<SupportedFormat>>
    where
        F: Fn(DeviceStatus<SupportedFormat>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.shared.device.add_event_listener(callback)
    }

    /// Test-only hook exposing whether a pass is currently marked as
    /// running, without needing real hardware or waiting on a real
    /// blocking capture thread to actually execute - this is what proves
    /// the start/stop trigger actually fires from real `request_stream`/
    /// stream-drop calls, on top of `LiveCount`'s own pure unit tests.
    #[cfg(test)]
    fn pass_running(&self) -> bool {
        self.shared.state.lock().unwrap().live.pass_running
    }
}

/// Starts the shared encode pass, reusing `capture::run_one_pass` (in turn
/// `Device::open`, `v4l2::run_capture_loop` and `h264::H264Encoder`) - the
/// same capture/encode machinery the pre-#004 `CaptureManager` used, just
/// with a different start/stop trigger, tied to `state.live` (this
/// module's own live-stream count) rather than a raw connected-session
/// counter. Must be called with `state`'s lock already held.
///
/// The device handle is opened by `run_one_pass` on the blocking thread,
/// not here: the open is real I/O against the card, and this runs inside
/// an async task holding a lock.
fn start_pass(shared: &Arc<Shared>, state: &mut State) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_task = Arc::clone(&stop);
    let settings = state.settings;
    let format = state.format.clone();
    let device = shared.device.clone();
    let video_bus = shared.video_bus_tx.clone();
    let force_keyframe = Arc::clone(&shared.force_keyframe);

    tracing::info!("video encoding started");
    let handle = tokio::task::spawn_blocking(move || super::run_one_pass(&device, &format, &settings, stop_task, video_bus, force_keyframe));

    let supervisor_shared = Arc::clone(shared);
    let supervisor_stop = Arc::clone(&stop);
    tokio::spawn(async move {
        let _ = handle.await;
        // `H264Encoder`'s `Drop` impl logs its own GPU teardown steps as
        // it goes, more useful than one generic line here - see
        // `capture::mod::run`'s own equivalent comment.
        let deliberate_stop = supervisor_stop.load(Ordering::Relaxed);
        let emitters = {
            let mut state = supervisor_shared.state.lock().unwrap();
            state.pass = None;
            if deliberate_stop {
                // A settings change stops the running pass and asks for a
                // replacement, which can only be started here: the card
                // negotiates its format on open, so the old pass has to
                // have let go of it first. Skipped if the stop was
                // actually the last live stream going away in the
                // meantime, which clears `pass_running` on its way out.
                if std::mem::take(&mut state.restart_pass) && state.live.pass_running {
                    start_pass(&supervisor_shared, &mut state);
                }
                Vec::new()
            } else {
                state.live.mark_pass_stopped();
                std::mem::take(&mut state.ended_emitters)
            }
        };
        // No retry/backoff, deliberately (see docs/capture-redesign-ideas.md
        // "Decided: no retry/backoff") - an unrecoverable pass error is
        // treated exactly like the device becoming unavailable: every
        // currently-live stream is told `ended`, once, and nothing here
        // tries to bring the pass back on its own.
        for emitter in emitters {
            if let Some(emitter) = emitter.upgrade() {
                emitter.dispatch(());
            }
        }
    });

    state.pass = Some(PassHandle { stop });
}

/// Decrements `CaptureCard`'s live-stream count on drop - this *is* the
/// guard from `docs/capture-redesign-ideas.md`'s idea 4, tied to
/// `CaptureStream`'s own lifetime by construction rather than a
/// separately-maintained counter.
struct LiveMarker {
    shared: Arc<Shared>,
}

impl Drop for LiveMarker {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap();
        if state.live.decrement()
            && let Some(pass) = &state.pass
        {
            pass.stop.store(true, Ordering::Relaxed);
        }
    }
}

/// Mirrors `MediaStreamTrack` - one per consumer.
pub struct CaptureStream {
    inner: Arc<StreamInner>,
}

struct StreamInner {
    /// Must be `tokio::sync::Mutex`, not `std::sync::Mutex` - verified
    /// directly: `next_frame` holds this lock across `.changed().await`,
    /// and spawning a caller of `next_frame` the way sessions spawn today
    /// (`tokio::spawn`) produces a real compile error
    /// (`future cannot be sent between threads safely`) with a
    /// `std::sync::MutexGuard` held across that await point, since it
    /// isn't `Send`. See `next_frame_can_be_polled_from_a_spawned_task`
    /// below - that's the test that would catch a regression back to
    /// `std::sync::Mutex`.
    frames: AsyncMutex<video_bus::Receiver>,
    ended: Arc<StateEmitter<()>>,
    _live: LiveMarker,
}

impl CaptureStream {
    /// Mirrors `track.addEventListener('ended', cb)`. Fires exactly once,
    /// the moment the underlying device becomes unavailable or the
    /// capture pass fails unrecoverably - never again afterward for this
    /// same stream (see `start_pass`'s supervisor: each stream's emitter
    /// is drained out of `ended_emitters` the one time it's dispatched
    /// to).
    ///
    /// Subscribing *after* that moment fires the callback straight away
    /// rather than registering one that can never fire: whether this
    /// stream has ended is state, not just an edge (see
    /// `device::StateEmitter`). Still exactly one notification per
    /// subscriber either way.
    pub fn add_event_listener<F, Fut>(&self, callback: F) -> Subscription<()>
    where
        F: Fn(()) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.inner.ended.add_event_listener(callback)
    }

    /// Reads the next frame off the shared `video_bus` fan-out, exactly as
    /// today - `None` once the sender side is gone (process shutting
    /// down).
    pub async fn next_frame(&self) -> Option<FrameEnvelope> {
        let mut frames = self.inner.frames.lock().await;
        if frames.changed().await.is_err() {
            return None;
        }
        frames.borrow().clone()
    }

    /// Mirrors `RTCRtpSender.generateKeyFrame()` (roughly) - asks the
    /// shared encode pass to produce a fresh keyframe as soon as possible,
    /// rather than waiting for its own periodic schedule. The atomic this
    /// sets is shared by every currently-live stream against the same
    /// pass, same as the encoder itself is shared - a keyframe request from
    /// any one session's decoder benefits every session, not just the one
    /// that asked. Used by `rtc::session::handle` in response to an RTCP
    /// PLI/FIR from the browser.
    pub fn request_keyframe(&self) {
        self.inner._live.shared.force_keyframe.store(true, Ordering::Relaxed);
    }
}

/// Recomputes the UI-facing `DeviceState` from the cached capabilities and
/// the applied settings, returning it only if it actually moved. The
/// caller dispatches it after releasing the state lock, so a listener
/// never runs with the lock held.
fn refresh_device_state(state: &mut State) -> Option<DeviceState> {
    let new_state = device_state_for(&state.format, &state.settings);
    if new_state == state.device_state {
        return None;
    }
    state.device_state = new_state.clone();
    Some(new_state)
}

/// Computes the in-memory default settings: 720p@5fps if the device
/// reports supporting it, otherwise the device's first reported
/// resolution/frame-rate combination. `None` (device never probed, or
/// probe failed) just falls back to the raw default.
fn default_settings(format: Option<&SupportedFormat>) -> CaptureSettings {
    let Some(format) = format else {
        return CaptureSettings { resolution: DEFAULT_RESOLUTION, fps: DEFAULT_FPS };
    };

    if format.resolutions.contains(&DEFAULT_RESOLUTION) {
        let fps = format.frame_rates.get(&DEFAULT_RESOLUTION).and_then(|rates| rates.contains(&DEFAULT_FPS).then_some(DEFAULT_FPS)).unwrap_or(DEFAULT_FPS);
        return CaptureSettings { resolution: DEFAULT_RESOLUTION, fps };
    }

    match format.resolutions.first().copied() {
        Some(resolution) => {
            let fps = format.frame_rates.get(&resolution).and_then(|rates| rates.first().copied()).unwrap_or(DEFAULT_FPS);
            CaptureSettings { resolution, fps }
        }
        None => CaptureSettings { resolution: DEFAULT_RESOLUTION, fps: DEFAULT_FPS },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc;

    // --- Pure `LiveCount` tests - no I/O, no async, no real hardware ---

    #[test]
    fn live_count_starts_pass_on_first_increment() {
        let mut live = LiveCount::new();
        assert!(live.increment(), "first stream should start the pass");
    }

    #[test]
    fn live_count_does_not_restart_an_already_running_pass() {
        let mut live = LiveCount::new();
        assert!(live.increment());
        assert!(!live.increment(), "second concurrent stream must not re-trigger a start");
    }

    #[test]
    fn live_count_stops_only_when_it_reaches_zero() {
        let mut live = LiveCount::new();
        live.increment();
        live.increment();
        assert!(!live.decrement(), "still one live stream left, must not stop yet");
        assert!(live.decrement(), "last live stream dropped, must stop now");
    }

    #[test]
    fn live_count_restarts_after_pass_stopped_on_its_own_even_if_still_live() {
        let mut live = LiveCount::new();
        live.increment();
        live.increment();
        // Pass failed/device lost while both streams are still held (not
        // yet dropped) - count never reaches zero, but the pass isn't
        // running any more.
        live.mark_pass_stopped();
        assert!(live.increment(), "a fresh request_stream() must restart the pass even though live count never hit zero");
    }

    // --- Startup defaults - pure, no device, no async ---

    fn format_with(resolutions: &[Resolution], rates: &[(Resolution, Vec<u32>)]) -> SupportedFormat {
        SupportedFormat { resolutions: resolutions.to_vec(), frame_rates: rates.iter().cloned().collect() }
    }

    #[test]
    fn default_settings_keep_the_startup_default_when_the_card_reports_it() {
        let format = format_with(&[Resolution { width: 1920, height: 1080 }, DEFAULT_RESOLUTION], &[(DEFAULT_RESOLUTION, vec![DEFAULT_FPS, 30])]);
        assert_eq!(default_settings(Some(&format)), CaptureSettings { resolution: DEFAULT_RESOLUTION, fps: DEFAULT_FPS });
    }

    #[test]
    fn default_settings_fall_back_to_the_first_reported_combination() {
        let first = Resolution { width: 640, height: 480 };
        let format = format_with(&[first, Resolution { width: 1920, height: 1080 }], &[(first, vec![15, 30])]);
        assert_eq!(default_settings(Some(&format)), CaptureSettings { resolution: first, fps: 15 });
    }

    // --- `CaptureCard`-level tests, against a real `CaptureDriver`
    // pointed at a plain temp file standing in for the device path - see
    // issue #004's acceptance criteria: "no real hardware needed if
    // CaptureDriver's open is exercised against a fake/mock path in
    // tests". A plain file always fails `CaptureDriver::probe` (it's not
    // a real V4L2 device), so `format` stays `None` deterministically -
    // which `run_one_pass` treats as "nothing to do", exiting immediately
    // (before it would even open the device) without ever asking to stop
    // deliberately. That's exactly what's needed to exercise the "pass
    // ended on its own -> ended() fires" path without any real hardware or
    // racy ioctl failure timing. ---

    async fn present_device_at(path: &str) -> CaptureDevice {
        // The zero-delay spawn path: this helper is about proving a
        // present device drives `request_stream`/`pass_running`, not
        // about `device`'s detect-to-probe delay (covered by its own
        // tests), so it doesn't need to wait the real 3 seconds out.
        let device = CaptureDevice::spawn_at_immediate(path);
        let (tx, mut rx) = mpsc::channel(1);
        let _sub = device.add_event_listener(move |status| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(status).await;
            }
        });
        let status = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.expect("device presence task should report a status quickly").expect("channel should still be open");
        assert!(matches!(status, DeviceStatus::Present(_)), "temp file must be observed as present");
        device
    }

    #[tokio::test]
    async fn request_stream_fails_immediately_when_device_absent() {
        let device = CaptureDevice::spawn_at("/nonexistent/simple-kvm-test-device");
        // No wait needed - `Device::open` checks presence synchronously
        // against whatever's already been observed, and a path that's
        // never existed starts (and stays) `Absent`.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let card = CaptureCard::new(device);
        let result = card.request_stream().await;
        assert_eq!(result.err(), Some(NoDevice));
    }

    #[tokio::test]
    async fn request_stream_succeeds_when_device_present() {
        let tmp = TempDevicePath::new();
        let device = present_device_at(tmp.as_str()).await;
        let card = CaptureCard::new(device);

        let result = card.request_stream().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn pass_starts_on_first_stream_and_stops_when_last_one_drops() {
        let tmp = TempDevicePath::new();
        let device = present_device_at(tmp.as_str()).await;
        let card = CaptureCard::new(device);

        let stream = card.request_stream().await.expect("device is present");
        assert!(card.pass_running(), "requesting a stream while none was running must start the pass");

        drop(stream);
        // `LiveMarker::drop` updates `state.live`/signals `stop`
        // synchronously - no need to wait for the (nonexistent, since
        // `format` is `None` for this fake path) blocking OS thread.
        assert!(!card.pass_running(), "dropping the last live stream must stop the pass");
    }

    #[tokio::test]
    async fn update_settings_changes_the_current_value_and_tells_listeners() {
        let card = CaptureCard::new(CaptureDevice::spawn_at("/nonexistent/simple-kvm-test-device"));
        assert_eq!(card.settings(), CaptureSettings { resolution: DEFAULT_RESOLUTION, fps: DEFAULT_FPS }, "a card that never appears leaves the startup defaults in place");

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _sub = card.add_settings_listener(move |settings| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(settings);
            }
        });

        let wanted = CaptureSettings { resolution: Resolution { width: 1920, height: 1080 }, fps: 30 };
        card.update_settings(wanted);

        assert_eq!(card.settings(), wanted);
        let seen = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.expect("settings listener should fire").expect("channel should still be open");
        assert_eq!(seen, wanted);
    }

    #[tokio::test]
    async fn ended_fires_exactly_once_on_unrecoverable_pass_failure() {
        let tmp = TempDevicePath::new();
        let device = present_device_at(tmp.as_str()).await;
        let card = CaptureCard::new(device);

        let stream = card.request_stream().await.expect("device is present");

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _sub = stream.add_event_listener(move |()| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(());
            }
        });

        // `format` is `None` for this fake path (see module doc above),
        // so `run_one_pass` returns immediately without ever setting
        // `stop` - the supervisor task sees that as "ended on its own"
        // and fires `ended` on every live stream.
        tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.expect("ended should fire").expect("channel should still be open");

        // Never fires again for the same stream.
        assert!(tokio::time::timeout(Duration::from_millis(200), rx.recv()).await.is_err(), "ended must not fire a second time for the same stream");

        drop(stream);
    }

    #[tokio::test]
    async fn ended_reaches_a_consumer_that_subscribes_after_the_pass_already_failed() {
        let tmp = TempDevicePath::new();
        let device = present_device_at(tmp.as_str()).await;
        let card = CaptureCard::new(device);

        let stream = card.request_stream().await.expect("device is present");

        // The window this test is about: a real consumer
        // (`rtc::session::try_attach_video`) gets its stream, then awaits
        // an `add_track` before it subscribes. The fake path's pass fails
        // immediately, so waiting here puts the whole `ended` dispatch
        // strictly before the subscription below - deterministically, not
        // as a race.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!card.pass_running(), "the fake device's pass should already have failed by now");

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _sub = stream.add_event_listener(move |()| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(());
            }
        });

        tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.expect("a consumer subscribing after its pass died must still be told the stream ended").expect("channel should still be open");

        // Late or not, still exactly one notification.
        assert!(tokio::time::timeout(Duration::from_millis(200), rx.recv()).await.is_err(), "ended must not fire a second time for the same stream");

        drop(stream);
    }

    #[tokio::test]
    async fn next_frame_can_be_polled_from_a_spawned_task() {
        let tmp = TempDevicePath::new();
        let device = present_device_at(tmp.as_str()).await;
        let card = CaptureCard::new(device);
        let stream = card.request_stream().await.expect("device is present");

        // This is the actual regression-catching mechanism for
        // `StreamInner::frames` needing `tokio::sync::Mutex`: spawning a
        // consumer of `next_frame()` the same way a real WebRTC session
        // would (`tokio::spawn`) simply fails to compile if `frames` were
        // a `std::sync::Mutex`, because its `MutexGuard` held across
        // `.changed().await` isn't `Send`.
        let video_bus_tx = card.shared.video_bus_tx.clone();
        let handle = tokio::spawn(async move { stream.next_frame().await });

        // Give the spawned task a moment to start waiting on `.changed()`
        // before publishing a frame.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let envelope = FrameEnvelope { data: Arc::from(vec![1u8, 2, 3]), captured_at: Duration::from_secs(0) };
        let _ = video_bus_tx.send(Some(envelope.clone()));

        let received = tokio::time::timeout(Duration::from_secs(1), handle).await.expect("spawned task should complete").expect("task should not panic");
        assert!(matches!(received, Some(got) if *got.data == *envelope.data));
    }

    /// A plain regular file standing in for a device path in tests (see
    /// the module doc comment above the `CaptureCard`-level tests) -
    /// deleted on drop so repeated test runs don't litter the system temp
    /// directory.
    struct TempDevicePath(std::path::PathBuf);

    impl TempDevicePath {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("simple-kvm-test-device-{}", unique_suffix()));
            std::fs::write(&path, b"not a real capture device").unwrap();
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

    /// A cheap unique-enough suffix for temp file names, without pulling
    /// in a `uuid` dependency just for tests.
    fn unique_suffix() -> u128 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        now ^ (counter as u128)
    }
}
