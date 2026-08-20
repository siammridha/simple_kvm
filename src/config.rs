//! Shared, live-mutable settings — changed by a dropdown on the page,
//! read by the capture task and the WebTransport session layer via
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
