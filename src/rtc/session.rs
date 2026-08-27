//! Per-connection handling: one task juggling video, input, and control
//! over `tokio::select!` — sending video frames as they arrive on the
//! shared `video_bus`, translating `input` data channel messages into
//! `SerialCommand`s, and reading `control` data channel JSON (the Save
//! button's settings update, paste). Settings changes only touch the
//! shared `watch<Settings>` — in memory only, the connection itself is
//! never disturbed, so applying new settings never drops the session.

use anyhow::Result;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use rtc::media::Sample;
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtcp::Packet;
use rtc::rtp_transceiver::PayloadType;
use tokio::sync::{mpsc, watch};
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::media_stream::Track;
use webrtc::peer_connection::{PeerConnection, RTCPeerConnectionState};
use webrtc::rtp_transceiver::RtpSender;

use crate::capture::v4l2::Resolution;
use crate::ch9329::keymap::{self, KeyCode};
use crate::ch9329::writer::SerialCommand;
use crate::config::{CaptureSettings, DeviceState, MouseMode};
use crate::video_bus;

use super::protocol::{ControlMessage, InputEvent, MouseModeWire, ServerMessage};

pub struct SessionContext {
    pub video_bus: video_bus::Receiver,
    pub serial_tx: mpsc::Sender<SerialCommand>,
    pub capture_settings_tx: watch::Sender<CaptureSettings>,
    pub capture_settings_rx: watch::Receiver<CaptureSettings>,
    pub mouse_mode_tx: watch::Sender<MouseMode>,
    pub mouse_mode_rx: watch::Receiver<MouseMode>,
    pub device_state_rx: watch::Receiver<DeviceState>,
    pub hid_connected_rx: watch::Receiver<bool>,
    pub force_keyframe: Arc<AtomicBool>,
    /// See `super::SharedChannels::client_count_tx` - `handle` below wraps
    /// this in a `ClientCountGuard` the moment the connection first reaches
    /// `Connected`, not before.
    pub client_count_tx: watch::Sender<u32>,
    /// Mirrors the `RTCPeerConnection`'s own connection state (see
    /// `rtc::Handler::on_connection_state_change`) - watched by `handle`'s
    /// main loop so the session shuts down as soon as the connection
    /// disconnects/fails/closes, rather than only when the `control` data
    /// channel happens to notice on its own.
    pub pc_state_rx: watch::Receiver<RTCPeerConnectionState>,
}

/// Held from the moment a session's `RTCPeerConnection` first reaches
/// `Connected` (see `handle`'s `pc_state_rx` arm) until the session ends;
/// its `Drop` is what guarantees the count comes back down no matter which
/// exit path (clean shutdown, peer-connection failure/close, panic)
/// actually ends it. `CaptureManager::run` only opens/streams the capture
/// card while at least one of these is held.
struct ClientCountGuard(watch::Sender<u32>);

impl ClientCountGuard {
    fn new(tx: watch::Sender<u32>) -> Self {
        tx.send_modify(|n| *n += 1);
        Self(tx)
    }
}

impl Drop for ClientCountGuard {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n = n.saturating_sub(1));
    }
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

pub async fn handle(
    _pc: Arc<dyn PeerConnection>,
    video_track: Arc<TrackLocalStaticSample>,
    video_sender: Arc<dyn RtpSender>,
    dc_rx: mpsc::UnboundedReceiver<Arc<dyn DataChannel>>,
    ctx: SessionContext,
) -> Result<()> {
    let DataChannels { input, control } = collect_data_channels(dc_rx).await?;
    let video_target = video_target(&video_track, &video_sender).await;
    if let Err(err) = &video_target {
        tracing::warn!(%err, "H.264 track not usable for this session; video will not work, input/control are unaffected");
    }

    let mut video_rx = ctx.video_bus.clone();
    let mut device_state_rx = ctx.device_state_rx.clone();
    let mut capture_settings_rx = ctx.capture_settings_rx.clone();
    let mut mouse_mode_rx = ctx.mouse_mode_rx.clone();
    let mut hid_connected_rx = ctx.hid_connected_rx.clone();
    let mut pc_state_rx = ctx.pc_state_rx.clone();
    let mut keyboard = KeyboardState::default();

    let mut input_active = true;
    let mut control_open = false;
    // Created the moment the connection first reaches `Connected` - see the
    // `pc_state_rx` arm below. Kept `None` until then so the capture card
    // is never opened/streamed to before the connection is fully stable.
    let mut client_count_guard: Option<ClientCountGuard> = None;
    // The real capture-time delta between consecutive frames, used as each
    // RTP sample's duration so the browser's jitter buffer gets correct
    // pacing info — see `send_frame`. `None` only for the very first frame
    // of a session, which has no prior frame to diff against.
    let mut last_captured_at: Option<Duration> = None;
    // Set once the video_bus sender is gone for good (only happens if the
    // capture task itself is torn down, e.g. process shutdown - it no
    // longer exits just because the card is absent or unplugged, see
    // `CaptureManager::run`). Keyboard and mouse must keep working either
    // way, so this only disables the video branch — it must never end the
    // session.
    let mut video_closed = false;

    loop {
        tokio::select! {
            changed = video_rx.changed(), if !video_closed => {
                match changed {
                    Ok(()) => {
                        let frame = video_rx.borrow_and_update().clone();
                        if let Some(frame) = frame {
                            let duration = last_captured_at.map(|t| frame.captured_at.saturating_sub(t)).unwrap_or(Duration::ZERO);
                            last_captured_at = Some(frame.captured_at);
                            send_frame(&frame.data, duration, &video_track, video_target.as_ref().ok().copied()).await;
                        }
                    }
                    Err(_) => video_closed = true,
                }
            }
            event = video_track.poll(), if video_target.is_ok() => {
                if let Some(TrackLocalEvent::OnRtcpPacket(packets)) = event
                    && packets.iter().any(|p| is_keyframe_request(p.as_ref()))
                {
                    ctx.force_keyframe.store(true, Ordering::Relaxed);
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
                        // `CaptureManager::run` last cached from probing the card
                        // when it was plugged in - opening this channel doesn't
                        // trigger a fresh probe of its own.
                        control_open = push_initial_state(&control, &mut device_state_rx, &mut hid_connected_rx, &mut capture_settings_rx, &mut mouse_mode_rx).await.is_ok();
                    }
                    Some(DataChannelEvent::OnMessage(msg)) => {
                        match serde_json::from_slice::<ControlMessage>(&msg.data) {
                            Ok(msg) => handle_control_message(msg, &ctx),
                            Err(err) => tracing::debug!(%err, "ignoring malformed control message"),
                        }
                    }
                    Some(DataChannelEvent::OnClose) | None => break,
                    _ => {}
                }
            }
            changed = device_state_rx.changed(), if control_open => {
                if changed.is_ok() {
                    let state = device_state_rx.borrow_and_update().clone();
                    let msg = ServerMessage::DeviceState(state);
                    if send_server_message(&control, &msg).await.is_err() {
                        control_open = false;
                    }
                }
            }
            changed = capture_settings_rx.changed(), if control_open => {
                if changed.is_ok() {
                    let capture = *capture_settings_rx.borrow_and_update();
                    let msg = ServerMessage::Settings { capture, mouse_mode: *mouse_mode_rx.borrow() };
                    if send_server_message(&control, &msg).await.is_err() {
                        control_open = false;
                    }
                }
            }
            changed = mouse_mode_rx.changed(), if control_open => {
                if changed.is_ok() {
                    let mouse_mode = *mouse_mode_rx.borrow_and_update();
                    let msg = ServerMessage::Settings { capture: *capture_settings_rx.borrow(), mouse_mode };
                    if send_server_message(&control, &msg).await.is_err() {
                        control_open = false;
                    }
                }
            }
            changed = hid_connected_rx.changed(), if control_open => {
                if changed.is_ok() {
                    let available = *hid_connected_rx.borrow_and_update();
                    let msg = ServerMessage::HidState { available };
                    if send_server_message(&control, &msg).await.is_err() {
                        control_open = false;
                    }
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
                if state == RTCPeerConnectionState::Connected && client_count_guard.is_none() {
                    // Only now is the connection fully stable and ready for
                    // video - see `ClientCountGuard` and
                    // `CaptureManager::run`, which only opens/streams the
                    // capture card while at least one of these is held.
                    client_count_guard = Some(ClientCountGuard::new(ctx.client_count_tx.clone()));
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

/// Pushes the full current state (device availability, HID connectivity,
/// settings) down the just-opened `control` channel. Needed because
/// `changed()` on a receiver only fires for changes from here on — it
/// doesn't tell a freshly connected tab about state that was already
/// current before it connected (see the call site in `handle`).
async fn push_initial_state(
    control: &Arc<dyn DataChannel>,
    device_state_rx: &mut watch::Receiver<DeviceState>,
    hid_connected_rx: &mut watch::Receiver<bool>,
    capture_settings_rx: &mut watch::Receiver<CaptureSettings>,
    mouse_mode_rx: &mut watch::Receiver<MouseMode>,
) -> Result<()> {
    let device_state = device_state_rx.borrow_and_update().clone();
    send_server_message(control, &ServerMessage::DeviceState(device_state)).await?;
    let hid_available = *hid_connected_rx.borrow_and_update();
    send_server_message(control, &ServerMessage::HidState { available: hid_available }).await?;
    let capture = *capture_settings_rx.borrow_and_update();
    let mouse_mode = *mouse_mode_rx.borrow_and_update();
    send_server_message(control, &ServerMessage::Settings { capture, mouse_mode }).await?;
    Ok(())
}

async fn send_server_message(control: &Arc<dyn DataChannel>, msg: &ServerMessage) -> Result<()> {
    let bytes = serde_json::to_vec(msg)?;
    control.send(BytesMut::from(bytes.as_slice())).await?;
    Ok(())
}

/// Sends one H.264 frame as RTP via the local video track. Errors are
/// dropped, not propagated — a not-yet-negotiated track just means this
/// frame is skipped, the same tolerance the video_bus watch channel
/// already gives every subscriber.
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
/// `video_track.poll()` branch).
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::v4l2::Resolution;
    use crate::rtc::protocol::CaptureSettingsWire;

    fn test_ctx() -> SessionContext {
        let (_video_tx, video_rx) = video_bus::channel();
        let (serial_tx, _serial_rx) = mpsc::channel(1);
        let (capture_settings_tx, capture_settings_rx) =
            watch::channel(CaptureSettings { resolution: Resolution { width: 1280, height: 720 }, fps: 5 });
        let (mouse_mode_tx, mouse_mode_rx) = watch::channel(MouseMode::Absolute);
        let (_device_state_tx, device_state_rx) = watch::channel(DeviceState::default());
        let (_hid_connected_tx, hid_connected_rx) = watch::channel(false);
        let (_pc_state_tx, pc_state_rx) = watch::channel(RTCPeerConnectionState::New);
        let (client_count_tx, _client_count_rx) = watch::channel(0u32);
        SessionContext {
            video_bus: video_rx,
            serial_tx,
            capture_settings_tx,
            capture_settings_rx,
            mouse_mode_tx,
            mouse_mode_rx,
            device_state_rx,
            hid_connected_rx,
            force_keyframe: Arc::new(AtomicBool::new(false)),
            client_count_tx,
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
}
