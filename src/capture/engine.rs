//! `CaptureEngine`/`CaptureStream` - the `getUserMedia`/`MediaStreamTrack`
//! equivalent described in `docs/capture-redesign-ideas.md`, built on top
//! of `Device<CaptureDriver>` (see `capture::driver`). Wired into the real
//! session layer by `rtc::session::handle` (issue #006): a session asks
//! `request_stream()` for a live stream once its connection is stable, and
//! subscribes to the returned `CaptureStream`'s `ended` event to know when
//! to drop it.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::Mutex as AsyncMutex;

use crate::capture::driver::CaptureDevice;
use crate::capture::v4l2::{Resolution, SupportedFormat};
use crate::config::CaptureSettings;
use crate::device::DeviceStatus;
use crate::event::{EventEmitter, Subscription};
use crate::video_bus::{self, FrameEnvelope};

/// Startup default, falling back to the device's first reported
/// resolution/frame-rate combination if this specific one isn't
/// supported - see `default_settings`.
const DEFAULT_RESOLUTION: Resolution = Resolution { width: 1920, height: 1080 };
const DEFAULT_FPS: u32 = 10;

/// Mirrors `getUserMedia()` rejecting when no matching device exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoDevice;

pub struct CaptureEngine {
    shared: Arc<Shared>,
}

struct Shared {
    device: CaptureDevice,
    video_bus_tx: video_bus::Sender,
    video_bus_rx: video_bus::Receiver,
    force_keyframe: Arc<AtomicBool>,
    state: Mutex<State>,
    /// Keeps `Shared`'s `format`/settings cache current - see
    /// `CaptureEngine::new`. Held only for its lifetime effect; never read
    /// directly.
    _device_status_sub: Subscription<DeviceStatus<SupportedFormat>>,
}

struct State {
    settings: CaptureSettings,
    format: Option<SupportedFormat>,
    live: LiveCount,
    pass: Option<PassHandle>,
    /// One entry per currently-live `CaptureStream` created against the
    /// pass that's running (or was, if this is left over from a pass that
    /// just ended) - `Weak` so a stream that's already been dropped
    /// doesn't keep its `EventEmitter` alive just by being listed here.
    /// Drained and dispatched to, then cleared, exactly once whenever a
    /// pass ends - whether that's this Vec's generation or a later one -
    /// so a given stream's `ended` can only ever be fired for the one
    /// pass generation it was registered against.
    ended_emitters: Vec<Weak<EventEmitter<()>>>,
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

impl CaptureEngine {
    pub fn new(device: CaptureDevice) -> Self {
        let (video_bus_tx, video_bus_rx) = video_bus::channel();
        let state = Mutex::new(State { settings: default_settings(None), format: None, live: LiveCount::new(), pass: None, ended_emitters: Vec::new() });

        // `add_event_listener` only needs `&device` (not the not-yet-built
        // `Shared`), so it's registered before `device` moves into
        // `Shared` below - `Arc::new_cyclic` then supplies the listener
        // closure with a `Weak<Shared>` it can upgrade each time it fires,
        // so the two only need to reference each other, not exist in a
        // fixed order.
        let shared = Arc::new_cyclic(|weak_shared: &Weak<Shared>| {
            let weak_shared = weak_shared.clone();
            let sub = device.add_event_listener(move |status| {
                let weak_shared = weak_shared.clone();
                async move {
                    let Some(shared) = weak_shared.upgrade() else {
                        return;
                    };
                    let mut state = shared.state.lock().unwrap();
                    match status {
                        DeviceStatus::Present(Some(info)) => state.format = Some(info),
                        DeviceStatus::Present(None) => {}
                        DeviceStatus::Absent => state.format = None,
                    }
                }
            });
            Shared { device, video_bus_tx, video_bus_rx, force_keyframe: Arc::new(AtomicBool::new(false)), state, _device_status_sub: sub }
        });

        Self { shared }
    }

    /// Mirrors `getUserMedia()`. Fails immediately (never hangs) if the
    /// device isn't currently present; otherwise (re)uses the shared
    /// encode pass, starting it if it isn't already running, and hands
    /// back a new per-consumer `CaptureStream`.
    pub async fn request_stream(&self, settings: CaptureSettings) -> Result<CaptureStream, NoDevice> {
        let raw = self.shared.device.open(&settings).map_err(|_| NoDevice)?;
        let device_path = raw.device_path().to_string();

        let ended = Arc::new(EventEmitter::new());
        {
            let mut state = self.shared.state.lock().unwrap();
            let should_start = state.live.increment();
            if should_start {
                state.settings = settings;
                start_pass(&self.shared, &mut state, device_path);
            }
            state.ended_emitters.push(Arc::downgrade(&ended));
        }

        Ok(CaptureStream { inner: Arc::new(StreamInner { frames: AsyncMutex::new(self.shared.video_bus_rx.clone()), ended, _live: LiveMarker { shared: Arc::clone(&self.shared) } }) })
    }

    /// Mirrors `navigator.mediaDevices.ondevicechange` for the specific
    /// device this engine wraps - forwards presence/capability transitions
    /// exactly as `Device<CaptureDriver>` reports them. What
    /// `rtc::session::handle` subscribes to in order to retry
    /// `request_stream()` once a previously-unavailable device becomes
    /// present again, without needing a new browser connection - matches
    /// `request_stream`'s own presence-only gating (`Device::open` matches
    /// on `DeviceStatus::Present(_)` regardless of whether probing
    /// succeeded), rather than the stricter "successfully probed" signal
    /// `DeviceState` carries for the UI.
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

/// Starts the shared encode pass against `device_path`, reusing
/// `capture::run_one_pass` (in turn `v4l2::run_capture_loop` and
/// `h264::H264Encoder`) - the same capture/encode machinery the pre-#004
/// `CaptureManager` used, just with a different start/stop trigger, tied
/// to `state.live` (this module's own live-stream count) rather than a
/// raw connected-session counter. Must be called with `state`'s lock
/// already held.
fn start_pass(shared: &Arc<Shared>, state: &mut State, device_path: String) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_task = Arc::clone(&stop);
    let settings = state.settings;
    let format = state.format.clone();
    let video_bus = shared.video_bus_tx.clone();
    let force_keyframe = Arc::clone(&shared.force_keyframe);

    tracing::info!("video encoding started");
    let handle = tokio::task::spawn_blocking(move || super::run_one_pass(&device_path, &format, &settings, stop_task, video_bus, force_keyframe));

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

/// Decrements `CaptureEngine`'s live-stream count on drop - this *is* the
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
    ended: Arc<EventEmitter<()>>,
    _live: LiveMarker,
}

impl CaptureStream {
    /// Mirrors `track.addEventListener('ended', cb)`. Fires exactly once,
    /// the moment the underlying device becomes unavailable or the
    /// capture pass fails unrecoverably - never again afterward for this
    /// same stream (see `start_pass`'s supervisor: each stream's emitter
    /// is drained out of `ended_emitters` the one time it's dispatched
    /// to).
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

/// Computes the in-memory default settings: 1080p@10fps if the device
/// reports supporting it, otherwise the device's first reported
/// resolution/frame-rate combination - see issue #004's "Owns settings in
/// memory only for now" note. `None` (device never probed, or probe
/// failed) just falls back to the raw default.
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

    // --- `CaptureEngine`-level tests, against a real `CaptureDriver`
    // pointed at a plain temp file standing in for the device path - see
    // issue #004's acceptance criteria: "no real hardware needed if
    // CaptureDriver's open is exercised against a fake/mock path in
    // tests". A plain file always fails `v4l2::enumerate`'s probe (it's
    // not a real V4L2 device), so `format` stays `None` deterministically
    // - which `run_one_pass` treats as "nothing to do", exiting
    // immediately without ever asking to stop deliberately. That's
    // exactly what's needed to exercise the "pass ended on its own ->
    // ended() fires" path without any real hardware or racy ioctl
    // failure timing. ---

    async fn present_device_at(path: &str) -> CaptureDevice {
        let device = CaptureDevice::spawn(path, "video4linux");
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
        let device = CaptureDevice::spawn("/nonexistent/simple-kvm-test-device", "video4linux");
        // No wait needed - `Device::open` checks presence synchronously
        // against whatever's already been observed, and a path that's
        // never existed starts (and stays) `Absent`.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let engine = CaptureEngine::new(device);
        let result = engine.request_stream(CaptureSettings { resolution: DEFAULT_RESOLUTION, fps: DEFAULT_FPS }).await;
        assert_eq!(result.err(), Some(NoDevice));
    }

    #[tokio::test]
    async fn request_stream_succeeds_when_device_present() {
        let tmp = TempDevicePath::new();
        let device = present_device_at(tmp.as_str()).await;
        let engine = CaptureEngine::new(device);

        let result = engine.request_stream(CaptureSettings { resolution: DEFAULT_RESOLUTION, fps: DEFAULT_FPS }).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn pass_starts_on_first_stream_and_stops_when_last_one_drops() {
        let tmp = TempDevicePath::new();
        let device = present_device_at(tmp.as_str()).await;
        let engine = CaptureEngine::new(device);

        let stream = engine.request_stream(CaptureSettings { resolution: DEFAULT_RESOLUTION, fps: DEFAULT_FPS }).await.expect("device is present");
        assert!(engine.pass_running(), "requesting a stream while none was running must start the pass");

        drop(stream);
        // `LiveMarker::drop` updates `state.live`/signals `stop`
        // synchronously - no need to wait for the (nonexistent, since
        // `format` is `None` for this fake path) blocking OS thread.
        assert!(!engine.pass_running(), "dropping the last live stream must stop the pass");
    }

    #[tokio::test]
    async fn ended_fires_exactly_once_on_unrecoverable_pass_failure() {
        let tmp = TempDevicePath::new();
        let device = present_device_at(tmp.as_str()).await;
        let engine = CaptureEngine::new(device);

        let stream = engine.request_stream(CaptureSettings { resolution: DEFAULT_RESOLUTION, fps: DEFAULT_FPS }).await.expect("device is present");

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
    async fn next_frame_can_be_polled_from_a_spawned_task() {
        let tmp = TempDevicePath::new();
        let device = present_device_at(tmp.as_str()).await;
        let engine = CaptureEngine::new(device);
        let stream = engine.request_stream(CaptureSettings { resolution: DEFAULT_RESOLUTION, fps: DEFAULT_FPS }).await.expect("device is present");

        // This is the actual regression-catching mechanism for
        // `StreamInner::frames` needing `tokio::sync::Mutex`: spawning a
        // consumer of `next_frame()` the same way a real WebRTC session
        // would (`tokio::spawn`) simply fails to compile if `frames` were
        // a `std::sync::Mutex`, because its `MutexGuard` held across
        // `.changed().await` isn't `Send`.
        let video_bus_tx = engine.shared.video_bus_tx.clone();
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
    /// the module doc comment above the `CaptureEngine`-level tests) -
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
