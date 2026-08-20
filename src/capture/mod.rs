pub mod h264;
pub mod mjpeg;
pub mod v4l2;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::watch;

use crate::config::{CaptureSettings, VideoMode};
use crate::video_bus::{self, FrameEnvelope, FrameKind};
use v4l2::{PixelFormat, SupportedFormat};

pub struct CaptureManager {
    device_path: String,
    formats: Vec<SupportedFormat>,
}

impl CaptureManager {
    /// Probes `device_path` for its supported formats. Never fails — if
    /// there's no capture device (e.g. in the devcontainer), `formats` is
    /// just empty and `run` becomes a no-op, per the soft-unavailable
    /// design.
    pub fn probe(device_path: &str) -> Self {
        let formats = v4l2::enumerate(device_path).unwrap_or_else(|err| {
            tracing::warn!(%err, device_path, "no capture device found, video will be unavailable");
            Vec::new()
        });
        Self { device_path: device_path.to_string(), formats }
    }

    pub fn is_available(&self) -> bool {
        !self.formats.is_empty()
    }

    pub fn default_settings(&self) -> Option<CaptureSettings> {
        let (_pixel_format, resolution) = v4l2::pick_default(&self.formats)?;
        Some(CaptureSettings { video_mode: VideoMode::Mjpeg, resolution })
    }

    /// The resolutions available in whichever pixel format `video_mode`
    /// would actually use (see `pixel_format_for`). Used to build the
    /// resolution dropdown so it matches whatever video mode the page
    /// starts in, including a persisted mode from `settings_store`.
    pub fn resolutions_for(&self, video_mode: VideoMode) -> Vec<v4l2::Resolution> {
        let Some(pixel_format) = pixel_format_for(&self.formats, video_mode) else {
            return Vec::new();
        };
        self.formats
            .iter()
            .find(|f| f.pixel_format == pixel_format)
            .map(|f| f.resolutions.clone())
            .unwrap_or_default()
    }

    /// Whether the card can actually run in `video_mode` at `resolution`.
    /// Used to validate a persisted setting before trusting it as the
    /// startup default — the card on hand may have changed since the
    /// setting was saved.
    pub fn supports(&self, video_mode: VideoMode, resolution: v4l2::Resolution) -> bool {
        let Some(pixel_format) = pixel_format_for(&self.formats, video_mode) else {
            return false;
        };
        self.formats.iter().any(|f| f.pixel_format == pixel_format && f.resolutions.contains(&resolution))
    }

    /// Runs forever, restarting the capture loop whenever `settings`
    /// changes, publishing frames onto `video_bus`.
    pub async fn run(self, mut settings: watch::Receiver<CaptureSettings>, video_bus: video_bus::Sender) {
        if !self.is_available() {
            tracing::info!("no capture device available, video capture task exiting");
            return;
        }

        loop {
            let current = *settings.borrow();
            let device_path = self.device_path.clone();
            let formats = self.formats.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_task = Arc::clone(&stop);
            let video_bus_task = video_bus.clone();

            let handle = tokio::task::spawn_blocking(move || {
                run_one_pass(&device_path, &formats, &current, stop_task, video_bus_task)
            });

            if settings.changed().await.is_err() {
                break;
            }
            stop.store(true, Ordering::Relaxed);
            let _ = handle.await;
        }
    }
}

fn run_one_pass(
    device_path: &str,
    formats: &[SupportedFormat],
    settings: &CaptureSettings,
    stop: Arc<AtomicBool>,
    video_bus: video_bus::Sender,
) {
    let Some(pixel_format) = pixel_format_for(formats, settings.video_mode) else {
        tracing::error!(?settings.video_mode, "capture device doesn't support the pixel format this video mode needs, no video this pass");
        return;
    };
    let video_mode = settings.video_mode;

    // `make_handler` runs once `run_capture_loop` knows the *actual*
    // negotiated resolution (which the driver is free to pick differently
    // than what was requested) — sizing the JPEG/H.264 conversion buffers
    // from anything else risks reading past the end of a real frame.
    let result = v4l2::run_capture_loop(device_path, pixel_format, settings.resolution, || stop.load(Ordering::Relaxed), move |actual_resolution| {
        let mut h264_encoder = if video_mode == VideoMode::H264 {
            match h264::H264Encoder::new(actual_resolution.width, actual_resolution.height) {
                Ok(encoder) => Some(encoder),
                Err(err) => {
                    tracing::error!(%err, "failed to create H.264 encoder, dropping frames in this pass");
                    None
                }
            }
        } else {
            None
        };

        move |frame: &[u8]| {
            let envelope = match video_mode {
                VideoMode::Mjpeg => {
                    let jpeg = match pixel_format {
                        PixelFormat::Mjpeg => frame.to_vec(),
                        PixelFormat::Yuyv => match mjpeg::yuyv_to_jpeg(frame, actual_resolution.width, actual_resolution.height) {
                            Ok(bytes) => bytes,
                            Err(err) => {
                                tracing::error!(%err, "JPEG fallback encode failed");
                                return;
                            }
                        },
                    };
                    FrameEnvelope { kind: FrameKind::Mjpeg, data: jpeg.into() }
                }
                VideoMode::H264 => {
                    let Some(encoder) = h264_encoder.as_mut() else {
                        return;
                    };
                    match encoder.encode_yuyv_frame(frame) {
                        Ok(bytes) => FrameEnvelope { kind: FrameKind::H264, data: bytes.into() },
                        Err(err) => {
                            tracing::error!(%err, "H.264 encode failed");
                            return;
                        }
                    }
                }
            };
            let _ = video_bus.send(Some(envelope));
        }
    });

    if let Err(err) = result {
        tracing::error!(%err, "capture loop exited with error");
    }
}

/// H.264 mode always needs raw YUYV to encode from; MJPEG mode prefers the
/// card's own hardware MJPEG, falling back to raw YUYV (converted to JPEG
/// in software) if the card doesn't offer it. Returns `None` if the
/// device supports neither format this mode needs.
fn pixel_format_for(formats: &[SupportedFormat], video_mode: VideoMode) -> Option<PixelFormat> {
    let has = |pixel_format: PixelFormat| formats.iter().any(|f| f.pixel_format == pixel_format);
    match video_mode {
        VideoMode::Mjpeg if has(PixelFormat::Mjpeg) => Some(PixelFormat::Mjpeg),
        VideoMode::Mjpeg if has(PixelFormat::Yuyv) => Some(PixelFormat::Yuyv),
        VideoMode::H264 if has(PixelFormat::Yuyv) => Some(PixelFormat::Yuyv),
        _ => None,
    }
}
