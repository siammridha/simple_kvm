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

    pub fn formats(&self) -> &[SupportedFormat] {
        &self.formats
    }

    pub fn default_settings(&self) -> Option<CaptureSettings> {
        let (_pixel_format, resolution) = v4l2::pick_default(&self.formats)?;
        Some(CaptureSettings { video_mode: VideoMode::Mjpeg, resolution })
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
    let pixel_format = pixel_format_for(formats, settings.video_mode);
    let mut h264_encoder = if settings.video_mode == VideoMode::H264 {
        match h264::H264Encoder::new(settings.resolution.width, settings.resolution.height) {
            Ok(encoder) => Some(encoder),
            Err(err) => {
                tracing::error!(%err, "failed to create H.264 encoder, dropping frames in this pass");
                None
            }
        }
    } else {
        None
    };

    let result = v4l2::run_capture_loop(
        device_path,
        pixel_format,
        settings.resolution,
        || stop.load(Ordering::Relaxed),
        |frame| {
            let envelope = match settings.video_mode {
                VideoMode::Mjpeg => {
                    let jpeg = match pixel_format {
                        PixelFormat::Mjpeg => frame.to_vec(),
                        PixelFormat::Yuyv => {
                            match mjpeg::yuyv_to_jpeg(frame, settings.resolution.width, settings.resolution.height) {
                                Ok(bytes) => bytes,
                                Err(err) => {
                                    tracing::error!(%err, "JPEG fallback encode failed");
                                    return;
                                }
                            }
                        }
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
        },
    );

    if let Err(err) = result {
        tracing::error!(%err, "capture loop exited with error");
    }
}

/// H.264 mode always captures raw YUYV to encode from; MJPEG mode prefers
/// the card's own hardware MJPEG, falling back to a software JPEG
/// conversion of raw YUYV if the card doesn't offer it.
fn pixel_format_for(formats: &[SupportedFormat], video_mode: VideoMode) -> PixelFormat {
    match video_mode {
        VideoMode::Mjpeg if formats.iter().any(|f| f.pixel_format == PixelFormat::Mjpeg) => PixelFormat::Mjpeg,
        _ => PixelFormat::Yuyv,
    }
}
