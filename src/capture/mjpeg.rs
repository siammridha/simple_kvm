//! JPEG frame handling for the "passthrough" video mode: real hardware
//! MJPEG frames need no processing at all (see [`crate::capture::v4l2`]'s
//! preference for the card's own MJPEG format), but cards without
//! hardware MJPEG only offer raw YUYV — this converts those frames to
//! JPEG in software so the mode still works, just not for free.

use anyhow::{Context, Result};
use jpeg_encoder::{ColorType, Encoder};

const FALLBACK_JPEG_QUALITY: u8 = 75;

/// Converts a raw YUYV (4:2:2) frame into a JPEG, duplicating each pair's
/// shared chroma samples into full YCbCr (4:4:4) as `jpeg-encoder` expects.
pub fn yuyv_to_jpeg(yuyv: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let mut ycbcr = vec![0u8; width as usize * height as usize * 3];
    for (i, pair) in yuyv.chunks_exact(4).enumerate() {
        let (y0, u, y1, v) = (pair[0], pair[1], pair[2], pair[3]);
        let base = i * 6;
        ycbcr[base] = y0;
        ycbcr[base + 1] = u;
        ycbcr[base + 2] = v;
        ycbcr[base + 3] = y1;
        ycbcr[base + 4] = u;
        ycbcr[base + 5] = v;
    }

    let mut out = Vec::new();
    let encoder = Encoder::new(&mut out, FALLBACK_JPEG_QUALITY);
    encoder
        .encode(&ycbcr, width as u16, height as u16, ColorType::Ycbcr)
        .context("encoding fallback JPEG frame from YUYV")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_a_solid_color_frame_to_a_valid_jpeg() {
        // 2x2 YUYV frame: two horizontal pairs, arbitrary but valid bytes.
        let yuyv = [128u8, 64, 128, 192, 128, 64, 128, 192];
        let jpeg = yuyv_to_jpeg(&yuyv, 2, 2).unwrap();
        // JPEG magic bytes (SOI marker) and non-trivial size.
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]);
        assert!(jpeg.len() > 4);
    }
}
