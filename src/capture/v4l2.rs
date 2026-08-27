//! V4L2 enumeration and capture, using the `v4l` crate.
//!
//! The device+stream pairing is kept as *local variables within one
//! function* (`run_capture_loop`), not stored together in a struct: the
//! `v4l` crate's `MmapStream<'a>` borrows the `Device` it was built from,
//! and the two are only ever used together inside a single blocking loop
//! anyway (per the concurrency design — a settings change stops the loop
//! and starts a fresh one), so there's no need to fight the lifetime.
//!
//! Only the raw YUYV (4:2:2) format is ever requested — that's what the
//! H.264 encoder needs to encode from (see `capture::h264`).

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use v4l::buffer::Type as BufferType;
use v4l::format::FourCC;
use v4l::frameinterval::FrameIntervalEnum;
use v4l::io::mmap::Stream as MmapStream;
use v4l::io::traits::CaptureStream;
use v4l::video::capture::Parameters;
use v4l::video::Capture;
use v4l::{Device, Format};

pub use v4l::timestamp::Timestamp;

const YUYV_FOURCC: FourCC = FourCC { repr: *b"YUYV" };

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct SupportedFormat {
    pub resolutions: Vec<Resolution>,
    /// Frame rates (fps) the card actually reports for each resolution
    /// (`VIDIOC_ENUM_FRAMEINTERVALS`) — support genuinely varies by
    /// resolution on real hardware. A resolution missing from this map
    /// (continuous/"stepwise" rates, or the query failing) just means no
    /// discrete list is available for it.
    pub frame_rates: HashMap<Resolution, Vec<u32>>,
}

/// Queries the card at `path` for the YUYV resolutions/frame rates it
/// supports. Returns `None` if the card doesn't offer YUYV at all.
pub fn enumerate(path: &str) -> Result<Option<SupportedFormat>> {
    let dev = Device::with_path(path).with_context(|| format!("opening {path}"))?;
    let Some(desc) = dev.enum_formats().with_context(|| format!("enumerating formats on {path}"))?.into_iter().find(|d| d.fourcc == YUYV_FOURCC) else {
        return Ok(None);
    };

    let frame_sizes = dev.enum_framesizes(desc.fourcc).with_context(|| format!("enumerating frame sizes on {path}"))?;
    let mut resolutions: Vec<Resolution> = frame_sizes.into_iter().flat_map(|frame_size| frame_size.size.to_discrete()).map(|d| Resolution { width: d.width, height: d.height }).collect();
    resolutions.sort_by_key(|r| std::cmp::Reverse(r.width * r.height));
    resolutions.dedup();

    let mut frame_rates = HashMap::new();
    for resolution in &resolutions {
        let rates = frame_rates_for(&dev, desc.fourcc, *resolution);
        if !rates.is_empty() {
            frame_rates.insert(*resolution, rates);
        }
    }

    Ok(Some(SupportedFormat { resolutions, frame_rates }))
}

/// Discrete frame rates (fps) the card reports for `fourcc` at
/// `resolution`, sorted descending and deduped. Continuous/"stepwise"
/// entries are skipped (none showed up on the real device this targets),
/// and a failed query just yields an empty list rather than an error —
/// callers already treat a missing/empty entry as "fall back to whatever's
/// currently configured".
fn frame_rates_for(dev: &Device, fourcc: FourCC, resolution: Resolution) -> Vec<u32> {
    let intervals = match dev.enum_frameintervals(fourcc, resolution.width, resolution.height) {
        Ok(intervals) => intervals,
        Err(err) => {
            tracing::debug!(%err, ?resolution, "failed to enumerate frame intervals for this resolution");
            return Vec::new();
        }
    };
    let mut rates: Vec<u32> = intervals
        .into_iter()
        .filter_map(|interval| match interval.interval {
            FrameIntervalEnum::Discrete(fraction) if fraction.numerator != 0 => Some(fraction.denominator / fraction.numerator),
            _ => None,
        })
        .collect();
    rates.sort_unstable_by_key(|&fps| std::cmp::Reverse(fps));
    rates.dedup();
    rates
}

/// Runs a blocking YUYV capture loop against `path` at the requested
/// resolution, until `should_stop` returns true. Meant to run inside
/// `tokio::task::spawn_blocking`.
///
/// V4L2 drivers are free to negotiate a different resolution than what's
/// requested (`set_format` returns the format actually in effect) — so
/// `make_handler` is called once, *after* negotiation, with the *actual*
/// resolution, and must build the per-frame handler from that. Sizing a
/// frame handler from the merely-requested resolution instead is a real
/// bug: a mismatch between it and the driver's actual frame size means
/// buffer-indexing code (I420 conversion) reads past the end of a frame it
/// assumed was a different size.
pub fn run_capture_loop<H>(path: &str, resolution: Resolution, fps: u32, mut should_stop: impl FnMut() -> bool, make_handler: impl FnOnce(Resolution) -> Result<H>) -> Result<()>
where
    H: FnMut(&[u8], Timestamp),
{
    let dev = Device::with_path(path).with_context(|| format!("opening {path}"))?;
    let requested = Format::new(resolution.width, resolution.height, YUYV_FOURCC);
    let actual = dev.set_format(&requested).context("negotiating capture format")?;
    let actual_resolution = Resolution { width: actual.width, height: actual.height };

    match dev.set_params(&Parameters::with_fps(fps)) {
        Ok(actual_params) => {
            if let Some(actual_fps) = fps_mismatch(fps, actual_params.interval) {
                tracing::warn!(requested_fps = fps, actual_fps, "capture device negotiated a different frame rate than requested");
            }
        }
        Err(err) => tracing::warn!(%err, requested_fps = fps, "failed to set capture frame rate, continuing at device default"),
    }

    // If GPU setup fails here, we return before ever opening the mmap
    // capture stream below - no frames are read for a pass that can't
    // encode them anyway.
    let mut on_frame = make_handler(actual_resolution)?;

    let mut stream = MmapStream::with_buffers(&dev, BufferType::VideoCapture, 4).context("starting mmap capture stream")?;
    stream.set_timeout(Duration::from_millis(500));

    while !should_stop() {
        match stream.next() {
            Ok((buf, meta)) => {
                // `buf` is the full fixed-size mmap buffer slot, not the
                // real frame - the driver reports the actual filled length
                // separately (`bytesused`).
                let len = (meta.bytesused as usize).min(buf.len());
                on_frame(&buf[..len], meta.timestamp);
            }
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(err) => return Err(err).context("reading capture frame"),
        }
    }
    Ok(())
}

/// Converts a driver capture timestamp to a `Duration` since some
/// arbitrary but consistent monotonic origin — only meaningful as a delta
/// between two frames, never as an absolute value (see
/// `video_bus::FrameEnvelope::captured_at`).
pub fn timestamp_to_duration(ts: Timestamp) -> Duration {
    Duration::new(ts.sec.max(0) as u64, (ts.usec.max(0) as u32).saturating_mul(1000))
}

/// `None` if `interval` matches `requested` fps (or is degenerate); `Some`
/// with the actual negotiated fps otherwise, for a warn-level log — unlike
/// resolution, nothing downstream sizes a buffer from fps, so a log line is
/// the only way a driver-side mismatch is ever observable.
fn fps_mismatch(requested: u32, interval: v4l::fraction::Fraction) -> Option<u32> {
    if interval.numerator == 0 {
        return None;
    }
    let actual_fps = interval.denominator / interval.numerator;
    (actual_fps != requested).then_some(actual_fps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fps_mismatch_reports_when_driver_negotiated_differently() {
        assert_eq!(fps_mismatch(10, v4l::fraction::Fraction::new(1, 5)), Some(5));
    }

    #[test]
    fn fps_mismatch_is_none_when_matched() {
        assert_eq!(fps_mismatch(5, v4l::fraction::Fraction::new(1, 5)), None);
    }

    #[test]
    fn fps_mismatch_is_none_for_degenerate_interval() {
        assert_eq!(fps_mismatch(5, v4l::fraction::Fraction::new(0, 0)), None);
    }
}
