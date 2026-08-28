pub mod protocol;
pub mod session;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch, Mutex, OnceCell};
use rtc::ice::mdns::MulticastDnsMode;
use rtc::peer_connection::configuration::media_engine::MIME_TYPE_H264;
use rtc::peer_connection::configuration::setting_engine::SettingEngine;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind};
use webrtc::data_channel::DataChannel;
use webrtc::peer_connection::{
    register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceGatheringState, RTCPeerConnectionState, RTCSessionDescription, Registry,
};

use crate::capture::CaptureSettings;
use crate::capture::engine::CaptureEngine;
use crate::config::DeviceState;
use crate::device::{DeviceStatus, Subscription};
use crate::hid::{Hid, MouseMode};
use session::SessionContext;

/// Everything a new WebRTC session needs, shared across every browser tab
/// that connects. Mirrors what `webtransport::serve` used to be handed
/// directly — now it's router state instead, since signaling rides plain
/// HTTP requests rather than a long-lived server loop.
#[derive(Clone)]
pub struct SharedChannels {
    /// Shared across every session - `session::handle` calls
    /// `request_stream()` on this once its connection is stable, and again
    /// any time a device-availability event says it's worth retrying (see
    /// `CaptureEngine::add_event_listener`). The encode pass this wraps
    /// starts/stops as a direct consequence of how many sessions currently
    /// hold a live `CaptureStream` - nothing here counts that by hand.
    pub capture_engine: Arc<CaptureEngine>,
    /// Shared across every session - `session` calls `send` on this for
    /// every key/mouse event. The queue, the port and the drain worker
    /// live behind it; nothing here holds any of them.
    pub hid: Arc<Hid>,
    pub capture_settings_tx: watch::Sender<CaptureSettings>,
    pub device_state_rx: watch::Receiver<DeviceState>,
    /// Temporary: `session` still *polls* HID presence, so `new` bridges
    /// `Hid`'s presence events into this `watch`. #019 replaces it with a
    /// per-session `hid.add_event_listener` subscription and deletes both
    /// this field and `_hid_presence_sub`.
    pub hid_connected_rx: watch::Receiver<bool>,
    /// Temporary, for the same reason and with the same fate under #019:
    /// `hid` owns the mouse mode itself, and `new` bridges its change
    /// event into this `watch` so a session's `select!` can wait on it.
    /// Only a change *signal* - a session reads the current value straight
    /// from `hid`.
    pub mouse_mode_rx: watch::Receiver<MouseMode>,
    /// Keep the bridging listeners registered for as long as any session
    /// can still read the receivers above (see `Subscription`).
    _hid_presence_sub: Arc<Subscription<DeviceStatus<()>>>,
    _mouse_mode_sub: Arc<Subscription<MouseMode>>,
}

impl SharedChannels {
    pub fn new(
        capture_engine: Arc<CaptureEngine>,
        hid: Arc<Hid>,
        capture_settings_tx: watch::Sender<CaptureSettings>,
        device_state_rx: watch::Receiver<DeviceState>,
    ) -> Self {
        let (hid_connected_tx, hid_connected_rx) = watch::channel(false);
        let hid_presence_sub = hid.add_event_listener(move |status| {
            let hid_connected_tx = hid_connected_tx.clone();
            async move {
                let _ = hid_connected_tx.send(matches!(status, DeviceStatus::Present(_)));
            }
        });

        let (mouse_mode_tx, mouse_mode_rx) = watch::channel(hid.mouse_mode());
        let mouse_mode_sub = hid.add_mouse_mode_listener(move |mode| {
            let mouse_mode_tx = mouse_mode_tx.clone();
            async move {
                let _ = mouse_mode_tx.send(mode);
            }
        });

        Self {
            capture_engine,
            hid,
            capture_settings_tx,
            device_state_rx,
            hid_connected_rx,
            mouse_mode_rx,
            _hid_presence_sub: Arc::new(hid_presence_sub),
            _mouse_mode_sub: Arc::new(mouse_mode_sub),
        }
    }
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
    state_tx: watch::Sender<RTCPeerConnectionState>,
    /// Filled in with a weak handle to this connection's own
    /// `RTCPeerConnection` right after it's built (see `negotiate()`) —
    /// `Handler` is constructed and handed to `PeerConnectionBuilder`
    /// before the `RTCPeerConnection` it belongs to exists, so it can't
    /// simply hold an `Arc` from the start. Weak, not strong, because the
    /// peer connection itself owns this `Handler` (as a
    /// `PeerConnectionEventHandler`) — a strong `Arc` back would be a
    /// reference cycle that leaks the connection.
    pc: Arc<OnceCell<Weak<dyn PeerConnection>>>,
    /// Carries a freshly created offer's SDP out to `session::handle`,
    /// which owns the `control` data channel and actually sends it to the
    /// browser as a `ServerMessage::Offer` (see `on_negotiation_needed`
    /// below and the `renegotiation_rx` arm in `session::handle`).
    renegotiation_tx: mpsc::UnboundedSender<String>,
    /// False until `negotiate()` has finished the *initial* offer/answer
    /// exchange and captured its answer SDP. Without this guard,
    /// `on_negotiation_needed` fires spuriously during that very exchange:
    /// the browser's offer always includes a recvonly video transceiver
    /// (see `assets/web/app.js`), so the moment our own answer reaches
    /// `RTCSignalingState::Stable`, the crate re-runs its own
    /// negotiation-needed check and finds a `sendonly`-direction
    /// transceiver (the automatic reverse of the browser's `recvonly`)
    /// with no sender attached yet - which the crate always reports as
    /// "still needs negotiating", track or no track. Reacting to that by
    /// immediately building a new offer races `negotiate()`'s own
    /// `local_description()` read below: `set_local_description` on the
    /// spurious offer can land first, so the HTTP response ends up
    /// carrying an *offer* (always `a=setup:actpass`) instead of the
    /// intended answer - which every browser correctly rejects, since an
    /// answer must commit to `active`/`passive`. Confirmed by direct
    /// reproduction against this exact crate version. Set once, true for
    /// good, at the end of `negotiate()` - see the `ready.store` there.
    ready: Arc<AtomicBool>,
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
        // `session::handle`'s main loop watches this to shut the session
        // down promptly on disconnect/failure/close — see the `pc_state_rx`
        // arm there — instead of only reacting once (if ever) the control
        // data channel happens to notice on its own.
        let _ = self.state_tx.send(state);
    }

    async fn on_data_channel(&self, data_channel: Arc<dyn DataChannel>) {
        let _ = self.dc_tx.send(data_channel);
    }

    /// Fires whenever this connection has something new to negotiate
    /// after its initial offer/answer exchange — e.g. `session::handle`
    /// added or removed the video track in response to real
    /// capture-device availability (`CaptureEngine::request_stream`/
    /// `CaptureStream`'s `ended` event). Builds a fresh offer and hands
    /// its SDP to `session::handle` to forward over `control`; the crate's
    /// own
    /// `NegotiationNeededState` coalescing (confirmed in `rtc-0.20.3`)
    /// already prevents this from firing again for the same connection
    /// until the resulting round trip completes, so no additional
    /// debouncing is needed here.
    async fn on_negotiation_needed(&self) {
        if !self.ready.load(Ordering::Relaxed) {
            return;
        }
        let Some(pc) = self.pc.get().and_then(Weak::upgrade) else {
            return;
        };
        let offer = match pc.create_offer(None).await {
            Ok(offer) => offer,
            Err(err) => {
                tracing::debug!(%err, "skipping renegotiation: couldn't create offer");
                return;
            }
        };
        if let Err(err) = pc.set_local_description(offer).await {
            tracing::debug!(%err, "skipping renegotiation: couldn't set local description");
            return;
        }
        let Some(local_description) = pc.local_description().await else {
            tracing::debug!("skipping renegotiation: no local description after setting one");
            return;
        };
        let _ = self.renegotiation_tx.send(local_description.sdp);
    }
}

/// Builds a fresh `RTCPeerConnection` for one browser tab, negotiates the
/// initial offer/answer exchange, and spawns `session::handle` to run for
/// the life of the connection. Returns the answer SDP to send back once
/// ICE gathering completes (there's nothing left to negotiate after that -
/// the browser did the same non-trickle wait before sending its offer).
/// No video track is attached here — the browser's offer already includes
/// a recvonly video transceiver (see `assets/web/app.js`'s `connect()`),
/// but the session starts with nothing sending on it; `session::handle`
/// attaches one later, once its connection is stable and a capture stream
/// is actually available (see `Handler::on_negotiation_needed`).
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

    let mut setting_engine = SettingEngine::default();
    // This device is only ever used on a local network, never over STUN/TURN
    // (none is configured). Some browsers hide their real LAN IP behind a
    // random "<uuid>.local" mDNS name instead of sending it directly, and
    // with no STUN/TURN server there's no other candidate to fall back on.
    // Disabling mDNS here means we don't try (and fail) to resolve that name
    // - we just drop it, same as if it were never sent.
    setting_engine.set_multicast_dns_mode(MulticastDnsMode::Disabled);

    let (gather_complete, gather_complete_rx) = OnceSignal::new();
    let (dc_tx, dc_rx) = mpsc::unbounded_channel();
    let (state_tx, pc_state_rx) = watch::channel(RTCPeerConnectionState::New);
    let (renegotiation_tx, renegotiation_rx) = mpsc::unbounded_channel();
    let pc_cell: Arc<OnceCell<Weak<dyn PeerConnection>>> = Arc::new(OnceCell::new());
    let ready = Arc::new(AtomicBool::new(false));
    let handler = Arc::new(Handler { gather_complete, dc_tx, state_tx, pc: pc_cell.clone(), renegotiation_tx, ready: ready.clone() });

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
    // Only settable now that `peer_connection` actually exists - see the
    // doc comment on `Handler::pc`. Never fails: this is the only place
    // that ever sets it, once, right here.
    let _ = pc_cell.set(Arc::downgrade(&peer_connection));

    let offer = RTCSessionDescription::offer(offer_sdp).context("parsing browser's SDP offer")?;
    peer_connection.set_remote_description(offer).await.context("applying browser's offer")?;
    let answer = peer_connection.create_answer(None).await.context("creating SDP answer")?;
    peer_connection.set_local_description(answer).await.context("setting local description")?;

    // Block until ICE gathering is complete (non-trickle) - see the doc
    // comment on `offer_handler`.
    let _ = gather_complete_rx.await;

    let local_description = peer_connection.local_description().await.context("no local description after ICE gathering completed")?;
    // Only now does `Handler::on_negotiation_needed` start acting on
    // renegotiation triggers - see the doc comment on `Handler::ready` for
    // why this can't just be unconditional.
    ready.store(true, Ordering::Relaxed);

    let ctx = SessionContext {
        capture_engine: channels.capture_engine,
        hid: channels.hid,
        capture_settings_tx: channels.capture_settings_tx.clone(),
        capture_settings_rx: channels.capture_settings_tx.subscribe(),
        mouse_mode_rx: channels.mouse_mode_rx.clone(),
        device_state_rx: channels.device_state_rx,
        hid_connected_rx: channels.hid_connected_rx,
        h264_codec: h264_codec.rtp_codec,
        pc_state_rx,
    };
    let pc_for_session = peer_connection.clone();
    tokio::spawn(async move {
        if let Err(err) = session::handle(pc_for_session, dc_rx, renegotiation_rx, ctx).await {
            tracing::debug!(%err, "WebRTC session ended");
        }
    });

    Ok(local_description.sdp)
}

fn rand_u32() -> u32 {
    use std::hash::BuildHasher;
    std::collections::hash_map::RandomState::new().hash_one(std::time::Instant::now()) as u32
}
