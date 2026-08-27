pub mod driver;
pub mod engine;
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

/// Backoff delay after a capture pass ends unexpectedly (most likely the
/// card was unplugged mid-capture), so a persistent non-unplug error (e.g.
/// a permissions problem) doesn't turn into a busy loop. Device reconnects
/// are detected purely via kernel uevents (see `capture::uevent`), not by
/// polling.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(2);

pub struct CaptureManager {
    device_path: String,
}

impl CaptureManager {
    /// Just records the path - no ioctls, no opening the device. The card
    /// is never probed or opened except at the two moments `run` below
    /// gates on: the device becoming newly present (probe, once, to learn
    /// capabilities for the settings UI - see `run`'s doc comment for
    /// exactly when this does and doesn't fire) and a session's WebRTC
    /// connection reaching `connected` (actually start streaming, via
    /// `client_count`). Opening the device automatically - at startup or
    /// otherwise unprompted - has reliably crashed the real hardware this
    /// targets (see README's "boot-crash" known issue), so nothing here
    /// ever does that on its own.
    pub fn new(device_path: &str) -> Self {
        Self { device_path: device_path.to_string() }
    }

    /// Runs forever. The capture device is only ever probed for its
    /// capabilities when it transitions from absent to present - a genuine
    /// replug, or simply being plugged in for the first time after having
    /// started absent - and the result is cached in `format` in memory for
    /// as long as it stays present, so a browser connecting later just
    /// reads the cached `DeviceState` instead of triggering a fresh probe.
    /// The one exception is the very first time this loop ever checks and
    /// finds the device already present (e.g. it was plugged in before the
    /// service started) - that's the boot-crash-risk moment (right after
    /// USB enumeration finishes at startup), so that specific transition is
    /// never auto-probed.
    ///
    /// Actual streaming (`run_one_pass`) only starts once `client_count` is
    /// nonzero - which only happens once a session's WebRTC connection is
    /// fully stable (`connected`), not merely negotiated - and stops as
    /// soon as it drops back to zero.
    pub async fn run(
        self,
        mut settings: watch::Receiver<CaptureSettings>,
        video_bus: video_bus::Sender,
        device_state_tx: watch::Sender<DeviceState>,
        force_keyframe: Arc<AtomicBool>,
        mut client_count: watch::Receiver<u32>,
    ) {
        let device_path = self.device_path;
        let mut format: Option<SupportedFormat> = None;
        let mut known_present = false;
        let mut first_check = true;
        let mut uevents = match uevent::UeventListener::open() {
            Ok(listener) => Some(listener),
            Err(err) => {
                tracing::warn!(%err, "failed to open kernel uevent listener, capture device reconnects won't be detected until the next settings change");
                None
            }
        };

        loop {
            let device_present = Path::new(&device_path).exists();
            let skip_probe_this_transition = first_check && device_present;
            first_check = false;

            if !device_present {
                if known_present {
                    tracing::warn!(device_path = %device_path, "capture device disconnected, pausing video until it reconnects");
                    known_present = false;
                    format = None;
                    let _ = device_state_tx.send(DeviceState::default());
                }
                if wait_for_device_or_shutdown(&device_path, &mut settings, &mut uevents).await.is_err() {
                    break;
                }
                continue;
            }

            if !known_present {
                known_present = true;
                tracing::info!(device_path = %device_path, "capture device connected");
                if !skip_probe_this_transition {
                    format = probe(&device_path);
                }
            }
            publish_device_state(&device_state_tx, &format, &settings.borrow());

            if *client_count.borrow() == 0 {
                if wait_for_client_or_shutdown(&mut client_count, &mut settings, &format, &device_state_tx, &mut uevents).await.is_err() {
                    break;
                }
                continue;
            }

            let device_path_task = device_path.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_task = Arc::clone(&stop);
            let video_bus_task = video_bus.clone();
            let force_keyframe_task = force_keyframe.clone();
            let format_task = format.clone();
            let current = *settings.borrow();

            tracing::info!("video encoding started");
            let mut handle = tokio::task::spawn_blocking(move || run_one_pass(&device_path_task, &format_task, &current, stop_task, video_bus_task, force_keyframe_task));

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

/// Probes `device_path` for its supported YUYV resolutions/frame rates.
/// Never fails - if there's no capture device (e.g. in the devcontainer) or
/// it doesn't support YUYV, or the ioctl itself errors, this just returns
/// `None` and logs a warning; the caller treats that the same as "not
/// probed yet".
fn probe(device_path: &str) -> Option<SupportedFormat> {
    v4l2::enumerate(device_path).unwrap_or_else(|err| {
        tracing::warn!(%err, device_path, "capture device probe failed, video will be unavailable this cycle");
        None
    })
}

/// Recomputes `device_state_for` from whatever `format` is currently
/// cached and publishes it if it actually changed. Cheap and ioctl-free -
/// safe to call on every settings change or loop iteration, unlike an
/// actual probe.
fn publish_device_state(device_state_tx: &watch::Sender<DeviceState>, format: &Option<SupportedFormat>, settings: &CaptureSettings) {
    let new_state = device_state_for(format, settings);
    device_state_tx.send_if_modified(|s| {
        if *s == new_state {
            false
        } else {
            *s = new_state;
            true
        }
    });
}

/// Waits until `client_count` becomes nonzero, or returns `Err` once the
/// settings channel closes (server shutting down, nothing left to wait
/// for). While waiting, also handles a settings change (recomputes and
/// republishes `device_state` for the new default resolution, without
/// probing).
///
/// Also returns `Ok` early on a video4linux uevent (plug or unplug), even
/// though `client_count` is still zero - without this, a device
/// disconnect/reconnect that happens while no browser is connected would
/// never be noticed at all, since nothing else here watches for it. The
/// caller's loop re-checks device presence (and probes on a fresh connect)
/// at its top, and calls back in here if `client_count` is still zero, so
/// this just hands control back rather than handling the transition
/// itself.
async fn wait_for_client_or_shutdown(
    client_count: &mut watch::Receiver<u32>,
    settings: &mut watch::Receiver<CaptureSettings>,
    format: &Option<SupportedFormat>,
    device_state_tx: &watch::Sender<DeviceState>,
    uevents: &mut Option<uevent::UeventListener>,
) -> Result<(), ()> {
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
                publish_device_state(device_state_tx, format, &settings.borrow());
            }
            () = wait_for_uevent(uevents) => {
                return Ok(());
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

/// `pub(crate)` (rather than private) so `capture::engine`'s `CaptureEngine`
/// can reuse this exact function for its own encode pass - see issue #004,
/// which is required to reuse this machinery rather than reinvent it.
pub(crate) fn run_one_pass(device_path: &str, format: &Option<SupportedFormat>, settings: &CaptureSettings, stop: Arc<AtomicBool>, video_bus: video_bus::Sender, force_keyframe: Arc<AtomicBool>) {
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

/// Shared by `publish_device_state` (used throughout `run`) - kept as a
/// free function over `&Option<SupportedFormat>` so it doesn't need a
/// `CaptureManager` instance.
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
