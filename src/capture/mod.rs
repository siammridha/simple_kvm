pub mod h264;
pub mod mjpeg;
pub mod v4l2;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::config::{CaptureSettings, DeviceState, VideoMode};
use crate::uevent;
use crate::video_bus::{self, FrameEnvelope, FrameKind};
use v4l2::{PixelFormat, SupportedFormat};

/// Safety-net interval for checking whether the capture card is plugged in
/// while it's absent. `UeventListener` (see `capture::uevent`) normally
/// notices a reconnect immediately, straight from the kernel - this timer
/// only matters if that listener failed to open (e.g. no permission) or a
/// notification was somehow missed, so it doesn't need to be tight.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Preferred fps for `CaptureManager::default_settings()` — picked from
/// whatever the card actually supports at its default resolution rather
/// than assumed, since fps support varies by both pixel format and
/// resolution on real hardware (confirmed via `v4l2-ctl
/// --list-formats-ext` on the real device).
const DEFAULT_TARGET_FPS: u32 = 10;

pub struct CaptureManager {
    device_path: String,
    formats: Vec<SupportedFormat>,
}

impl CaptureManager {
    /// Probes `device_path` for its supported formats, once, for the
    /// startup snapshot (`device_state`/`default_settings`/etc. below —
    /// used to build the page's initial state). Never fails — if there's
    /// no capture device (e.g. in the devcontainer), `formats` is just
    /// empty. `run` below does its own live presence checking and doesn't
    /// depend on this snapshot staying accurate.
    pub fn probe(device_path: &str) -> Self {
        let formats = v4l2::enumerate(device_path).unwrap_or_else(|err| {
            tracing::warn!(%err, device_path, "no capture device found, video will be unavailable");
            Vec::new()
        });
        Self { device_path: device_path.to_string(), formats }
    }

    pub fn default_settings(&self) -> Option<CaptureSettings> {
        let (pixel_format, resolution) = v4l2::pick_default(&self.formats)?;
        let fps = self
            .formats
            .iter()
            .find(|f| f.pixel_format == pixel_format)
            .and_then(|f| f.frame_rates.get(&resolution))
            .and_then(|rates| rates.iter().copied().filter(|&r| r <= DEFAULT_TARGET_FPS).max().or_else(|| rates.iter().copied().min()))
            .unwrap_or(DEFAULT_TARGET_FPS);
        Some(CaptureSettings { video_mode: VideoMode::Mjpeg, resolution, fps })
    }

    /// The card's current availability and resolution list for `settings`'s
    /// video mode — the startup snapshot used to seed the `DeviceState`
    /// watch channel. `run`'s hot-plug loop recomputes the same thing live
    /// via the free function `device_state_for` below.
    pub fn device_state(&self, settings: &CaptureSettings) -> DeviceState {
        device_state_for(&self.formats, settings)
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

    /// Runs forever: whenever the capture card is present, restarts the
    /// capture loop whenever `settings` changes and publishes frames onto
    /// `video_bus`; whenever it's absent (never plugged in, or unplugged
    /// mid-session), polls until it reappears instead of exiting for good.
    pub async fn run(
        self,
        mut settings: watch::Receiver<CaptureSettings>,
        video_bus: video_bus::Sender,
        device_state_tx: watch::Sender<DeviceState>,
        force_keyframe: Arc<AtomicBool>,
    ) {
        let device_path = self.device_path;
        let mut known_present = false;
        let mut uevents = match uevent::UeventListener::open() {
            Ok(listener) => Some(listener),
            Err(err) => {
                tracing::warn!(%err, "failed to open kernel uevent listener, falling back to polling only for capture device reconnects");
                None
            }
        };

        loop {
            if !Path::new(&device_path).exists() {
                if known_present {
                    tracing::warn!(device_path = %device_path, "capture device disconnected, pausing video until it reconnects");
                    known_present = false;
                    let _ = device_state_tx.send(DeviceState::default());
                }
                if wait_for_device_or_shutdown(&device_path, &mut settings, &mut uevents).await.is_err() {
                    break;
                }
                continue;
            }

            let formats = match v4l2::enumerate(&device_path) {
                Ok(formats) => formats,
                Err(err) => {
                    tracing::error!(%err, device_path = %device_path, "failed to enumerate capture device, will retry");
                    tokio::time::sleep(DEVICE_POLL_INTERVAL).await;
                    continue;
                }
            };
            if !known_present {
                tracing::info!(device_path = %device_path, "capture device connected");
                known_present = true;
            }

            let current = *settings.borrow();
            let new_state = device_state_for(&formats, &current);
            device_state_tx.send_if_modified(|s| {
                if *s == new_state {
                    false
                } else {
                    *s = new_state;
                    true
                }
            });

            let device_path_task = device_path.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_task = Arc::clone(&stop);
            let video_bus_task = video_bus.clone();
            let force_keyframe_task = force_keyframe.clone();

            let mut handle = tokio::task::spawn_blocking(move || {
                run_one_pass(&device_path_task, &formats, &current, stop_task, video_bus_task, force_keyframe_task)
            });

            tokio::select! {
                changed = settings.changed() => {
                    stop.store(true, Ordering::Relaxed);
                    let _ = handle.await;
                    if changed.is_err() {
                        break;
                    }
                }
                // The pass ended on its own - most likely the card was
                // unplugged mid-capture and the next loop iteration's
                // presence check will notice. Could also be some other
                // capture error; either way, looping back (rather than
                // exiting the task) is what makes it retryable. The sleep
                // keeps a persistent non-unplug error (e.g. a permissions
                // problem) from busy-looping instead of just polling.
                _ = &mut handle => {
                    tokio::time::sleep(DEVICE_POLL_INTERVAL).await;
                }
            }
        }
    }
}

/// Waits until `device_path` exists, or returns `Err` once the settings
/// channel closes (server shutting down, nothing left to wait for).
/// Wakes up immediately on a matching kernel uevent when `uevents` is
/// available; the timer is just the fallback for when it isn't (see
/// `DEVICE_POLL_INTERVAL`).
async fn wait_for_device_or_shutdown(
    device_path: &str,
    settings: &mut watch::Receiver<CaptureSettings>,
    uevents: &mut Option<uevent::UeventListener>,
) -> Result<(), ()> {
    loop {
        if Path::new(device_path).exists() {
            return Ok(());
        }
        tokio::select! {
            _ = wait_for_uevent(uevents) => {}
            _ = tokio::time::sleep(DEVICE_POLL_INTERVAL) => {}
            changed = settings.changed() => {
                if changed.is_err() {
                    return Err(());
                }
            }
        }
    }
}

/// `video4linux` is the kernel subsystem name for V4L2 capture devices
/// like `/dev/video0` - unrelated uevents (USB, network, ...) are ignored.
/// Never resolves when `uevents` is `None` (listener failed to open), so
/// this branch simply never wins the `select!` above and the timer takes
/// over instead.
async fn wait_for_uevent(uevents: &mut Option<uevent::UeventListener>) {
    match uevents {
        Some(listener) => listener.wait_for_subsystem("video4linux").await,
        None => std::future::pending().await,
    }
}

fn run_one_pass(
    device_path: &str,
    formats: &[SupportedFormat],
    settings: &CaptureSettings,
    stop: Arc<AtomicBool>,
    video_bus: video_bus::Sender,
    force_keyframe: Arc<AtomicBool>,
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
    let result = v4l2::run_capture_loop(device_path, pixel_format, settings.resolution, settings.fps, || stop.load(Ordering::Relaxed), move |actual_resolution| {
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

        move |frame: &[u8], captured_at: v4l2::Timestamp| {
            let captured_at = v4l2::timestamp_to_duration(captured_at);
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
                    FrameEnvelope { kind: FrameKind::Mjpeg, data: jpeg.into(), captured_at }
                }
                VideoMode::H264 => {
                    let Some(encoder) = h264_encoder.as_mut() else {
                        return;
                    };
                    // A session asked (via RTCP PLI/FIR) for a fresh
                    // keyframe sooner than the encoder's own periodic
                    // schedule — see `rtc::session::handle`'s
                    // `video_track.poll()` branch.
                    if force_keyframe.swap(false, Ordering::Relaxed) {
                        encoder.force_intra_frame();
                    }
                    match encoder.encode_yuyv_frame(frame) {
                        Ok(bytes) => FrameEnvelope { kind: FrameKind::H264, data: bytes.into(), captured_at },
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

/// Shared by `CaptureManager::device_state` (the startup snapshot) and
/// `run`'s hot-plug loop (recomputed fresh on every reconnect) — kept as a
/// free function over plain `&[SupportedFormat]` so both call sites can use
/// it without needing a full `CaptureManager` instance.
fn device_state_for(formats: &[SupportedFormat], settings: &CaptureSettings) -> DeviceState {
    if formats.is_empty() {
        return DeviceState::default();
    }
    let format = pixel_format_for(formats, settings.video_mode).and_then(|pf| formats.iter().find(|f| f.pixel_format == pf));
    let resolutions = format.map(|f| f.resolutions.clone()).unwrap_or_default();
    // Falls back to just the currently-applied fps if the card didn't
    // report a discrete list for this resolution — the dropdown should
    // never be empty, and the applied value is always a valid option.
    let frame_rates = format.and_then(|f| f.frame_rates.get(&settings.resolution)).cloned().unwrap_or_else(|| vec![settings.fps]);
    DeviceState { available: true, resolutions, frame_rates, default_resolution: Some(settings.resolution) }
}
