//! Sanity check for the H.264-encode and VPP `BufferType` wrappers added on top of the crate's
//! core plumbing (h264.rs, proc_pipeline.rs, enc_misc.rs, and the new `BufferType` variants in
//! buffer.rs).
//!
//! Builds one instance of each new wrapper with realistic placeholder values, confirms
//! construction doesn't panic, and then (going beyond the task's minimum bar, since it wasn't much
//! extra effort) actually drives each one through `Context::create_buffer` against the real
//! driver, for both an H.264 encode context and a VPP context.

use libva_rs::{
    self as va, BufferType, Display, EncMiscParameter, EncMiscParameterFrameRate,
    EncMiscParameterRateControl, EncPictureParameterBufferH264, EncSequenceParameterBufferH264,
    EncSliceParameterBufferH264, H264EncPicFields, H264EncSeqFields, PictureH264,
    ProcPipelineParameterBuffer, RcFlags, UsageHint, VAConfigAttrib, VAConfigAttribType,
    VAEntrypoint, VAProfile, VARectangle, VA_RT_FORMAT_YUV420,
};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 64;

fn main() {
    println!("== libva-rs buffer sanity check ==");

    // --- 1. Construct every new wrapper type in isolation, with placeholder values. ---
    // This part never touches the driver, so it runs even without a usable device.

    let seq_fields = H264EncSeqFields::new(
        1, // chroma_format_idc (4:2:0)
        1, // frame_mbs_only_flag
        0, // mb_adaptive_frame_field_flag
        0, // seq_scaling_matrix_present_flag
        1, // direct_8x8_inference_flag
        4, // log2_max_frame_num_minus4
        0, // pic_order_cnt_type
        4, // log2_max_pic_order_cnt_lsb_minus4
        0, // delta_pic_order_always_zero_flag
    );
    let mbs_w = (WIDTH as u16).div_ceil(16);
    let mbs_h = (HEIGHT as u16).div_ceil(16);
    let seq_param = EncSequenceParameterBufferH264::new(
        0,     // seq_parameter_set_id
        30,    // level_idc (3.0)
        30,    // intra_period
        30,    // intra_idr_period
        1,     // ip_period
        2_000_000, // bits_per_second
        1,     // max_num_ref_frames
        mbs_w,
        mbs_h,
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
    );
    println!(
        "EncSequenceParameterBufferH264 built: picture_width_in_mbs={}",
        seq_param.inner().picture_width_in_mbs
    );

    let pic_fields = H264EncPicFields::new(
        1, // idr_pic_flag
        1, // reference_pic_flag
        0, // entropy_coding_mode_flag (CAVLC)
        0, // weighted_pred_flag
        0, // weighted_bipred_idc
        0, // constrained_intra_pred_flag
        0, // transform_8x8_mode_flag
        0, // deblocking_filter_control_present_flag
        0, // redundant_pic_cnt_present_flag
        0, // pic_order_present_flag
        0, // pic_scaling_matrix_present_flag
    );
    let curr_pic = PictureH264::new(0, 0, 0, 0, 0);
    let reference_frames: [PictureH264; 16] = std::array::from_fn(|_| PictureH264::invalid());
    let pic_param = EncPictureParameterBufferH264::new(
        curr_pic,
        reference_frames,
        va::VA_INVALID_ID, // coded_buf (placeholder; a real caller passes an EncCodedBuffer id)
        0,                 // pic_parameter_set_id
        0,                 // seq_parameter_set_id
        0,                 // last_picture
        0,                 // frame_num
        26,                // pic_init_qp
        0,                 // num_ref_idx_l0_active_minus1
        0,                 // num_ref_idx_l1_active_minus1
        0,                 // chroma_qp_index_offset
        0,                 // second_chroma_qp_index_offset
        &pic_fields,
    );
    println!(
        "EncPictureParameterBufferH264 built: pic_init_qp={}",
        pic_param.inner().pic_init_qp
    );

    let ref_pic_list_0: [PictureH264; 32] = std::array::from_fn(|_| PictureH264::invalid());
    let ref_pic_list_1: [PictureH264; 32] = std::array::from_fn(|_| PictureH264::invalid());
    let slice_param = EncSliceParameterBufferH264::new(
        0,                 // macroblock_address
        (mbs_w as u32) * (mbs_h as u32), // num_macroblocks
        va::VA_INVALID_ID, // macroblock_info
        2,                 // slice_type (I slice)
        0,                 // pic_parameter_set_id
        0,                 // idr_pic_id
        0,                 // pic_order_cnt_lsb
        0,                 // delta_pic_order_cnt_bottom
        [0i32; 2],         // delta_pic_order_cnt
        0,                 // direct_spatial_mv_pred_flag
        0,                 // num_ref_idx_active_override_flag
        0,                 // num_ref_idx_l0_active_minus1
        0,                 // num_ref_idx_l1_active_minus1
        ref_pic_list_0,
        ref_pic_list_1,
        0, // luma_log2_weight_denom
        0, // chroma_log2_weight_denom
        0, // luma_weight_l0_flag
        [0i16; 32],
        [0i16; 32],
        0, // chroma_weight_l0_flag
        [[0i16; 2]; 32],
        [[0i16; 2]; 32],
        0, // luma_weight_l1_flag
        [0i16; 32],
        [0i16; 32],
        0, // chroma_weight_l1_flag
        [[0i16; 2]; 32],
        [[0i16; 2]; 32],
        0,  // cabac_init_idc
        0,  // slice_qp_delta
        0,  // disable_deblocking_filter_idc
        0,  // slice_alpha_c0_offset_div2
        0,  // slice_beta_offset_div2
    );
    println!(
        "EncSliceParameterBufferH264 built: num_macroblocks={}",
        slice_param.inner().num_macroblocks
    );

    let rc_flags = RcFlags::new(0, 0, 0, 0, 0, 0, 0, 0, 0);
    let rc_param = EncMiscParameterRateControl::new(
        2_000_000, // bits_per_second
        100,       // target_percentage
        1000,      // window_size
        26,        // initial_qp
        1,         // min_qp
        0,         // basic_unit_size
        rc_flags,
        0, // icq_quality_factor
        51, // max_qp
        0,  // quality_factor
        0,  // target_frame_size
    );
    // `MiscEncParamBuffer`'s `hdr`/`value` fields are private (matching cros-libva), so there's
    // nothing further to inspect here beyond "it built and has the right size".
    println!(
        "EncMiscParameterRateControl built: size={} bytes",
        std::mem::size_of_val(rc_param.inner())
    );

    let fr_param = EncMiscParameterFrameRate::new(30, 0);
    println!(
        "EncMiscParameterFrameRate built: size={} bytes",
        std::mem::size_of_val(fr_param.inner())
    );

    let mut proc_pipeline = ProcPipelineParameterBuffer::new(
        va::VA_INVALID_ID, // surface (placeholder)
        Some(VARectangle {
            x: 0,
            y: 0,
            width: WIDTH as u16,
            height: HEIGHT as u16,
        }),
    );
    println!(
        "ProcPipelineParameterBuffer built: surface={}",
        proc_pipeline.inner().surface
    );

    println!("all wrapper types constructed without panicking");

    // --- 2. Nice-to-have: drive each one through Context::create_buffer against the real driver. ---

    let display = match Display::open() {
        Some(d) => d,
        None => {
            println!(
                "Display::open() found no usable DRM/VAAPI device; skipping the on-device \
                 create_buffer checks"
            );
            println!("== buffer sanity check PASSED (construction only) ==");
            return;
        }
    };

    // H.264 encode context.
    let h264_profile = VAProfile::VAProfileH264Main;
    let entrypoints = display
        .query_config_entrypoints(h264_profile)
        .expect("query_config_entrypoints(VAProfileH264Main) failed");
    let enc_entrypoint = if entrypoints.contains(&VAEntrypoint::VAEntrypointEncSliceLP) {
        VAEntrypoint::VAEntrypointEncSliceLP
    } else if entrypoints.contains(&VAEntrypoint::VAEntrypointEncSlice) {
        VAEntrypoint::VAEntrypointEncSlice
    } else {
        panic!("driver supports neither VAEntrypointEncSliceLP nor VAEntrypointEncSlice");
    };
    let mut enc_attrs = vec![VAConfigAttrib {
        type_: VAConfigAttribType::VAConfigAttribRTFormat,
        value: 0,
    }];
    display
        .get_config_attributes(h264_profile, enc_entrypoint, &mut enc_attrs)
        .expect("get_config_attributes (H.264 encode) failed");
    let enc_config = display
        .create_config(enc_attrs, h264_profile, enc_entrypoint)
        .expect("create_config (H.264 encode) failed");
    let enc_surfaces = display
        .create_surfaces(
            VA_RT_FORMAT_YUV420,
            None,
            WIDTH,
            HEIGHT,
            Some(UsageHint::USAGE_HINT_ENCODER),
            1,
        )
        .expect("create_surfaces (H.264 encode) failed");
    let enc_context = display
        .create_context(&enc_config, WIDTH, HEIGHT, Some(&enc_surfaces), true)
        .expect("create_context (H.264 encode) failed");

    let coded_buf = enc_context
        .create_enc_coded(WIDTH as usize * HEIGHT as usize * 3)
        .expect("create_enc_coded failed");

    // Rebuild pic_param with a real coded_buf id now that we have one.
    let curr_pic2 = PictureH264::new(enc_surfaces[0].id(), 0, 0, 0, 0);
    let reference_frames2: [PictureH264; 16] = std::array::from_fn(|_| PictureH264::invalid());
    let pic_param2 = EncPictureParameterBufferH264::new(
        curr_pic2,
        reference_frames2,
        coded_buf.id(),
        0,
        0,
        0,
        0,
        26,
        0,
        0,
        0,
        0,
        &pic_fields,
    );

    let _seq_buf = enc_context
        .create_buffer(BufferType::EncSequenceParameter(seq_param))
        .expect("create_buffer(EncSequenceParameter) failed");
    println!("Context::create_buffer(EncSequenceParameter) OK");

    let _pic_buf = enc_context
        .create_buffer(BufferType::EncPictureParameter(pic_param2))
        .expect("create_buffer(EncPictureParameter) failed");
    println!("Context::create_buffer(EncPictureParameter) OK");

    let _slice_buf = enc_context
        .create_buffer(BufferType::EncSliceParameter(slice_param))
        .expect("create_buffer(EncSliceParameter) failed");
    println!("Context::create_buffer(EncSliceParameter) OK");

    let _rc_buf = enc_context
        .create_buffer(BufferType::EncMiscParameter(EncMiscParameter::RateControl(
            rc_param,
        )))
        .expect("create_buffer(EncMiscParameter::RateControl) failed");
    println!("Context::create_buffer(EncMiscParameter::RateControl) OK");

    let _fr_buf = enc_context
        .create_buffer(BufferType::EncMiscParameter(EncMiscParameter::FrameRate(
            fr_param,
        )))
        .expect("create_buffer(EncMiscParameter::FrameRate) failed");
    println!("Context::create_buffer(EncMiscParameter::FrameRate) OK");

    drop(enc_context);
    drop(enc_surfaces);
    drop(enc_config);

    // VPP context, to exercise ProcPipelineParameter with a real surface id.
    let vpp_profile = VAProfile::VAProfileNone;
    let mut vpp_attrs = vec![VAConfigAttrib {
        type_: VAConfigAttribType::VAConfigAttribRTFormat,
        value: 0,
    }];
    display
        .get_config_attributes(
            vpp_profile,
            VAEntrypoint::VAEntrypointVideoProc,
            &mut vpp_attrs,
        )
        .expect("get_config_attributes (VPP) failed");
    let vpp_config = display
        .create_config(vpp_attrs, vpp_profile, VAEntrypoint::VAEntrypointVideoProc)
        .expect("create_config (VPP) failed");
    let vpp_surfaces = display
        .create_surfaces(
            VA_RT_FORMAT_YUV420,
            None,
            WIDTH,
            HEIGHT,
            Some(UsageHint::USAGE_HINT_VPP_WRITE),
            1,
        )
        .expect("create_surfaces (VPP) failed");
    let vpp_context = display
        .create_context(&vpp_config, WIDTH, HEIGHT, Some(&vpp_surfaces), true)
        .expect("create_context (VPP) failed");

    proc_pipeline = ProcPipelineParameterBuffer::new(vpp_surfaces[0].id(), None);
    let _proc_buf = vpp_context
        .create_buffer(BufferType::ProcPipelineParameter(proc_pipeline))
        .expect("create_buffer(ProcPipelineParameter) failed");
    println!("Context::create_buffer(ProcPipelineParameter) OK");

    drop(vpp_context);
    drop(vpp_surfaces);
    drop(vpp_config);

    println!("== buffer sanity check PASSED (construction + on-device create_buffer) ==");
}
