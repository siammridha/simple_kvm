//! Shared, live-mutable settings — changed by a dropdown on the page,
//! read by the capture task and the WebRTC session layer via
//! `tokio::sync::watch`, so a change never has to touch the connection
//! itself. Also the on-disk shape used by `settings_store` to persist the
//! current choices across a service restart.

use serde::{Deserialize, Serialize};

use crate::capture::v4l2::Resolution;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoMode {
    Mjpeg,
    H264,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureSettings {
    pub video_mode: VideoMode,
    pub resolution: Resolution,
    pub fps: u32,
}

/// Live state of the capture card itself — whether it's plugged in right
/// now, and what it supports in each video mode. Published by
/// `CaptureManager::run`'s hot-plug loop over a `watch` channel, and pushed
/// to the web page over the `control` data channel (see
/// `rtc::session::handle`) so an already-open tab reflects a hot-plug/unplug
/// instead of being frozen at server-startup values.
///
/// Carries both modes' data (not just whichever is currently applied) so
/// the page can repopulate its resolution/fps dropdowns the moment the
/// video-mode dropdown is changed, before Save is clicked — fps and
/// resolution support genuinely differ between MJPEG and H.264 on real
/// hardware (see `capture::device_state_for`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DeviceState {
    pub available: bool,
    pub mjpeg: VideoModeState,
    pub h264: VideoModeState,
}

/// What the card supports for one video mode: every resolution it offers,
/// which one to default to, and the discrete frame rates available at each
/// of those resolutions.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct VideoModeState {
    pub resolutions: Vec<Resolution>,
    pub default_resolution: Option<Resolution>,
    pub frame_rates: Vec<ResolutionFrameRates>,
}

/// One resolution's discrete frame-rate list — `Vec` rather than a
/// `Resolution`-keyed map, since JSON object keys must be strings and
/// `Resolution` isn't one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResolutionFrameRates {
    pub resolution: Resolution,
    pub rates: Vec<u32>,
}

/// Mouse mode doesn't affect the capture pipeline — it's purely which
/// datagram shape the browser sends — so it's tracked separately from
/// `CaptureSettings` rather than folded into the capture task's watch
/// channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseMode {
    Absolute,
    Relative,
}

/// The full set of user-adjustable settings, as written to and read from
/// the settings file.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PersistedSettings {
    pub capture: CaptureSettings,
    pub mouse_mode: MouseMode,
}
