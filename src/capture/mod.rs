pub mod engine;
pub mod h264;
mod v4l2;
mod video_bus;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context as _;

/// The frame bus itself stays private to `capture`, but the frame it
/// carries is what a session pulls off a `CaptureStream`, so the type has
/// to cross this module's boundary.
pub use video_bus::FrameEnvelope;

/// The capture card's own types come from `device`, which owns the driver
/// that produces them (its `Info`, its `Settings`, and the resolution it
/// negotiates). They're re-exported here because they're part of what
/// `CaptureCard`'s API takes and reports, so callers never have to reach
/// past `capture` into `device` for them.
pub use crate::device::{CaptureDevice, CaptureSettings, Resolution, SupportedFormat};

/// `pub(crate)` (rather than private) so `capture::engine`'s `CaptureCard`
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
