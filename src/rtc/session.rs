//! Per-connection handling: one task juggling video, input, and control
//! over `tokio::select!` — sending video frames from this session's own
//! `CaptureStream` once one is attached (see `CaptureCard::
//! request_stream`), forwarding `input` data channel messages to `hid` as
//! `InputCommand`s, and reading `control` data channel JSON (the Save
//! button's settings update, paste, SDP renegotiation answers). Settings
//! changes only touch in-memory state — `capture` itself for the capture
//! settings, `hid` itself for mouse mode; the connection is never
//! disturbed, so applying new settings never drops the session.

use anyhow::{Context, Result};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use rtc::media::Sample;
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtcp::Packet;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind};
use rtc::rtp_transceiver::PayloadType;
use tokio::sync::{mpsc, watch};
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::media_stream::{MediaStreamTrack, Track};
use webrtc::peer_connection::{PeerConnection, RTCPeerConnectionState, RTCSessionDescription};
use webrtc::rtp_transceiver::RtpSender;

use crate::capture::FrameEnvelope;
use crate::capture::engine::{CaptureCard, CaptureStream};
use crate::capture::{CaptureDevice, CaptureSettings, Resolution, SupportedFormat};
use crate::hid::{Ch9329Device, Hid, InputCommand, MouseMode};
use crate::device::{DeviceStatus, Subscription};

use super::device_state::{device_state_for, DeviceState};
use super::protocol::{ControlMessage, InputEvent, MouseModeWire, ServerMessage};

pub struct SessionContext {
    /// Shared across every session on this server - see `super::Rtc`.
    /// `handle` calls `request_stream()` on this once the connection is
    /// stable, and again on every later device-availability signal while
    /// this session still has no track. Capture settings are read from and
    /// written to it directly, and subscribed to per session (see
    /// `handle`).
    pub capture_card: Arc<CaptureCard>,
    /// A clone of the same `CaptureDevice` handle `capture_card` holds
    /// internally (via `CaptureCard::device`) - see `super::Rtc`. This
    /// session subscribes to it directly for presence/capability changes
    /// and computes `DeviceState` from what it reports plus
    /// `capture_card.settings()` (see `current_device_state`); `capture`
    /// itself no longer tracks or computes either.
    pub capture_device: CaptureDevice,
    /// The HID bridge - see `super::Rtc`. Input and mouse mode go through
    /// it, as commands and subscriptions; it no longer tracks or reports
    /// CH9329 presence at all (see `hid_device`, below).
    pub hid: Arc<Hid>,
    /// A clone of the same `Ch9329Device` handle `hid` holds internally
    /// (via `Hid::device`) - see `super::Rtc`. This session subscribes to
    /// it directly for presence and reads HID-available state straight off
    /// it (see `push_initial_state`, `_hid_presence_sub`); `hid` itself no
    /// longer tracks or reports presence at all.
    pub hid_device: Ch9329Device,
    /// The H.264 codec registered on this connection's `MediaEngine` (see
    /// `negotiate()`), needed to build a fresh `TrackLocalStaticSample`
    /// whenever a live capture stream becomes available — see
    /// `add_video_track`.
    pub h264_codec: RTCRtpCodec,
    /// Mirrors the `RTCPeerConnection`'s own connection state (see
    /// `rtc::Handler::on_connection_state_change`) - watched by `handle`'s
    /// main loop so the session shuts down as soon as the connection
    /// disconnects/fails/closes, rather than only when the `control` data
    /// channel happens to notice on its own.
    pub pc_state_rx: watch::Receiver<RTCPeerConnectionState>,
}

/// The two data channels the browser creates before sending its offer
/// (see `assets/web/app.js`) — matched by label as they arrive through
/// `on_data_channel` (see `Handler` in `super::mod`).
pub struct DataChannels {
    pub input: Arc<dyn DataChannel>,
    pub control: Arc<dyn DataChannel>,
}

/// Waits for both expected data channels to arrive. The browser creates
/// them in a fixed set before the offer is even sent, so this only has to
/// tolerate arrival order, not a missing channel.
pub async fn collect_data_channels(mut dc_rx: mpsc::UnboundedReceiver<Arc<dyn DataChannel>>) -> Result<DataChannels> {
    let (mut input, mut control) = (None, None);
    while input.is_none() || control.is_none() {
        let dc = dc_rx.recv().await.ok_or_else(|| anyhow::anyhow!("peer connection closed before all data channels opened"))?;
        match dc.label().await?.as_str() {
            "input" => input = Some(dc),
            "control" => control = Some(dc),
            other => tracing::debug!(label = other, "ignoring unexpected data channel"),
        }
    }
    Ok(DataChannels { input: input.unwrap(), control: control.unwrap() })
}

/// Resolves the SSRC and negotiated payload type for the local H.264
/// track's sender, needed on every `write_sample` call. Available as soon
/// as the answer's local description is set — negotiation doesn't wait
/// for the connection to actually come up.
async fn video_target(video_track: &TrackLocalStaticSample, video_sender: &Arc<dyn RtpSender>) -> Result<(u32, PayloadType)> {
    let ssrc = *video_track.ssrcs().await.first().ok_or_else(|| anyhow::anyhow!("H.264 track has no SSRCs"))?;
    let payload_type = video_sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|codec| codec.payload_type)
        .ok_or_else(|| anyhow::anyhow!("H.264 sender has no negotiated codec"))?;
    Ok((ssrc, payload_type))
}

/// One session's live H.264 track, plus the `CaptureStream` that feeds it
/// and drives its removal - built fresh every time a stream becomes
/// available, rather than shared across sessions or built up front, since
/// `TrackLocalStaticSample` only supports one peer-connection binding at a
/// time (see `docs/video-track-per-session.md`). Holding the
/// `CaptureStream` here for exactly as long as this session has a track is
/// what ties the shared encode pass's own start/stop to real consumers
/// existing (see `CaptureCard`'s own live-stream count) - dropping
/// `VideoState`, however that happens (deliberate removal or the whole
/// session ending), releases this session's hold on it.
struct VideoState {
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    /// SSRC and negotiated payload type for `write_sample` calls. Stays
    /// `None` until the renegotiation this track's `add_track` triggered
    /// has actually completed (see the `ControlMessage::Answer` arm in
    /// `handle`) — the sender has no negotiated codec parameters before
    /// then, even though the track and sender objects already exist.
    target: Option<(u32, PayloadType)>,
    stream: CaptureStream,
    /// Kept alive only for its `Drop`-based deregistration - forwards the
    /// stream's `ended` event into `handle`'s `video_ended_tx` (see
    /// `try_attach_video`).
    _ended_sub: Subscription<()>,
}

/// Just the RTP-track half of what a live capture stream needs -
/// `try_attach_video` combines this with the `CaptureStream` it's for.
struct RawVideoTrack {
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
}

/// Builds a fresh H.264 track and attaches it to `pc`. Confirmed against
/// the vendored `rtc-0.20.3` source that `add_track` reuses the
/// transceiver the browser already offered for video (matching `kind()`/an
/// empty `sender()`) rather than adding a duplicate `m=video` line, and
/// itself calls `trigger_negotiation_needed()` — which is what drives
/// `Handler::on_negotiation_needed` (see `super::Handler`) to create a
/// fresh offer and hand it to this session over `renegotiation_rx`.
async fn add_video_track(pc: &Arc<dyn PeerConnection>, codec: &RTCRtpCodec) -> Result<RawVideoTrack> {
    let ssrc = super::rand_u32();
    let track = Arc::new(
        TrackLocalStaticSample::new(MediaStreamTrack::new(
            "simple_kvm-video".to_string(),
            "simple_kvm-video".to_string(),
            "simple_kvm-video".to_string(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
                codec: codec.clone(),
                ..Default::default()
            }],
        ))
        .context("building H.264 track")?,
    );
    let sender = pc.add_track(track.clone() as Arc<dyn TrackLocal>).await.context("adding H.264 track")?;
    Ok(RawVideoTrack { track, sender })
}

/// Asks the capture card for a live stream (`CaptureCard::
/// request_stream`, mirroring `getUserMedia`) and, on success, builds and
/// attaches a fresh RTP track for it. Returns `None` if the open itself
/// failed (device absent, or negotiation failed - see `CaptureCard::
/// request_stream`'s `OpenError`) or attaching the track itself failed -
/// either way the caller just leaves the session without video, ready to
/// try again the next time `handle`'s `presence_rx` reports the device is
/// available.
async fn try_attach_video(pc: &Arc<dyn PeerConnection>, capture_card: &CaptureCard, codec: &RTCRtpCodec, video_ended_tx: &mpsc::UnboundedSender<()>) -> Option<VideoState> {
    let stream = match capture_card.request_stream().await {
        Ok(stream) => stream,
        Err(err) => {
            tracing::debug!(%err, "no capture stream available");
            return None;
        }
    };
    let raw = match add_video_track(pc, codec).await {
        Ok(raw) => raw,
        Err(err) => {
            tracing::warn!(%err, "failed to add video track");
            return None;
        }
    };
    let video_ended_tx = video_ended_tx.clone();
    let ended_sub = stream.add_event_listener(move |()| {
        let video_ended_tx = video_ended_tx.clone();
        async move {
            tracing::info!("capture stream ended: video track will be removed");
            let _ = video_ended_tx.send(());
        }
    });
    let settings = capture_card.settings();
    tracing::info!(width = settings.resolution.width, height = settings.resolution.height, fps = settings.fps, "capture device available: added video track");
    Some(VideoState { track: raw.track, sender: raw.sender, target: None, stream, _ended_sub: ended_sub })
}

/// Whether the capture device is not just present but successfully
/// probed - `rtc` only attempts `request_stream()` when this is true
/// (approved scope addition to issue #027; see the issue's own comment
/// thread / ARCHITECTURE.md §3.4 for the reasoning: a one-time probe
/// failure right after hot-plug means no auto-retry until replug, which
/// is an accepted tradeoff, not an oversight).
fn device_probed_available(capture_device: &CaptureDevice) -> bool {
    matches!(capture_device.latest_status(), Some(DeviceStatus::Present(Some(_))))
}

/// What a presence-status change should do to this session's video track,
/// given whether the connection is stable and whether one is currently
/// attached. Split out from the `presence_rx` arm in `handle()` purely so
/// this decision is unit-testable without a real `CaptureStream` - nothing
/// in this crate can fabricate one outside `capture::engine` (see #027),
/// and this container has no working fake V4L2 device.
#[derive(Debug, PartialEq)]
enum PresenceAction {
    TryAttach,
    Remove,
    Nothing,
}

fn presence_action(status: &DeviceStatus<SupportedFormat>, connected: bool, has_video: bool) -> PresenceAction {
    match status {
        DeviceStatus::Present(Some(_)) if connected && !has_video => PresenceAction::TryAttach,
        DeviceStatus::Absent if has_video => PresenceAction::Remove,
        _ => PresenceAction::Nothing,
    }
}

/// Removes this session's video track (in response to its `CaptureStream`'s
/// `ended` event, forwarded via `handle`'s `video_ended_rx`) and
/// renegotiates the same way `try_attach_video`'s `add_track` did -
/// `remove_track`, like `add_track`, calls the crate's own
/// `trigger_negotiation_needed()`. Always drops `video` regardless of
/// whether `remove_track` itself succeeds: by the time `ended` has fired,
/// the underlying capture pass is already gone, so there's nothing left
/// worth preserving state for - a later device-availability event just
/// requests a fresh stream and track from scratch.
async fn remove_video_track(pc: &Arc<dyn PeerConnection>, video: VideoState) {
    if let Err(err) = pc.remove_track(&video.sender).await {
        tracing::warn!(%err, "failed to remove video track");
    }
    tracing::info!("capture device unavailable: removed video track");
}

/// Polls the current video stream for its next frame, or never resolves if
/// there isn't one. A plain `video.as_ref().unwrap().stream.next_frame()`
/// in the `tokio::select!` branch below would panic when `video` is
/// `None`: an `if` guard only stops the resulting future from being
/// *polled*, not the branch expression from being *evaluated* — evaluation
/// happens on every loop iteration regardless of the guard. Wrapping the
/// `None` case in `std::future::pending()` here keeps evaluation itself
/// infallible.
async fn poll_frame(video: &Option<VideoState>) -> Option<FrameEnvelope> {
    match video {
        Some(v) => v.stream.next_frame().await,
        None => std::future::pending().await,
    }
}

/// Same guard-evaluation reasoning as `poll_frame`, for the current video
/// track's RTCP events instead of its frames.
async fn poll_rtcp(video: &Option<VideoState>) -> Option<TrackLocalEvent> {
    match video {
        Some(v) => v.track.poll().await,
        None => std::future::pending().await,
    }
}

/// Applies the browser's answer to a server-initiated renegotiation (see
/// `ControlMessage::Answer`), completing the round trip
/// `Handler::on_negotiation_needed` started. Once applied, the current
/// video track's sender (if any) has its codec actually negotiated, so
/// `video`'s RTP send target is (re)resolved here too — this is the first
/// point after `add_video_track` where `video_target` can succeed.
async fn apply_renegotiation_answer(pc: &Arc<dyn PeerConnection>, sdp: String, video: &mut Option<VideoState>) {
    let answer = match RTCSessionDescription::answer(sdp) {
        Ok(answer) => answer,
        Err(err) => {
            tracing::debug!(%err, "ignoring malformed renegotiation answer");
            return;
        }
    };
    if let Err(err) = pc.set_remote_description(answer).await {
        tracing::warn!(%err, "failed to apply browser's renegotiation answer");
        return;
    }
    if let Some(v) = video {
        v.target = video_target(&v.track, &v.sender).await.ok();
    }
}

pub async fn handle(
    pc: Arc<dyn PeerConnection>,
    dc_rx: mpsc::UnboundedReceiver<Arc<dyn DataChannel>>,
    mut renegotiation_rx: mpsc::UnboundedReceiver<String>,
    ctx: SessionContext,
) -> Result<()> {
    let DataChannels { input, control } = collect_data_channels(dc_rx).await?;
    // Starts with no video track at all - one is added later, once the
    // connection is stable *and* the capture device is available (see the
    // `pc_state_rx`/`presence_rx` arms below), added live via
    // renegotiation rather than being present from the start.
    let mut video: Option<VideoState> = None;

    let mut pc_state_rx = ctx.pc_state_rx.clone();

    // Forwards the capture device's own presence events into this
    // session's `select!` loop - what drives retrying `request_stream()`
    // once a previously-unavailable device becomes present again, without
    // needing a new browser connection. Kept alive for the life of this
    // session via `_presence_sub`.
    let (presence_tx, mut presence_rx) = mpsc::unbounded_channel::<DeviceStatus<SupportedFormat>>();
    let _presence_sub = ctx.capture_device.add_event_listener(move |status| {
        let presence_tx = presence_tx.clone();
        async move {
            let _ = presence_tx.send(status);
        }
    });

    // Fed by whichever `VideoState` is currently attached (see its
    // `_ended_sub`) - fires once the underlying capture pass ends (device
    // lost, unrecoverable error), however many times this session
    // attaches/loses a stream over its lifetime. Only ever one live
    // `VideoState`/subscription at a time, so there's no risk of a signal
    // meant for an already-replaced stream arriving late: `video` can only
    // go back to `Some` after this channel's message for the previous one
    // has already been drained (that's the only thing that clears it).
    let (video_ended_tx, mut video_ended_rx) = mpsc::unbounded_channel::<()>();

    // Every outbound state message goes through this queue instead of
    // `control.send()` directly - `relay_outbound` below is the only task
    // that ever writes to `control`, so concurrent producers (the
    // `changed()` arms, `push_initial_state`, and the renegotiation-offer
    // arm) can never race each other on the wire and reorder same-type
    // updates.
    let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let control_open = Arc::new(AtomicBool::new(false));
    tokio::spawn({
        let control = Arc::clone(&control);
        let control_open = Arc::clone(&control_open);
        async move {
            relay_outbound(outbound_rx, control_open, |msg| {
                let control = Arc::clone(&control);
                async move { send_server_message(&control, &msg).await }
            })
            .await;
        }
    });

    // This session's own subscriptions to the two modules that own the
    // state the page shows. Each one enqueues onto the outbound queue
    // above rather than touching `control` itself, so an event landing at
    // the same moment as a renegotiation offer or the initial push can't
    // reorder anything on the wire.
    //
    // Every callback re-reads the current value from the owning module
    // instead of using the payload it was handed: listeners are dispatched
    // fire-and-forget on their own tasks, so two changes in quick
    // succession can run in either order, and a payload-carrying callback
    // that lost that race would leave the tab on the older value for good.
    // Re-reading makes the worst case a harmless duplicate of the current
    // value. Nothing runs before `control` is open - `relay_outbound`
    // would fail its first send against a not-yet-open channel and stop
    // for good - so the `control_open` guard is what the `OnOpen` arm's
    // `push_initial_state` hands over from.
    let _device_state_sub = ctx.capture_device.add_event_listener({
        let (capture_device, capture_card, outbound_tx, control_open) = (ctx.capture_device.clone(), Arc::clone(&ctx.capture_card), outbound_tx.clone(), Arc::clone(&control_open));
        move |_| {
            let (capture_device, capture_card, outbound_tx, control_open) = (capture_device.clone(), Arc::clone(&capture_card), outbound_tx.clone(), Arc::clone(&control_open));
            async move {
                if control_open.load(Ordering::Relaxed) {
                    let _ = outbound_tx.send(ServerMessage::DeviceState(current_device_state(&capture_device, &capture_card)));
                }
            }
        }
    });
    // Capture settings and mouse mode are two halves of one
    // `ServerMessage::Settings`, so both subscriptions send the same
    // snapshot of both values. A settings change can also move
    // `DeviceState` (it affects `default_resolution`/frame-rate fallback),
    // so this pushes a fresh one alongside the settings snapshot - the
    // same thing `capture`'s own `update_settings`-triggered dispatch used
    // to cover before device state moved here (issue #026).
    let _settings_sub = ctx.capture_card.add_settings_listener({
        let (capture_device, capture_card, hid, outbound_tx, control_open) =
            (ctx.capture_device.clone(), Arc::clone(&ctx.capture_card), Arc::clone(&ctx.hid), outbound_tx.clone(), Arc::clone(&control_open));
        move |_| {
            let (capture_device, capture_card, hid, outbound_tx, control_open) =
                (capture_device.clone(), Arc::clone(&capture_card), Arc::clone(&hid), outbound_tx.clone(), Arc::clone(&control_open));
            async move {
                if control_open.load(Ordering::Relaxed) {
                    let _ = outbound_tx.send(ServerMessage::Settings { capture: capture_card.settings(), mouse_mode: hid.mouse_mode() });
                    let _ = outbound_tx.send(ServerMessage::DeviceState(current_device_state(&capture_device, &capture_card)));
                }
            }
        }
    });
    let _mouse_mode_sub = ctx.hid.add_mouse_mode_listener({
        let (capture_card, hid, outbound_tx, control_open) = (Arc::clone(&ctx.capture_card), Arc::clone(&ctx.hid), outbound_tx.clone(), Arc::clone(&control_open));
        move |_| {
            let (capture_card, hid, outbound_tx, control_open) = (Arc::clone(&capture_card), Arc::clone(&hid), outbound_tx.clone(), Arc::clone(&control_open));
            async move {
                if control_open.load(Ordering::Relaxed) {
                    let _ = outbound_tx.send(ServerMessage::Settings { capture: capture_card.settings(), mouse_mode: hid.mouse_mode() });
                }
            }
        }
    });
    let _hid_presence_sub = ctx.hid_device.add_event_listener({
        let (hid_device, outbound_tx, control_open) = (ctx.hid_device.clone(), outbound_tx.clone(), Arc::clone(&control_open));
        move |_| {
            let (hid_device, outbound_tx, control_open) = (hid_device.clone(), outbound_tx.clone(), Arc::clone(&control_open));
            async move {
                if control_open.load(Ordering::Relaxed) {
                    let _ = outbound_tx.send(ServerMessage::HidState { available: hid_device.is_present() });
                }
            }
        }
    });

    let mut input_active = true;
    // True from the moment the connection first reaches `Connected` - see
    // the `pc_state_rx` arm below. Gates every `try_attach_video` attempt
    // (initial and retry) so the capture device is never touched before
    // the connection is fully stable.
    let mut connected = false;
    // The real capture-time delta between consecutive frames, used as each
    // RTP sample's duration so the browser's jitter buffer gets correct
    // pacing info — see `send_frame`. `None` whenever there's no prior
    // frame in the *current* stream to diff against - reset every time a
    // fresh stream is attached (both `try_attach_video` call sites below),
    // not just once per session.
    let mut last_captured_at: Option<Duration> = None;

    loop {
        tokio::select! {
            frame = poll_frame(&video), if video.is_some() => {
                if let (Some(v), Some(frame)) = (&video, frame) {
                    let duration = last_captured_at.map(|t| frame.captured_at.saturating_sub(t)).unwrap_or(Duration::ZERO);
                    last_captured_at = Some(frame.captured_at);
                    send_frame(&frame.data, duration, &v.track, v.target).await;
                }
                // `None` means the shared `video_bus` sender is gone for
                // good (process shutting down, not just the card being
                // absent/unplugged) - this just stops delivering further
                // frames; keyboard and mouse must keep working either way,
                // so this never ends the session on its own.
            }
            event = poll_rtcp(&video), if video.as_ref().is_some_and(|v| v.target.is_some()) => {
                if let Some(TrackLocalEvent::OnRtcpPacket(packets)) = event
                    && packets.iter().any(|p| is_keyframe_request(p.as_ref()))
                    && let Some(v) = &video
                {
                    v.stream.request_keyframe();
                }
            }
            Some(()) = video_ended_rx.recv() => {
                if let Some(v) = video.take() {
                    remove_video_track(&pc, v).await;
                }
            }
            Some(status) = presence_rx.recv() => {
                let action = presence_action(&status, connected, video.is_some());
                tracing::debug!(?status, connected, has_video = video.is_some(), ?action, "session: capture device presence notification received");
                match action {
                    PresenceAction::TryAttach => {
                        if let Some(v) = try_attach_video(&pc, &ctx.capture_card, &ctx.h264_codec, &video_ended_tx).await {
                            last_captured_at = None;
                            video = Some(v);
                        }
                    }
                    PresenceAction::Remove => {
                        if let Some(v) = video.take() {
                            remove_video_track(&pc, v).await;
                        }
                    }
                    PresenceAction::Nothing => {}
                }
            }
            Some(sdp) = renegotiation_rx.recv() => {
                if control_open.load(Ordering::Relaxed) {
                    let _ = outbound_tx.send(ServerMessage::Offer { sdp });
                }
            }
            event = input.poll(), if input_active => {
                match event {
                    Some(DataChannelEvent::OnMessage(msg)) => {
                        if let Some(event) = InputEvent::parse(&msg.data) {
                            handle_input_event(event, &ctx).await;
                        }
                    }
                    Some(DataChannelEvent::OnClose) | None => input_active = false,
                    _ => {}
                }
            }
            event = control.poll() => {
                match event {
                    Some(DataChannelEvent::OnOpen) => {
                        // A listener only fires for a change that happens *after*
                        // it subscribes - it says nothing about state that was
                        // already current when this tab connected. So the current
                        // state is read straight from the owning modules and
                        // pushed once here; the subscriptions above cover every
                        // update from this point on. The device state is
                        // recomputed from whatever `device` last probed when the
                        // card was plugged in (`current_device_state`) - opening
                        // this channel doesn't trigger a fresh probe of its own.
                        if push_initial_state(&outbound_tx, &ctx.capture_device, &ctx.capture_card, &ctx.hid, &ctx.hid_device).is_ok() {
                            control_open.store(true, Ordering::Relaxed);
                        }
                    }
                    Some(DataChannelEvent::OnMessage(msg)) => {
                        match serde_json::from_slice::<ControlMessage>(&msg.data) {
                            Ok(ControlMessage::Answer { sdp }) => {
                                apply_renegotiation_answer(&pc, sdp, &mut video).await;
                            }
                            Ok(msg) => handle_control_message(msg, &ctx),
                            Err(err) => tracing::debug!(%err, "ignoring malformed control message"),
                        }
                    }
                    Some(DataChannelEvent::OnClose) | None => break,
                    _ => {}
                }
            }
            changed = pc_state_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let state = *pc_state_rx.borrow_and_update();
                if matches!(state, RTCPeerConnectionState::Disconnected | RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed) {
                    tracing::debug!(?state, "peer connection state ended, closing session");
                    break;
                }
                if state == RTCPeerConnectionState::Connected && !connected {
                    // Only now is the connection fully stable and ready for
                    // video - ask the capture engine right away in case a
                    // device is already available; if not, `presence_rx`
                    // above retries later.
                    connected = true;
                    if device_probed_available(&ctx.capture_device)
                        && let Some(v) = try_attach_video(&pc, &ctx.capture_card, &ctx.h264_codec, &video_ended_tx).await
                    {
                        last_captured_at = None;
                        video = Some(v);
                    }
                }
            }
        }
    }

    tracing::info!("WebRTC video stopped");
    Ok(())
}

/// Recomputes `DeviceState` from the capture device's last-known status
/// (`CaptureDevice::latest_status` - no fresh probe, mirroring the same
/// "ask without subscribing" contract `is_present` already gave `capture`)
/// plus the capture engine's current settings. This is what `rtc` now owns
/// in place of the `CaptureCard::device_state()` cache removed in issue
/// #026 - both the per-event subscriptions above and the initial push
/// below (`push_initial_state`) go through this so they can never disagree
/// on how the value is computed.
fn current_device_state(capture_device: &CaptureDevice, capture_card: &CaptureCard) -> DeviceState {
    let info = match capture_device.latest_status() {
        Some(DeviceStatus::Present(Some(info))) => Some(info),
        _ => None,
    };
    device_state_for(&info, &capture_card.settings())
}

/// Enqueues the full current state (device availability, HID connectivity,
/// settings) onto the outbound queue for the just-opened `control` channel,
/// read straight from the modules that own it. Needed because a listener
/// only fires for changes from here on — it doesn't tell a freshly
/// connected tab about state that was already current before it connected
/// (see the call site in `handle`).
fn push_initial_state(outbound_tx: &mpsc::UnboundedSender<ServerMessage>, capture_device: &CaptureDevice, capture_card: &CaptureCard, hid: &Hid, hid_device: &Ch9329Device) -> Result<()> {
    let closed = || anyhow::anyhow!("outbound queue closed");
    outbound_tx.send(ServerMessage::DeviceState(current_device_state(capture_device, capture_card))).map_err(|_| closed())?;
    outbound_tx.send(ServerMessage::HidState { available: hid_device.is_present() }).map_err(|_| closed())?;
    outbound_tx.send(ServerMessage::Settings { capture: capture_card.settings(), mouse_mode: hid.mouse_mode() }).map_err(|_| closed())?;
    Ok(())
}

/// Drains a session's outbound queue in order, handing each message to
/// `send` one at a time — the single point where messages actually reach
/// the wire, so two producers enqueueing back to back can never race each
/// other and land out of order (see the `outbound_tx`/`relay_outbound` spawn
/// site in `handle`). Generic over `send` rather than tied to
/// `Arc<dyn DataChannel>` so the ordering guarantee is unit-testable
/// without a `DataChannel` mock. Stops for good on the first failed send,
/// which is how the session now learns `control` is closed — mirrored back
/// to `handle`'s `select!` guards via `control_open`.
async fn relay_outbound<F, Fut>(mut rx: mpsc::UnboundedReceiver<ServerMessage>, control_open: Arc<AtomicBool>, mut send: F)
where
    F: FnMut(ServerMessage) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    while let Some(msg) = rx.recv().await {
        if send(msg).await.is_err() {
            control_open.store(false, Ordering::Relaxed);
            break;
        }
    }
}

async fn send_server_message(control: &Arc<dyn DataChannel>, msg: &ServerMessage) -> Result<()> {
    let bytes = serde_json::to_vec(msg)?;
    control.send(BytesMut::from(bytes.as_slice())).await?;
    Ok(())
}

/// Sends one H.264 frame as RTP via the local video track. Errors are
/// dropped, not propagated — a not-yet-negotiated track just means this
/// frame is skipped, the same tolerance the capture stream already gives
/// every subscriber.
async fn send_frame(data: &Arc<[u8]>, duration: Duration, video_track: &TrackLocalStaticSample, video_target: Option<(u32, PayloadType)>) {
    let Some((ssrc, payload_type)) = video_target else { return };
    let sample = Sample { data: Bytes::copy_from_slice(data), duration, ..Default::default() };
    if let Err(err) = video_track.sample_writer(ssrc, payload_type).write_sample(&sample).await {
        tracing::debug!(%err, "failed to send H.264 frame, dropping it");
    }
}

/// Whether any packet in an incoming RTCP batch is a keyframe request
/// (PLI or FIR) — the browser sends these automatically when its decoder
/// can't make progress without a fresh keyframe (see `handle`'s
/// `poll_rtcp` branch).
fn is_keyframe_request(packet: &dyn Packet) -> bool {
    packet.as_any().downcast_ref::<PictureLossIndication>().is_some() || packet.as_any().downcast_ref::<FullIntraRequest>().is_some()
}

/// Above this, the queue-time log below fires at `warn` instead of `debug`
/// — a slow enqueue means the CH9329 writer task is falling behind, which
/// is exactly what shows up as "typing/clicking lags".
const SLOW_ENQUEUE_THRESHOLD: Duration = Duration::from_millis(50);

/// Says what the peer did and hands it to `hid`. Nothing here knows how a
/// key or a click reaches the CH9329 - which keys are held, what a usage
/// code is and what a report looks like all live behind `hid::send`.
async fn handle_input_event(event: InputEvent, ctx: &SessionContext) {
    let start = Instant::now();
    let cmd = match event {
        InputEvent::KeyEvent { pressed, code } => {
            tracing::debug!(code = %code, pressed, "typing: key event received from browser");
            InputCommand::Key { code, pressed }
        }
        InputEvent::MouseAbsoluteMove { x_frac, y_frac } => InputCommand::PointerMoveAbsolute { x_frac, y_frac },
        InputEvent::MouseRelativeMove { buttons, dx, dy, wheel } => InputCommand::PointerMoveRelative { buttons, dx, dy, wheel },
        InputEvent::MouseButtons { buttons, wheel } => InputCommand::PointerButtons { buttons, wheel },
    };
    let kind = cmd.kind();
    let sent = ctx.hid.send(cmd).await.is_ok();
    let elapsed = start.elapsed();
    if elapsed > SLOW_ENQUEUE_THRESHOLD {
        tracing::warn!(kind, sent, elapsed_ms = elapsed.as_millis(), "input event took longer than expected to queue for CH9329");
    } else {
        tracing::debug!(kind, sent, elapsed_ms = elapsed.as_millis(), "queued input event for CH9329");
    }
}

fn handle_control_message(msg: ControlMessage, ctx: &SessionContext) {
    match msg {
        // Applies whichever half the page included, sent when the page's
        // Save button is clicked (see `assets/web/app.js`) - dropdowns no
        // longer apply live. `capture` is only present if the page saw a
        // capture card connected; `mouse_mode` only if it saw the CH9329
        // connected - there's nothing meaningful to apply for a device
        // that isn't there, so that half is just left as it was. In-memory
        // only: nothing here is ever written to disk.
        ControlMessage::UpdateSettings { capture, mouse_mode } => {
            if let Some(capture) = capture {
                // `capture` owns the value - it's the module that decides
                // what reaches the card, and restarts a running encode
                // pass so the new resolution/frame rate actually take
                // effect - and its change event is what reaches every
                // already-open tab, including this one (see the
                // subscriptions in `handle`).
                ctx.capture_card.update_settings(CaptureSettings { resolution: Resolution { width: capture.width, height: capture.height }, fps: capture.fps });
                tracing::info!(width = capture.width, height = capture.height, fps = capture.fps, "capture settings updated");
            }
            // `hid` owns the value - it's the module that decides what
            // gets written to the chip - and its change event is what
            // reaches every already-open tab, including this one (see the
            // subscriptions in `handle`).
            if let Some(mouse_mode) = mouse_mode {
                let mouse_mode = match mouse_mode {
                    MouseModeWire::Absolute => MouseMode::Absolute,
                    MouseModeWire::Relative => MouseMode::Relative,
                };
                ctx.hid.set_mouse_mode(mouse_mode);
                tracing::info!(?mouse_mode, "mouse mode updated");
            }
        }
        ControlMessage::Paste { text } => {
            let hid = Arc::clone(&ctx.hid);
            tokio::spawn(async move {
                let _ = hid.send(InputCommand::PasteText(text)).await;
            });
        }
        // Matched directly in `handle`'s `control.poll()` arm, before it
        // ever falls through to this function - it needs state (`pc`,
        // `video`) that this function, which only takes `&ctx`, doesn't
        // have access to.
        ControlMessage::Answer { .. } => {
            unreachable!("Answer is handled in handle()'s control.poll() arm")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtc::protocol::CaptureSettingsWire;

    fn test_ctx() -> SessionContext {
        let (_pc_state_tx, pc_state_rx) = watch::channel(RTCPeerConnectionState::New);
        // Points at a path that will never exist - these tests only touch
        // `handle_control_message`, which never asks the engine for a
        // stream, so a real capture device is neither needed nor wanted.
        let capture_device = CaptureDevice::spawn_at("/nonexistent-simple-kvm-test-device");
        let capture_card = Arc::new(CaptureCard::new(capture_device));
        // Points at a path that will never exist, same reasoning as
        // `capture_device` above - these tests only reach
        // `handle_control_message`.
        let hid = Hid::spawn_for_test();
        let hid_device = hid.device();
        SessionContext {
            capture_device: capture_card.device(),
            capture_card,
            hid,
            hid_device,
            h264_codec: RTCRtpCodec::default(),
            pc_state_rx,
        }
    }

    fn present_with_info() -> DeviceStatus<SupportedFormat> {
        DeviceStatus::Present(Some(SupportedFormat { resolutions: vec![], frame_rates: Default::default() }))
    }

    #[test]
    fn presence_action_attaches_when_present_connected_and_no_video() {
        assert_eq!(presence_action(&present_with_info(), true, false), PresenceAction::TryAttach);
    }

    #[test]
    fn presence_action_does_nothing_when_present_but_already_has_video() {
        assert_eq!(presence_action(&present_with_info(), true, true), PresenceAction::Nothing);
    }

    #[test]
    fn presence_action_does_nothing_when_present_but_not_connected() {
        assert_eq!(presence_action(&present_with_info(), false, false), PresenceAction::Nothing);
    }

    #[test]
    fn presence_action_removes_when_absent_and_has_video() {
        assert_eq!(presence_action(&DeviceStatus::Absent, true, true), PresenceAction::Remove);
        assert_eq!(presence_action(&DeviceStatus::Absent, false, true), PresenceAction::Remove);
    }

    #[test]
    fn presence_action_does_nothing_when_absent_and_no_video() {
        assert_eq!(presence_action(&DeviceStatus::Absent, true, false), PresenceAction::Nothing);
    }

    #[test]
    fn presence_action_does_nothing_when_present_but_unprobed() {
        assert_eq!(presence_action(&DeviceStatus::Present(None), true, false), PresenceAction::Nothing);
        assert_eq!(presence_action(&DeviceStatus::Present(None), true, true), PresenceAction::Nothing);
        assert_eq!(presence_action(&DeviceStatus::Present(None), false, false), PresenceAction::Nothing);
    }

    #[tokio::test]
    async fn update_settings_with_capture_only_leaves_mouse_mode_untouched() {
        let ctx = test_ctx();

        handle_control_message(
            ControlMessage::UpdateSettings { capture: Some(CaptureSettingsWire { width: 1920, height: 1080, fps: 25 }), mouse_mode: None },
            &ctx,
        );

        assert_eq!(ctx.capture_card.settings(), CaptureSettings { resolution: Resolution { width: 1920, height: 1080 }, fps: 25 });
        assert_eq!(ctx.hid.mouse_mode(), MouseMode::Absolute);
    }

    #[tokio::test]
    async fn update_settings_with_mouse_mode_only_leaves_capture_untouched() {
        let ctx = test_ctx();

        handle_control_message(ControlMessage::UpdateSettings { capture: None, mouse_mode: Some(MouseModeWire::Relative) }, &ctx);

        assert_eq!(ctx.capture_card.settings(), CaptureSettings { resolution: Resolution { width: 1280, height: 720 }, fps: 5 });
        assert_eq!(ctx.hid.mouse_mode(), MouseMode::Relative);
    }

    #[tokio::test]
    async fn update_settings_with_neither_field_leaves_both_untouched() {
        let ctx = test_ctx();

        handle_control_message(ControlMessage::UpdateSettings { capture: None, mouse_mode: None }, &ctx);

        assert_eq!(ctx.capture_card.settings(), CaptureSettings { resolution: Resolution { width: 1280, height: 720 }, fps: 5 });
        assert_eq!(ctx.hid.mouse_mode(), MouseMode::Absolute);
    }

    #[tokio::test]
    async fn relay_outbound_delivers_messages_in_the_order_they_were_sent() {
        let (tx, rx) = mpsc::unbounded_channel::<ServerMessage>();
        let control_open = Arc::new(AtomicBool::new(true));
        let received: Arc<std::sync::Mutex<Vec<ServerMessage>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

        // Two rapid same-type updates - the exact scenario the reordering
        // race could otherwise bite, per issue 007.
        let first = ServerMessage::Settings { capture: CaptureSettings { resolution: Resolution { width: 1280, height: 720 }, fps: 5 }, mouse_mode: MouseMode::Absolute };
        let second =
            ServerMessage::Settings { capture: CaptureSettings { resolution: Resolution { width: 1920, height: 1080 }, fps: 30 }, mouse_mode: MouseMode::Relative };
        tx.send(first.clone()).unwrap();
        tx.send(second.clone()).unwrap();
        drop(tx);

        let recorded = Arc::clone(&received);
        relay_outbound(rx, control_open, move |msg| {
            let recorded = Arc::clone(&recorded);
            async move {
                recorded.lock().unwrap().push(msg);
                Ok(())
            }
        })
        .await;

        // `ServerMessage` has no `PartialEq` (it's a wire type, compared by
        // its JSON shape elsewhere) - serialize both sides for the equality
        // check instead of adding a derive this issue doesn't otherwise need.
        let got: Vec<String> = received.lock().unwrap().iter().map(|m| serde_json::to_string(m).unwrap()).collect();
        let want: Vec<String> = [&first, &second].iter().map(|m| serde_json::to_string(m).unwrap()).collect();
        assert_eq!(got, want);
    }

    #[tokio::test]
    async fn relay_outbound_stops_and_marks_closed_after_a_failed_send() {
        let (tx, rx) = mpsc::unbounded_channel::<ServerMessage>();
        let control_open = Arc::new(AtomicBool::new(true));
        let attempts = Arc::new(std::sync::Mutex::new(0u32));

        tx.send(ServerMessage::HidState { available: true }).unwrap();
        tx.send(ServerMessage::HidState { available: false }).unwrap();
        drop(tx);

        let attempts_clone = Arc::clone(&attempts);
        relay_outbound(rx, Arc::clone(&control_open), move |_msg| {
            let attempts = Arc::clone(&attempts_clone);
            async move {
                *attempts.lock().unwrap() += 1;
                Err(anyhow::anyhow!("simulated closed channel"))
            }
        })
        .await;

        assert_eq!(*attempts.lock().unwrap(), 1, "should stop after the first failure, not keep draining the queue");
        assert!(!control_open.load(Ordering::Relaxed), "control_open should flip to closed once the relay task sees a send fail");
    }
}
