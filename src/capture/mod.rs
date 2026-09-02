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

use crate::device::OpenError;

/// `pub(crate)` (rather than private) so `capture::engine`'s `CaptureCard`
/// can reuse this exact function for its own encode pass - see issue #004,
/// which is required to reuse this machinery rather than reinvent it.
///
/// Attempts to open the capture device for real. Folds "we already know
/// from probe this format isn't supported" into the same fallible outcome
/// as a genuine negotiate failure - callers (see `capture::engine::
/// start_pass`) treat both identically: a direct, awaited failure from
/// `request_stream()`, never a stream that gets created only to
/// immediately end (issue #027).
///
/// This is where the capture card is actually opened, on the blocking
/// thread the pass runs on and only once a consumer has asked for a
/// stream - never from presence detection or probing, which is what keeps
/// the boot-crash the real hardware suffers from off the table (see
/// `main`'s capture comment).
pub(crate) fn open_capture(device: &CaptureDevice, format: &Option<SupportedFormat>, settings: &CaptureSettings) -> Result<crate::device::CaptureHandle, OpenError> {
    if format.is_none() {
        return Err(OpenError("capture device doesn't support YUYV capture".to_string()));
    }

    device.open(settings)
}

/// Runs the blocking read/encode loop against an already-open handle until
/// `stop` or an unrecoverable read/encode error - split out from the open
/// step (`open_capture`) so `start_pass` can await just the open half
/// before deciding whether a `CaptureStream` should exist at all (issue
/// #027). Never fails the caller - the same "log and stop this pass" logic
/// `run_one_pass` used to have for a loop error.
///
/// Takes no `CaptureSettings` - unlike `open_capture`, `v4l2::
/// run_capture_loop` sizes everything from the handle's own negotiated
/// resolution, so there's nothing here for the requested settings to do.
pub(crate) fn run_capture_loop_forever(capture: &crate::device::CaptureHandle, stop: Arc<AtomicBool>, video_bus: video_bus::Sender, force_keyframe: Arc<AtomicBool>) {
    // `make_handler` runs with the *actual* negotiated resolution the open
    // handle carries (which the driver is free to pick differently than
    // what was requested) — sizing the H.264 conversion buffers from
    // anything else risks reading past the end of a real frame. It's
    // fallible: if GPU setup fails, `run_capture_loop` returns before ever
    // opening the capture stream, instead of reading and dropping frames
    // for a pass that can't encode them anyway.
    let result = v4l2::run_capture_loop(capture, || stop.load(Ordering::Relaxed), move |actual_resolution| -> anyhow::Result<_> {
        let mut encoder = h264::H264Encoder::new(actual_resolution.width, actual_resolution.height).context("Failed to set up GPU")?;

        Ok(move |frame: &[u8], captured_at: v4l2::Timestamp, sequence: u32| {
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
                    // `sequence` names which capture frame this was, so a
                    // run of these can be checked against the driver's own
                    // count for gaps (a dropped frame, not just a short
                    // one). The hex dump is debug-level, since it's only
                    // useful when actively chasing one of these down.
                    tracing::error!(%err, sequence, "H.264 encode failed");
                    tracing::debug!("bad frame contents:\n{}", hex_dump(frame));
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

/// Formats `data` like `hexdump -C`, collapsing runs of identical 16-byte
/// lines down to a single `*` - a raw YUYV frame is often large stretches
/// of one repeated pixel, and this keeps a dumped frame from flooding the
/// log with lines that say nothing new.
fn hex_dump(data: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let mut last_line: Option<&[u8]> = None;
    let mut skipping = false;
    for (i, chunk) in data.chunks(16).enumerate() {
        if last_line == Some(chunk) {
            if !skipping {
                out.push_str("*\n");
                skipping = true;
            }
            continue;
        }
        skipping = false;
        last_line = Some(chunk);
        let offset = i * 16;
        let hex: String = chunk.iter().map(|b| format!("{b:02x} ")).collect();
        let ascii: String = chunk.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
        let _ = writeln!(out, "{offset:08x}  {hex:<49}|{ascii}|");
    }
    let _ = writeln!(out, "{:08x}", data.len());
    out
}
