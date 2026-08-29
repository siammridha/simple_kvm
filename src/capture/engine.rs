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
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::Mutex as AsyncMutex;

use crate::capture::video_bus::{self, FrameEnvelope};
use crate::device::{CaptureDevice, CaptureSettings, DeviceStatus, EventEmitter, OpenError, Resolution, StateEmitter, Subscription};

/// Startup-only placeholder before the device has ever been probed - see
/// `startup_placeholder_settings`.
const DEFAULT_RESOLUTION: Resolution = Resolution { width: 1280, height: 720 };
const DEFAULT_FPS: u32 = 5;

/// What a shared open attempt settled on - private to this file, never
/// crosses a module boundary. `request_stream()` callers never see this
/// directly; it's what `await_open_result` translates into the real
/// `Result<CaptureStream, OpenError>` they get back (issue #027).
#[derive(Clone, Debug, PartialEq)]
enum OpenOutcome {
    Opened,
    Failed(Arc<str>),
}

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
}

struct State {
    settings: CaptureSettings,
    /// True until someone applies settings by hand (`update_settings`).
    /// While it holds, the startup defaults are still provisional: the
    /// card hasn't necessarily reported its capabilities yet, and once it
    /// does they're recomputed against what it actually supports.
    settings_are_defaults: bool,
    live: LiveCount,
    pass: Option<PassHandle>,
    /// Test-only observability hook (see `CaptureCard::open_attempts`):
    /// how many times `start_pass` has actually kicked off an open
    /// attempt. Incremented once per call, synchronously, while `state`'s
    /// lock is already held - not behavior, just what proves concurrent
    /// `request_stream()` calls against the same starting pass share one
    /// attempt instead of each opening the device.
    open_attempts: usize,
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
    /// Settles once the pass's own open attempt finishes - `Opened` or
    /// `Failed`. A `StateEmitter` (not a plain `EventEmitter`) so any
    /// number of concurrent `request_stream()` calls sharing this same
    /// starting pass can each subscribe and get the right outcome, whether
    /// they were already waiting or only just joined (see
    /// `await_open_result`).
    open_result: Arc<StateEmitter<OpenOutcome>>,
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

    /// The pass currently starting never made it to a live stream at all -
    /// see `start_pass`'s `!opened` branch. Resets fully rather than a
    /// single `decrement()` because an arbitrary number of concurrent
    /// callers may have piled onto this one still-opening attempt before
    /// it failed.
    fn mark_pass_failed_to_start(&mut self) {
        self.count = 0;
        self.pass_running = false;
    }
}

impl CaptureCard {
    /// Builds the engine and its own `CaptureDevice` together, so a caller
    /// that just wants a working engine - `rtc`, which constructs its own
    /// dependencies (`ARCHITECTURE.md` §3.2/§3.4) - never has to touch
    /// `device` itself to get one.
    pub fn spawn() -> Arc<Self> {
        Arc::new(Self::new(CaptureDevice::spawn()))
    }

    pub fn new(device: CaptureDevice) -> Self {
        let (video_bus_tx, video_bus_rx) = video_bus::channel();
        let state = Mutex::new(State {
            settings: startup_placeholder_settings(),
            settings_are_defaults: true,
            live: LiveCount::new(),
            pass: None,
            open_attempts: 0,
            restart_pass: false,
            ended_emitters: Vec::new(),
        });

        let shared = Arc::new(Shared { device, video_bus_tx, video_bus_rx, force_keyframe: Arc::new(AtomicBool::new(false)), state, settings_changed: Arc::new(EventEmitter::new()) });

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
        {
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
        }
        self.shared.settings_changed.dispatch(settings);
    }

    /// Applies a default resolution/frame rate `rtc` computed from the
    /// device's own reported capabilities (see `ARCHITECTURE.md` §3.4) - a
    /// no-op once a person has applied settings by hand via `update_settings`,
    /// and never itself counts as that person having done so, so a later
    /// capability change (e.g. a different card plugged in) can still apply a
    /// fresh default as long as nobody has actually hit Save. Still fires
    /// `settings_changed` when it actually changes something, same as
    /// `update_settings`, so every open tab sees it live.
    pub fn apply_default_settings(&self, settings: CaptureSettings) {
        let changed = {
            let mut state = self.shared.state.lock().unwrap();
            if !state.settings_are_defaults || state.settings == settings {
                None
            } else {
                state.settings = settings;
                Some(settings)
            }
        };
        if let Some(settings) = changed {
            self.shared.settings_changed.dispatch(settings);
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

    /// Hands back the same capture device this card holds - a clone, not a
    /// second `Device::spawn()` (see `ARCHITECTURE.md` §3.1 "one instance
    /// per physical device"). `rtc` uses this to subscribe to
    /// presence/capability changes and compute the UI-facing device state
    /// itself (§3.4) - this card no longer does either.
    pub fn device(&self) -> CaptureDevice {
        self.shared.device.clone()
    }

    /// Mirrors `getUserMedia()`: genuinely attempts and awaits the real
    /// device open when no pass is currently running - `Err` comes
    /// straight from that attempt (device absent, or a real negotiate
    /// failure - see `capture::open_capture`), and no stream is ever
    /// created for a call that fails this way. If a pass is already
    /// running (or already failed to start and another call is mid-open),
    /// this joins the same attempt via `await_open_result` rather than
    /// opening a second time. Takes no settings: the card owns them, and a
    /// pass is shared by every consumer, so there is only ever one set in
    /// play (see `update_settings`).
    pub async fn request_stream(&self) -> Result<CaptureStream, OpenError> {
        let open_result = {
            let mut state = self.shared.state.lock().unwrap();
            let should_start = state.live.increment();
            if should_start {
                start_pass(&self.shared, &mut state);
            }
            Arc::clone(&state.pass.as_ref().expect("start_pass always sets state.pass before returning").open_result)
        };

        match await_open_result(&open_result).await {
            OpenOutcome::Opened => {
                // A `StateEmitter`: the caller gets this stream back
                // before it can subscribe, and does await-heavy work (a
                // WebRTC session adds and negotiates a video track) in
                // between, so a pass that fails fast ends the stream while
                // nobody is listening yet. Latching it means that
                // subscriber is still told, instead of being stranded with
                // a track for a pass that is already dead (issue #023).
                let ended = Arc::new(StateEmitter::new());
                {
                    let mut state = self.shared.state.lock().unwrap();
                    state.ended_emitters.push(Arc::downgrade(&ended));
                }
                Ok(CaptureStream { inner: Arc::new(StreamInner { frames: AsyncMutex::new(self.shared.video_bus_rx.clone()), ended, _live: LiveMarker { shared: Arc::clone(&self.shared) } }) })
            }
            OpenOutcome::Failed(msg) => Err(OpenError(msg.to_string())),
        }
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

    /// Test-only observability hook: how many times `start_pass` has
    /// actually kicked off a real open attempt - what proves concurrent
    /// `request_stream()` calls against the same starting pass share one
    /// attempt rather than each opening the device (issue #027).
    #[cfg(test)]
    fn open_attempts(&self) -> usize {
        self.shared.state.lock().unwrap().open_attempts
    }
}

/// Waits for the shared open attempt a pass's `PassHandle` represents to
/// settle. `StateEmitter`'s own contract (exactly one notification per
/// subscriber, replayed if it already happened) is what makes this safe
/// for N concurrent `request_stream()` calls against the same starting
/// pass to share one open attempt and each get the right outcome, whether
/// they were already waiting or only just subscribed.
async fn await_open_result(open_result: &Arc<StateEmitter<OpenOutcome>>) -> OpenOutcome {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let tx = std::sync::Mutex::new(Some(tx));
    let _sub = open_result.add_event_listener(move |outcome| {
        let sent = tx.lock().unwrap().take().map(|tx| tx.send(outcome));
        async move {
            let _ = sent;
        }
    });
    rx.await.unwrap_or_else(|_| OpenOutcome::Failed(Arc::from("capture pass ended before completing its open attempt")))
}

/// Starts the shared encode pass, reusing `capture::open_capture`/
/// `capture::run_capture_loop_forever` (in turn `Device::open`, `v4l2::
/// run_capture_loop` and `h264::H264Encoder`) - the same capture/encode
/// machinery the pre-#004 `CaptureManager` used, just with a different
/// start/stop trigger, tied to `state.live` (this module's own live-stream
/// count) rather than a raw connected-session counter. Must be called with
/// `state`'s lock already held.
///
/// The device handle is opened by `open_capture` on the blocking thread,
/// not here: the open is real I/O against the card, and this runs inside
/// an async task holding a lock. `request_stream()` awaits `open_result`
/// (via `await_open_result`) to learn whether that open succeeded before
/// it ever hands back a `CaptureStream` (issue #027).
fn start_pass(shared: &Arc<Shared>, state: &mut State) {
    state.open_attempts += 1;
    let open_result: Arc<StateEmitter<OpenOutcome>> = Arc::new(StateEmitter::new());
    let stop = Arc::new(AtomicBool::new(false));
    let stop_task = Arc::clone(&stop);
    let settings = state.settings;
    let format = match shared.device.latest_status() {
        Some(DeviceStatus::Present(Some(info))) => Some(info),
        _ => None,
    };
    let device = shared.device.clone();
    let video_bus = shared.video_bus_tx.clone();
    let force_keyframe = Arc::clone(&shared.force_keyframe);
    let open_result_task = Arc::clone(&open_result);

    tracing::info!("video encoding started");
    let handle = tokio::task::spawn_blocking(move || match super::open_capture(&device, &format, &settings) {
        Ok(capture) => {
            open_result_task.dispatch(OpenOutcome::Opened);
            super::run_capture_loop_forever(&capture, stop_task, video_bus, force_keyframe);
        }
        Err(err) => {
            tracing::error!(%err, "failed to open capture device, no video this pass");
            open_result_task.dispatch(OpenOutcome::Failed(Arc::from(err.to_string())));
        }
    });

    let supervisor_shared = Arc::clone(shared);
    let supervisor_stop = Arc::clone(&stop);
    let supervisor_open_result = Arc::clone(&open_result);
    tokio::spawn(async move {
        let _ = handle.await;
        // `H264Encoder`'s `Drop` impl logs its own GPU teardown steps as
        // it goes, more useful than one generic line here - see
        // `capture::mod::run`'s own equivalent comment.
        let opened = matches!(supervisor_open_result.latest(), Some(OpenOutcome::Opened));
        let deliberate_stop = supervisor_stop.load(Ordering::Relaxed);
        let emitters = {
            let mut state = supervisor_shared.state.lock().unwrap();
            state.pass = None;
            if !opened {
                // Never made it to a live stream at all - every caller
                // waiting on this attempt gets `Err` directly (see
                // `request_stream`), never a `CaptureStream`/`LiveMarker`
                // to decrement later, so their combined contribution to
                // the live count has to be dropped in one go.
                state.restart_pass = false;
                state.live.mark_pass_failed_to_start();
                Vec::new()
            } else if deliberate_stop {
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

    state.pass = Some(PassHandle { stop, open_result });
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

/// The in-memory placeholder before the device has ever been probed - never
/// preferred over real capabilities, and never user-visible on its own: no
/// encode pass runs and no session attaches video before a successful
/// probe (`ARCHITECTURE.md` §3.4). Superseded the moment `rtc` calls
/// `apply_default_settings` with what the device actually reports (issue
/// #032) - this function has nothing to do with that computation any more.
fn startup_placeholder_settings() -> CaptureSettings {
    CaptureSettings { resolution: DEFAULT_RESOLUTION, fps: DEFAULT_FPS }
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

    #[test]
    fn live_count_resets_completely_when_the_pass_never_opens() {
        let mut live = LiveCount::new();
        assert!(live.increment());
        live.increment();
        live.mark_pass_failed_to_start();
        assert!(live.increment(), "a fresh request_stream() after a failed open must be able to start again");
    }

    // --- `CaptureCard`-level tests, against a real `CaptureDriver`
    // pointed at a plain temp file standing in for the device path - see
    // issue #004's acceptance criteria: "no real hardware needed if
    // CaptureDriver's open is exercised against a fake/mock path in
    // tests". A plain file always fails `CaptureDriver::probe` (it's not
    // a real V4L2 device), so `format` stays `None` deterministically -
    // which `open_capture` (issue #027) now treats as a fallible outcome,
    // exactly like a real negotiate failure, folded into `request_stream`'s
    // own `Err` before any stream is ever created.
    //
    // That's a real loss of coverage in this hardware-free environment: a
    // present-but-unsupported device used to make it far enough to hand
    // back a stream that then immediately fired `ended` (the old
    // `run_one_pass`'s `format.is_none()` check happened *after* a stream
    // already existed) - which is what every "ended"/live-stream test below
    // used to ride on. After this issue's change, `request_stream()` fails
    // outright for this fixture instead, the same as it now does for a
    // genuine negotiate failure, so none of that is reachable here any
    // more:
    //
    //   - A live `CaptureStream` actually being handed back and used (drop
    //     stopping the pass, `next_frame` being pollable from a spawned
    //     task) needs a real successful `Device::open` - only provable on
    //     real hardware, via `./test-on-device.sh` (issue #027's own
    //     acceptance criteria already requires this: "confirm the video
    //     track still attaches normally when the card is present").
    //   - `ended` firing for a pass that started successfully and later
    //     died, including the late-subscriber replay case, likewise needs a
    //     real successful open first - also only provable on real hardware
    //     now (`./test-on-device.sh`: "unplugging mid-stream still ends the
    //     stream the same way it does today"). The `StateEmitter` replay
    //     contract itself (issue #023) that made the late-subscriber case
    //     correct is generic and already has its own dedicated tests in
    //     `src/device/event.rs` (`state_emitter_replays_the_last_value_to_a
    //     _late_subscriber` and friends), so this integration-level test
    //     losing its `CaptureStream`-specific case is a smaller loss than it
    //     looks.
    //
    // What this fixture can still prove without real hardware: a present
    // device that never successfully probes correctly fails the open
    // (`request_stream_fails_when_device_present_but_unsupported`), and
    // concurrent callers against the same still-opening pass share one
    // open attempt (`concurrent_calls_share_one_open_attempt_and_the_same_
    // outcome`). ---

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
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn request_stream_fails_when_device_present_but_unsupported() {
        let tmp = TempDevicePath::new();
        let device = present_device_at(tmp.as_str()).await;
        let card = CaptureCard::new(device);

        let result = card.request_stream().await;
        assert!(result.is_err(), "a present device that never reports a supported format must fail the open, not return a stream that immediately ends");

        // The supervisor task that clears `pass_running` (via
        // `mark_pass_failed_to_start`) runs independently of the task that
        // resolves `await_open_result` - both react to the same failed
        // open, but with no ordering guarantee between them, so `request_
        // stream()` returning `Err` doesn't itself guarantee `pass_running`
        // has settled yet. Poll briefly instead of asserting immediately.
        tokio::time::timeout(Duration::from_secs(1), async {
            while card.pass_running() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("pass_running should settle to false shortly after a failed open");
    }

    #[tokio::test]
    async fn device_hands_back_a_live_clone_of_the_held_device() {
        // `rtc` (issue #026) now relies on `device()` for its own presence
        // subscription and `DeviceState` computation, instead of the
        // engine forwarding either - this is what proves the handle it
        // gets back is genuinely live, not a snapshot.
        let tmp = TempDevicePath::new();
        let device = present_device_at(tmp.as_str()).await;
        let card = CaptureCard::new(device);

        assert!(card.device().is_present(), "device() should hand back a working clone of the same device the card holds");
    }

    #[tokio::test]
    async fn concurrent_calls_share_one_open_attempt_and_the_same_outcome() {
        let tmp = TempDevicePath::new();
        let device = present_device_at(tmp.as_str()).await;
        let card = CaptureCard::new(device);

        let (a, b) = tokio::join!(card.request_stream(), card.request_stream());

        assert!(a.is_err() && b.is_err(), "a present-but-unsupported device must fail both concurrent calls");
        assert_eq!(card.open_attempts(), 1, "two concurrent calls against a pass that hasn't started yet must share exactly one open attempt, not open twice");
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
    async fn apply_default_settings_applies_when_nobody_has_chosen_settings_by_hand() {
        let card = CaptureCard::new(CaptureDevice::spawn_at("/nonexistent/simple-kvm-test-device"));

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _sub = card.add_settings_listener(move |settings| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(settings);
            }
        });

        let reported = CaptureSettings { resolution: Resolution { width: 1920, height: 1080 }, fps: 60 };
        card.apply_default_settings(reported);

        assert_eq!(card.settings(), reported);
        let seen = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.expect("settings listener should fire").expect("channel should still be open");
        assert_eq!(seen, reported);
    }

    #[tokio::test]
    async fn apply_default_settings_is_a_noop_once_a_person_has_applied_settings_by_hand() {
        let card = CaptureCard::new(CaptureDevice::spawn_at("/nonexistent/simple-kvm-test-device"));

        let chosen = CaptureSettings { resolution: Resolution { width: 1920, height: 1080 }, fps: 30 };
        card.update_settings(chosen);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _sub = card.add_settings_listener(move |settings| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(settings);
            }
        });

        let reported = CaptureSettings { resolution: Resolution { width: 640, height: 480 }, fps: 15 };
        card.apply_default_settings(reported);

        assert_eq!(card.settings(), chosen, "a person's own choice must never be overwritten by a computed default");
        assert!(tokio::time::timeout(Duration::from_millis(100), rx.recv()).await.is_err(), "settings_changed must not fire for a no-op default");
    }

    // --- `await_open_result` - directly unit-testable in isolation, no
    // `CaptureCard`/hardware needed at all. ---

    #[tokio::test]
    async fn await_open_result_fans_out_one_dispatch_to_every_concurrent_waiter() {
        let open_result = Arc::new(StateEmitter::<OpenOutcome>::new());

        let dispatcher = {
            let open_result = Arc::clone(&open_result);
            tokio::spawn(async move {
                // Gives both `await_open_result` calls below time to
                // register their listener before anything is dispatched -
                // so both are genuinely pending waiters when the dispatch
                // happens, not late subscribers replaying a value that was
                // already there (that contract belongs to `device::event`'s
                // own `StateEmitter` tests, not this one).
                tokio::time::sleep(Duration::from_millis(50)).await;
                open_result.dispatch(OpenOutcome::Opened);
            })
        };

        let (a, b) = tokio::join!(await_open_result(&open_result), await_open_result(&open_result));

        dispatcher.await.unwrap();
        assert_eq!(a, OpenOutcome::Opened, "every concurrent waiter must see the one dispatch");
        assert_eq!(b, OpenOutcome::Opened, "every concurrent waiter must see the one dispatch");
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
