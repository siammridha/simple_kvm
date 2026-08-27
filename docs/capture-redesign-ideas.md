# Capture/session redesign ideas (not started)

Notes from a design discussion, kept for later - none of this has been
planned in detail or built yet. Written down so the reasoning behind it
isn't lost before work on it actually starts.

The goal is to closely follow how a browser's own device/track model
works (`navigator.mediaDevices`, `getUserMedia`, `MediaStreamTrack`) rather
than adapt today's code piecemeal - the target design below isn't
constrained by what exists in `src/` right now, up to and including a full
rewrite if that's what it takes to get there.

## Target model: mirror the browser's device/track lifecycle

Mapping each browser concept onto a server-side equivalent, since the
server is the side that actually owns a physical device here (the capture
card, the CH9329) - the browser in this app never calls `getUserMedia`
itself, it only receives.

| Browser concept | Server-side equivalent |
|---|---|
| `navigator.mediaDevices` (the device registry) | The presence module (idea 1) - one instance per physical device: the capture card, the CH9329 (decided below). |
| `ondevicechange` (coarse "something changed, re-enumerate") | The presence module's broadcast notification. |
| `enumerateDevices()` | The presence module's cached, already-probed capability data. |
| `getUserMedia()` | A call a consumer (a WebRTC session) makes to the presence module asking for a live handle to the device. Only succeeds if the device is currently available - fails otherwise, the same way `getUserMedia` rejects when no matching device exists. |
| `MediaStreamTrack` | The handle object that call returns - scoped to that one consumer, not shared. |
| `readyState` / the `ended` event | The handle's own end-of-life signal, set directly by the presence module the moment it loses the device - not proxied through some other layer watching a shared count. |
| `track.stop()` (consumer done with it) | The session/peer-connection ending, which drops the handle through ownership, same reasoning as idea 4. |

The important unification this produces: the "`getUserMedia`-equivalent"
handle *is* idea 4's guard, and it's also what idea 3's "session layer
decides whether to add/remove the video track" was reaching for. One
object does all three jobs that were previously discussed as three
separate pieces:

1. Whether the `rtc`/session layer is even allowed to attach a video track
   right now (it only can if it successfully got a handle).
2. The live/ended signal that tells the session layer when to add or
   remove that track and renegotiate.
3. The thing whose count (live handles currently vended) tells the
   encoder whether to be running at all - the presence module can track
   this itself, since it's the one handing handles out and the one that
   gets told when each is dropped, rather than a separate counter living
   somewhere else.

**Decided: presence and encoding stay two separate modules**, bridged by
one call rather than merged into one. See the API sketch below
(`DeviceHandle`/`CaptureEngine`) - `DeviceHandle` is presence/capability
tracking only and is the sole owner of the raw device path (idea 5);
`CaptureEngine` is the actual read+encode pipeline and never sees that
path, only whatever `DeviceHandle::open()` hands back.

## API sketch

Settled on callback-based subscriptions throughout - `addEventListener`
style, not a plain `watch` channel - after comparing against a
thread/Rayon-style parallel-dispatch approach and against a pull-based
channel. See the reasoning trail at the bottom of this section for why.

One small reusable piece both public types are built on, mirroring the
DOM's `EventTarget` (which is what `MediaDevices` and `MediaStreamTrack`
both actually inherit `addEventListener` from):

```rust
/// Mirrors EventTarget. Each instance corresponds to one event kind (its
/// payload type `T`) rather than the DOM's string-keyed multiplexing of
/// many event types through one object - Rust doesn't need that, since
/// each event kind already gets its own field and its own type, so a
/// mismatch is a compile error instead of a silent no-op.
struct EventEmitter<T: Clone + Send + 'static> {
    listeners: Mutex<HashMap<SubscriptionId, Box<dyn Fn(T) -> BoxFuture<'static, ()> + Send + Sync>>>,
    next_id: AtomicU64,
}

impl<T: Clone + Send + 'static> EventEmitter<T> {
    /// Mirrors addEventListener(type, callback).
    fn add_event_listener<F, Fut>(self: &Arc<Self>, callback: F) -> Subscription<T>
    where
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let id = SubscriptionId(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.listeners.lock().unwrap().insert(id, Box::new(move |v| Box::pin(callback(v))));
        Subscription { emitter: Arc::downgrade(self), id }
    }

    /// Mirrors dispatchEvent - fire-and-forget per listener (`tokio::spawn`,
    /// no join), so one slow or broken listener can't stall another
    /// listener or the caller.
    fn dispatch(&self, value: T) {
        for cb in self.listeners.lock().unwrap().values() {
            tokio::spawn(cb(value.clone()));
        }
    }
}

/// Mirrors removeEventListener - automatic, on drop, instead of a call
/// something has to remember to make. This is the actual problem a plain
/// callback registry has that a channel-based subscription doesn't: a
/// registered closure needs to be explicitly deregistered somehow, or it
/// leaks / gets called against state that's gone. Tying removal to drop
/// closes that gap the same way the video-track guard does (idea 4).
struct Subscription<T> { emitter: Weak<EventEmitter<T>>, id: SubscriptionId }

impl<T> Drop for Subscription<T> {
    fn drop(&mut self) {
        if let Some(emitter) = self.emitter.upgrade() {
            emitter.listeners.lock().unwrap().remove(&self.id);
        }
    }
}
```

**Device: presence + capability tracking only, mirrors `navigator.mediaDevices`.**
The sole owner of the raw device path (idea 5) - nothing else in the
codebase ever sees it.

```rust
pub struct DeviceHandle { inner: Arc<DeviceInner> }

struct DeviceInner {
    device_path: String,   // private - never leaves this module
    devicechange: Arc<EventEmitter<DeviceInfo>>,
    current: Mutex<DeviceInfo>,
    // presence task state: uevent listener, known_present, etc.
}

impl DeviceHandle {
    pub fn spawn(device_path: impl Into<String>) -> Self { ... }

    pub fn add_event_listener<F, Fut>(&self, callback: F) -> Subscription<DeviceInfo>
    where F: Fn(DeviceInfo) -> Fut + Send + Sync + 'static, Fut: Future<Output = ()> + Send + 'static {
        self.inner.devicechange.add_event_listener(callback)
    }

    /// The only place `device_path` is ever touched outside this module.
    /// Fails immediately if the device isn't currently available. Hands
    /// back an OS-level handle, never the path string itself.
    pub fn open(&self, settings: CaptureSettings) -> Result<RawCapture, NoDevice> { ... }
}
```

**Capture: the actual encode pipeline. Knows nothing about `/dev/video0`.**

```rust
pub struct CaptureEngine { device: DeviceHandle, /* encode-loop state */ }

impl CaptureEngine {
    pub fn new(device: DeviceHandle) -> Self { ... }

    /// Mirrors getUserMedia(). Calls device.open(...) internally to get a
    /// RawCapture, (re)starts the shared encode loop if it isn't already
    /// running, and hands back a per-consumer stream. Fails the same way
    /// device.open() fails if the device isn't available.
    pub async fn request_stream(&self, settings: CaptureSettings) -> Result<CaptureStream, NoDevice> { ... }
}

/// Mirrors MediaStreamTrack - one per consumer (one per WebRTC session).
pub struct CaptureStream { inner: Arc<StreamInner> }

struct StreamInner {
    frames: Mutex<video_bus::Receiver>,  // same shared broadcast fan-out as
                                          // today - an internal detail,
                                          // invisible at this API, same as
                                          // a browser may share one
                                          // physical camera behind several
                                          // independent MediaStreamTracks
    ended: Arc<EventEmitter<()>>,
    _live: LiveMarker,   // Drop decrements CaptureEngine's live-handle
                          // count - this *is* idea 4's guard now
}

impl CaptureStream {
    /// Mirrors track.addEventListener('ended', cb). Fires once, the
    /// instant CaptureEngine loses the device - directly, per stream, not
    /// proxied through some other layer watching a shared count.
    pub fn add_event_listener<F, Fut>(&self, callback: F) -> Subscription<()>
    where F: Fn(()) -> Fut + Send + Sync + 'static, Fut: Future<Output = ()> + Send + 'static {
        self.inner.ended.add_event_listener(callback)
    }

    pub async fn next_frame(&mut self) -> Option<FrameEnvelope> { ... }
}
```

Usage ends up reading close to the real browser version:

```rust
let _sub = device.add_event_listener(|info: DeviceInfo| async move {
    // push to the UI over the control channel
});

let stream = capture.request_stream(settings).await?;
let _ended_sub = stream.add_event_listener(|()| async move {
    // remove_track + renegotiate
});
```

**Why callbacks over a plain `watch` channel:** initially sketched as
`watch::Receiver<DeviceInfo>` (pull-based - a subscriber calls `.changed()`
then reads the value itself). Revisited after being shown that Rust
supports genuine parallel callback dispatch (threads/Rayon, each listener
independent, no listener blocking another) - a callback registry is
clearly *possible* in Rust, so the choice came down to which fits this
specific case, not whether Rust can do it at all:

- Callbacks need explicit deregistration or they leak / call into dropped
  state - solved by tying removal to `Subscription`'s `Drop`, same
  ownership-driven cleanup principle as idea 4's guard.
- Dispatching each listener via its own `tokio::spawn` (this codebase's
  async equivalent of the doc's `thread::spawn` pattern), with no `.join()`,
  means a slow or broken listener can't stall `dispatch()` or any other
  listener.
- Matches the browser's own `addEventListener` shape directly, which was
  the actual goal, rather than a Rust-idiomatic channel that happens to
  carry similar information.

One open concern this raises: several independent listeners for the same
session (device-info changes, HID state, settings) could each spawn a
task that wants to write to that session's *same* `control` data channel
at once, and nothing here serializes those writes against each other -
unlike a single `select!` loop, where only one branch runs at a time by
construction. Not resolved yet - see open questions below.

## The trigger: `CaptureManager::run` does too many unrelated things

Looking at the current loop (`src/capture/mod.rs`), it's one function
juggling several separate concerns at once: whether the card is physically
present, probing its capabilities, publishing `DeviceState` for the UI,
waiting for a browser to be ready, starting/stopping the actual
capture+encode pass, and retry/backoff after a failed pass. Nine distinct
things happening in one loop (see the walkthrough from the conversation
this came out of).

## Idea 1: a separate device-presence/capabilities module

Pull "is the card plugged in, and what can it do" out of `CaptureManager`
entirely, into its own module. Its job:

- Watch for the card being physically present/absent (the kernel uevent
  listener logic that's currently inline in `capture::run`).
- Probe capabilities exactly once whenever the card transitions from
  absent to present - same rule already implemented today: skip the very
  first check if the card is already present at the moment the service
  starts (the boot-crash-risk moment), probe on every other such
  transition.
- Publish that state so other parts of the code can subscribe to it,
  rather than being wired directly into the capture loop the way
  `device_state_tx` is now.

Known subscribers:

- Whatever currently pushes `DeviceState` to connected browser tabs (today
  that's `device_state_rx` inside `SessionContext`) - this module would
  become the sole owner/source of that state instead of it being computed
  inline in the capture loop (`device_state_for`/`publish_device_state`).
- The `rtc`/session layer - not `CaptureManager` (see idea 2's revision
  below). This is the subscriber that decides, per session, whether to add
  or remove that session's video track in response to a presence change.

## Idea 2: the encoder only runs when there's a video track - presence is handled one level up

Originally framed as "gate the encoder on device-presence AND video-track
count." Revised: the encoder only needs to watch video-track count.
Device-presence becomes the `rtc`/session layer's concern, not
`CaptureManager`'s - it's the thing deciding whether a session's video
track should exist at all (see idea 3). Since a track can only exist while
the device is present (the session layer only adds one when the
device-presence module says so, and removes it the moment presence is
lost - see idea 3's second half), video-track count already implies device
presence by construction. `CaptureManager` doesn't need its own separate
subscription to device-presence for start/stop gating - just idea 4's
track-count guard.

It still needs to *read* the currently-probed format (resolution/frame
rates) from the device-presence module at the moment it actually starts a
pass, to know what to capture at - that's a value lookup when starting,
not a gating condition.

## Idea 3: stop creating the video track unconditionally at negotiate() time

Today, every session gets a `MediaStreamTrack`/`TrackLocalStaticSample`
built and `add_track()`'d during `negotiate()`, before the browser's offer
is even applied - regardless of whether the capture card is present or
working. See [video-track-per-session.md](video-track-per-session.md) for
why each session needs its own track object (the library only supports
one binding per track).

Discussed instead: add the track *later*, once the session is `Connected`
**and** the device-presence module (idea 1) says the card is available -
using WebRTC's renegotiation support, which this library does have
(`on_negotiation_needed`, `create_offer` can be called again after initial
negotiation - confirmed by reading `webrtc-0.20.3`'s source).

This is not automatic, though - confirmed in the library source
(`peer_connection/driver.rs`): `on_negotiation_needed` just calls back into
our own handler, which currently does nothing. Making this work needs:

- A `Handler::on_negotiation_needed` implementation that calls
  `create_offer()` on the peer connection.
- A way to get that new offer to the browser and get an answer back -
  there's no channel left open for this after the initial handshake except
  the already-open `control` data channel, so this would mean new message
  types carried over it (a second offer server->browser, a second answer
  browser->server), plus matching browser-side JS in `app.js` to receive
  the offer and answer it. `app.js`'s current `RTCPeerConnection.
  onnegotiationneeded` (the browser-side equivalent) doesn't apply here,
  since it only fires when the browser's *own* local setup changes - it's
  the *server* adding a track in this design, so the browser only finds
  out once we actually send it a new offer over our own channel.

**Removal works the same way, in reverse.** When the device-presence
module (idea 1) reports the card is gone, the session layer removes that
session's video track (`remove_track`) and renegotiates the same way, for
every currently-connected session that has one. Once the last such track
is gone, `CaptureManager` sees the track count drop to zero (idea 4) and
stops the encoder on its own - it never needs to know *why* the count hit
zero, whether that's a real unplug or a session ending normally.

## Idea 4: replace `client_count` with a guard scoped to video-track lifetime

Current `ClientCountGuard` (`src/rtc/session.rs`) is created the moment a
session's connection reaches `Connected`, regardless of whether the
capture card is even there, and its `Drop` decrements a shared counter
that `CaptureManager` watches.

Discussed: keep the same `Drop`-based guard mechanism (there's no way to
get an async wakeup from `Arc` reference counts alone - something still
has to explicitly signal "count changed"), but re-scope what it counts:
create it only once a video track is actually attached to that session
(idea 3), not just once the connection is stable. Drop it when the
session ends, same as today.

The reasoning behind this (not wanting a hand-maintained session/track
counter at all): when a peer connection disconnects, its whole resource
tree - the connection, the video track, the guard - should go away
together as one consequence of Rust's own ownership rules, the same way
[the peer connection's own teardown already works](video-track-per-session.md)
today with no explicit `remove_track()`/`close()` call anywhere. The
encoder stopping should fall out of that cleanup happening, not out of a
separate piece of bookkeeping that has to be kept in step with it by
hand. The guard's `Drop` is what turns "this Arc tree just got cleaned up"
into a signal the capture side can actually wait on - it's still one
explicit line of code, but it's tied to the same drop, not a second
independent piece of state that could drift out of sync with it.

## Idea 5: only the presence modules know the actual device path

The capture-card presence module is the only thing that should know
`/dev/video0` (or wherever `VIDEO_PATH` points); the CH9329 presence
module is the only thing that should know its serial path. Every other
part of the code - the encoder, the UI-state pusher, the `rtc`/session
layer, the CH9329 writer - only ever sees "available" / "unavailable"
(plus, for capture, the probed capabilities) through the subscription.
Nothing outside the two presence modules holds or references a raw device
path.

## Decided

- A mid-session unplug actively removes that session's video track and
  renegotiates, rather than leaving it attached but idle - see idea 3's
  removal half. `CaptureManager` finds out indirectly, through the
  track-count guard (idea 4) dropping to zero, not through its own
  device-presence subscription.
- CH9329 (serial/HID) presence tracking follows the same module pattern as
  the capture card - its own presence-tracking module, publishing to
  subscribers, instead of whatever ad hoc detection it uses today.
- Subscriptions are callback-based (`EventEmitter`/`add_event_listener`,
  mirroring the DOM's `EventTarget`), not a pull-based `watch` channel -
  see the API sketch above for the full reasoning trail.
- Presence and encoding stay two separate modules (`DeviceHandle` and
  `CaptureEngine`), not merged into one - `DeviceHandle::open()` is the
  one bridge between them, and the only way `CaptureEngine` ever touches
  the device, without it ever holding the raw path itself. Resolves idea
  5's open question about how the encoder gets to actually open the
  device without knowing its path.

## Decided: one generic device module, not one-off per device kind

Rather than a separate hand-rolled presence module per device (capture
card, CH9329, and whatever else eventually shows up - a USB UART/TTL
serial adapter was named as a likely future one), there's one generic
`Device` abstraction that handles presence/capability tracking for *any*
device kind. Mirrors `navigator.mediaDevices` generalizing across camera,
mic, and speaker through one interface rather than a separate API per
kind.

What's actually shared (device-kind-independent): presence detection
(path exists + kernel uevent listening), the `EventEmitter`-based
notification mechanism, and owning/hiding the raw device path (idea 5).
What's device-specific and has to stay pluggable: *how* to probe a
device's capabilities, and *how* to open it for actual use - a v4l2
capture card, an HID serial adapter, and a UART/TTL serial device are
each opened and queried in a completely different way. That split
suggests a trait each device kind implements for its own probing/opening,
with the generic `Device<D: DeviceDriver>` core handling everything else:

```rust
/// What differs between device kinds - probing and opening. Everything
/// else (presence detection, event dispatch, path encapsulation) is
/// shared by the generic Device<D> core.
trait DeviceDriver {
    type Info: Clone + Send + 'static;   // capabilities, e.g. resolutions/frame_rates
    type Settings;                        // e.g. CaptureSettings
    type Open;                            // what open() hands back, e.g. RawCapture
    fn probe(device_path: &str) -> Option<Self::Info>;
    fn open(device_path: &str, settings: &Self::Settings) -> Result<Self::Open, OpenError>;
}

pub struct Device<D: DeviceDriver> { inner: Arc<DeviceInner<D>> }
// presence task, EventEmitter<DeviceStatus<D::Info>>, current status - all generic over D

struct CaptureDriver;
impl DeviceDriver for CaptureDriver { /* wraps v4l2::enumerate / v4l2::run_capture_loop's open step */ }

struct Ch9329Driver;
impl DeviceDriver for Ch9329Driver { /* wraps whatever CH9329 presence/open looks like */ }

type CaptureDevice = Device<CaptureDriver>;
type Ch9329Device = Device<Ch9329Driver>;
```

`CaptureEngine` would hold a `CaptureDevice` internally exactly as
sketched earlier; the CH9329 side gets the same treatment through
`Ch9329Driver` without duplicating the presence-tracking logic itself.
Adding a future device (the UART/TTL serial adapter mentioned) means
writing one more `DeviceDriver` impl, not another whole presence module.

## Decided: one outbound queue per session, single writer to `control`

Resolves the API sketch's closing concern. Confirmed first that this is a
real risk, not just a theoretical one: `DataChannel::send` takes `&self`
(`async fn send(&self, data: BytesMut) -> Result<()>`, `webrtc-0.20.3`),
so calling it concurrently from multiple tasks compiles and won't corrupt
an individual message's bytes - but nothing guarantees the *order* two
concurrent callers' messages go out in. Since every message here is a
full-state snapshot (`DeviceState`, `Settings`, `HidState`, never a
delta), cross-type reordering mostly doesn't matter - but the *same*
message type firing twice in quick succession (e.g. two rapid device-info
changes) could have the older one delivered second, leaving a browser tab
showing stale data. A real correctness bug, not cosmetic.

Fix: one `mpsc` queue per session, and exactly one task per session that's
ever allowed to call `control.send()`.

```rust
// Per session, created once when the session starts:
let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<ServerMessage>();

// Every listener enqueues instead of writing to `control` directly:
device.add_event_listener(move |info| {
    let outbound_tx = outbound_tx.clone();
    async move { let _ = outbound_tx.send(ServerMessage::DeviceState(info.into())); }
});

// The only thing that ever touches `control`:
tokio::spawn(async move {
    while let Some(msg) = outbound_rx.recv().await {
        let _ = send_server_message(&control, &msg).await;
    }
});
```

Writes end up serialized by construction, in true event order, no lock
needed - `mpsc::UnboundedSender::send` doesn't block or await, so a
listener callback stays trivial. Considered and rejected: wrapping
`control` in a `Mutex` - doesn't fix ordering (two tasks can still race
for the lock in either order), and holding a lock across an `.await` in
async Rust is generally worth avoiding rather than reaching for.

## Checked against the original 9-item list - what's still uncovered

Stock-take against the walkthrough of everything `CaptureManager::run`
currently handles (see "The trigger" above). Presence detection and the
old client-count/waiting mechanism are cleanly resolved by `DeviceHandle`
+ `request_stream()`. Four things weren't addressed by anything above at
the time this stock-take was written - all four are now resolved,
addressed by the "Decided" entries that follow:

- Publishing device state to the UI needing settings folded in, and
  settings changing while a stream is running - both resolved by the
  `CaptureEngine`-owns-settings decision directly below.
- Process shutdown and retry/backoff - resolved by their own "Decided"
  entries further down.

## Decided: `CaptureEngine` owns settings, in memory only - no more file persistence

Settings (resolution/fps) move from a separately-threaded shared
`watch::Sender<CaptureSettings>` (today's `capture_settings_tx`, loaded
from and saved to a file via `settings_store`) into something
`CaptureEngine` owns directly, in memory, for the life of the process.

- On startup, the encoder's default is 1080p@10fps, falling back to the
  first entry from the device's own reported capabilities if that
  specific combination isn't supported. No file is read.
- A settings change is applied by calling something like
  `capture.update_settings(new)` - this restarts the currently-running
  encode pass with the new values (same stop-then-restart mechanism
  already described above for "settings changing mid-stream"), updates
  the in-memory current value, and fires a settings-change event (same
  `EventEmitter`/`add_event_listener` pattern as everything else) to any
  subscribers - open tabs update live. Nothing is written to disk.
- A newly-opened tab just reads whatever `CaptureEngine`'s current
  in-memory settings are at that moment, the same way it'd read current
  `DeviceInfo` - there's no separate "load defaults from disk" step
  anymore, and no stale-persisted-value case to reason about (today's
  README note about a stale saved resolution being trusted until the
  first probe corrects it goes away entirely).
- This also resolves the device-state-needs-settings problem above:
  since `CaptureEngine` now holds both the device's capabilities (via its
  `DeviceHandle`) and the current settings, it's the natural place to
  compute and publish the combined UI-facing state (today's
  `device_state_for(format, settings)`) - not `DeviceHandle` alone, which
  only ever knows about the device side.

**Resolved: `mouse_mode` persistence is removed too.** No more file at
all, for either setting - `settings_store`, `PersistedSettings`, and the
`SETTINGS_PATH` env var all go away entirely. `mouse_mode` becomes
in-memory-only state, same treatment as capture settings: some default at
startup, changes only ever held in memory, a newly-opened tab just reads
whatever the current in-memory value is.

## Decided: process shutdown needs no separate mechanism

Not its own problem - a special case of "many sessions end at once,"
which the ownership cascade already handles. In Rust's async model,
cancelling a task (what happens to a still-running task when the tokio
runtime shuts down) means dropping that task's future in place, which
runs the destructors of everything it's holding, same as a normal return
would. So the same chain already relied on for one browser disconnecting
handles the process exiting too, without anything new: process exits ->
runtime shuts down -> every still-running session task gets dropped -> its
peer connection, video track, and `CaptureStream` all drop with it -> the
`CaptureStream`'s live-guard decrements `CaptureEngine`'s live count ->
count hits zero -> encoding stops. `DeviceHandle`'s own background
presence task falls under the same umbrella - it holds nothing needing a
more graceful release than what dropping its socket already does.

## Decided: no retry/backoff

Out of scope for now, deliberately - not deferred as an open question.
Today's 2-second sleep-and-retry after `run_one_pass` dies unexpectedly
(not a clean unplug - a driver/USB error the v4l2 read loop can't recover
from) is dropped entirely from the target design, not replaced with
anything. On that kind of error, `CaptureEngine` just logs it and stops -
no automatic retry.

Consequence this implies for any currently-live `CaptureStream`s: since
nothing is trying to bring the pass back on its own, treat this the same
as the device becoming unavailable - fire `ended()` on every live stream,
the same path as an actual unplug, rather than leaving them silently
frozen with no signal. That avoids a third state to design around; a
session that gets `ended()` reacts exactly like it already does for an
unplug, and a later `request_stream()` (e.g. after whatever caused it
clears up) works the same as any other reconnect.
