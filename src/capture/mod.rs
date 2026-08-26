pub mod h264;
pub mod v4l2;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use tokio::sync::watch;

use crate::config::{CaptureSettings, DeviceState, ResolutionFrameRates};
use crate::uevent;
use crate::video_bus::{self, FrameEnvelope};
use v4l2::SupportedFormat;

/// Backoff delay used when retrying after a capture-device enumeration
/// error or an unexpected end to a capture pass, so a persistent error
/// doesn't turn into a busy loop. Device reconnects are detected purely via
/// kernel uevents (see `capture::uevent`), not by polling.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Preferred fps for `CaptureManager::default_settings()` — picked from
/// whatever the card actually supports at its default resolution rather
/// than assumed, since fps support varies by resolution on real hardware
/// (confirmed via `v4l2-ctl --list-formats-ext` on the real device).
const DEFAULT_TARGET_FPS: u32 = 10;

pub struct CaptureManager {
    device_path: String,
    format: Option<SupportedFormat>,
}

impl CaptureManager {
    /// Probes `device_path` for its supported YUYV resolutions/frame rates,
    /// once, for the startup snapshot (`device_state`/`default_settings`/etc.
    /// below — used to build the page's initial state). Never fails - if
    /// there's no capture device (e.g. in the devcontainer) or it doesn't
    /// support YUYV, `format` is just `None`. `run` below does its own live
    /// presence checking and doesn't depend on this snapshot staying
    /// accurate.
    pub fn probe(device_path: &str) -> Self {
        let format = v4l2::enumerate(device_path).unwrap_or_else(|err| {
            tracing::warn!(%err, device_path, "no capture device found, video will be unavailable");
            None
        });
        Self { device_path: device_path.to_string(), format }
    }

    pub fn default_settings(&self) -> Option<CaptureSettings> {
        let resolution = v4l2::pick_default(self.format.as_ref())?;
        let fps = self
            .format
            .as_ref()
            .and_then(|f| f.frame_rates.get(&resolution))
            .and_then(|rates| rates.iter().copied().filter(|&r| r <= DEFAULT_TARGET_FPS).max().or_else(|| rates.iter().copied().min()))
            .unwrap_or(DEFAULT_TARGET_FPS);
        Some(CaptureSettings { resolution, fps })
    }

    /// The card's current availability and resolution list for the startup
    /// snapshot used to seed the `DeviceState` watch channel. `run`'s
    /// hot-plug loop recomputes the same thing live via the free function
    /// `device_state_for` below.
    pub fn device_state(&self, settings: &CaptureSettings) -> DeviceState {
        device_state_for(&self.format, settings)
    }

    /// Whether the card can actually capture at `resolution`. Used to
    /// validate a persisted setting before trusting it as the startup
    /// default — the card on hand may have changed since the setting was
    /// saved.
    pub fn supports(&self, resolution: v4l2::Resolution) -> bool {
        self.format.as_ref().is_some_and(|f| f.resolutions.contains(&resolution))
    }

    /// Runs forever: whenever the capture card is present and at least one
    /// WebRTC client is connected, restarts the capture loop whenever
    /// `settings` changes and publishes frames onto `video_bus`; pauses
    /// (without restarting) while the client count is zero; whenever the
    /// device is absent (never plugged in, or unplugged mid-session), polls
    /// until it reappears instead of exiting for good.
    pub async fn run(
        self,
        mut settings: watch::Receiver<CaptureSettings>,
        video_bus: video_bus::Sender,
        device_state_tx: watch::Sender<DeviceState>,
        force_keyframe: Arc<AtomicBool>,
        mut client_count: watch::Receiver<u32>,
    ) {
        let device_path = self.device_path;
        let mut known_present = false;
        let mut uevents = match uevent::UeventListener::open() {
            Ok(listener) => Some(listener),
            Err(err) => {
                tracing::warn!(%err, "failed to open kernel uevent listener, capture device reconnects won't be detected until the next settings change");
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

            let format = match v4l2::enumerate(&device_path) {
                Ok(format) => format,
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
            let new_state = device_state_for(&format, &current);
            device_state_tx.send_if_modified(|s| {
                if *s == new_state {
                    false
                } else {
                    *s = new_state;
                    true
                }
            });

            if *client_count.borrow() == 0 {
                if wait_for_client_or_shutdown(&mut client_count, &mut settings).await.is_err() {
                    break;
                }
                continue;
            }

            let device_path_task = device_path.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_task = Arc::clone(&stop);
            let video_bus_task = video_bus.clone();
            let force_keyframe_task = force_keyframe.clone();

            tracing::info!("video encoding started");
            let mut handle = tokio::task::spawn_blocking(move || run_one_pass(&device_path_task, &format, &current, stop_task, video_bus_task, force_keyframe_task));

            let mut shutdown = false;
            let mut pass_ended_on_its_own = false;
            loop {
                tokio::select! {
                    changed = settings.changed() => {
                        stop.store(true, Ordering::Relaxed);
                        let _ = handle.await;
                        shutdown = changed.is_err();
                        break;
                    }
                    changed = client_count.changed() => {
                        if changed.is_err() {
                            stop.store(true, Ordering::Relaxed);
                            let _ = handle.await;
                            shutdown = true;
                            break;
                        }
                        if *client_count.borrow() == 0 {
                            stop.store(true, Ordering::Relaxed);
                            let _ = handle.await;
                            break;
                        }
                        // Count changed but is still nonzero (e.g. a second
                        // client joined, or one of several left) - the
                        // running pass isn't disrupted, keep waiting.
                    }
                    // The pass ended on its own - most likely the card was
                    // unplugged mid-capture and the next loop iteration's
                    // presence check will notice. Could also be some other
                    // capture error; either way, looping back (rather than
                    // exiting the task) is what makes it retryable. The sleep
                    // keeps a persistent non-unplug error (e.g. a permissions
                    // problem) from busy-looping instead of just polling.
                    _ = &mut handle => {
                        pass_ended_on_its_own = true;
                        break;
                    }
                }
            }
            // `H264Encoder`'s `Drop` impl logs its own GPU teardown steps
            // (color converter, then H.264 encoder) as it goes, which is
            // more useful than one generic line here.

            if shutdown {
                break;
            }
            if pass_ended_on_its_own {
                tokio::time::sleep(DEVICE_POLL_INTERVAL).await;
            }
        }
    }
}

/// Waits until `client_count` becomes nonzero, or returns `Err` once the
/// settings channel closes (server shutting down, nothing left to wait
/// for). Also returns `Ok` on a settings change even while the count is
/// still zero, so the caller re-evaluates from the top of the loop (e.g. to
/// keep `device_state_tx` current) before waiting again.
async fn wait_for_client_or_shutdown(client_count: &mut watch::Receiver<u32>, settings: &mut watch::Receiver<CaptureSettings>) -> Result<(), ()> {
    while *client_count.borrow() == 0 {
        tokio::select! {
            changed = client_count.changed() => {
                if changed.is_err() {
                    return Err(());
                }
            }
            changed = settings.changed() => {
                if changed.is_err() {
                    return Err(());
                }
                break;
            }
        }
    }
    Ok(())
}

/// Waits until `device_path` exists, or returns `Err` once the settings
/// channel closes (server shutting down, nothing left to wait for). Relies
/// entirely on kernel uevents to notice a reconnect - if `uevents` is
/// `None` (the listener failed to open), this only wakes on a settings
/// change.
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
/// this branch simply never wins the `select!` above - the settings-change
/// branch is the only other way to wake up in that case.
async fn wait_for_uevent(uevents: &mut Option<uevent::UeventListener>) {
    match uevents {
        Some(listener) => listener.wait_for_subsystem("video4linux").await,
        None => std::future::pending().await,
    }
}

fn run_one_pass(device_path: &str, format: &Option<SupportedFormat>, settings: &CaptureSettings, stop: Arc<AtomicBool>, video_bus: video_bus::Sender, force_keyframe: Arc<AtomicBool>) {
    if format.is_none() {
        tracing::error!("capture device doesn't support YUYV capture, no video this pass");
        return;
    }

    // `make_handler` runs once `run_capture_loop` knows the *actual*
    // negotiated resolution (which the driver is free to pick differently
    // than what was requested) — sizing the H.264 conversion buffers from
    // anything else risks reading past the end of a real frame. It's
    // fallible: if GPU setup fails, `run_capture_loop` returns before ever
    // opening the capture stream, instead of reading and dropping frames
    // for a pass that can't encode them anyway.
    let result = v4l2::run_capture_loop(device_path, settings.resolution, settings.fps, || stop.load(Ordering::Relaxed), move |actual_resolution| -> anyhow::Result<_> {
        let mut encoder = h264::H264Encoder::new(actual_resolution.width, actual_resolution.height).context("Failed to set up GPU")?;

        Ok(move |frame: &[u8], captured_at: v4l2::Timestamp| {
            let captured_at = v4l2::timestamp_to_duration(captured_at);
            // A session asked (via RTCP PLI/FIR) for a fresh keyframe
            // sooner than the encoder's own periodic schedule - see
            // `rtc::session::handle`'s `video_track.poll()` branch.
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

/// Shared by `CaptureManager::device_state` (the startup snapshot) and
/// `run`'s hot-plug loop (recomputed fresh on every reconnect) — kept as a
/// free function over `&Option<SupportedFormat>` so both call sites can use
/// it without needing a full `CaptureManager` instance.
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
