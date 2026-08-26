// Ported from cros-libva's lib/src/buffer/proc_pipeline.rs, trimmed to `ProcPipelineParameterBuffer`
// and `ProcColorProperties`. `BlendState` and `HdrMetaData` are dropped since nothing in this
// crate's simplified `ProcPipelineParameterBuffer::new` constructor (below) ever sets
// `blend_state`/`output_hdr_metadata` to anything other than `None`.
//
// Deviation from a straight port, per the task brief: cros-libva's `ProcPipelineParameterBuffer::new`
// takes 20 positional arguments. In this crate's intended usage (YUYV surface -> NV12 surface VPP
// conversion, nothing else) only the input surface and the output region ever vary; everything
// else is always 0/None/default. So the public constructor here only exposes `surface` and
// `output_region`, defaulting every other field the same way cros-libva's callers already do
// (verified against actual simple_kvm callers isn't possible from this crate alone, so this
// mirrors the "always 0/None/default" pattern the task brief calls out). The underlying
// `c_params`/owned-pointee-storage struct shape is otherwise unchanged, so a fuller constructor
// (or direct field access via `inner_mut`) can still be added later without restructuring.
use crate::bindings;
use std::ptr;

/// Wrapper over the `VAProcColorProperties` ffi type.
pub struct ProcColorProperties(bindings::VAProcColorProperties);

impl ProcColorProperties {
    /// Creates the bindgen field
    pub fn new(
        chroma_sample_location: u8,
        color_range: u8,
        colour_primaries: u8,
        transfer_characteristics: u8,
        matrix_coefficients: u8,
    ) -> Self {
        Self(bindings::VAProcColorProperties {
            chroma_sample_location,
            color_range,
            colour_primaries,
            transfer_characteristics,
            matrix_coefficients,
            reserved: Default::default(),
        })
    }
}

impl Default for ProcColorProperties {
    fn default() -> Self {
        Self::new(0, 0, 0, 0, 0)
    }
}

/// Wrapper over the `VAProcPipelineParameterBuffer` FFI type.
pub struct ProcPipelineParameterBuffer {
    c_params: Box<bindings::VAProcPipelineParameterBuffer>,

    // Fields that own the data for the pointers in `c_params`.
    surface_region: Option<Box<bindings::VARectangle>>,
    output_region: Option<Box<bindings::VARectangle>>,
    filters: Option<Vec<bindings::VABufferID>>,
    forward_references: Option<Vec<bindings::VASurfaceID>>,
    backward_references: Option<Vec<bindings::VASurfaceID>>,
    additional_outputs: Option<Vec<bindings::VASurfaceID>>,
}

impl ProcPipelineParameterBuffer {
    /// Creates a `VAProcPipelineParameterBuffer` for a straightforward one-surface-in,
    /// one-region-out VPP conversion (e.g. YUYV -> NV12): `surface` is the only input, and
    /// `output_region` (if set) crops/scales into the output surface. Every other field cros-libva
    /// exposes here (surface region, color standards, filters, reference lists, rotation/blend/
    /// mirror state, additional outputs, color properties, HDR metadata, pipeline flags) is always
    /// its zero/`None`/default value, matching how simple_kvm's callers already use them.
    pub fn new(surface: bindings::VASurfaceID, output_region: Option<bindings::VARectangle>) -> Self {
        let mut slf = Self {
            // SAFETY: The VA-API structures are C-compatible so zeroing is safe.
            c_params: Box::new(unsafe { std::mem::zeroed() }),
            surface_region: None,
            output_region: output_region.map(Box::new),
            filters: None,
            forward_references: None,
            backward_references: None,
            additional_outputs: None,
        };

        slf.c_params = Box::new(bindings::VAProcPipelineParameterBuffer {
            surface,
            surface_region: slf
                .surface_region
                .as_deref()
                .map_or(ptr::null_mut(), |r| r as *const _ as *mut _),
            surface_color_standard: 0,
            output_region: slf
                .output_region
                .as_deref()
                .map_or(ptr::null_mut(), |r| r as *const _ as *mut _),
            output_background_color: 0,
            output_color_standard: 0,
            pipeline_flags: 0,
            filter_flags: 0,
            filters: slf
                .filters
                .as_deref()
                .map_or(ptr::null_mut(), |f| f.as_ptr() as *mut _),
            num_filters: slf.filters.as_ref().map_or(0, |f| f.len() as u32),
            forward_references: slf
                .forward_references
                .as_deref()
                .map_or(ptr::null_mut(), |r| r.as_ptr() as *mut _),
            num_forward_references: slf
                .forward_references
                .as_ref()
                .map_or(0, |f| f.len() as u32),
            backward_references: slf
                .backward_references
                .as_deref()
                .map_or(ptr::null_mut(), |r| r.as_ptr() as *mut _),
            num_backward_references: slf
                .backward_references
                .as_ref()
                .map_or(0, |b| b.len() as u32),
            rotation_state: 0,
            blend_state: ptr::null(),
            mirror_state: 0,
            additional_outputs: slf
                .additional_outputs
                .as_deref()
                .map_or(ptr::null_mut(), |a| a.as_ptr() as *mut _),
            num_additional_outputs: slf
                .additional_outputs
                .as_ref()
                .map_or(0, |a| a.len() as u32),
            input_surface_flag: 0,
            output_surface_flag: 0,
            input_color_properties: ProcColorProperties::default().0,
            output_color_properties: ProcColorProperties::default().0,
            processing_mode: 0,
            output_hdr_metadata: ptr::null_mut(),
            va_reserved: Default::default(),
        });

        slf
    }

    pub(crate) fn inner_mut(&mut self) -> &mut bindings::VAProcPipelineParameterBuffer {
        self.c_params.as_mut()
    }

    /// Returns the inner FFI type. Useful for testing purposes.
    pub fn inner(&self) -> &bindings::VAProcPipelineParameterBuffer {
        self.c_params.as_ref()
    }
}
