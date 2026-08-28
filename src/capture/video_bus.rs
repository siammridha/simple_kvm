//! The single source of truth for "the latest video frame". Deliberately a
//! `watch` channel, not `broadcast`: only the newest value is ever kept,
//! which gives free frame-dropping under load — a slow WebRTC session
//! just misses frames instead of building an unbounded backlog on this
//! CPU-constrained device.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

#[derive(Debug, Clone)]
pub struct FrameEnvelope {
    pub data: Arc<[u8]>,
    /// When the capture driver says this frame actually came off the
    /// card (`v4l::buffer::Metadata::timestamp`, converted to a
    /// `Duration` since some arbitrary but consistent monotonic origin)
    /// — used to pace H.264 RTP timestamps by the real inter-frame
    /// interval instead of a nominal fps. Only the delta between two
    /// frames' values is meaningful, never the absolute value.
    pub captured_at: Duration,
}

pub type Sender = watch::Sender<Option<FrameEnvelope>>;
pub type Receiver = watch::Receiver<Option<FrameEnvelope>>;

pub fn channel() -> (Sender, Receiver) {
    watch::channel(None)
}
