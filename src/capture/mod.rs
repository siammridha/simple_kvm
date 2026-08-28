pub mod engine;
pub mod h264;
mod v4l2;
mod video_bus;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context as _;
use serde::Serialize;

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

/// Live state of the capture card itself — whether it's plugged in right
/// now, and what resolutions/frame rates it supports. Computed and
/// published by `CaptureEngine`, which is the only thing holding both the
/// card's reported capabilities and the currently-applied settings, and
/// pushed to the web page over the `control` data channel (see
/// `rtc::session::handle`) so an already-open tab reflects a
/// hot-plug/unplug instead of being frozen at server-startup values.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DeviceState {
    pub available: bool,
    pub resolutions: Vec<Resolution>,
    pub default_resolution: Option<Resolution>,
    pub frame_rates: Vec<ResolutionFrameRates>,
}

/// One resolution's discrete frame-rate list — `Vec` rather than a
/// `Resolution`-keyed map, since JSON object keys must be strings and
/// `Resolution` isn't one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResolutionFrameRates {
    pub resolution: Resolution,
    pub rates: Vec<u32>,
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

/// Called by `CaptureEngine` every time either half of what it depends on
/// moves - the card's reported capabilities or the applied settings. Cheap
/// and ioctl-free, so it's safe to run on every presence event or settings
/// change, unlike an actual probe. Kept as a free function over
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
