pub mod engine;
pub mod h264;
mod v4l2;
mod video_bus;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use tokio::sync::watch;

use crate::config::{DeviceState, ResolutionFrameRates};
use crate::device::DeviceStatus;

/// The frame bus itself stays private to `capture`, but the frame it
/// carries is what a session pulls off a `CaptureStream`, so the type has
/// to cross this module's boundary.
pub use video_bus::FrameEnvelope;

/// The capture card's own types come from `device`, which owns the driver
/// that produces them (its `Info`, its `Settings`, and the resolution it
/// negotiates). They're re-exported here because they're part of what
/// `CaptureEngine`'s API takes and reports, so callers never have to reach
/// past `capture` into `device` for them.
pub use crate::device::{CaptureDevice, CaptureSettings, Resolution, SupportedFormat};

/// Publishes `DeviceState` for the web UI from the capture device's own
/// presence/capability stream (`Device<CaptureDriver>`, via `device`) and
/// the live `CaptureSettings` - the sole owner/source of that state now
/// that presence tracking lives in `Device<D>` and streaming lives in
/// `CaptureEngine`, replacing the inline probe-and-push loop
/// `CaptureManager::run` used to own. Runs for the life of the process,
/// via the subscription kept alive inside the spawned task below; the
/// returned `watch::Receiver` starts at `DeviceState::default()` until the
/// first presence event or settings snapshot arrives.
pub fn watch_device_state(device: CaptureDevice, mut settings_rx: watch::Receiver<CaptureSettings>) -> watch::Receiver<DeviceState> {
    let (tx, rx) = watch::channel(DeviceState::default());
    let format: Arc<Mutex<Option<SupportedFormat>>> = Arc::new(Mutex::new(None));

    let tx_for_presence = tx.clone();
    let format_for_presence = Arc::clone(&format);
    let settings_for_presence = settings_rx.clone();
    let sub = device.add_event_listener(move |status| {
        let tx = tx_for_presence.clone();
        let format = Arc::clone(&format_for_presence);
        let settings = settings_for_presence.clone();
        async move {
            // `Present(None)` is the boot-crash-risk "already present,
            // deliberately not probed" transition (see `device::Device`'s
            // doc comment) - `format` is left exactly as it was (`None`
            // the first time this ever fires), so `DeviceState` correctly
            // stays unavailable until a genuine transition actually probes
            // the device.
            let new_format = match status {
                DeviceStatus::Present(Some(info)) => Some(info),
                DeviceStatus::Present(None) => format.lock().unwrap().clone(),
                DeviceStatus::Absent => None,
            };
            *format.lock().unwrap() = new_format.clone();
            publish_device_state(&tx, &new_format, &settings.borrow());
        }
    });

    // Also republishes on every settings change - `device_state_for`'s
    // `default_resolution` depends on the currently-applied `settings`,
    // not just on `format`. Holds `sub` alive for as long as this task
    // runs, which is the life of the process (the settings channel only
    // closes at shutdown).
    tokio::spawn(async move {
        let _sub = sub;
        while settings_rx.changed().await.is_ok() {
            let current = *settings_rx.borrow();
            let current_format = format.lock().unwrap().clone();
            publish_device_state(&tx, &current_format, &current);
        }
    });

    rx
}

/// Recomputes `device_state_for` from whatever `format` is currently
/// cached and publishes it if it actually changed. Cheap and ioctl-free -
/// safe to call on every presence event or settings change, unlike an
/// actual probe.
fn publish_device_state(device_state_tx: &watch::Sender<DeviceState>, format: &Option<SupportedFormat>, settings: &CaptureSettings) {
    let new_state = device_state_for(format, settings);
    device_state_tx.send_if_modified(|s| {
        if *s == new_state {
            false
        } else {
            *s = new_state;
            true
        }
    });
}

/// `pub(crate)` (rather than private) so `capture::engine`'s `CaptureEngine`
/// can reuse this exact function for its own encode pass - see issue #004,
/// which is required to reuse this machinery rather than reinvent it.
///
/// This is where the capture card is actually opened, on the blocking
/// thread the pass runs on and only once a consumer has asked for a
/// stream - never from presence detection or probing, which is what keeps
/// the boot-crash the real hardware suffers from off the table (see
/// `main`'s capture comment).
pub(crate) fn run_one_pass(device: &CaptureDevice, format: &Option<SupportedFormat>, settings: &CaptureSettings, stop: Arc<AtomicBool>, video_bus: video_bus::Sender, force_keyframe: Arc<AtomicBool>) {
    if format.is_none() {
        tracing::error!("capture device doesn't support YUYV capture, no video this pass");
        return;
    }

    let capture = match device.open(settings) {
        Ok(capture) => capture,
        Err(err) => {
            tracing::error!(%err, "failed to open capture device, no video this pass");
            return;
        }
    };

    // `make_handler` runs with the *actual* negotiated resolution the open
    // handle carries (which the driver is free to pick differently than
    // what was requested) — sizing the H.264 conversion buffers from
    // anything else risks reading past the end of a real frame. It's
    // fallible: if GPU setup fails, `run_capture_loop` returns before ever
    // opening the capture stream, instead of reading and dropping frames
    // for a pass that can't encode them anyway.
    let result = v4l2::run_capture_loop(&capture, || stop.load(Ordering::Relaxed), move |actual_resolution| -> anyhow::Result<_> {
        let mut encoder = h264::H264Encoder::new(actual_resolution.width, actual_resolution.height).context("Failed to set up GPU")?;

        Ok(move |frame: &[u8], captured_at: v4l2::Timestamp| {
            let captured_at = v4l2::timestamp_to_duration(captured_at);
            // A session asked (via RTCP PLI/FIR) for a fresh keyframe
            // sooner than the encoder's own periodic schedule - see
            // `CaptureStream::request_keyframe` and
            // `rtc::session::handle`'s RTCP-poll branch.
            if force_keyframe.swap(false, Ordering::Relaxed) {
                encoder.force_intra_frame();
            }
            let envelope = match encoder.encode_yuyv_frame(frame) {
                Ok(bytes) => FrameEnvelope { data: bytes.into(), captured_at },
                Err(err) => {
                    tracing::error!(%err, "H.264 encode failed");
                    return;
                }
            };
            let _ = video_bus.send(Some(envelope));
        })
    });

    if let Err(err) = result {
        tracing::error!(%err, "capture loop exited with error");
    }
}

/// Shared by `publish_device_state` - kept as a free function over
/// `&Option<SupportedFormat>` rather than tied to any particular struct.
fn device_state_for(format: &Option<SupportedFormat>, settings: &CaptureSettings) -> DeviceState {
    let Some(format) = format else {
        return DeviceState::default();
    };
    let default_resolution = if format.resolutions.contains(&settings.resolution) { Some(settings.resolution) } else { format.resolutions.first().copied() };
    let frame_rates = format
        .resolutions
        .iter()
        .map(|&resolution| {
            // Falls back to just the currently-applied fps if the card
            // didn't report a discrete list for this resolution — the
            // dropdown should never be empty, and the applied value is
            // always a valid option.
            let rates = format.frame_rates.get(&resolution).cloned().unwrap_or_else(|| vec![settings.fps]);
            ResolutionFrameRates { resolution, rates }
        })
        .collect();
    DeviceState { available: true, resolutions: format.resolutions.clone(), default_resolution, frame_rates }
}
