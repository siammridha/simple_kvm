//! GPU-accelerated H.264 encoding via `ffmpeg`'s `h264_vaapi` encoder,
//! driven as a subprocess. Runs in `CQP` (constant-QP) rate-control mode -
//! no bitrate target, matching how the software encoder this replaces
//! always worked (its default "Quality" mode never used a bitrate either).
//! Emits Annex-B access units (start-code-prefixed NAL units), matching
//! what WebCodecs' `VideoDecoder` expects when configured for
//! `avc.format: "annexb"`.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

/// How long `encode_yuyv_frame` will wait for its access unit before giving
/// up on this one frame. Needed because the AU splitter can only recognize
/// frame K's output is complete once it sees the *start* of frame K+1's
/// output arriving from `ffmpeg` - which can't happen until this very call
/// writes frame K+1 and returns. A plain blocking `recv()` would therefore
/// deadlock forever on however many frames of internal pipeline latency
/// `ffmpeg`/VAAPI happens to have (unverified, and not guaranteed to be
/// exactly one). Bounding the wait instead means those warm-up calls just
/// come back as an ordinary dropped-frame error - already handled
/// gracefully by the caller - regardless of how deep that pipeline is.
/// Matches `v4l2.rs`'s own capture-read timeout for consistency.
const ACCESS_UNIT_TIMEOUT: Duration = Duration::from_millis(500);

/// A joining/reconnecting browser only ever sees the *latest* published
/// frame (`video_bus` keeps no backlog - see the module doc there), so
/// without a periodic keyframe, everyone who connects after the very first
/// frame of a capture pass would only ever receive delta frames and could
/// never start decoding.
const INTRA_FRAME_PERIOD: u32 = 60;

/// Fixed quantization parameter for `h264_vaapi`'s `CQP` rate-control mode
/// - lower means higher quality and more bits, with no bitrate target to
/// fall back on instead. Carried over from the old software encoder's
/// fixed initial QP as a starting point; this is the one knob to retune by
/// eye if output looks too soft or too heavy.
const QP_VALUE: u32 = 26;

const VAAPI_DEVICE: &str = "/dev/dri/renderD128";

pub struct H264Encoder {
    child: Child,
    stdin: ChildStdin,
    access_units: mpsc::Receiver<Vec<u8>>,
    width: u32,
    height: u32,
    fps: u32,
    restart_pending: bool,
}

impl H264Encoder {
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        let (child, stdin, access_units) = spawn_ffmpeg(width, height, fps)?;
        Ok(Self { child, stdin, access_units, width, height, fps, restart_pending: false })
    }

    /// Forces the next encoded frame to be a keyframe, ahead of the
    /// periodic schedule (`INTRA_FRAME_PERIOD`) - used when a session's
    /// RTCP feedback (PLI/FIR) says its decoder needs one sooner, see
    /// `rtc::session::handle`'s `video_track.poll()` branch.
    ///
    /// Unlike the old software encoder, there's no cheap in-process way to
    /// ask `h264_vaapi` for an out-of-schedule IDR through `ffmpeg`'s CLI,
    /// so this restarts the whole subprocess instead - acted on lazily at
    /// the top of the next `encode_yuyv_frame` call (a fresh child needs a
    /// frame to encode before it can produce anything, so there's nothing
    /// useful to do here immediately).
    pub fn force_intra_frame(&mut self) {
        self.restart_pending = true;
    }

    /// Encodes one raw YUYV (4:2:2) frame into an H.264 access unit.
    ///
    /// Real capture hardware occasionally hands back a short/incomplete
    /// frame (e.g. a dropped USB packet) whose buffer is smaller than
    /// `width * height * 2` - checked up front rather than trusting the
    /// driver, since writing anything other than exactly one full frame to
    /// `ffmpeg`'s `rawvideo` stdin would desync the frame boundary for
    /// every frame after it.
    pub fn encode_yuyv_frame(&mut self, yuyv: &[u8]) -> Result<Vec<u8>> {
        if self.restart_pending {
            self.respawn().context("restarting ffmpeg for a forced keyframe")?;
            self.restart_pending = false;
        }

        let expected_len = self.width as usize * self.height as usize * 2;
        anyhow::ensure!(yuyv.len() >= expected_len, "short capture frame: got {} bytes, expected {expected_len}", yuyv.len());

        self.stdin.write_all(&yuyv[..expected_len]).context("writing frame to ffmpeg stdin")?;
        self.access_units.recv_timeout(ACCESS_UNIT_TIMEOUT).context("no access unit from ffmpeg within the timeout (pipeline still warming up, or ffmpeg died)")
    }

    fn respawn(&mut self) -> Result<()> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let (child, stdin, access_units) = spawn_ffmpeg(self.width, self.height, self.fps)?;
        self.child = child;
        self.stdin = stdin;
        self.access_units = access_units;
        Ok(())
    }
}

impl Drop for H264Encoder {
    /// Kills the child and - via the field drop that runs right after this
    /// - closes its stdin, so a settings change / hot-plug cycle never
    /// leaves an orphaned `ffmpeg` process running on the device.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawns `ffmpeg` reading raw YUYV frames from stdin and writing an
/// Annex-B H.264 stream (via the GPU's `h264_vaapi` encoder) to stdout,
/// plus the two background threads that drain it: one splitting stdout
/// into access units onto the returned channel, one forwarding stderr to
/// `tracing` so `ffmpeg` diagnostics land in the service log instead of
/// being silently discarded down a closed pipe.
fn spawn_ffmpeg(width: u32, height: u32, fps: u32) -> Result<(Child, ChildStdin, mpsc::Receiver<Vec<u8>>)> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuyv422",
            "-s",
            &format!("{width}x{height}"),
            "-r",
            &fps.to_string(),
            "-i",
            "pipe:0",
            "-vf",
            "format=nv12,hwupload",
            "-vaapi_device",
            VAAPI_DEVICE,
            "-c:v",
            "h264_vaapi",
            "-profile:v",
            "constrained_baseline",
            "-g",
            &INTRA_FRAME_PERIOD.to_string(),
            "-bf",
            "0",
            "-rc_mode",
            "CQP",
            "-qp",
            &QP_VALUE.to_string(),
            "-f",
            "h264",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning ffmpeg")?;

    let stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || read_access_units(stdout, tx));
    thread::spawn(move || log_stderr(stderr));

    Ok((child, stdin, rx))
}

/// Reads `ffmpeg`'s raw Annex-B stdout and pushes one complete access unit
/// per encoded frame onto `tx`. Runs until stdout hits EOF (the child
/// exited) or the receiving end is gone (the encoder was dropped or
/// restarted) - either way, dropping `tx` on return unblocks any
/// `encode_yuyv_frame` call waiting on `access_units.recv()` with an `Err`.
fn read_access_units(mut stdout: impl Read, tx: mpsc::Sender<Vec<u8>>) {
    let mut splitter = AccessUnitSplitter::default();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = match stdout.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        for au in splitter.push(&buf[..n]) {
            if tx.send(au).is_err() {
                return;
            }
        }
    }
}

fn log_stderr(stderr: impl Read) {
    for line in BufReader::new(stderr).lines().map_while(std::io::Result::ok) {
        tracing::warn!(target: "ffmpeg", "{line}");
    }
}

/// Splits a raw Annex-B byte stream (as produced by `ffmpeg -f h264`) into
/// discrete access units. Baseline profile with no B-frames means every
/// encoded frame is exactly one VCL slice NAL (type 1 or 5), optionally
/// preceded by non-VCL NALs (SPS/PPS/AUD/SEI) - so buffering bytes until a
/// *second* VCL slice start code appears, then flushing everything before
/// it, always yields exactly one frame's worth of NAL units per access
/// unit.
#[derive(Default)]
struct AccessUnitSplitter {
    buffer: Vec<u8>,
}

impl AccessUnitSplitter {
    /// Feeds newly read bytes in and returns any access units they
    /// completed. Bytes that don't yet complete an access unit are held
    /// internally for the next call.
    fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(bytes);

        let vcl_starts = find_vcl_nal_starts(&self.buffer);
        if vcl_starts.len() < 2 {
            return Vec::new();
        }

        let mut completed = Vec::with_capacity(vcl_starts.len() - 1);
        let mut boundary = 0;
        for &start in &vcl_starts[1..] {
            completed.push(self.buffer[boundary..start].to_vec());
            boundary = start;
        }
        self.buffer.drain(0..boundary);
        completed
    }
}

/// Finds the byte offset of every VCL slice NAL's start code (`00 00 01`
/// or `00 00 00 01`, immediately followed by a NAL header byte whose type
/// is 1 or 5) in `buf`. A NAL whose header byte hasn't arrived yet (start
/// code sitting right at the end of `buf`) is left for the next call
/// rather than guessed at.
///
/// Scanning for the literal byte sequence is safe here because Annex-B's
/// emulation prevention guarantees `00 00 00`/`00 00 01`/`00 00 02`/`00 00
/// 03` never occur inside a NAL's payload unescaped - only as real start
/// codes.
fn find_vcl_nal_starts(buf: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= buf.len() {
        let prefix_len = if buf[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if buf[i..i + 3] == [0, 0, 1] {
            3
        } else {
            i += 1;
            continue;
        };

        let header_pos = i + prefix_len;
        if header_pos >= buf.len() {
            break;
        }
        if matches!(buf[header_pos] & 0x1F, 1 | 5) {
            starts.push(i);
        }
        i = header_pos;
    }
    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    const SC4: &[u8] = &[0, 0, 0, 1];
    const SC3: &[u8] = &[0, 0, 1];

    fn nal(start_code: &[u8], nal_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = start_code.to_vec();
        out.push(nal_type);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn buffers_until_second_vcl_nal_before_emitting() {
        let mut splitter = AccessUnitSplitter::default();
        let sps = nal(SC4, 7, &[0xAA]);
        let pps = nal(SC4, 8, &[0xBB]);
        let idr = nal(SC4, 5, &[0xCC, 0xDD]);

        let mut chunk = Vec::new();
        chunk.extend_from_slice(&sps);
        chunk.extend_from_slice(&pps);
        chunk.extend_from_slice(&idr);
        assert!(splitter.push(&chunk).is_empty(), "only one VCL NAL seen so far, nothing should flush yet");
    }

    #[test]
    fn splits_multiple_aus_back_to_back() {
        let mut splitter = AccessUnitSplitter::default();
        let sps = nal(SC4, 7, &[0xAA]);
        let pps = nal(SC4, 8, &[0xBB]);
        let idr = nal(SC4, 5, &[0xCC, 0xDD]);
        let p1 = nal(SC3, 1, &[0x01]);
        let p2 = nal(SC3, 1, &[0x02]);

        let mut stream = Vec::new();
        stream.extend_from_slice(&sps);
        stream.extend_from_slice(&pps);
        stream.extend_from_slice(&idr);
        stream.extend_from_slice(&p1);
        stream.extend_from_slice(&p2);

        let aus = splitter.push(&stream);
        // Only 2 complete AUs available: [sps+pps+idr] and [p1]; p2 stays
        // buffered until a 3rd VCL NAL confirms it's actually complete.
        assert_eq!(aus.len(), 2);
        let mut expected_au1 = Vec::new();
        expected_au1.extend_from_slice(&sps);
        expected_au1.extend_from_slice(&pps);
        expected_au1.extend_from_slice(&idr);
        assert_eq!(aus[0], expected_au1);
        assert_eq!(aus[1], p1);
    }

    #[test]
    fn handles_start_code_split_across_pushes() {
        let mut splitter = AccessUnitSplitter::default();
        let idr = nal(SC4, 5, &[0xCC]);
        let p1 = nal(SC4, 1, &[0x01]);

        assert!(splitter.push(&idr).is_empty());

        // Split p1's 4-byte start code right down the middle, across two
        // separate reads - simulates a stdout read landing mid-start-code.
        assert!(splitter.push(&p1[..2]).is_empty());
        let aus = splitter.push(&p1[2..]);
        assert_eq!(aus, vec![idr]);
    }

    #[test]
    fn ignores_non_vcl_nal_types_between_slices() {
        let mut splitter = AccessUnitSplitter::default();
        let idr = nal(SC4, 5, &[0xCC]);
        let sei = nal(SC4, 6, &[0xEE]);
        let p1 = nal(SC4, 1, &[0x01]);

        let mut stream = Vec::new();
        stream.extend_from_slice(&idr);
        stream.extend_from_slice(&sei);
        stream.extend_from_slice(&p1);

        // p1 is the 2nd VCL NAL; idr+sei together form the first AU, since
        // the SEI in between isn't itself a slice.
        let aus = splitter.push(&stream);
        assert_eq!(aus.len(), 1);
        let mut expected = Vec::new();
        expected.extend_from_slice(&idr);
        expected.extend_from_slice(&sei);
        assert_eq!(aus[0], expected);
    }
}
