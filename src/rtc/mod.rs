mod device_state;
pub mod protocol;
pub mod session;

pub use device_state::DeviceState;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use anyhow::{Context, Result};
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

use crate::capture::engine::CaptureCard;
use crate::capture::CaptureDevice;
use crate::device::{DeviceStatus, Subscription, SupportedFormat};
use crate::hid::{Ch9329Device, Hid};
use session::SessionContext;

/// The WebRTC module itself: peer sessions and the offer→answer command
/// `web` calls on every signaling request. Just the two modules a session
/// talks to — there is no shared state here beyond them, and no channel:
/// everything a session needs to know it either asks for
/// (`CaptureCard::settings`, `Hid::mouse_mode`, …) or subscribes to for
/// itself (see `session::handle`), so nothing has to be pre-wired per
/// process and cloned into every tab.
///
/// Nothing here knows it is reached over HTTP — no request type, no status
/// code, no framework import (`ARCHITECTURE.md` §3.4/§I6). `web` does the
/// parsing and the status mapping.
#[derive(Clone)]
pub struct Rtc {
    /// Shared across every session - `session::handle` calls
    /// `request_stream()` on this once its connection is stable, and again
    /// any time a device-availability event says it's worth retrying (see
    /// `capture_device`, below). The encode pass this wraps starts/stops as
    /// a direct consequence of how many sessions currently hold a live
    /// `CaptureStream` - nothing here counts that by hand.
    capture_card: Arc<CaptureCard>,
    /// A clone of the same `CaptureDevice` handle `capture_card` holds
    /// internally (via `CaptureCard::device`, not a second
    /// `Device::spawn()` - `ARCHITECTURE.md` §3.1). `rtc` is now the thing
    /// that subscribes to it for presence/capability changes and computes
    /// `DeviceState` from what it reports plus `capture_card.settings()`
    /// (§3.4) - `capture` itself no longer does either.
    capture_device: CaptureDevice,
    /// Shared across every session - `session` calls `send` on this for
    /// every key/mouse event. The queue, the port and the drain worker
    /// live behind it; nothing here holds any of them.
    hid: Arc<Hid>,
    /// A clone of the same `Ch9329Device` handle `hid` holds internally
    /// (via `Hid::device`, not a second `Device::spawn()`). `rtc` is now
    /// the thing that subscribes to it for presence - `hid` itself no
    /// longer does.
    hid_device: Ch9329Device,
    /// Applies the device's own first reported resolution/frame rate as the
    /// default the moment capabilities become known, as long as nobody has
    /// saved settings by hand (`CaptureCard::apply_default_settings` is
    /// itself the guard - see `ARCHITECTURE.md` §3.4). Lives for the life of
    /// the process, not per-session: `Arc`-wrapped since `Rtc` is `Clone` and
    /// a bare `Subscription` isn't - every clone shares this one subscription,
    /// which deregisters only once the last `Rtc` clone drops.
    _default_settings_sub: Arc<Subscription<DeviceStatus<SupportedFormat>>>,
}

impl Rtc {
    /// The composition root for `rtc`'s own dependencies, the same way
    /// `main` is the composition root for `rtc`/`web` (`ARCHITECTURE.md`
    /// §3.4/§3.6) — building this is what starts the capture card's
    /// presence task and `Hid`'s own device/drain worker.
    ///
    /// The capture card is never opened automatically right here at
    /// startup. Opening it unprompted has reliably crashed the real
    /// hardware this targets right at boot (see README's "boot-crash"
    /// known issue) - `Device<CaptureDriver>`'s presence task (spawned as
    /// part of `CaptureCard::spawn` below) deliberately never probes the
    /// very first time it finds the device already present, for exactly
    /// that reason. `CaptureCard` owns the capture settings and the UI-facing
    /// device state; both are in-memory only, and nothing here is ever
    /// read from or written to disk. Nothing here ever sees the raw
    /// device path either - the device reads it from its own config.
    ///
    /// Serial gets the same soft-unavailable treatment as capture. `Hid`
    /// owns its own device, queue, drain worker and mouse mode; commands
    /// sent before its port is open queue up rather than being lost, so
    /// nothing here holds up the HTTP page starting.
    pub fn spawn() -> Self {
        Self::new(CaptureCard::spawn(), Hid::spawn())
    }

    fn new(capture_card: Arc<CaptureCard>, hid: Arc<Hid>) -> Self {
        let capture_device = capture_card.device();
        let hid_device = hid.device();

        let default_settings_sub = {
            let capture_card = Arc::clone(&capture_card);
            capture_device.add_event_listener(move |status| {
                let capture_card = Arc::clone(&capture_card);
                async move {
                    if let DeviceStatus::Present(Some(info)) = status
                        && let Some(settings) = device_state::first_reported_settings(&info)
                    {
                        capture_card.apply_default_settings(settings);
                    }
                }
            })
        };

        Self { capture_card, capture_device, hid, hid_device, _default_settings_sub: Arc::new(default_settings_sub) }
    }

    /// The whole signaling surface: hand it the browser's offer SDP, get
    /// the answer SDP back. One call is the entire exchange (see
    /// `assets/web/app.js`'s `connect()`) — no trickle ICE, no WebSocket:
    /// the browser waits for its own ICE gathering to finish before
    /// sending the offer, and this waits for its own gathering to finish
    /// before returning, so one round trip is enough.
    pub async fn handle_offer(&self, offer_sdp: String) -> Result<String, SignalingError> {
        negotiate(offer_sdp, self.clone()).await.map_err(SignalingError)
    }
}

/// Signaling failed. Every way it can fail is the caller's offer being
/// unusable — a malformed SDP, or one this peer can't build a connection
/// for — so there is nothing here for a caller to branch on, only
/// something to report.
#[derive(Debug)]
pub struct SignalingError(anyhow::Error);

impl std::fmt::Display for SignalingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for SignalingError {}

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
    /// capture-device availability (`CaptureCard::request_stream`/
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
async fn negotiate(offer_sdp: String, rtc: Rtc) -> Result<String> {
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
    // comment on `Rtc::handle_offer`.
    let _ = gather_complete_rx.await;

    let local_description = peer_connection.local_description().await.context("no local description after ICE gathering completed")?;
    // Only now does `Handler::on_negotiation_needed` start acting on
    // renegotiation triggers - see the doc comment on `Handler::ready` for
    // why this can't just be unconditional.
    ready.store(true, Ordering::Relaxed);

    let ctx = SessionContext { capture_card: rtc.capture_card, capture_device: rtc.capture_device, hid: rtc.hid, hid_device: rtc.hid_device, h264_codec: h264_codec.rtp_codec, pc_state_rx };
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
