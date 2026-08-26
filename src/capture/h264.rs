//! Hardware H.264 encoding via VAAPI (Intel GPU), replacing the previous
//! CPU-only `openh264` path (see `docs/gpu-encoding-investigation.md` for
//! the investigation that proved this out). Raw YUYV frames are uploaded to
//! the GPU, color-converted to NV12 with the driver's video-processing
//! (VPP) pipeline, and encoded with the driver's H.264 encode pipeline -
//! all on-GPU, no CPU pixel work.
//!
//! This device's `i965` driver has broken auto-generated slice headers
//! (confirmed in the investigation): trusting them produces bitstreams that
//! parse correctly but decode as corrupt on every macroblock. Both I-slice
//! and P-slice headers are hand-packed here and submitted as
//! `VAEncPackedHeaderSlice` buffers instead, exactly the way ffmpeg's own
//! VAAPI encoder does it. SPS and PPS are hand-packed too and submitted as
//! `VAEncPackedHeaderSequence`/`VAEncPackedHeaderPicture` buffers: once any
//! packed header type is negotiated on this driver, it stops emitting its
//! own SPS/PPS into the coded output, so leaving them to auto-generation (as
//! originally assumed) produced a stream with no PPS at all - confirmed via
//! `ffmpeg -f null -` reporting "non-existing PPS 0 referenced" on every
//! frame during on-device verification.
//!
//! There is deliberately no CPU fallback: if GPU setup fails, `new` returns
//! an error and the app fails to start rather than silently falling back to
//! software encoding.

use std::ops::Deref;
use std::rc::Rc;

use anyhow::Context as _;
use anyhow::Result;
use libva_rs::{
    BorrowedBufferType, BufferType, Context, Display, EncMiscParameter,
    EncMiscParameterFrameRate, EncMiscParameterRateControl, EncPackedHeaderParameter,
    EncPackedHeaderType, EncPictureParameterBufferH264, EncSequenceParameterBufferH264,
    EncSliceParameterBufferH264, H264EncPicFields, H264EncSeqFields, Image, MappedCodedBuffer,
    Picture, PictureH264, ProcPipelineParameterBuffer, RcFlags, Surface, UsageHint,
    VAConfigAttrib, VAConfigAttribType, VAEntrypoint, VAProfile, VARectangle,
    VA_ATTRIB_NOT_SUPPORTED, VA_ENC_PACKED_HEADER_PICTURE, VA_ENC_PACKED_HEADER_SEQUENCE,
    VA_ENC_PACKED_HEADER_SLICE, VA_FOURCC_YUY2, VA_INVALID_ID,
    VA_PICTURE_H264_SHORT_TERM_REFERENCE, VA_RC_CBR, VA_RT_FORMAT_YUV420, VA_RT_FORMAT_YUV422,
};

/// H.264 Constrained Baseline profile - matches the browser's negotiated
/// SDP `profile-level-id=42e01f` (see `rtc::mod`): `0x42` = 66. Written
/// directly into the hand-packed SPS below (see "SPS and PPS are
/// hand-packed too" above) - the `VAProfile` passed at `Config` creation is
/// a separate, driver-facing negotiation and doesn't by itself determine
/// the bytes that end up in the SPS once packed headers are in use.
const PROFILE_IDC: u8 = 66;
/// `constraint_set0_flag..constraint_set2_flag` all set, matching the `e0`
/// byte in `42e01f`.
const PROFILE_COMPATIBILITY: u8 = 0xE0;

/// A joining/reconnecting browser only ever sees the *latest* published
/// frame (`video_bus` keeps no backlog - see the module doc there), so
/// without a periodic keyframe, everyone who connects after the very first
/// frame of a capture pass would only ever receive delta frames and could
/// never start decoding.
const INTRA_FRAME_PERIOD: u64 = 60;

/// Default/fallback bitrate, used when no persisted setting exists yet (see
/// `main.rs`) and by `CaptureManager::default_settings`. 2 Mbps has been
/// validated with a live end-to-end capture test; see `MAX_SAFE_BITRATE_BPS`
/// below for the confirmed upper bound.
pub const DEFAULT_BITRATE_BPS: u32 = 2_000_000;

/// Hard ceiling for any user-supplied bitrate (the web UI's dropdown, or a
/// hand-crafted control message) - clamped to this value server-side before
/// it's ever applied, see `rtc::session::handle_control_message`.
///
/// Confirmed clean in manual testing at 5 Mbps (1080p@10fps and
/// 720p@25fps) - see docs/gpu-encoding-investigation.md.
pub const MAX_SAFE_BITRATE_BPS: u32 = 5_000_000;

/// `log2_max_frame_num_minus4` / `log2_max_pic_order_cnt_lsb_minus4` are
/// both set to 4, giving an 8-bit range (0-255) for `frame_num` and
/// `pic_order_cnt_lsb`. Comfortably larger than `INTRA_FRAME_PERIOD`, so
/// neither ever wraps within a GOP.
const FRAME_NUM_BITS: u32 = 8;
const POC_LSB_BITS: u32 = 8;

/// Level 4.0: the lowest standard level whose max frame size (8192
/// macroblocks) covers this device's 1920x1088 coded resolution (8160
/// macroblocks). The browser negotiates baseline profile at level 3.1 in
/// the SDP (see `rtc::mod::MIME_TYPE_H264`'s `sdp_fmtp_line`), but that's a
/// capability negotiation formality - decoders configure themselves from
/// the in-band SPS, not the SDP level, so this mismatch is harmless.
const LEVEL_IDC: u8 = 40;

/// Rate-control hint only (`EncMiscParameterFrameRate`) - actual encode
/// calls are driven by frame arrival from the capture loop, not by a timer,
/// so this doesn't need to track the real capture fps exactly.
const ENCODE_FRAMERATE_HINT: u32 = 10;

const PIC_INIT_QP: u8 = 26;

/// The GPU's video-processing (VPP) context that does YUYV -> NV12 color
/// conversion. A distinct type (rather than a bare `Rc<Context>` field on
/// `H264Encoder`) so its own teardown - separate from the H.264 encode
/// context below - is logged at the point it actually happens.
struct GpuColorConverter(Rc<Context>);

impl Deref for GpuColorConverter {
    type Target = Rc<Context>;

    fn deref(&self) -> &Rc<Context> {
        &self.0
    }
}

impl Drop for GpuColorConverter {
    fn drop(&mut self) {
        tracing::info!("GPU color converter stopped");
    }
}

/// The GPU's H.264 encode context. See `GpuColorConverter` above for why
/// this is a distinct type rather than a bare `Rc<Context>` field.
struct GpuH264EncodeContext(Rc<Context>);

impl Deref for GpuH264EncodeContext {
    type Target = Rc<Context>;

    fn deref(&self) -> &Rc<Context> {
        &self.0
    }
}

impl Drop for GpuH264EncodeContext {
    fn drop(&mut self) {
        tracing::info!("GPU H.264 encoder stopped");
    }
}

pub struct H264Encoder {
    enc_context: GpuH264EncodeContext,
    vpp_context: GpuColorConverter,
    yuyv_surface: Surface,
    yuyv_image_format: libva_rs::VAImageFormat,
    /// A 2-surface NV12 reference pool: `num_ref_frames = 1` is enough with
    /// no B-frames, so ping-ponging between two surfaces guarantees the
    /// frame currently being encoded and the previous reconstructed
    /// reference are never the same buffer.
    ref_surfaces: [Rc<Surface>; 2],
    width: u32,
    height: u32,
    coded_width: u32,
    coded_height: u32,
    bitrate: u32,
    frame_count: u64,
    /// Index into `ref_surfaces` that the *next* call to
    /// `encode_yuyv_frame` will render/encode into.
    next_target: usize,
    frame_num: u16,
    pic_order_cnt: u32,
    idr_pic_id: u16,
    force_intra: bool,
    /// Packed SPS/PPS (bytes, bit length), built once at construction -
    /// their content never changes across frames - and resubmitted as
    /// `VAEncPackedHeaderSequence`/`VAEncPackedHeaderPicture` buffers on
    /// every IDR frame.
    packed_sps: (Vec<u8>, usize),
    packed_pps: (Vec<u8>, usize),
}

impl H264Encoder {
    pub fn new(width: u32, height: u32, bitrate: u32) -> Result<Self> {
        let display =
            Display::open().context("Display::open() found no usable DRM/VAAPI device")?;

        let coded_width = width.div_ceil(16) * 16;
        let coded_height = height.div_ceil(16) * 16;

        // --- H.264 encode config/context ---
        //
        // ConstrainedBaseline (CAVLC, no B-frames) matches both what this
        // pipeline actually produces and the profile the browser negotiates
        // in SDP (profile-level-id=42e01f, see `rtc::mod`).
        let h264_profile = VAProfile::VAProfileH264ConstrainedBaseline;
        let enc_entrypoints = display
            .query_config_entrypoints(h264_profile)
            .context("query_config_entrypoints(VAProfileH264ConstrainedBaseline)")?;
        anyhow::ensure!(
            enc_entrypoints.contains(&VAEntrypoint::VAEntrypointEncSlice),
            "driver does not support VAEntrypointEncSlice for VAProfileH264ConstrainedBaseline"
        );

        let mut enc_attrs = vec![
            VAConfigAttrib { type_: VAConfigAttribType::VAConfigAttribRTFormat, value: 0 },
            VAConfigAttrib { type_: VAConfigAttribType::VAConfigAttribEncPackedHeaders, value: 0 },
            VAConfigAttrib { type_: VAConfigAttribType::VAConfigAttribRateControl, value: 0 },
        ];
        display
            .get_config_attributes(h264_profile, VAEntrypoint::VAEntrypointEncSlice, &mut enc_attrs)
            .context("get_config_attributes (H.264 encode)")?;
        anyhow::ensure!(
            enc_attrs[0].value != VA_ATTRIB_NOT_SUPPORTED,
            "driver did not report a supported RT format for H.264 encode"
        );
        let required_packed_headers = VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE | VA_ENC_PACKED_HEADER_SLICE;
        anyhow::ensure!(
            enc_attrs[1].value & required_packed_headers == required_packed_headers,
            "driver does not support hand-packed sequence/picture/slice headers (needs {:#x}, \
             reports {:#x}) - see docs/gpu-encoding-investigation.md",
            required_packed_headers,
            enc_attrs[1].value
        );
        // Without explicitly negotiating CBR here, the driver defaults to
        // constant-QP and silently ignores `EncMiscParameterRateControl`'s
        // bitrate target entirely - confirmed on-device: encoded output
        // measured ~12.6 Mbps (vs. the 2 Mbps target) before this was added.
        anyhow::ensure!(
            enc_attrs[2].value & VA_RC_CBR != 0,
            "driver does not support CBR rate control (reports {:#x}) - required for the \
             configured bitrate to actually be honored, see docs/gpu-encoding-investigation.md",
            enc_attrs[2].value
        );

        let enc_config = display
            .create_config(
                vec![
                    VAConfigAttrib { type_: VAConfigAttribType::VAConfigAttribRTFormat, value: enc_attrs[0].value },
                    VAConfigAttrib {
                        type_: VAConfigAttribType::VAConfigAttribEncPackedHeaders,
                        value: required_packed_headers,
                    },
                    VAConfigAttrib { type_: VAConfigAttribType::VAConfigAttribRateControl, value: VA_RC_CBR },
                ],
                h264_profile,
                VAEntrypoint::VAEntrypointEncSlice,
            )
            .context("create_config (H.264 encode)")?;

        let ref_surfaces_vec = display
            .create_surfaces(
                VA_RT_FORMAT_YUV420,
                None,
                coded_width,
                coded_height,
                Some(UsageHint::USAGE_HINT_ENCODER | UsageHint::USAGE_HINT_VPP_WRITE),
                2,
            )
            .context("create_surfaces (NV12 reference pool)")?;

        let enc_context = display
            .create_context(&enc_config, coded_width, coded_height, Some(&ref_surfaces_vec), true)
            .context("create_context (H.264 encode)")?;

        // --- VPP (color conversion) config/context ---
        let vpp_profile = VAProfile::VAProfileNone;
        let vpp_entrypoints = display
            .query_config_entrypoints(vpp_profile)
            .context("query_config_entrypoints(VAProfileNone)")?;
        anyhow::ensure!(
            vpp_entrypoints.contains(&VAEntrypoint::VAEntrypointVideoProc),
            "driver does not support VAEntrypointVideoProc"
        );

        let mut vpp_attrs = vec![VAConfigAttrib { type_: VAConfigAttribType::VAConfigAttribRTFormat, value: 0 }];
        display
            .get_config_attributes(vpp_profile, VAEntrypoint::VAEntrypointVideoProc, &mut vpp_attrs)
            .context("get_config_attributes (VPP)")?;

        let vpp_config = display
            .create_config(vpp_attrs, vpp_profile, VAEntrypoint::VAEntrypointVideoProc)
            .context("create_config (VPP)")?;

        let vpp_context = display
            .create_context(&vpp_config, coded_width, coded_height, Some(&ref_surfaces_vec), true)
            .context("create_context (VPP)")?;

        // --- YUYV input surface (the raw shape the camera delivers) ---
        let mut yuyv_surfaces = display
            .create_surfaces(VA_RT_FORMAT_YUV422, Some(VA_FOURCC_YUY2), width, height, Some(UsageHint::USAGE_HINT_VPP_READ), 1)
            .context("create_surfaces (YUYV input)")?;
        let yuyv_surface = yuyv_surfaces.remove(0);

        let yuyv_image_format = display
            .query_image_formats()
            .context("query_image_formats")?
            .into_iter()
            .find(|f| f.fourcc == VA_FOURCC_YUY2)
            .context("driver does not report a YUY2 image format")?;

        let ref_surfaces: [Rc<Surface>; 2] = ref_surfaces_vec
            .into_iter()
            .map(Rc::new)
            .collect::<Vec<_>>()
            .try_into()
            .map_err(|_| anyhow::anyhow!("expected exactly 2 reference surfaces"))?;

        let packed_sps = pack_sps(coded_width / 16, coded_height / 16);
        let packed_pps = pack_pps();

        Ok(Self {
            enc_context: GpuH264EncodeContext(enc_context),
            vpp_context: GpuColorConverter(vpp_context),
            yuyv_surface,
            yuyv_image_format,
            ref_surfaces,
            width,
            height,
            coded_width,
            coded_height,
            bitrate,
            frame_count: 0,
            next_target: 0,
            frame_num: 0,
            pic_order_cnt: 0,
            idr_pic_id: 0,
            force_intra: false,
            packed_sps,
            packed_pps,
        })
    }

    /// Forces the next encoded frame to be a keyframe, ahead of the
    /// periodic schedule (`INTRA_FRAME_PERIOD`) — used when a session's
    /// RTCP feedback (PLI/FIR) says its decoder needs one sooner, see
    /// `rtc::session::handle` and `capture::run_one_pass`.
    pub fn force_intra_frame(&mut self) {
        self.force_intra = true;
    }

    /// Encodes one raw YUYV (4:2:2) frame into an H.264 access unit.
    ///
    /// Real capture hardware occasionally hands back a short/incomplete
    /// frame (e.g. a dropped USB packet) whose buffer is smaller than
    /// `width * height * 2` — checked up front rather than trusting the
    /// driver, since indexing into it as if it were full-sized would read
    /// past the end of the buffer.
    pub fn encode_yuyv_frame(&mut self, yuyv: &[u8]) -> Result<Vec<u8>> {
        let expected_len = self.width as usize * self.height as usize * 2;
        anyhow::ensure!(yuyv.len() >= expected_len, "short capture frame: got {} bytes, expected {expected_len}", yuyv.len());

        self.upload_yuyv(&yuyv[..expected_len])?;

        let is_idr = self.force_intra || self.frame_count.is_multiple_of(INTRA_FRAME_PERIOD);
        self.force_intra = false;

        let target = self.next_target;
        let prev = 1 - target;

        let (frame_num, poc, idr_pic_id) = if is_idr {
            let idr_pic_id = self.idr_pic_id;
            self.idr_pic_id = self.idr_pic_id.wrapping_add(1);
            (0u16, 0u32, idr_pic_id)
        } else {
            (
                (self.frame_num as u32 + 1) as u16 % (1u16 << FRAME_NUM_BITS),
                self.pic_order_cnt + 1,
                self.idr_pic_id,
            )
        };

        self.convert_to_nv12(target)?;
        let bitstream = self.encode_nv12(target, prev, is_idr, frame_num, poc, idr_pic_id)?;

        self.frame_num = frame_num;
        self.pic_order_cnt = poc;
        self.next_target = prev;
        self.frame_count += 1;

        Ok(bitstream)
    }

    /// Uploads a raw YUYV frame into `self.yuyv_surface` via `libva::Image`
    /// (`vaCreateImage`/`vaGetImage`, write, `vaPutImage` on drop). Copies
    /// row by row rather than as one flat blit, since the driver's reported
    /// stride (`pitches[0]`) can be wider than `width * 2` for alignment.
    fn upload_yuyv(&mut self, yuyv: &[u8]) -> Result<()> {
        let mut image = Image::create_from(&self.yuyv_surface, self.yuyv_image_format, (self.width, self.height), (self.width, self.height))
            .context("Image::create_from (YUYV upload)")?;

        let stride = image.image().pitches[0] as usize;
        let row_bytes = self.width as usize * 2;
        let height = self.height as usize;
        let buf = image.as_mut();
        for row in 0..height {
            let src = &yuyv[row * row_bytes..(row + 1) * row_bytes];
            let dst_start = row * stride;
            buf[dst_start..dst_start + row_bytes].copy_from_slice(src);
        }
        Ok(())
        // `image` drops here, which does the actual `vaPutImage` upload.
    }

    /// Runs the VPP pipeline, converting `self.yuyv_surface` into
    /// `self.ref_surfaces[target]` (NV12), entirely on the GPU.
    fn convert_to_nv12(&mut self, target: usize) -> Result<()> {
        let proc_pipeline = ProcPipelineParameterBuffer::new(
            self.yuyv_surface.id(),
            Some(VARectangle { x: 0, y: 0, width: self.width as u16, height: self.height as u16 }),
        );
        let buffer = self.vpp_context.create_buffer(BufferType::ProcPipelineParameter(proc_pipeline)).context("create_buffer(ProcPipelineParameter)")?;

        let mut picture = Picture::new(self.frame_count, Rc::clone(&self.vpp_context), Rc::clone(&self.ref_surfaces[target]));
        picture.add_buffer(buffer);

        let picture = picture.begin().context("vaBeginPicture (VPP)")?;
        let picture = picture.render().context("vaRenderPicture (VPP)")?;
        let picture = picture.end().context("vaEndPicture (VPP)")?;
        picture.sync().map_err(|(err, _)| err).context("vaSyncSurface (VPP)")?;

        Ok(())
    }

    /// Encodes `self.ref_surfaces[target]` (already-converted NV12) as an I
    /// or P slice, referencing `self.ref_surfaces[prev]` for P slices, and
    /// returns the resulting Annex-B access unit.
    #[allow(clippy::too_many_arguments)]
    fn encode_nv12(&mut self, target: usize, prev: usize, is_idr: bool, frame_num: u16, poc: u32, idr_pic_id: u16) -> Result<Vec<u8>> {
        let coded_buf = self
            .enc_context
            .create_enc_coded(self.coded_width as usize * self.coded_height as usize * 3)
            .context("create_enc_coded")?;

        let curr_pic = PictureH264::new(self.ref_surfaces[target].id(), frame_num as u32, VA_PICTURE_H264_SHORT_TERM_REFERENCE, poc as i32, poc as i32);

        let mut reference_frames: [PictureH264; 16] = std::array::from_fn(|_| PictureH264::invalid());
        if !is_idr {
            reference_frames[0] = PictureH264::new(self.ref_surfaces[prev].id(), self.frame_num as u32, VA_PICTURE_H264_SHORT_TERM_REFERENCE, self.pic_order_cnt as i32, self.pic_order_cnt as i32);
        }

        let pic_fields = H264EncPicFields::new(
            is_idr as u32, // idr_pic_flag
            1,             // reference_pic_flag - both I and P frames are references here
            0,             // entropy_coding_mode_flag (CAVLC)
            0,             // weighted_pred_flag
            0,             // weighted_bipred_idc
            0,             // constrained_intra_pred_flag
            0,             // transform_8x8_mode_flag
            1,             // deblocking_filter_control_present_flag
            0,             // redundant_pic_cnt_present_flag
            0,             // pic_order_present_flag
            0,             // pic_scaling_matrix_present_flag
        );

        let pic_param = EncPictureParameterBufferH264::new(
            curr_pic,
            reference_frames,
            coded_buf.id(),
            0, // pic_parameter_set_id
            0, // seq_parameter_set_id
            0, // last_picture
            frame_num,
            PIC_INIT_QP,
            0, // num_ref_idx_l0_active_minus1
            0, // num_ref_idx_l1_active_minus1
            0, // chroma_qp_index_offset
            0, // second_chroma_qp_index_offset
            &pic_fields,
        );

        let slice_type: u8 = if is_idr { 2 } else { 0 };
        let num_macroblocks = (self.coded_width / 16) * (self.coded_height / 16);
        let ref_pic_list_0: [PictureH264; 32] = std::array::from_fn(|_| PictureH264::invalid());
        let ref_pic_list_1: [PictureH264; 32] = std::array::from_fn(|_| PictureH264::invalid());
        let slice_param = EncSliceParameterBufferH264::new(
            0, // macroblock_address
            num_macroblocks,
            VA_INVALID_ID, // macroblock_info
            slice_type,
            0, // pic_parameter_set_id
            idr_pic_id,
            poc as u16,
            0,      // delta_pic_order_cnt_bottom
            [0, 0], // delta_pic_order_cnt
            0,      // direct_spatial_mv_pred_flag
            0,      // num_ref_idx_active_override_flag
            0,      // num_ref_idx_l0_active_minus1
            0,      // num_ref_idx_l1_active_minus1
            ref_pic_list_0,
            ref_pic_list_1,
            0, // luma_log2_weight_denom
            0, // chroma_log2_weight_denom
            0,
            [0i16; 32],
            [0i16; 32],
            0,
            [[0i16; 2]; 32],
            [[0i16; 2]; 32],
            0,
            [0i16; 32],
            [0i16; 32],
            0,
            [[0i16; 2]; 32],
            [[0i16; 2]; 32],
            0, // cabac_init_idc
            0, // slice_qp_delta
            0, // disable_deblocking_filter_idc
            0, // slice_alpha_c0_offset_div2
            0, // slice_beta_offset_div2
        );

        let (nal_ref_idc, nal_unit_type) = if is_idr { (3u8, 5u8) } else { (2u8, 1u8) };
        let (packed_bytes, packed_bit_len) = pack_slice_header(PackedSliceHeaderArgs {
            is_idr,
            slice_type,
            frame_num,
            idr_pic_id,
            pic_order_cnt_lsb: poc as u16,
            nal_ref_idc,
            nal_unit_type,
        });
        let packed_header_param = EncPackedHeaderParameter::new(EncPackedHeaderType::Slice, packed_bit_len as u32, true);

        let mut picture = Picture::new(self.frame_count, Rc::clone(&self.enc_context), Rc::clone(&self.ref_surfaces[target]));

        if is_idr {
            let seq_param = self.build_seq_param();
            picture.add_buffer(self.enc_context.create_buffer(BufferType::EncSequenceParameter(seq_param)).context("create_buffer(EncSequenceParameter)")?);

            let (sps_bytes, sps_bit_len) = &self.packed_sps;
            let sps_header_param = EncPackedHeaderParameter::new(EncPackedHeaderType::Sequence, *sps_bit_len as u32, true);
            picture.add_buffer(self.enc_context.create_buffer(BufferType::EncPackedHeaderParameter(sps_header_param)).context("create_buffer(EncPackedHeaderParameter, sequence)")?);
            picture.add_buffer(
                self.enc_context
                    .create_buffer_borrowed(BorrowedBufferType::EncPackedHeaderData(sps_bytes))
                    .context("create_buffer(EncPackedHeaderData, sequence)")?,
            );

            let (pps_bytes, pps_bit_len) = &self.packed_pps;
            let pps_header_param = EncPackedHeaderParameter::new(EncPackedHeaderType::Picture, *pps_bit_len as u32, true);
            picture.add_buffer(self.enc_context.create_buffer(BufferType::EncPackedHeaderParameter(pps_header_param)).context("create_buffer(EncPackedHeaderParameter, picture)")?);
            picture.add_buffer(
                self.enc_context
                    .create_buffer_borrowed(BorrowedBufferType::EncPackedHeaderData(pps_bytes))
                    .context("create_buffer(EncPackedHeaderData, picture)")?,
            );
        }

        let rc_flags = RcFlags::new(0, 0, 0, 0, 0, 0, 0, 0, 0);
        let rc_param = EncMiscParameterRateControl::new(self.bitrate, 100, 1000, PIC_INIT_QP as u32, 1, 0, rc_flags, 0, 51, 0, 0);
        picture.add_buffer(self.enc_context.create_buffer(BufferType::EncMiscParameter(EncMiscParameter::RateControl(rc_param))).context("create_buffer(EncMiscParameter::RateControl)")?);

        let fr_param = EncMiscParameterFrameRate::new(ENCODE_FRAMERATE_HINT, 0);
        picture.add_buffer(self.enc_context.create_buffer(BufferType::EncMiscParameter(EncMiscParameter::FrameRate(fr_param))).context("create_buffer(EncMiscParameter::FrameRate)")?);

        picture.add_buffer(self.enc_context.create_buffer(BufferType::EncPictureParameter(pic_param)).context("create_buffer(EncPictureParameter)")?);
        picture.add_buffer(self.enc_context.create_buffer(BufferType::EncSliceParameter(slice_param)).context("create_buffer(EncSliceParameter)")?);
        picture.add_buffer(self.enc_context.create_buffer(BufferType::EncPackedHeaderParameter(packed_header_param)).context("create_buffer(EncPackedHeaderParameter)")?);
        picture.add_buffer(
            self.enc_context
                .create_buffer_borrowed(BorrowedBufferType::EncPackedHeaderData(&packed_bytes))
                .context("create_buffer(EncPackedHeaderData)")?,
        );

        let picture = picture.begin().context("vaBeginPicture (encode)")?;
        let picture = picture.render().context("vaRenderPicture (encode)")?;
        let picture = picture.end().context("vaEndPicture (encode)")?;
        picture.sync().map_err(|(err, _)| err).context("vaSyncSurface (encode)")?;

        let mapped = MappedCodedBuffer::new(&coded_buf).context("MappedCodedBuffer::new")?;
        let mut bitstream = Vec::new();
        for segment in mapped.iter() {
            bitstream.extend_from_slice(segment.buf);
        }
        Ok(bitstream)
    }

    fn build_seq_param(&self) -> EncSequenceParameterBufferH264 {
        let seq_fields = H264EncSeqFields::new(
            1, // chroma_format_idc (4:2:0)
            1, // frame_mbs_only_flag
            0, // mb_adaptive_frame_field_flag
            0, // seq_scaling_matrix_present_flag
            1, // direct_8x8_inference_flag
            FRAME_NUM_BITS - 4,
            0, // pic_order_cnt_type
            POC_LSB_BITS - 4,
            0, // delta_pic_order_always_zero_flag
        );
        EncSequenceParameterBufferH264::new(
            0, // seq_parameter_set_id
            LEVEL_IDC,
            INTRA_FRAME_PERIOD as u32, // intra_period
            INTRA_FRAME_PERIOD as u32, // intra_idr_period
            1,                         // ip_period
            self.bitrate,
            1, // max_num_ref_frames
            (self.coded_width / 16) as u16,
            (self.coded_height / 16) as u16,
            &seq_fields,
            0, // bit_depth_luma_minus8
            0, // bit_depth_chroma_minus8
            0, // num_ref_frames_in_pic_order_cnt_cycle
            0, // offset_for_non_ref_pic
            0, // offset_for_top_to_bottom_field
            [0i32; 256],
            None, // frame_crop
            None, // vui_fields
            0,    // aspect_ratio_idc
            0,    // sar_width
            0,    // sar_height
            0,    // num_units_in_tick
            0,    // time_scale
        )
    }
}

struct PackedSliceHeaderArgs {
    is_idr: bool,
    slice_type: u8,
    frame_num: u16,
    idr_pic_id: u16,
    pic_order_cnt_lsb: u16,
    nal_ref_idc: u8,
    nal_unit_type: u8,
}

/// Hand-packs an H.264 slice header (I or P) and wraps it as a
/// start-code-prefixed NAL unit, ready to submit as a `VAEncPackedHeaderSlice`
/// buffer. See the module doc and `docs/gpu-encoding-investigation.md` for
/// why this is necessary on this device's driver.
///
/// Returns `(packed_bytes, bit_length)`. `bit_length` is the exact number of
/// meaningful bits in `packed_bytes` (start code + NAL header + slice header
/// fields, including any emulation-prevention bytes inserted) - the driver
/// continues writing slice data immediately after this exact bit position,
/// so it must not include any padding used to round `packed_bytes` up to a
/// whole number of bytes.
fn pack_slice_header(args: PackedSliceHeaderArgs) -> (Vec<u8>, usize) {
    let mut w = BitWriter::new();
    w.write_ue(0); // first_mb_in_slice
    w.write_ue(args.slice_type as u32);
    w.write_ue(0); // pic_parameter_set_id
    w.write_bits(args.frame_num as u32, FRAME_NUM_BITS);
    if args.is_idr {
        w.write_ue(args.idr_pic_id as u32);
    }
    // pic_order_cnt_type == 0:
    w.write_bits(args.pic_order_cnt_lsb as u32, POC_LSB_BITS);
    // pic_order_present_flag == 0: no delta_pic_order_cnt_bottom.
    if !args.is_idr {
        w.write_bit(0); // num_ref_idx_active_override_flag
        w.write_bit(0); // ref_pic_list_modification_flag_l0
    }
    // nal_ref_idc != 0 for both I and P slices here (both are references).
    if args.is_idr {
        w.write_bit(0); // no_output_of_prior_pics_flag
        w.write_bit(0); // long_term_reference_flag
    } else {
        w.write_bit(0); // adaptive_ref_pic_marking_mode_flag
    }
    // entropy_coding_mode_flag == 0 (CAVLC): no cabac_init_idc.
    w.write_se(0); // slice_qp_delta
    // deblocking_filter_control_present_flag == 1:
    w.write_ue(0); // disable_deblocking_filter_idc (enabled)
    w.write_se(0); // slice_alpha_c0_offset_div2
    w.write_se(0); // slice_beta_offset_div2

    let header_bits = w.bit_len();
    let header_bytes = w.into_bytes();

    let nal_header_byte = (args.nal_ref_idc << 5) | args.nal_unit_type;
    let mut rbsp = Vec::with_capacity(1 + header_bytes.len());
    rbsp.push(nal_header_byte);
    rbsp.extend_from_slice(&header_bytes);

    let rbsp_with_epb = add_emulation_prevention(&rbsp);
    let epb_bytes_inserted = rbsp_with_epb.len() - rbsp.len();

    let mut packed = Vec::with_capacity(3 + rbsp_with_epb.len());
    packed.extend_from_slice(&[0x00, 0x00, 0x01]);
    packed.extend_from_slice(&rbsp_with_epb);

    let bit_length = 24 + 8 + header_bits + epb_bytes_inserted * 8;
    (packed, bit_length)
}

/// Hand-packs an SPS (Sequence Parameter Set) and wraps it as a
/// start-code-prefixed NAL unit, ready to submit as a
/// `VAEncPackedHeaderSequence` buffer. Constant across the encoder's
/// lifetime (built once in `H264Encoder::new`), since none of its fields
/// change frame to frame.
fn pack_sps(width_in_mbs: u32, height_in_mbs: u32) -> (Vec<u8>, usize) {
    let mut w = BitWriter::new();
    w.write_bits(PROFILE_IDC as u32, 8);
    w.write_bits(PROFILE_COMPATIBILITY as u32, 8);
    w.write_bits(LEVEL_IDC as u32, 8);
    w.write_ue(0); // seq_parameter_set_id
    // PROFILE_IDC (66, ConstrainedBaseline) isn't in the high-profile set
    // that carries chroma_format_idc/bit_depth/scaling-list fields here -
    // those are implied (4:2:0, 8-bit) and omitted, per spec 7.3.2.1.1.
    w.write_ue(FRAME_NUM_BITS - 4); // log2_max_frame_num_minus4
    w.write_ue(0); // pic_order_cnt_type
    w.write_ue(POC_LSB_BITS - 4); // log2_max_pic_order_cnt_lsb_minus4
    w.write_ue(1); // max_num_ref_frames
    w.write_bit(0); // gaps_in_frame_num_value_allowed_flag
    w.write_ue(width_in_mbs - 1); // pic_width_in_mbs_minus1
    // frame_mbs_only_flag == 1: pic_height_in_map_units == pic_height_in_mbs.
    w.write_ue(height_in_mbs - 1); // pic_height_in_map_units_minus1
    w.write_bit(1); // frame_mbs_only_flag
    w.write_bit(1); // direct_8x8_inference_flag
    w.write_bit(0); // frame_cropping_flag (no crop - see module doc on 1088 vs 1080)
    w.write_bit(0); // vui_parameters_present_flag
    w.write_bit(1); // rbsp_stop_one_bit (rbsp_trailing_bits - SPS is a complete NAL)

    let nal_header_byte = (3u8 << 5) | 7; // nal_ref_idc=3, nal_unit_type=7 (SPS)
    finish_standalone_nal(nal_header_byte, &w.into_bytes())
}

/// Hand-packs a PPS (Picture Parameter Set) and wraps it as a
/// start-code-prefixed NAL unit, ready to submit as a
/// `VAEncPackedHeaderPicture` buffer. Constant across the encoder's
/// lifetime, same as `pack_sps`.
fn pack_pps() -> (Vec<u8>, usize) {
    let mut w = BitWriter::new();
    w.write_ue(0); // pic_parameter_set_id
    w.write_ue(0); // seq_parameter_set_id
    w.write_bit(0); // entropy_coding_mode_flag (CAVLC)
    w.write_bit(0); // bottom_field_pic_order_in_frame_present_flag
    w.write_ue(0); // num_slice_groups_minus1
    w.write_ue(0); // num_ref_idx_l0_default_active_minus1
    w.write_ue(0); // num_ref_idx_l1_default_active_minus1
    w.write_bit(0); // weighted_pred_flag
    w.write_bits(0, 2); // weighted_bipred_idc
    w.write_se(PIC_INIT_QP as i32 - 26); // pic_init_qp_minus26
    w.write_se(0); // pic_init_qs_minus26
    w.write_se(0); // chroma_qp_index_offset
    w.write_bit(1); // deblocking_filter_control_present_flag
    w.write_bit(0); // constrained_intra_pred_flag
    w.write_bit(0); // redundant_pic_cnt_present_flag
    w.write_bit(1); // rbsp_stop_one_bit (rbsp_trailing_bits - PPS is a complete NAL)

    let nal_header_byte = (3u8 << 5) | 8; // nal_ref_idc=3, nal_unit_type=8 (PPS)
    finish_standalone_nal(nal_header_byte, &w.into_bytes())
}

/// Shared tail end for `pack_sps`/`pack_pps`: prepends the NAL header byte,
/// applies emulation prevention, and prefixes the `00 00 01` start code.
/// Unlike `pack_slice_header`, these NALs are complete and self-terminating
/// (already ending on a byte boundary via `rbsp_trailing_bits`), so the full
/// byte length is the meaningful bit length - nothing else gets appended
/// after them by the driver.
fn finish_standalone_nal(nal_header_byte: u8, rbsp_payload: &[u8]) -> (Vec<u8>, usize) {
    let mut rbsp = Vec::with_capacity(1 + rbsp_payload.len());
    rbsp.push(nal_header_byte);
    rbsp.extend_from_slice(rbsp_payload);
    let rbsp_with_epb = add_emulation_prevention(&rbsp);

    let mut packed = Vec::with_capacity(3 + rbsp_with_epb.len());
    packed.extend_from_slice(&[0x00, 0x00, 0x01]);
    packed.extend_from_slice(&rbsp_with_epb);

    let bit_length = packed.len() * 8;
    (packed, bit_length)
}

/// Inserts `emulation_prevention_three_byte` (0x03) after any run of two
/// 0x00 bytes immediately followed by a byte <= 0x03, per H.264 7.4.1.1 -
/// without this, such a run inside RBSP data would be indistinguishable
/// from a start code.
fn add_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 3 + 1);
    let mut zero_run = 0u32;
    for &b in data {
        if zero_run >= 2 && b <= 3 {
            out.push(0x03);
            zero_run = 0;
        }
        out.push(b);
        zero_run = if b == 0 { zero_run + 1 } else { 0 };
    }
    out
}

/// Minimal MSB-first bit writer for exp-golomb-coded H.264 syntax elements.
struct BitWriter {
    bytes: Vec<u8>,
    cur: u8,
    bit_pos: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self { bytes: Vec::new(), cur: 0, bit_pos: 0 }
    }

    fn write_bit(&mut self, bit: u32) {
        self.cur = (self.cur << 1) | (bit as u8 & 1);
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bytes.push(self.cur);
            self.cur = 0;
            self.bit_pos = 0;
        }
    }

    fn write_bits(&mut self, value: u32, num_bits: u32) {
        for i in (0..num_bits).rev() {
            self.write_bit((value >> i) & 1);
        }
    }

    /// `ue(v)`: unsigned exp-golomb.
    fn write_ue(&mut self, value: u32) {
        let coded = value + 1;
        let num_bits = 32 - coded.leading_zeros();
        for _ in 0..num_bits - 1 {
            self.write_bit(0);
        }
        self.write_bits(coded, num_bits);
    }

    /// `se(v)`: signed exp-golomb (H.264 9.1.1's mapping to `ue(v)`).
    fn write_se(&mut self, value: i32) {
        let code_num = if value <= 0 { (-value) as u32 * 2 } else { value as u32 * 2 - 1 };
        self.write_ue(code_num);
    }

    fn bit_len(&self) -> usize {
        self.bytes.len() * 8 + self.bit_pos as usize
    }

    fn into_bytes(mut self) -> Vec<u8> {
        if self.bit_pos > 0 {
            self.cur <<= 8 - self.bit_pos;
            self.bytes.push(self.cur);
        }
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitwriter_write_ue_matches_exp_golomb() {
        let mut w = BitWriter::new();
        w.write_ue(0);
        assert_eq!(w.bit_len(), 1);
        let mut w = BitWriter::new();
        w.write_ue(1);
        assert_eq!(w.bit_len(), 3);
        let mut w = BitWriter::new();
        w.write_ue(5);
        assert_eq!(w.bit_len(), 5);
    }

    #[test]
    fn bitwriter_write_se_matches_signed_mapping() {
        // se(v) code_num mapping: 0->0, 1->1, -1->2, 2->3, -2->4 ...
        let mut zero = BitWriter::new();
        zero.write_se(0);
        let mut one = BitWriter::new();
        one.write_ue(0);
        assert_eq!(zero.into_bytes(), one.into_bytes());

        let mut neg_one = BitWriter::new();
        neg_one.write_se(-1);
        let mut code_num_two = BitWriter::new();
        code_num_two.write_ue(2);
        assert_eq!(neg_one.into_bytes(), code_num_two.into_bytes());
    }

    #[test]
    fn bitwriter_pads_final_byte_with_zero_bits() {
        let mut w = BitWriter::new();
        w.write_bits(0b101, 3);
        let bytes = w.into_bytes();
        assert_eq!(bytes, vec![0b1010_0000]);
    }

    #[test]
    fn emulation_prevention_inserts_after_two_zero_bytes() {
        assert_eq!(add_emulation_prevention(&[0x00, 0x00, 0x00]), vec![0x00, 0x00, 0x03, 0x00]);
        assert_eq!(add_emulation_prevention(&[0x00, 0x00, 0x01]), vec![0x00, 0x00, 0x03, 0x01]);
        assert_eq!(add_emulation_prevention(&[0x00, 0x00, 0x03]), vec![0x00, 0x00, 0x03, 0x03]);
    }

    #[test]
    fn emulation_prevention_leaves_non_start_code_data_unchanged() {
        assert_eq!(add_emulation_prevention(&[0x00, 0x00, 0x04]), vec![0x00, 0x00, 0x04]);
        assert_eq!(add_emulation_prevention(&[0x01, 0x02, 0x03]), vec![0x01, 0x02, 0x03]);
    }

    #[test]
    fn pack_slice_header_idr_starts_with_start_code_and_nal_header() {
        let (packed, bit_len) = pack_slice_header(PackedSliceHeaderArgs {
            is_idr: true,
            slice_type: 2,
            frame_num: 0,
            idr_pic_id: 0,
            pic_order_cnt_lsb: 0,
            nal_ref_idc: 3,
            nal_unit_type: 5,
        });
        // 00 00 01 (start code) + 0x65 (nal_ref_idc=3, nal_unit_type=5).
        assert_eq!(&packed[..4], &[0x00, 0x00, 0x01, 0x65]);
        assert!(bit_len > 32, "expected header bits beyond the start code + NAL header");
        assert!(bit_len <= packed.len() * 8);
    }

    #[test]
    fn pack_slice_header_p_uses_non_idr_nal_type() {
        let (packed, _) = pack_slice_header(PackedSliceHeaderArgs {
            is_idr: false,
            slice_type: 0,
            frame_num: 1,
            idr_pic_id: 0,
            pic_order_cnt_lsb: 1,
            nal_ref_idc: 2,
            nal_unit_type: 1,
        });
        // 00 00 01 (start code) + 0x41 (nal_ref_idc=2, nal_unit_type=1).
        assert_eq!(&packed[..4], &[0x00, 0x00, 0x01, 0x41]);
    }

    #[test]
    fn pack_sps_starts_with_start_code_profile_and_level() {
        let (packed, bit_len) = pack_sps(120, 68);
        // 00 00 01 (start code) + 0x67 (nal_ref_idc=3, nal_unit_type=7) +
        // profile_idc (0x42) + constraint flags (0xE0) + level_idc (0x28 = 40).
        assert_eq!(&packed[..7], &[0x00, 0x00, 0x01, 0x67, 0x42, 0xE0, 0x28]);
        // A complete, self-terminating NAL: bit_length is always a whole
        // number of bytes (ends with rbsp_trailing_bits).
        assert_eq!(bit_len % 8, 0);
        assert_eq!(bit_len, packed.len() * 8);
    }

    #[test]
    fn pack_pps_starts_with_start_code_and_nal_header() {
        let (packed, bit_len) = pack_pps();
        // 00 00 01 (start code) + 0x68 (nal_ref_idc=3, nal_unit_type=8).
        assert_eq!(&packed[..4], &[0x00, 0x00, 0x01, 0x68]);
        assert_eq!(bit_len, packed.len() * 8);
    }
}
