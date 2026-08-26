// Ported from cros-libva's lib/src/buffer/enc_misc.rs, trimmed to only what's used: the generic
// `MiscEncParamBuffer<T>` plumbing, `EncMiscParameterRateControl` + `RcFlags`, and
// `EncMiscParameterFrameRate`. `EncMiscParameterMaxSliceSize`, `EncMiscParameterBufferMaxFrameSize`,
// `EncMiscParameterSkipFrame`, `EncMiscParameterHRD`, `EncMiscParameterBufferQualityLevel`, and
// `EncMiscParameterQuantization` are dropped, per the task brief.

use crate::bindings;

#[repr(C)]
#[derive(Default)]
pub struct MiscEncParamBuffer<T> {
    hdr: bindings::VAEncMiscParameterBuffer,
    value: T,
}

impl<T> MiscEncParamBuffer<T> {
    fn new(type_: bindings::VAEncMiscParameterType::Type, value: T) -> Self {
        Self {
            hdr: bindings::_VAEncMiscParameterBuffer {
                type_,
                data: Default::default(),
            },
            value,
        }
    }

    fn new_boxed(type_: bindings::VAEncMiscParameterType::Type, value: T) -> Box<Self> {
        Box::new(Self::new(type_, value))
    }
}

#[derive(Default)]
pub struct EncMiscParameterFrameRate(
    Box<MiscEncParamBuffer<bindings::VAEncMiscParameterFrameRate>>,
);

impl EncMiscParameterFrameRate {
    pub fn new(framerate: u32, temporal_id: u32) -> Self {
        let _bitfield_1 =
            bindings::_VAEncMiscParameterFrameRate__bindgen_ty_1__bindgen_ty_1::new_bitfield_1(
                temporal_id,
                Default::default(),
            );

        Self(MiscEncParamBuffer::new_boxed(
            bindings::VAEncMiscParameterType::VAEncMiscParameterTypeFrameRate,
            bindings::_VAEncMiscParameterFrameRate {
                framerate,
                framerate_flags: bindings::_VAEncMiscParameterFrameRate__bindgen_ty_1 {
                    bits: bindings::_VAEncMiscParameterFrameRate__bindgen_ty_1__bindgen_ty_1 {
                        _bitfield_align_1: Default::default(),
                        _bitfield_1,
                    },
                },
                ..Default::default()
            },
        ))
    }

    pub fn inner(&self) -> &MiscEncParamBuffer<bindings::VAEncMiscParameterFrameRate> {
        &self.0
    }

    pub(crate) fn inner_mut(
        &mut self,
    ) -> &mut MiscEncParamBuffer<bindings::VAEncMiscParameterFrameRate> {
        &mut self.0
    }
}

#[derive(Default)]
pub struct RcFlags(bindings::_VAEncMiscParameterRateControl__bindgen_ty_1);

impl RcFlags {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        reset: u32,
        disable_frame_skip: u32,
        disable_bit_stuffing: u32,
        mb_rate_control: u32,
        temporal_id: u32,
        cfs_i_frames: u32,
        enable_parallel_brc: u32,
        enable_dynamic_scaling: u32,
        frame_tolerance_mode: u32,
    ) -> Self {
        let _bitfield_1 =
            bindings::_VAEncMiscParameterRateControl__bindgen_ty_1__bindgen_ty_1::new_bitfield_1(
                reset,
                disable_frame_skip,
                disable_bit_stuffing,
                mb_rate_control,
                temporal_id,
                cfs_i_frames,
                enable_parallel_brc,
                enable_dynamic_scaling,
                frame_tolerance_mode,
                Default::default(),
            );

        Self(bindings::_VAEncMiscParameterRateControl__bindgen_ty_1 {
            bits: bindings::_VAEncMiscParameterRateControl__bindgen_ty_1__bindgen_ty_1 {
                _bitfield_align_1: Default::default(),
                _bitfield_1,
            },
        })
    }
}

#[derive(Default)]
pub struct EncMiscParameterRateControl(
    Box<MiscEncParamBuffer<bindings::VAEncMiscParameterRateControl>>,
);

impl EncMiscParameterRateControl {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bits_per_second: u32,
        target_percentage: u32,
        window_size: u32,
        initial_qp: u32,
        min_qp: u32,
        basic_unit_size: u32,
        rc_flags: RcFlags,
        icq_quality_factor: u32,
        max_qp: u32,
        quality_factor: u32,
        target_frame_size: u32,
    ) -> Self {
        Self(MiscEncParamBuffer::new_boxed(
            bindings::VAEncMiscParameterType::VAEncMiscParameterTypeRateControl,
            bindings::VAEncMiscParameterRateControl {
                bits_per_second,
                target_percentage,
                window_size,
                initial_qp,
                min_qp,
                basic_unit_size,
                rc_flags: rc_flags.0,
                ICQ_quality_factor: icq_quality_factor,
                max_qp,
                quality_factor,
                target_frame_size,
                ..Default::default()
            },
        ))
    }

    pub fn inner(&self) -> &MiscEncParamBuffer<bindings::VAEncMiscParameterRateControl> {
        &self.0
    }

    pub(crate) fn inner_mut(
        &mut self,
    ) -> &mut MiscEncParamBuffer<bindings::VAEncMiscParameterRateControl> {
        &mut self.0
    }
}
