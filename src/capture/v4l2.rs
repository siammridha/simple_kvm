//! V4L2 enumeration and capture, using the `v4l` crate.
//!
//! The device+stream pairing is kept as *local variables within one
//! function* (`run_capture_loop`), not stored together in a struct: the
//! `v4l` crate's `MmapStream<'a>` borrows the `Device` it was built from,
//! and the two are only ever used together inside a single blocking loop
//! anyway (per the concurrency design — a settings change stops the loop
//! and starts a fresh one), so there's no need to fight the lifetime.

use std::time::Duration;

use anyhow::{Context, Result};
use v4l::buffer::Type as BufferType;
use v4l::format::FourCC;
use v4l::io::mmap::Stream as MmapStream;
use v4l::io::traits::CaptureStream;
use v4l::video::Capture;
use v4l::{Device, Format};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    Mjpeg,
    Yuyv,
}

impl PixelFormat {
    fn fourcc(self) -> FourCC {
        match self {
            PixelFormat::Mjpeg => FourCC::new(b"MJPG"),
            PixelFormat::Yuyv => FourCC::new(b"YUYV"),
        }
    }

    fn from_fourcc(fourcc: FourCC) -> Option<Self> {
        match fourcc.str().ok() {
            Some("MJPG") => Some(PixelFormat::Mjpeg),
            Some("YUYV") => Some(PixelFormat::Yuyv),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SupportedFormat {
    pub pixel_format: PixelFormat,
    pub resolutions: Vec<Resolution>,
}

/// Queries the card at `path` for every capture format/resolution it
/// actually supports (formats we don't know how to use are skipped).
pub fn enumerate(path: &str) -> Result<Vec<SupportedFormat>> {
    let dev = Device::with_path(path).with_context(|| format!("opening {path}"))?;
    let mut out = Vec::new();
    for desc in dev.enum_formats().with_context(|| format!("enumerating formats on {path}"))? {
        let Some(pixel_format) = PixelFormat::from_fourcc(desc.fourcc) else {
            continue;
        };
        let frame_sizes = match dev.enum_framesizes(desc.fourcc) {
            Ok(sizes) => sizes,
            Err(err) => {
                // Some real UVC devices advertise a format via enum_formats
                // but fail enum_framesizes for it specifically. Skip just
                // that format rather than losing every format the device
                // offers.
                tracing::warn!(%err, ?pixel_format, path, "failed to enumerate frame sizes for this format, skipping it");
                continue;
            }
        };
        let mut resolutions: Vec<Resolution> = frame_sizes
            .into_iter()
            .flat_map(|frame_size| frame_size.size.to_discrete())
            .map(|d| Resolution { width: d.width, height: d.height })
            .collect();
        resolutions.sort_by_key(|r| std::cmp::Reverse(r.width * r.height));
        resolutions.dedup();
        out.push(SupportedFormat { pixel_format, resolutions });
    }
    Ok(out)
}

/// Picks a sensible default (format, resolution) from an enumeration:
/// prefer MJPEG (cheapest on this hardware) over raw YUYV, then the
/// largest resolution offered, capped at 1080p. Kept separate from the
/// ioctl calls above so it's unit-testable on plain data.
pub fn pick_default(formats: &[SupportedFormat]) -> Option<(PixelFormat, Resolution)> {
    const MAX_PIXELS: u32 = 1920 * 1080;

    let best_of = |pixel_format: PixelFormat| {
        formats
            .iter()
            .find(|f| f.pixel_format == pixel_format)
            .and_then(|f| f.resolutions.iter().filter(|r| r.width * r.height <= MAX_PIXELS).max_by_key(|r| r.width * r.height))
            .map(|r| (pixel_format, *r))
    };

    best_of(PixelFormat::Mjpeg).or_else(|| best_of(PixelFormat::Yuyv))
}

/// Runs a blocking capture loop against `path` at the given format and
/// requested resolution, until `should_stop` returns true. Meant to run
/// inside `tokio::task::spawn_blocking`.
///
/// V4L2 drivers are free to negotiate a different resolution than what's
/// requested (`set_format` returns the format actually in effect) — so
/// `make_handler` is called once, *after* negotiation, with the *actual*
/// resolution, and must build the per-frame handler from that. Sizing a
/// frame handler from the merely-requested resolution instead is a real
/// bug: a mismatch between it and the driver's actual frame size means
/// buffer-indexing code (JPEG/I420 conversion) reads past the end of a
/// frame it assumed was a different size.
pub fn run_capture_loop<H>(
    path: &str,
    pixel_format: PixelFormat,
    resolution: Resolution,
    mut should_stop: impl FnMut() -> bool,
    make_handler: impl FnOnce(Resolution) -> H,
) -> Result<()>
where
    H: FnMut(&[u8]),
{
    let dev = Device::with_path(path).with_context(|| format!("opening {path}"))?;
    let requested = Format::new(resolution.width, resolution.height, pixel_format.fourcc());
    let actual = dev.set_format(&requested).context("negotiating capture format")?;
    let actual_resolution = Resolution { width: actual.width, height: actual.height };
    let mut on_frame = make_handler(actual_resolution);

    let mut stream = MmapStream::with_buffers(&dev, BufferType::VideoCapture, 4).context("starting mmap capture stream")?;
    stream.set_timeout(Duration::from_millis(500));

    while !should_stop() {
        match stream.next() {
            Ok((buf, _meta)) => on_frame(buf),
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(err) => return Err(err).context("reading capture frame"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn formats(entries: &[(PixelFormat, &[(u32, u32)])]) -> Vec<SupportedFormat> {
        entries
            .iter()
            .map(|(pixel_format, resolutions)| SupportedFormat {
                pixel_format: *pixel_format,
                resolutions: resolutions.iter().map(|&(width, height)| Resolution { width, height }).collect(),
            })
            .collect()
    }

    #[test]
    fn prefers_mjpeg_over_yuyv_when_both_available() {
        let formats = formats(&[
            (PixelFormat::Yuyv, &[(1920, 1080)]),
            (PixelFormat::Mjpeg, &[(1280, 720)]),
        ]);
        assert_eq!(pick_default(&formats), Some((PixelFormat::Mjpeg, Resolution { width: 1280, height: 720 })));
    }

    #[test]
    fn picks_largest_resolution_within_1080p_cap() {
        let formats = formats(&[(PixelFormat::Mjpeg, &[(640, 480), (1920, 1080), (3840, 2160)])]);
        assert_eq!(pick_default(&formats), Some((PixelFormat::Mjpeg, Resolution { width: 1920, height: 1080 })));
    }

    #[test]
    fn falls_back_to_yuyv_when_no_mjpeg() {
        let formats = formats(&[(PixelFormat::Yuyv, &[(800, 600)])]);
        assert_eq!(pick_default(&formats), Some((PixelFormat::Yuyv, Resolution { width: 800, height: 600 })));
    }

    #[test]
    fn no_supported_formats_returns_none() {
        assert_eq!(pick_default(&[]), None);
    }
}
