//! Per-connection handling: one task juggling video, input, and control
//! over `tokio::select!` — sending video frames from this session's own
//! `CaptureStream` once one is attached (see `CaptureEngine::
//! request_stream`), translating `input` data channel messages into
//! `SerialCommand`s, and reading `control` data channel JSON (the Save
//! button's settings update, paste, SDP renegotiation answers). Settings
//! changes only touch the shared `watch<Settings>` — in memory only, the
//! connection itself is never disturbed, so applying new settings never
//! drops the session.

use anyhow::{Context, Result};
use std::collections::HashSet;
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

use crate::capture::engine::{CaptureEngine, CaptureStream, NoDevice};
use crate::capture::v4l2::{Resolution, SupportedFormat};
use crate::capture::FrameEnvelope;
use crate::hid::keymap::{self, KeyCode};
use crate::hid::writer::SerialCommand;
use crate::config::{CaptureSettings, DeviceState, MouseMode};
use crate::device::{DeviceStatus, Subscription};

use super::protocol::{ControlMessage, InputEvent, MouseModeWire, ServerMessage};

pub struct SessionContext {
    /// Shared across every session on this server - see
    /// `super::SharedChannels::capture_engine`. `handle` calls
    /// `request_stream()` on this once the connection is stable, and again
    /// on every later device-availability signal while this session still
    /// has no track.
    pub capture_engine: Arc<CaptureEngine>,
    pub serial_tx: mpsc::Sender<SerialCommand>,
    pub capture_settings_tx: watch::Sender<CaptureSettings>,
    pub capture_settings_rx: watch::Receiver<CaptureSettings>,
    pub mouse_mode_tx: watch::Sender<MouseMode>,
    pub mouse_mode_rx: watch::Receiver<MouseMode>,
    pub device_state_rx: watch::Receiver<DeviceState>,
    pub hid_connected_rx: watch::Receiver<bool>,
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
/// existing (see `CaptureEngine`'s own live-stream count) - dropping
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

/// Asks the capture engine for a live stream (`CaptureEngine::
/// request_stream`, mirroring `getUserMedia`) and, on success, builds and
/// attaches a fresh RTP track for it. Returns `None` if the device isn't
/// currently available (`NoDevice`) or attaching the track itself failed -
/// either way the caller just leaves the session without video, ready to
/// try again the next time `handle`'s `presence_rx` reports the device is
/// available.
async fn try_attach_video(
    pc: &Arc<dyn PeerConnection>,
    capture_engine: &CaptureEngine,
    settings: CaptureSettings,
    codec: &RTCRtpCodec,
    video_ended_tx: &mpsc::UnboundedSender<()>,
) -> Option<VideoState> {
    let stream = match capture_engine.request_stream(settings).await {
        Ok(stream) => stream,
        Err(NoDevice) => return None,
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
            let _ = video_ended_tx.send(());
        }
    });
    tracing::info!("capture device available: added video track");
    Some(VideoState { track: raw.track, sender: raw.sender, target: None, stream, _ended_sub: ended_sub })
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

    let mut device_state_rx = ctx.device_state_rx.clone();
    let mut capture_settings_rx = ctx.capture_settings_rx.clone();
    let mut mouse_mode_rx = ctx.mouse_mode_rx.clone();
    let mut hid_connected_rx = ctx.hid_connected_rx.clone();
    let mut pc_state_rx = ctx.pc_state_rx.clone();
    let mut keyboard = KeyboardState::default();

    // Forwards `CaptureEngine`'s own device-presence events into this
    // session's `select!` loop - what drives retrying `request_stream()`
    // once a previously-unavailable device becomes present again, without
    // needing a new browser connection. Kept alive for the life of this
    // session via `_presence_sub`.
    let (presence_tx, mut presence_rx) = mpsc::unbounded_channel::<DeviceStatus<SupportedFormat>>();
    let _presence_sub = ctx.capture_engine.add_event_listener(move |status| {
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
                if connected && video.is_none() && matches!(status, DeviceStatus::Present(_)) {
                    let settings = *capture_settings_rx.borrow();
                    if let Some(v) = try_attach_video(&pc, &ctx.capture_engine, settings, &ctx.h264_codec, &video_ended_tx).await {
                        last_captured_at = None;
                        video = Some(v);
                    }
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
                            handle_input_event(event, &mut keyboard, &ctx).await;
                        }
                    }
                    Some(DataChannelEvent::OnClose) | None => input_active = false,
                    _ => {}
                }
            }
            event = control.poll() => {
                match event {
                    Some(DataChannelEvent::OnOpen) => {
                        // `changed()` on a freshly subscribed watch::Receiver only
                        // fires for a change that happens *after* the subscribe -
                        // it won't fire for state that was already current when
                        // this tab connected. So the current state has to be
                        // pushed explicitly here, once, rather than relying on the
                        // `changed()` arms below (which still handle every update
                        // from this point on). `DeviceState` here is whatever
                        // `capture::watch_device_state` last cached from probing
                        // the card when it was plugged in - opening this channel
                        // doesn't trigger a fresh probe of its own.
                        if push_initial_state(&outbound_tx, &mut device_state_rx, &mut hid_connected_rx, &mut capture_settings_rx, &mut mouse_mode_rx).is_ok() {
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
            changed = device_state_rx.changed(), if control_open.load(Ordering::Relaxed) => {
                if changed.is_ok() {
                    let state = device_state_rx.borrow_and_update().clone();
                    let _ = outbound_tx.send(ServerMessage::DeviceState(state));
                }
            }
            changed = capture_settings_rx.changed(), if control_open.load(Ordering::Relaxed) => {
                if changed.is_ok() {
                    let capture = *capture_settings_rx.borrow_and_update();
                    let _ = outbound_tx.send(ServerMessage::Settings { capture, mouse_mode: *mouse_mode_rx.borrow() });
                }
            }
            changed = mouse_mode_rx.changed(), if control_open.load(Ordering::Relaxed) => {
                if changed.is_ok() {
                    let mouse_mode = *mouse_mode_rx.borrow_and_update();
                    let _ = outbound_tx.send(ServerMessage::Settings { capture: *capture_settings_rx.borrow(), mouse_mode });
                }
            }
            changed = hid_connected_rx.changed(), if control_open.load(Ordering::Relaxed) => {
                if changed.is_ok() {
                    let available = *hid_connected_rx.borrow_and_update();
                    let _ = outbound_tx.send(ServerMessage::HidState { available });
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
                    let settings = *capture_settings_rx.borrow();
                    if let Some(v) = try_attach_video(&pc, &ctx.capture_engine, settings, &ctx.h264_codec, &video_ended_tx).await {
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

#[derive(Default)]
struct KeyboardState {
    held: HashSet<String>,
}

impl KeyboardState {
    /// Updates held-key state and returns the full report to send, unless
    /// `code` isn't a key we recognize.
    fn apply(&mut self, code: &str, pressed: bool) -> Option<(u8, [u8; 6])> {
        keymap::lookup(code)?;
        if pressed {
            self.held.insert(code.to_string());
        } else {
            self.held.remove(code);
        }

        let mut modifiers = 0u8;
        let mut keys = [0u8; 6];
        let mut slot = 0;
        for held_code in &self.held {
            match keymap::lookup(held_code) {
                Some(KeyCode::Modifier(bit)) => modifiers |= bit,
                Some(KeyCode::Usage(usage)) if slot < keys.len() => {
                    keys[slot] = usage;
                    slot += 1;
                }
                _ => {}
            }
        }
        Some((modifiers, keys))
    }
}

/// Enqueues the full current state (device availability, HID connectivity,
/// settings) onto the outbound queue for the just-opened `control` channel.
/// Needed because `changed()` on a receiver only fires for changes from
/// here on — it doesn't tell a freshly connected tab about state that was
/// already current before it connected (see the call site in `handle`).
fn push_initial_state(
    outbound_tx: &mpsc::UnboundedSender<ServerMessage>,
    device_state_rx: &mut watch::Receiver<DeviceState>,
    hid_connected_rx: &mut watch::Receiver<bool>,
    capture_settings_rx: &mut watch::Receiver<CaptureSettings>,
    mouse_mode_rx: &mut watch::Receiver<MouseMode>,
) -> Result<()> {
    let device_state = device_state_rx.borrow_and_update().clone();
    outbound_tx.send(ServerMessage::DeviceState(device_state)).map_err(|_| anyhow::anyhow!("outbound queue closed"))?;
    let hid_available = *hid_connected_rx.borrow_and_update();
    outbound_tx
        .send(ServerMessage::HidState { available: hid_available })
        .map_err(|_| anyhow::anyhow!("outbound queue closed"))?;
    let capture = *capture_settings_rx.borrow_and_update();
    let mouse_mode = *mouse_mode_rx.borrow_and_update();
    outbound_tx.send(ServerMessage::Settings { capture, mouse_mode }).map_err(|_| anyhow::anyhow!("outbound queue closed"))?;
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

async fn handle_input_event(event: InputEvent, keyboard: &mut KeyboardState, ctx: &SessionContext) {
    let start = Instant::now();
    let cmd = match event {
        InputEvent::KeyEvent { pressed, code } => {
            let Some((modifiers, keys)) = keyboard.apply(&code, pressed) else {
                return;
            };
            tracing::debug!(code = %code, pressed, "typing: key event received from browser");
            SerialCommand::KeyReport { modifiers, keys }
        }
        InputEvent::MouseAbsoluteMove { x_frac, y_frac } => SerialCommand::MouseAbsoluteMove { x_frac, y_frac },
        InputEvent::MouseRelativeMove { buttons, dx, dy, wheel } => SerialCommand::MouseRelativeMove { buttons, dx, dy, wheel },
        InputEvent::MouseButtons { buttons, wheel } => SerialCommand::MouseButtons { buttons, wheel },
    };
    let kind = cmd.kind();
    let sent = ctx.serial_tx.send(cmd).await.is_ok();
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
                ctx.capture_settings_tx.send_modify(|s| {
                    s.resolution = Resolution { width: capture.width, height: capture.height };
                    s.fps = capture.fps;
                });
                tracing::info!(width = capture.width, height = capture.height, fps = capture.fps, "capture settings updated");
            }
            // The server doesn't need mouse mode to translate input events —
            // that's purely which message variant the client sends (see
            // `InputEvent`) — but it's tracked here anyway so it can be
            // reported back as the default for the life of this run.
            if let Some(mouse_mode) = mouse_mode {
                let mouse_mode = match mouse_mode {
                    MouseModeWire::Absolute => MouseMode::Absolute,
                    MouseModeWire::Relative => MouseMode::Relative,
                };
                ctx.mouse_mode_tx.send_replace(mouse_mode);
                tracing::info!(?mouse_mode, "mouse mode updated");
            }
        }
        ControlMessage::Paste { text } => {
            let tx = ctx.serial_tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(SerialCommand::PasteText(text)).await;
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
    use crate::capture::driver::CaptureDevice;
    use crate::capture::v4l2::Resolution;
    use crate::rtc::protocol::CaptureSettingsWire;

    fn test_ctx() -> SessionContext {
        let (serial_tx, _serial_rx) = mpsc::channel(1);
        let (capture_settings_tx, capture_settings_rx) =
            watch::channel(CaptureSettings { resolution: Resolution { width: 1280, height: 720 }, fps: 5 });
        let (mouse_mode_tx, mouse_mode_rx) = watch::channel(MouseMode::Absolute);
        let (_device_state_tx, device_state_rx) = watch::channel(DeviceState::default());
        let (_hid_connected_tx, hid_connected_rx) = watch::channel(false);
        let (_pc_state_tx, pc_state_rx) = watch::channel(RTCPeerConnectionState::New);
        // Points at a path that will never exist - these tests only touch
        // `handle_control_message`, which never asks the engine for a
        // stream, so a real capture device is neither needed nor wanted.
        let capture_device = CaptureDevice::spawn("/nonexistent-simple-kvm-test-device", "video4linux");
        SessionContext {
            capture_engine: Arc::new(CaptureEngine::new(capture_device)),
            serial_tx,
            capture_settings_tx,
            capture_settings_rx,
            mouse_mode_tx,
            mouse_mode_rx,
            device_state_rx,
            hid_connected_rx,
            h264_codec: RTCRtpCodec::default(),
            pc_state_rx,
        }
    }

    #[tokio::test]
    async fn update_settings_with_capture_only_leaves_mouse_mode_untouched() {
        let ctx = test_ctx();

        handle_control_message(
            ControlMessage::UpdateSettings { capture: Some(CaptureSettingsWire { width: 1920, height: 1080, fps: 25 }), mouse_mode: None },
            &ctx,
        );

        assert_eq!(*ctx.capture_settings_rx.borrow(), CaptureSettings { resolution: Resolution { width: 1920, height: 1080 }, fps: 25 });
        assert_eq!(*ctx.mouse_mode_rx.borrow(), MouseMode::Absolute);
    }

    #[tokio::test]
    async fn update_settings_with_mouse_mode_only_leaves_capture_untouched() {
        let ctx = test_ctx();

        handle_control_message(ControlMessage::UpdateSettings { capture: None, mouse_mode: Some(MouseModeWire::Relative) }, &ctx);

        assert_eq!(
            *ctx.capture_settings_rx.borrow(),
            CaptureSettings { resolution: Resolution { width: 1280, height: 720 }, fps: 5 }
        );
        assert_eq!(*ctx.mouse_mode_rx.borrow(), MouseMode::Relative);
    }

    #[tokio::test]
    async fn update_settings_with_neither_field_leaves_both_untouched() {
        let ctx = test_ctx();

        handle_control_message(ControlMessage::UpdateSettings { capture: None, mouse_mode: None }, &ctx);

        assert_eq!(
            *ctx.capture_settings_rx.borrow(),
            CaptureSettings { resolution: Resolution { width: 1280, height: 720 }, fps: 5 }
        );
        assert_eq!(*ctx.mouse_mode_rx.borrow(), MouseMode::Absolute);
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
