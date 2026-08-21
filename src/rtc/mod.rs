pub mod protocol;
pub mod session;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use rtc::peer_connection::configuration::media_engine::MIME_TYPE_H264;
use rtc::peer_connection::configuration::setting_engine::{SctpMaxMessageSize, SettingEngine};
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind};
use webrtc::data_channel::DataChannel;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::MediaStreamTrack;
use webrtc::peer_connection::{
    register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceGatheringState, RTCPeerConnectionState, RTCSessionDescription, Registry,
};

use crate::ch9329::writer::SerialCommand;
use crate::config::{CaptureSettings, DeviceState, MouseMode};
use crate::video_bus;
use session::SessionContext;

/// Everything a new WebRTC session needs, shared across every browser tab
/// that connects. Mirrors what `webtransport::serve` used to be handed
/// directly — now it's router state instead, since signaling rides plain
/// HTTP requests rather than a long-lived server loop.
#[derive(Clone)]
pub struct SharedChannels {
    pub video_bus: video_bus::Receiver,
    pub serial_tx: mpsc::Sender<SerialCommand>,
    pub capture_settings_tx: watch::Sender<CaptureSettings>,
    pub mouse_mode_tx: watch::Sender<MouseMode>,
    pub device_state_rx: watch::Receiver<DeviceState>,
    pub hid_connected_rx: watch::Receiver<bool>,
    pub settings_path: PathBuf,
    /// Set by a session on an RTCP keyframe request (PLI/FIR), cleared by
    /// the capture task once it's forced a fresh keyframe — see
    /// `session::handle`'s `video_track.poll()` branch and
    /// `capture::run_one_pass`.
    pub force_keyframe: Arc<AtomicBool>,
}

#[derive(Deserialize)]
pub struct OfferRequest {
    sdp: String,
}

#[derive(Serialize)]
pub struct AnswerResponse {
    sdp: String,
}

/// `POST /rtc/offer`: the browser's entire signaling exchange in one round
/// trip (see `assets/web/app.js`'s `connect()`). No trickle ICE, no
/// WebSocket — the browser waits for its own ICE gathering to finish
/// before sending the offer, and this handler waits for its own gathering
/// to finish before responding, so one request/response is enough.
pub async fn offer_handler(State(channels): State<SharedChannels>, Json(body): Json<OfferRequest>) -> Result<Json<AnswerResponse>, StatusCode> {
    match negotiate(body.sdp, channels).await {
        Ok(answer_sdp) => Ok(Json(AnswerResponse { sdp: answer_sdp })),
        Err(err) => {
            tracing::warn!(%err, "failed to negotiate WebRTC session");
            Err(StatusCode::BAD_REQUEST)
        }
    }
}

/// Fires once per event, the first time it happens - a bounded `mpsc`
/// used as a single-shot signal, since `oneshot::Sender` isn't `Clone` and
/// the callback trait requires `&self`.
struct OnceSignal {
    tx: Mutex<Option<oneshot::Sender<()>>>,
}

impl OnceSignal {
    fn new() -> (Arc<Self>, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        (Arc::new(Self { tx: Mutex::new(Some(tx)) }), rx)
    }

    async fn fire(&self) {
        if let Some(tx) = self.tx.lock().await.take() {
            let _ = tx.send(());
        }
    }
}

struct Handler {
    gather_complete: Arc<OnceSignal>,
    dc_tx: mpsc::UnboundedSender<Arc<dyn DataChannel>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gather_complete.fire().await;
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        tracing::debug!(?state, "WebRTC connection state changed");
    }

    async fn on_data_channel(&self, data_channel: Arc<dyn DataChannel>) {
        let _ = self.dc_tx.send(data_channel);
    }
}

/// Builds a fresh `RTCPeerConnection` for one browser tab, negotiates the
/// offer/answer exchange, and spawns `session::handle` to run for the
/// life of the connection. Returns the answer SDP to send back once ICE
/// gathering completes (there's nothing left to negotiate after that -
/// the browser did the same non-trickle wait before sending its offer).
async fn negotiate(offer_sdp: String, channels: SharedChannels) -> Result<String> {
    let mut media_engine = MediaEngine::default();
    let h264_codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 102,
    };
    media_engine.register_codec(h264_codec.clone(), RtpCodecKind::Video).context("registering H.264 codec")?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine).context("registering RTP interceptors")?;

    // Neither side configures this otherwise, so both fall back to the RFC
    // 8841 default of 64KB — and since the crate takes the min of our
    // configured value and the browser's, our default is what ends up
    // binding. A JPEG frame from the capture card routinely exceeds 64KB,
    // so every MJPEG send fails outright without this. A generous local
    // value has no downside: the browser's own (lower) cap still applies.
    let mut setting_engine = SettingEngine::default();
    setting_engine.set_sctp_max_message_size(SctpMaxMessageSize::Bounded(4 * 1024 * 1024));

    let (gather_complete, gather_complete_rx) = OnceSignal::new();
    let (dc_tx, dc_rx) = mpsc::unbounded_channel();
    let handler = Arc::new(Handler { gather_complete, dc_tx });

    let peer_connection: Arc<dyn PeerConnection> = Arc::new(
        PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_setting_engine(setting_engine)
            .with_handler(handler)
            .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
            .build()
            .await
            .context("building RTCPeerConnection")?,
    );

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

    let offer = RTCSessionDescription::offer(offer_sdp).context("parsing browser's SDP offer")?;
    peer_connection.set_remote_description(offer).await.context("applying browser's offer")?;
    let answer = peer_connection.create_answer(None).await.context("creating SDP answer")?;
    peer_connection.set_local_description(answer).await.context("setting local description")?;

    // Block until ICE gathering is complete (non-trickle) - see the doc
    // comment on `offer_handler`.
    let _ = gather_complete_rx.await;

    let local_description = peer_connection.local_description().await.context("no local description after ICE gathering completed")?;

    let ctx = SessionContext {
        video_bus: channels.video_bus,
        serial_tx: channels.serial_tx,
        capture_settings_tx: channels.capture_settings_tx.clone(),
        capture_settings_rx: channels.capture_settings_tx.subscribe(),
        mouse_mode_tx: channels.mouse_mode_tx.clone(),
        mouse_mode_rx: channels.mouse_mode_tx.subscribe(),
        device_state_rx: channels.device_state_rx,
        hid_connected_rx: channels.hid_connected_rx,
        settings_path: channels.settings_path,
        force_keyframe: channels.force_keyframe,
    };
    let pc_for_session = peer_connection.clone();
    tokio::spawn(async move {
        if let Err(err) = session::handle(pc_for_session, video_track, video_sender, dc_rx, ctx).await {
            tracing::debug!(%err, "WebRTC session ended");
        }
    });

    Ok(local_description.sdp)
}

fn rand_u32() -> u32 {
    use std::hash::{BuildHasher, Hash, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    std::time::Instant::now().hash(&mut hasher);
    hasher.finish() as u32
}
