// Ported from cros-libva's lib/src/buffer/h264.rs, trimmed to the encode-side structs only
// (this crate has no H.264 decode support): `EncSequenceParameterBufferH264`,
// `H264EncSeqFields`, `EncPictureParameterBufferH264`, `H264EncPicFields`,
// `EncSliceParameterBufferH264`, and `PictureH264`. All decode-only types
// (`PictureParameterBufferH264`, `H264SeqFields`, `H264PicFields`, `SliceParameterBufferH264`,
// `IQMatrixBufferH264`) and the per-macroblock encode types (`EncMacroblockParameterBufferH264`,
// `H264EncMacroblockInfo`) are dropped.
//
// `H264VuiFields` and `H264EncFrameCropOffsets` are also ported even though the task brief's
// explicit list didn't name them: `EncSequenceParameterBufferH264::new`'s signature takes
// `Option<H264VuiFields>` and `Option<H264EncFrameCropOffsets>` in cros-libva, so they're
// necessary supporting types to port the constructor faithfully rather than a scope addition.
//
// `PictureH264::invalid()` is a new helper *not* present in cros-libva (checked: no such helper
// exists there). It's added here because the task brief calls for it, to build placeholder
// reference-list entries using `VA_PICTURE_H264_INVALID`/`VA_INVALID_ID`.

use crate::bindings;

/// Wrapper over the `VAPictureH264` FFI type.
pub struct PictureH264(bindings::VAPictureH264);

impl PictureH264 {
    /// Creates the wrapper
    pub fn new(
        picture_id: bindings::VASurfaceID,
        frame_idx: u32,
        flags: u32,
        top_field_order_cnt: i32,
        bottom_field_order_cnt: i32,
    ) -> Self {
        Self(bindings::VAPictureH264 {
            picture_id,
            frame_idx,
            flags,
            TopFieldOrderCnt: top_field_order_cnt,
            BottomFieldOrderCnt: bottom_field_order_cnt,
            va_reserved: Default::default(),
        })
    }

    /// Creates an invalid/placeholder `PictureH264`, for reference-list slots that aren't in use
    /// (e.g. padding out `ref_pic_list_0`/`ref_pic_list_1` past the number of active references).
    ///
    /// Not present in cros-libva; added here per the task brief.
    pub fn invalid() -> Self {
        Self(bindings::VAPictureH264 {
            picture_id: bindings::VA_INVALID_ID,
            frame_idx: 0,
            flags: bindings::VA_PICTURE_H264_INVALID,
            TopFieldOrderCnt: 0,
            BottomFieldOrderCnt: 0,
            va_reserved: Default::default(),
        })
    }
}

/// Wrapper over the `seq_fields` bindgen field in `VAEncSequenceParameterBufferH264`
pub struct H264EncSeqFields(bindings::_VAEncSequenceParameterBufferH264__bindgen_ty_1);

impl H264EncSeqFields {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chroma_format_idc: u32,
        frame_mbs_only_flag: u32,
        mb_adaptive_frame_field_flag: u32,
        seq_scaling_matrix_present_flag: u32,
        direct_8x8_inference_flag: u32,
        log2_max_frame_num_minus4: u32,
        pic_order_cnt_type: u32,
        log2_max_pic_order_cnt_lsb_minus4: u32,
        delta_pic_order_always_zero_flag: u32,
    ) -> Self {
        let _bitfield_1 =
            bindings::_VAEncSequenceParameterBufferH264__bindgen_ty_1__bindgen_ty_1::new_bitfield_1(
                chroma_format_idc,
                frame_mbs_only_flag,
                mb_adaptive_frame_field_flag,
                seq_scaling_matrix_present_flag,
                direct_8x8_inference_flag,
                log2_max_frame_num_minus4,
                pic_order_cnt_type,
                log2_max_pic_order_cnt_lsb_minus4,
                delta_pic_order_always_zero_flag,
            );

        Self(bindings::_VAEncSequenceParameterBufferH264__bindgen_ty_1 {
            bits: bindings::_VAEncSequenceParameterBufferH264__bindgen_ty_1__bindgen_ty_1 {
                _bitfield_align_1: Default::default(),
                _bitfield_1,
                __bindgen_padding_0: Default::default(),
            },
        })
    }

    pub fn inner(&self) -> &bindings::_VAEncSequenceParameterBufferH264__bindgen_ty_1 {
        &self.0
    }
}

/// Wrapper over the `vui_fields` bindgen field in `VAEncSequenceParameterBufferH264`.
#[derive(Default)]
pub struct H264VuiFields(bindings::_VAEncSequenceParameterBufferH264__bindgen_ty_2);

impl H264VuiFields {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        aspect_ratio_info_present_flag: u32,
        timing_info_present_flag: u32,
        bitstream_restriction_flag: u32,
        log2_max_mv_length_horizontal: u32,
        log2_max_mv_length_vertical: u32,
        fixed_frame_rate_flag: u32,
        low_delay_hrd_flag: u32,
        motion_vectors_over_pic_boundaries_flag: u32,
    ) -> Self {
        let _bitfield_1 =
            bindings::_VAEncSequenceParameterBufferH264__bindgen_ty_2__bindgen_ty_1::new_bitfield_1(
                aspect_ratio_info_present_flag,
                timing_info_present_flag,
                bitstream_restriction_flag,
                log2_max_mv_length_horizontal,
                log2_max_mv_length_vertical,
                fixed_frame_rate_flag,
                low_delay_hrd_flag,
                motion_vectors_over_pic_boundaries_flag,
                Default::default(),
            );

        Self(bindings::_VAEncSequenceParameterBufferH264__bindgen_ty_2 {
            bits: bindings::_VAEncSequenceParameterBufferH264__bindgen_ty_2__bindgen_ty_1 {
                _bitfield_align_1: Default::default(),
                _bitfield_1,
            },
        })
    }
}

/// Frame-cropping offsets, for when `picture_width_in_mbs`/`picture_height_in_mbs` (always
/// 16-pixel-aligned) overshoot the actual coded resolution.
#[derive(Default)]
pub struct H264EncFrameCropOffsets {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

impl H264EncFrameCropOffsets {
    pub fn new(
        frame_crop_left_offset: u32,
        frame_crop_right_offset: u32,
        frame_crop_top_offset: u32,
        frame_crop_bottom_offset: u32,
    ) -> Self {
        Self {
            left: frame_crop_left_offset,
            right: frame_crop_right_offset,
            top: frame_crop_top_offset,
            bottom: frame_crop_bottom_offset,
        }
    }
}

/// Wrapper over the `VAEncSequenceParameterBufferH264` FFI type.
pub struct EncSequenceParameterBufferH264(Box<bindings::VAEncSequenceParameterBufferH264>);

impl EncSequenceParameterBufferH264 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        seq_parameter_set_id: u8,
        level_idc: u8,
        intra_period: u32,
        intra_idr_period: u32,
        ip_period: u32,
        bits_per_second: u32,
        max_num_ref_frames: u32,
        picture_width_in_mbs: u16,
        picture_height_in_mbs: u16,
        seq_fields: &H264EncSeqFields,
        bit_depth_luma_minus8: u8,
        bit_depth_chroma_minus8: u8,
        num_ref_frames_in_pic_order_cnt_cycle: u8,
        offset_for_non_ref_pic: i32,
        offset_for_top_to_bottom_field: i32,
        offset_for_ref_frame: [i32; 256usize],
        frame_crop: Option<H264EncFrameCropOffsets>,
        vui_fields: Option<H264VuiFields>,
        aspect_ratio_idc: u8,
        sar_width: u32,
        sar_height: u32,
        num_units_in_tick: u32,
        time_scale: u32,
    ) -> Self {
        let seq_fields = seq_fields.0;

        let frame_cropping_flag = if frame_crop.is_some() { 1 } else { 0 };
        let frame_crop = frame_crop.unwrap_or_default();

        let frame_crop_left_offset = frame_crop.left;
        let frame_crop_right_offset = frame_crop.right;
        let frame_crop_top_offset = frame_crop.top;
        let frame_crop_bottom_offset = frame_crop.bottom;

        let vui_parameters_present_flag = if vui_fields.is_some() { 1 } else { 0 };
        let vui_fields = vui_fields.unwrap_or_default().0;

        Self(Box::new(bindings::VAEncSequenceParameterBufferH264 {
            seq_parameter_set_id,
            level_idc,
            intra_period,
            intra_idr_period,
            ip_period,
            bits_per_second,
            max_num_ref_frames,
            picture_width_in_mbs,
            picture_height_in_mbs,
            seq_fields,
            bit_depth_luma_minus8,
            bit_depth_chroma_minus8,
            num_ref_frames_in_pic_order_cnt_cycle,
            offset_for_non_ref_pic,
            offset_for_top_to_bottom_field,
            offset_for_ref_frame,
            frame_cropping_flag,
            frame_crop_left_offset,
            frame_crop_right_offset,
            frame_crop_top_offset,
            frame_crop_bottom_offset,
            vui_parameters_present_flag,
            vui_fields,
            aspect_ratio_idc,
            sar_width,
            sar_height,
            num_units_in_tick,
            time_scale,
            ..Default::default()
        }))
    }

    pub(crate) fn inner_mut(&mut self) -> &mut bindings::VAEncSequenceParameterBufferH264 {
        self.0.as_mut()
    }

    /// Returns the inner FFI type. Useful for testing purposes.
    pub fn inner(&self) -> &bindings::VAEncSequenceParameterBufferH264 {
        self.0.as_ref()
    }
}

/// Wrapper over the `pic_fields` bindgen field in `VAEncPictureParameterBufferH264`.
pub struct H264EncPicFields(bindings::_VAEncPictureParameterBufferH264__bindgen_ty_1);

impl H264EncPicFields {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        idr_pic_flag: u32,
        reference_pic_flag: u32,
        entropy_coding_mode_flag: u32,
        weighted_pred_flag: u32,
        weighted_bipred_idc: u32,
        constrained_intra_pred_flag: u32,
        transform_8x8_mode_flag: u32,
        deblocking_filter_control_present_flag: u32,
        redundant_pic_cnt_present_flag: u32,
        pic_order_present_flag: u32,
        pic_scaling_matrix_present_flag: u32,
    ) -> Self {
        let _bitfield_1 =
            bindings::_VAEncPictureParameterBufferH264__bindgen_ty_1__bindgen_ty_1::new_bitfield_1(
                idr_pic_flag,
                reference_pic_flag,
                entropy_coding_mode_flag,
                weighted_pred_flag,
                weighted_bipred_idc,
                constrained_intra_pred_flag,
                transform_8x8_mode_flag,
                deblocking_filter_control_present_flag,
                redundant_pic_cnt_present_flag,
                pic_order_present_flag,
                pic_scaling_matrix_present_flag,
            );

        Self(bindings::_VAEncPictureParameterBufferH264__bindgen_ty_1 {
            bits: bindings::_VAEncPictureParameterBufferH264__bindgen_ty_1__bindgen_ty_1 {
                _bitfield_align_1: Default::default(),
                _bitfield_1,
                __bindgen_padding_0: Default::default(),
            },
        })
    }
}

/// Wrapper over the `VAEncPictureParameterBufferH264` FFI type.
pub struct EncPictureParameterBufferH264(Box<bindings::VAEncPictureParameterBufferH264>);

impl EncPictureParameterBufferH264 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        curr_pic: PictureH264,
        reference_frames: [PictureH264; 16usize],
        coded_buf: bindings::VABufferID,
        pic_parameter_set_id: u8,
        seq_parameter_set_id: u8,
        last_picture: u8,
        frame_num: u16,
        pic_init_qp: u8,
        num_ref_idx_l0_active_minus1: u8,
        num_ref_idx_l1_active_minus1: u8,
        chroma_qp_index_offset: i8,
        second_chroma_qp_index_offset: i8,
        pic_fields: &H264EncPicFields,
    ) -> Self {
        let reference_frames = (0..16usize)
            .map(|i| reference_frames[i].0)
            .collect::<Vec<_>>()
            .try_into()
            // try_into is guaranteed to work because the iterator and target array have the same
            // size.
            .unwrap();

        let pic_fields = pic_fields.0;

        Self(Box::new(bindings::VAEncPictureParameterBufferH264 {
            CurrPic: curr_pic.0,
            ReferenceFrames: reference_frames,
            coded_buf,
            pic_parameter_set_id,
            seq_parameter_set_id,
            last_picture,
            frame_num,
            pic_init_qp,
            num_ref_idx_l0_active_minus1,
            num_ref_idx_l1_active_minus1,
            chroma_qp_index_offset,
            second_chroma_qp_index_offset,
            pic_fields,
            ..Default::default()
        }))
    }

    pub(crate) fn inner_mut(&mut self) -> &mut bindings::VAEncPictureParameterBufferH264 {
        self.0.as_mut()
    }

    /// Returns the inner FFI type. Useful for testing purposes.
    pub fn inner(&self) -> &bindings::VAEncPictureParameterBufferH264 {
        self.0.as_ref()
    }
}

/// Wrapper over the `VAEncSliceParameterBufferH264` FFI type.
pub struct EncSliceParameterBufferH264(Box<bindings::VAEncSliceParameterBufferH264>);

impl EncSliceParameterBufferH264 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        macroblock_address: u32,
        num_macroblocks: u32,
        macroblock_info: bindings::VABufferID,
        slice_type: u8,
        pic_parameter_set_id: u8,
        idr_pic_id: u16,
        pic_order_cnt_lsb: u16,
        delta_pic_order_cnt_bottom: i32,
        delta_pic_order_cnt: [i32; 2usize],
        direct_spatial_mv_pred_flag: u8,
        num_ref_idx_active_override_flag: u8,
        num_ref_idx_l0_active_minus1: u8,
        num_ref_idx_l1_active_minus1: u8,
        ref_pic_list_0: [PictureH264; 32usize],
        ref_pic_list_1: [PictureH264; 32usize],
        luma_log2_weight_denom: u8,
        chroma_log2_weight_denom: u8,
        luma_weight_l0_flag: u8,
        luma_weight_l0: [i16; 32usize],
        luma_offset_l0: [i16; 32usize],
        chroma_weight_l0_flag: u8,
        chroma_weight_l0: [[i16; 2usize]; 32usize],
        chroma_offset_l0: [[i16; 2usize]; 32usize],
        luma_weight_l1_flag: u8,
        luma_weight_l1: [i16; 32usize],
        luma_offset_l1: [i16; 32usize],
        chroma_weight_l1_flag: u8,
        chroma_weight_l1: [[i16; 2usize]; 32usize],
        chroma_offset_l1: [[i16; 2usize]; 32usize],
        cabac_init_idc: u8,
        slice_qp_delta: i8,
        disable_deblocking_filter_idc: u8,
        slice_alpha_c0_offset_div2: i8,
        slice_beta_offset_div2: i8,
    ) -> Self {
        let ref_pic_list_0 = ref_pic_list_0.map(|pic| pic.0);
        let ref_pic_list_1 = ref_pic_list_1.map(|pic| pic.0);

        Self(Box::new(bindings::VAEncSliceParameterBufferH264 {
            macroblock_address,
            num_macroblocks,
            macroblock_info,
            slice_type,
            pic_parameter_set_id,
            idr_pic_id,
            pic_order_cnt_lsb,
            delta_pic_order_cnt_bottom,
            delta_pic_order_cnt,
            direct_spatial_mv_pred_flag,
            num_ref_idx_active_override_flag,
            num_ref_idx_l0_active_minus1,
            num_ref_idx_l1_active_minus1,
            RefPicList0: ref_pic_list_0,
            RefPicList1: ref_pic_list_1,
            luma_log2_weight_denom,
            chroma_log2_weight_denom,
            luma_weight_l0_flag,
            luma_weight_l0,
            luma_offset_l0,
            chroma_weight_l0_flag,
            chroma_weight_l0,
            chroma_offset_l0,
            luma_weight_l1_flag,
            luma_weight_l1,
            luma_offset_l1,
            chroma_weight_l1_flag,
            chroma_weight_l1,
            chroma_offset_l1,
            cabac_init_idc,
            slice_qp_delta,
            disable_deblocking_filter_idc,
            slice_alpha_c0_offset_div2,
            slice_beta_offset_div2,
            ..Default::default()
        }))
    }

    pub(crate) fn inner_mut(&mut self) -> &mut bindings::VAEncSliceParameterBufferH264 {
        self.0.as_mut()
    }

    /// Returns the inner FFI type. Useful for testing purposes.
    pub fn inner(&self) -> &bindings::VAEncSliceParameterBufferH264 {
        self.0.as_ref()
    }
}
