//! `DeviceDriver` implementation for the capture card: the only code that
//! opens or enumerates a V4L2 node. Lives here rather than in `capture`
//! because probing and opening are the two calls that touch a device path,
//! and paths never leave this module (see `ARCHITECTURE.md` I3).
//!
//! Only the raw YUYV (4:2:2) format is ever requested — that's what the
//! H.264 encoder needs to encode from (see `capture::h264`).

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use v4l::format::FourCC;
use v4l::frameinterval::FrameIntervalEnum;
use v4l::video::Capture;
use v4l::video::capture::Parameters;
use v4l::{Device as V4l2Device, Format};

use super::{Device, DeviceDriver, OpenError};

const YUYV_FOURCC: FourCC = FourCC { repr: *b"YUYV" };

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

/// What settings a caller asks the card to be opened with. The driver is
/// free to negotiate something else - see `CaptureHandle::resolution`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureSettings {
    pub resolution: Resolution,
    pub fps: u32,
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

pub struct CaptureDriver;

pub type CaptureDevice = Device<CaptureDriver>;

/// An already-open, already-negotiated capture device. Hands out the OS
/// handle to read frames from and the resolution the driver actually
/// negotiated, and nothing else - in particular never the path it was
/// opened from.
///
/// The negotiated resolution is carried alongside the handle rather than
/// left for the caller to assume: V4L2 drivers are free to give back a
/// different format than the one requested, and a frame handler whose
/// buffers are sized from the merely-requested resolution reads past the
/// end of a real frame.
pub struct CaptureHandle {
    device: V4l2Device,
    resolution: Resolution,
}

impl CaptureHandle {
    /// The resolution actually in effect on this device, which may differ
    /// from the one `open` was asked for.
    pub fn resolution(&self) -> Resolution {
        self.resolution
    }

    /// The open V4L2 handle itself, for the caller's capture stream. The
    /// `v4l` crate's `Device` is a raw file descriptor wrapper - it holds
    /// no path and offers no way to recover one.
    pub fn v4l2_device(&self) -> &V4l2Device {
        &self.device
    }
}

impl DeviceDriver for CaptureDriver {
    type Info = SupportedFormat;
    type Settings = CaptureSettings;
    type Open = CaptureHandle;

    /// Never fails the caller: any probe failure is a warning plus `None`.
    fn probe(device_path: &str) -> Option<Self::Info> {
        enumerate(device_path).unwrap_or_else(|err| {
            tracing::warn!(%err, device_path, "capture device probe failed, video will be unavailable this cycle");
            None
        })
    }

    /// Opens the card and negotiates YUYV at `settings`. A frame rate the
    /// driver won't accept is only logged - nothing downstream sizes a
    /// buffer from fps - but a failed format negotiation fails the open,
    /// since there'd be no trustworthy resolution to hand back.
    fn open(device_path: &str, settings: &Self::Settings) -> Result<Self::Open, OpenError> {
        let device = V4l2Device::with_path(device_path).map_err(|err| OpenError(format!("opening capture device: {err}")))?;

        let requested = Format::new(settings.resolution.width, settings.resolution.height, YUYV_FOURCC);
        let actual = device.set_format(&requested).map_err(|err| OpenError(format!("negotiating capture format: {err}")))?;
        let resolution = Resolution { width: actual.width, height: actual.height };

        match device.set_params(&Parameters::with_fps(settings.fps)) {
            Ok(actual_params) => {
                if let Some(actual_fps) = fps_mismatch(settings.fps, actual_params.interval) {
                    tracing::warn!(requested_fps = settings.fps, actual_fps, "capture device negotiated a different frame rate than requested");
                }
            }
            Err(err) => tracing::warn!(%err, requested_fps = settings.fps, "failed to set capture frame rate, continuing at device default"),
        }

        Ok(CaptureHandle { device, resolution })
    }
}

/// Queries the card at `path` for the YUYV resolutions/frame rates it
/// supports. Returns `None` if the card doesn't offer YUYV at all.
fn enumerate(path: &str) -> Result<Option<SupportedFormat>> {
    let dev = V4l2Device::with_path(path).with_context(|| format!("opening {path}"))?;
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
fn frame_rates_for(dev: &V4l2Device, fourcc: FourCC, resolution: Resolution) -> Vec<u32> {
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

    /// A plain regular file opens fine as a file descriptor, but it isn't
    /// a V4L2 node, so format negotiation must fail the open rather than
    /// hand back a handle carrying a made-up resolution.
    #[test]
    fn open_fails_when_the_path_is_not_a_v4l2_device() {
        let path = std::env::temp_dir().join(format!("simple-kvm-capture-driver-test-{}", std::process::id()));
        std::fs::write(&path, b"not a real capture device").unwrap();

        let result = CaptureDriver::open(path.to_str().unwrap(), &CaptureSettings { resolution: Resolution { width: 1920, height: 1080 }, fps: 10 });

        let _ = std::fs::remove_file(&path);
        assert!(result.is_err());
    }
}
