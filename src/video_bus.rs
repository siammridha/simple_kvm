//! The single source of truth for "the latest video frame". Deliberately a
//! `watch` channel, not `broadcast`: only the newest value is ever kept,
//! which gives free frame-dropping under load — a slow WebRTC session
//! just misses frames instead of building an unbounded backlog on this
//! CPU-constrained device.

use std::sync::Arc;

use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Mjpeg,
    H264,
}

#[derive(Debug, Clone)]
pub struct FrameEnvelope {
    pub kind: FrameKind,
    pub data: Arc<[u8]>,
}

pub type Sender = watch::Sender<Option<FrameEnvelope>>;
pub type Receiver = watch::Receiver<Option<FrameEnvelope>>;

pub fn channel() -> (Sender, Receiver) {
    watch::channel(None)
}
