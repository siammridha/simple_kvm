//! Shared, live-mutable state — changed by a dropdown on the page, read
//! by the capture task and the WebRTC session layer via
//! `tokio::sync::watch`, so a change never has to touch the connection
//! itself. In-memory only: nothing here is ever persisted to disk.
//!
//! The capture settings type itself now lives with the driver that
//! consumes it (`device::capture_driver`, re-exported as
//! `capture::CaptureSettings`); this module is what's left, and #018
//! removes it entirely.

use serde::{Deserialize, Serialize};

use crate::capture::Resolution;

/// Live state of the capture card itself — whether it's plugged in right
/// now, and what resolutions/frame rates it supports. Published by
/// `capture::watch_device_state` over a `watch` channel, and pushed to the
/// web page over the `control` data channel (see `rtc::session::handle`)
/// so an already-open tab reflects a hot-plug/unplug instead of being
/// frozen at server-startup values.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DeviceState {
    pub available: bool,
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
