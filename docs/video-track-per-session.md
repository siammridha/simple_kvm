# Why each WebRTC session gets its own video track

Notes from investigating whether `negotiate()` (`src/rtc/mod.rs`) could build
one `MediaStreamTrack`/`TrackLocalStaticSample` once and reuse it for every
browser tab, instead of building a fresh one per session. Written down for
future reference rather than acted on immediately.

## What's already shared today

The expensive part - reading frames off the capture card and encoding them
to H.264 on the GPU - already happens exactly once, no matter how many
browser tabs are connected. `CaptureManager::run` (`src/capture/mod.rs`)
runs one capture+encode pass, and its output goes onto `video_bus`
(`src/capture/video_bus.rs`), a `tokio::sync::watch` channel carrying the latest
encoded frame. Every session's `session::handle` (`src/rtc/session.rs`)
clones its own `Receiver` of that same channel and reads the same encoded
bytes.

So "one client's worth of decoding/encoding work" is not the reason a
separate track exists per session.

## What isn't shared, and why

Every session builds its own `MediaStreamTrack` / `TrackLocalStaticSample`
in `negotiate()`:

```rust
let ssrc = rand_u32();
let video_track = Arc::new(
    TrackLocalStaticSample::new(MediaStreamTrack::new(
        "simple_kvm-video".to_string(),
        "simple_kvm-video".to_string(),
        "simple_kvm-video".to_string(),
        RtpCodecKind::Video,
        vec![RTCRtpEncodingParameters { rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() }, codec: h264_codec.rtp_codec, ..Default::default() }],
    ))
    .context("building H.264 track")?,
);
let video_sender = peer_connection.add_track(video_track.clone() as Arc<dyn TrackLocal>).await.context("adding H.264 track")?;
```

Checked the `webrtc` crate (0.20.3, pinned in `Cargo.toml`) directly to see
whether a single track object can be added to more than one
`RTCPeerConnection` at once. It can't. `TrackLocalStaticRTP` (which
`TrackLocalStaticSample` wraps internally) holds exactly one binding:

```rust
// webrtc-0.20.3/src/media_stream/track_local/static_rtp.rs
pub struct TrackLocalStaticRTP {
    pub(crate) track: Mutex<MediaStreamTrack>,
    pub(crate) ctx: Mutex<Option<TrackLocalContext>>,   // one slot, not a list
    pub(crate) evt_rx: Mutex<Option<Receiver<TrackLocalEvent>>>,
}

async fn bind(&self, ctx: TrackLocalContext, evt_rx: Receiver<TrackLocalEvent>) {
    *self.ctx.lock().await = Some(ctx);
    *self.evt_rx.lock().await = Some(evt_rx);
}
```

`peer_connection.add_track()` negotiates the track with that one peer
connection and calls `bind()`, which overwrites whatever was in `ctx`.
`write_rtp`/`write_rtcp` only ever send to the single context currently
sitting in that slot. If the same track object were added to a second
peer connection, that second `bind()` call would silently steal the slot
from the first - the first browser's session wouldn't error out, it would
just stop receiving frames, since its own writes would now be routed (or
lost) via the second connection's binding instead.

`TrackLocalEvent` is similarly scoped to one binding - it only carries
`OnRtcpPacket` (RTCP feedback, e.g. keyframe requests) for whichever
connection is currently bound, not per-connection-tagged feedback for a
set of connections.

## Bottom line

With this crate as it stands, `TrackLocalStaticSample` is a one-connection
object - not a fan-out primitive. Building a fresh one per session (cheap:
just in-memory packetizer/sequencer state, no I/O) is what makes each
session's binding independent of every other session's. The actual shared
resource - the capture card and the GPU encoder - is already a single
instance regardless of session count; only the thin RTP-packetizing wrapper
around each connection is duplicated.

Any future change here (e.g. a lower-level RTP fan-out that writes packets
directly per connection instead of going through `TrackLocal::bind`, or
patching/forking the crate to support multiple contexts) would need to work
around this one-slot design rather than just sharing the existing
`TrackLocalStaticSample` object across sessions.
