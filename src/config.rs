//! Shared, live-mutable settings — changed by a dropdown on the page,
//! read by the capture task and the WebTransport session layer via
//! `tokio::sync::watch`, so a change never has to touch the connection
//! itself.

use crate::capture::v4l2::Resolution;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoMode {
    Mjpeg,
    H264,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureSettings {
    pub video_mode: VideoMode,
    pub resolution: Resolution,
}
