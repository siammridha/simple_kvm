//! Software H.264 encoding via `openh264`. This is the video mode expected
//! to struggle on the Wyse 3040's Atom CPU (see the plan doc) — it exists
//! because it was explicitly requested, not because it's expected to be
//! the practical default. Emits Annex-B access units (start-code-prefixed
//! NAL units), matching what WebCodecs' `VideoDecoder` expects when
//! configured for `avc.format: "annexb"`.

use anyhow::{Context, Result};
use openh264::encoder::{Encoder, EncoderConfig, IntraFramePeriod};
use openh264::formats::YUVBuffer;
use openh264::OpenH264API;

/// A joining/reconnecting browser only ever sees the *latest* published
/// frame (`video_bus` keeps no backlog - see the module doc there), so
/// without a periodic keyframe, everyone who connects after the very first
/// frame of a capture pass would only ever receive delta frames and could
/// never start decoding.
const INTRA_FRAME_PERIOD: u32 = 60;

pub struct H264Encoder {
    encoder: Encoder,
    width: usize,
    height: usize,
}

impl H264Encoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let config = EncoderConfig::new().intra_frame_period(IntraFramePeriod::from_num_frames(INTRA_FRAME_PERIOD));
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config).context("creating openh264 encoder")?;
        Ok(Self { encoder, width: width as usize, height: height as usize })
    }

    /// Encodes one raw YUYV (4:2:2) frame into an H.264 access unit.
    pub fn encode_yuyv_frame(&mut self, yuyv: &[u8]) -> Result<Vec<u8>> {
        let i420 = yuyv_to_i420(yuyv, self.width, self.height);
        let yuv = YUVBuffer::from_vec(i420, self.width, self.height);
        let bitstream = self.encoder.encode(&yuv).context("encoding H.264 frame")?;
        Ok(bitstream.to_vec())
    }
}

/// Converts packed YUYV (4:2:2, full vertical chroma resolution) to planar
/// I420 (4:2:0), the layout `YUVBuffer::from_vec` expects. Chroma is
/// downsampled vertically by simply keeping even rows' samples — cheap,
/// and plenty good enough for a screen-sharing feed.
fn yuyv_to_i420(yuyv: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![0u8; width * height * 3 / 2];
    let (y_plane, rest) = out.split_at_mut(width * height);
    let (u_plane, v_plane) = rest.split_at_mut(width * height / 4);
    let chroma_width = width / 2;

    for row in 0..height {
        let src_row = &yuyv[row * width * 2..(row + 1) * width * 2];
        let y_row = &mut y_plane[row * width..(row + 1) * width];
        for (i, pair) in src_row.chunks_exact(4).enumerate() {
            y_row[i * 2] = pair[0];
            y_row[i * 2 + 1] = pair[2];
        }
        if row % 2 == 0 {
            let chroma_row = row / 2;
            let u_row = &mut u_plane[chroma_row * chroma_width..(chroma_row + 1) * chroma_width];
            let v_row = &mut v_plane[chroma_row * chroma_width..(chroma_row + 1) * chroma_width];
            for (i, pair) in src_row.chunks_exact(4).enumerate() {
                u_row[i] = pair[1];
                v_row[i] = pair[3];
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuyv_to_i420_produces_correctly_sized_planes() {
        let width = 4;
        let height = 2;
        let yuyv = vec![0u8; width * height * 2];
        let i420 = yuyv_to_i420(&yuyv, width, height);
        assert_eq!(i420.len(), width * height * 3 / 2);
    }

    #[test]
    fn yuyv_to_i420_preserves_luma_samples() {
        // width=2, height=2: row0 = Y0=10 U=0 Y1=20 V=0 ; row1 = Y0=30 U=0 Y1=40 V=0
        let yuyv = [10u8, 0, 20, 0, 30, 0, 40, 0];
        let i420 = yuyv_to_i420(&yuyv, 2, 2);
        assert_eq!(&i420[0..4], &[10, 20, 30, 40]);
    }
}
