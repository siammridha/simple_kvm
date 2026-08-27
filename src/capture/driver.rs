//! `DeviceDriver` implementation for the capture card, plugging the
//! existing v4l2 probing logic into the generic `Device<D>` presence core
//! from `crate::device` (issue #003). See issue #004 and
//! `docs/capture-redesign-ideas.md`'s "Decided: one generic device
//! module, not one-off per device kind".

use crate::capture::v4l2::{self, SupportedFormat};
use crate::config::CaptureSettings;
use crate::device::{Device, DeviceDriver, OpenError};

pub struct CaptureDriver;

/// What `CaptureDriver::open` hands back - just the validated device path,
/// wrapped so nothing outside `capture` can read it back out (mirrors
/// `docs/capture-redesign-ideas.md`'s idea 5: only the presence module -
/// here, `Device<CaptureDriver>` plus this driver - ever holds the raw
/// device path). The actual v4l2 open/ioctls happen later, inside
/// `run_one_pass`/`v4l2::run_capture_loop` when the encode pass actually
/// starts, exactly as `CaptureManager` does it today - `open` itself stays
/// a cheap, infallible bundling step rather than a second, redundant
/// device open.
pub struct RawCapture {
    device_path: String,
}

impl RawCapture {
    pub(super) fn device_path(&self) -> &str {
        &self.device_path
    }
}

impl DeviceDriver for CaptureDriver {
    type Info = SupportedFormat;
    type Settings = CaptureSettings;
    type Open = RawCapture;

    /// Same "never fails, returns `None` and logs a warning" contract as
    /// today's `capture::probe` - unchanged behavior, just relocated.
    fn probe(device_path: &str) -> Option<Self::Info> {
        v4l2::enumerate(device_path).unwrap_or_else(|err| {
            tracing::warn!(%err, device_path, "capture device probe failed, video will be unavailable this cycle");
            None
        })
    }

    fn open(device_path: &str, _settings: &Self::Settings) -> Result<Self::Open, OpenError> {
        Ok(RawCapture { device_path: device_path.to_string() })
    }
}

pub type CaptureDevice = Device<CaptureDriver>;
