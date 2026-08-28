//! The blocking V4L2 frame-reading loop, driven from a `CaptureHandle` the
//! `device` module has already opened and negotiated a format on.
//!
//! Nothing here opens a device or knows a path: the handle is the only way
//! in. The stream is built as a *local variable within one function*
//! (`run_capture_loop`), not stored in a struct: the `v4l` crate's
//! `MmapStream<'a>` borrows the device it was built from, and the two are
//! only ever used together inside a single blocking loop anyway (per the
//! concurrency design — a settings change stops the loop and starts a
//! fresh one), so there's no need to fight the lifetime.

use std::time::Duration;

use anyhow::{Context, Result};
use v4l::buffer::Type as BufferType;
use v4l::io::mmap::Stream as MmapStream;
use v4l::io::traits::CaptureStream;

use crate::device::{CaptureHandle, Resolution};

pub use v4l::timestamp::Timestamp;

/// Reads YUYV frames off `capture` until `should_stop` returns true. Meant
/// to run inside `tokio::task::spawn_blocking`.
///
/// `make_handler` is called once, with the resolution the driver actually
/// negotiated (`CaptureHandle::resolution`, which may differ from the one
/// that was requested) and must build the per-frame handler from that.
/// Sizing a frame handler from the merely-requested resolution instead is
/// a real bug: a mismatch between it and the driver's actual frame size
/// means buffer-indexing code (I420 conversion) reads past the end of a
/// frame it assumed was a different size. The requested resolution isn't
/// even in scope here — only the negotiated one is.
pub fn run_capture_loop<H>(capture: &CaptureHandle, mut should_stop: impl FnMut() -> bool, make_handler: impl FnOnce(Resolution) -> Result<H>) -> Result<()>
where
    H: FnMut(&[u8], Timestamp),
{
    // If GPU setup fails here, we return before ever opening the mmap
    // capture stream below - no frames are read for a pass that can't
    // encode them anyway.
    let mut on_frame = make_handler(capture.resolution())?;

    let mut stream = MmapStream::with_buffers(capture.v4l2_device(), BufferType::VideoCapture, 4).context("starting mmap capture stream")?;
    stream.set_timeout(Duration::from_millis(500));

    while !should_stop() {
        match stream.next() {
            Ok((buf, meta)) => {
                // `buf` is the full fixed-size mmap buffer slot, not the
                // real frame - the driver reports the actual filled length
                // separately (`bytesused`).
                let len = (meta.bytesused as usize).min(buf.len());
                on_frame(&buf[..len], meta.timestamp);
            }
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(err) => return Err(err).context("reading capture frame"),
        }
    }
    Ok(())
}

/// Converts a driver capture timestamp to a `Duration` since some
/// arbitrary but consistent monotonic origin — only meaningful as a delta
/// between two frames, never as an absolute value (see
/// `video_bus::FrameEnvelope::captured_at`).
pub fn timestamp_to_duration(ts: Timestamp) -> Duration {
    Duration::new(ts.sec.max(0) as u64, (ts.usec.max(0) as u32).saturating_mul(1000))
}
