//! Wrappers and helpers around `VABuffer`s.
//!
//! This is a heavily trimmed port of cros-libva's lib/src/buffer.rs: the codec-agnostic pieces
//! (`Buffer`, `EncCodedBuffer`, `MappedCodedBuffer`/`MappedCodedSegment`,
//! `EncPackedHeaderParameter`/`EncPackedHeaderType`, and the `BufferType`/`BorrowedBufferType`
//! enum shell) plus the H.264-encode and VPP buffer-type wrappers needed by simple_kvm's GPU
//! encoding pipeline (`h264`, `proc_pipeline`, `enc_misc` submodules). cros-libva's other
//! per-codec submodules (`mpeg2`, `vp8`, `vp9`, `hevc`, `av1`, `jpeg_baseline`, `enc_jpeg`) are out
//! of scope for this crate: it only supports H.264 encode + VPP.
//!
//! Unlike cros-libva, which has one `BufferType` variant per buffer kind wrapping a per-codec enum
//! (e.g. `EncSequenceParameter(EncSequenceParameter)` where `EncSequenceParameter` has `H264`,
//! `HEVC`, `VP8`, ... variants), the `EncSequenceParameter`/`EncPictureParameter`/`EncSliceParameter`
//! variants here wrap the H.264 struct directly (`EncSequenceParameter(h264::EncSequenceParameterBufferH264)`)
//! since H.264 is the only codec this crate supports and a single-variant wrapper enum would add a
//! layer of indirection with no payoff. `EncMiscParameter` keeps its wrapper enum (`EncMiscParameter`
//! with `RateControl`/`FrameRate` variants) since that's a real sum type of distinct misc-parameter
//! kinds sharing one `VABufferType`, not a per-codec choice.

mod enc_misc;
mod h264;
mod proc_pipeline;

pub use enc_misc::*;
pub use h264::*;
pub use proc_pipeline::*;

use std::rc::Rc;

use log::error;

use crate::bindings;
use crate::va_check;
use crate::Context;
use crate::VaError;

/// Wrapper type representing a buffer created with `vaCreateBuffer`.
pub struct Buffer {
    context: Rc<Context>,
    id: bindings::VABufferID,
}

impl Buffer {
    /// Creates a new buffer by wrapping a `vaCreateBuffer` call. This is just a helper for
    /// [`Context::create_buffer`].
    pub(crate) fn new(context: Rc<Context>, mut type_: BufferType) -> Result<Self, VaError> {
        let mut buffer_id = 0;
        let nb_elements = 1;

        let (ptr, size) = match type_ {
            BufferType::SliceData(ref mut data) => {
                (data.as_mut_ptr() as *mut std::ffi::c_void, data.len())
            }

            BufferType::EncCodedBuffer(size) => (std::ptr::null_mut(), size),

            BufferType::EncPackedHeaderParameter(ref mut wrapper) => (
                wrapper.inner_mut() as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of_val(wrapper.inner_mut()),
            ),

            BufferType::EncPackedHeaderData(ref data) => {
                (data.as_ptr() as *mut std::ffi::c_void, data.len())
            }

            BufferType::EncSequenceParameter(ref mut wrapper) => (
                wrapper.inner_mut() as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of_val(wrapper.inner_mut()),
            ),

            BufferType::EncPictureParameter(ref mut wrapper) => (
                wrapper.inner_mut() as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of_val(wrapper.inner_mut()),
            ),

            BufferType::EncSliceParameter(ref mut wrapper) => (
                wrapper.inner_mut() as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of_val(wrapper.inner_mut()),
            ),

            BufferType::EncMiscParameter(ref mut enc_misc_param) => match enc_misc_param {
                EncMiscParameter::FrameRate(ref mut wrapper) => (
                    wrapper.inner_mut() as *mut _ as *mut std::ffi::c_void,
                    std::mem::size_of_val(wrapper.inner_mut()),
                ),
                EncMiscParameter::RateControl(ref mut wrapper) => (
                    wrapper.inner_mut() as *mut _ as *mut std::ffi::c_void,
                    std::mem::size_of_val(wrapper.inner_mut()),
                ),
            },

            BufferType::ProcPipelineParameter(ref mut proc_pipeline_param) => (
                proc_pipeline_param.inner_mut() as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of_val(proc_pipeline_param.inner_mut()),
            ),
        };

        // Safe because `context` represents a valid `VAContext`. `ptr` and `size` are also ensured
        // to be correct, as `ptr` is just a cast to `*c_void` from a Rust struct, and `size` is
        // computed from `std::mem::size_of_val`.
        va_check(unsafe {
            bindings::vaCreateBuffer(
                context.display().handle(),
                context.id(),
                type_.inner(),
                size as u32,
                nb_elements as u32,
                ptr,
                &mut buffer_id,
            )
        })?;

        Ok(Self {
            context,
            id: buffer_id,
        })
    }

    /// Creates a new buffer from a `BorrowedBufferType`.
    pub(crate) fn new_borrowed(
        context: Rc<Context>,
        type_: BorrowedBufferType,
    ) -> Result<Self, VaError> {
        let mut buffer_id = 0;
        let nb_elements = 1;

        let (ptr, size) = match type_ {
            BorrowedBufferType::SliceData(data) => {
                (data.as_ptr() as *mut std::ffi::c_void, data.len())
            }
            BorrowedBufferType::EncPackedHeaderData(data) => {
                (data.as_ptr() as *mut std::ffi::c_void, data.len())
            }
        };

        // Safe because `context` represents a valid `VAContext`. `ptr` and `size` are also ensured
        // to be correct.
        va_check(unsafe {
            bindings::vaCreateBuffer(
                context.display().handle(),
                context.id(),
                type_.inner(),
                size as u32,
                nb_elements as u32,
                ptr,
                &mut buffer_id,
            )
        })?;

        Ok(Self {
            context,
            id: buffer_id,
        })
    }

    /// Convenience function to return a `VABufferID` vector from a slice of `Buffer`s in order to
    /// easily interface with the C API where a buffer array might be needed.
    pub fn as_id_vec(buffers: &[Self]) -> Vec<bindings::VABufferID> {
        buffers.iter().map(|buffer| buffer.id).collect()
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // Safe because `self` represents a valid buffer, created with
        // vaCreateBuffers.
        let status = va_check(unsafe {
            bindings::vaDestroyBuffer(self.context.display().handle(), self.id)
        });

        if status.is_err() {
            error!("vaDestroyBuffer failed: {}", status.unwrap_err());
        }
    }
}

/// A borrowed version of `BufferType` that avoids copying data.
pub enum BorrowedBufferType<'a> {
    /// Abstraction over `VASliceDataBufferType` that uses borrowed data.
    SliceData(&'a [u8]),
    /// Abstraction over `VAEncPackedHeaderDataBufferType` that uses borrowed data.
    EncPackedHeaderData(&'a [u8]),
}

impl<'a> BorrowedBufferType<'a> {
    /// Returns the inner FFI buffer type.
    pub(crate) fn inner(&self) -> bindings::VABufferType::Type {
        match self {
            BorrowedBufferType::SliceData(_) => bindings::VABufferType::VASliceDataBufferType,
            BorrowedBufferType::EncPackedHeaderData(_) => {
                bindings::VABufferType::VAEncPackedHeaderDataBufferType
            }
        }
    }
}

/// Abstraction over `VABufferType`s.
///
/// This only covers the codec-agnostic buffer types. Codec-specific buffer types (H.264 encode
/// parameters, VPP pipeline parameters, etc.) are out of scope for this crate and are meant to be
/// added as further variants by whoever builds the codec-specific layer on top of this one.
pub enum BufferType {
    /// Abstraction over `VASliceDataBufferType`.
    SliceData(Vec<u8>),
    /// Abstraction over `VAEncCodedBufferType`.
    EncCodedBuffer(usize),
    /// Abstraction over `VAEncPackedHeaderParameterBufferType`.
    EncPackedHeaderParameter(EncPackedHeaderParameter),
    /// Abstraction over `VAEncPackedHeaderDataBufferType`.
    EncPackedHeaderData(Vec<u8>),
    /// Abstraction over `VAEncSequenceParameterBufferType`. H.264 only (this crate's only
    /// supported codec), so this wraps `h264::EncSequenceParameterBufferH264` directly instead of
    /// going through a per-codec wrapper enum the way cros-libva's equivalent variant does.
    EncSequenceParameter(EncSequenceParameterBufferH264),
    /// Abstraction over `VAEncPictureParameterBufferType`. H.264 only; see `EncSequenceParameter`.
    EncPictureParameter(EncPictureParameterBufferH264),
    /// Abstraction over `VAEncSliceParameterBufferType`. H.264 only; see `EncSequenceParameter`.
    EncSliceParameter(EncSliceParameterBufferH264),
    /// Abstraction over `VAEncMiscParameterBufferType`.
    EncMiscParameter(EncMiscParameter),
    /// Abstraction over `VAProcPipelineParameterBufferType`.
    ProcPipelineParameter(ProcPipelineParameterBuffer),
}

/// Abstraction over the `VAEncMiscParameterBuffer` kinds we support.
pub enum EncMiscParameter {
    /// Wrapper over `VAEncMiscParameterBuffer` with `VAEncMiscParameterFrameRate`.
    FrameRate(EncMiscParameterFrameRate),
    /// Wrapper over `VAEncMiscParameterBuffer` with `VAEncMiscParameterRateControl`.
    RateControl(EncMiscParameterRateControl),
}

impl BufferType {
    /// Returns the inner FFI buffer type.
    pub(crate) fn inner(&self) -> bindings::VABufferType::Type {
        match self {
            BufferType::SliceData { .. } => bindings::VABufferType::VASliceDataBufferType,
            BufferType::EncCodedBuffer(_) => bindings::VABufferType::VAEncCodedBufferType,
            BufferType::EncPackedHeaderParameter(_) => {
                bindings::VABufferType::VAEncPackedHeaderParameterBufferType
            }
            BufferType::EncPackedHeaderData(_) => {
                bindings::VABufferType::VAEncPackedHeaderDataBufferType
            }
            BufferType::EncSequenceParameter(_) => {
                bindings::VABufferType::VAEncSequenceParameterBufferType
            }
            BufferType::EncPictureParameter(_) => {
                bindings::VABufferType::VAEncPictureParameterBufferType
            }
            BufferType::EncSliceParameter(_) => {
                bindings::VABufferType::VAEncSliceParameterBufferType
            }
            BufferType::EncMiscParameter(_) => bindings::VABufferType::VAEncMiscParameterBufferType,
            BufferType::ProcPipelineParameter(_) => {
                bindings::VABufferType::VAProcPipelineParameterBufferType
            }
        }
    }
}

/// Wrapper type representing a buffer created with `vaCreateBuffer` with VAEncCodedBufferType.
pub struct EncCodedBuffer(Buffer);

impl EncCodedBuffer {
    pub(crate) fn new(context: Rc<Context>, size: usize) -> Result<Self, VaError> {
        Ok(Self(Buffer::new(
            context,
            BufferType::EncCodedBuffer(size),
        )?))
    }

    /// Convenience function to return buffer's `VABufferID`.
    pub fn id(&self) -> bindings::VABufferID {
        self.0.id
    }
}

/// Helper to access a single segment of mapped coded buffer
pub struct MappedCodedSegment<'s> {
    pub bit_offset: u32,
    pub status: u32,
    pub buf: &'s [u8],
}

/// Helper to access segments of mapped coded buffer
pub struct MappedCodedBuffer<'p> {
    segments: Vec<MappedCodedSegment<'p>>,
    buffer: &'p EncCodedBuffer,
}

impl<'p> MappedCodedBuffer<'p> {
    /// Map a 'VAEncCodedBufferType' buffer.
    pub fn new(buffer: &'p EncCodedBuffer) -> Result<Self, VaError> {
        let mut addr = std::ptr::null_mut();
        let mut segments = Vec::new();

        va_check(unsafe {
            bindings::vaMapBuffer(buffer.0.context.display().handle(), buffer.id(), &mut addr)
        })?;

        while !addr.is_null() {
            let segment: &bindings::VACodedBufferSegment =
                unsafe { &*(addr as *const bindings::VACodedBufferSegment) };

            let size = segment.size;
            let buf = segment.buf;

            let buf = unsafe { std::slice::from_raw_parts(buf as *mut u8, size as usize) };

            segments.push(MappedCodedSegment {
                bit_offset: segment.bit_offset,
                status: segment.status,
                buf,
            });

            addr = segment.next;
        }

        Ok(Self { segments, buffer })
    }

    /// Returns the iterator over segments
    pub fn iter(&self) -> impl Iterator<Item = &MappedCodedSegment<'p>> {
        self.segments.iter()
    }

    /// Returns the segments of mapped coded buffers.
    pub fn segments(&self) -> &Vec<MappedCodedSegment<'p>> {
        &self.segments
    }
}

impl<'p> Drop for MappedCodedBuffer<'p> {
    fn drop(&mut self) {
        let status = va_check(unsafe {
            bindings::vaUnmapBuffer(self.buffer.0.context.display().handle(), self.buffer.id())
        });

        if status.is_err() {
            error!("vaUnmapBuffer failed: {}", status.unwrap_err());
        }
    }
}

/// Abstraction over the `VAEncPackedHeaderType` enum values we support.
#[repr(u32)]
pub enum EncPackedHeaderType {
    /// Sequence header
    Sequence = bindings::VAEncPackedHeaderType::VAEncPackedHeaderSequence,
    /// Picture header
    Picture = bindings::VAEncPackedHeaderType::VAEncPackedHeaderPicture,
    /// Slice header
    Slice = bindings::VAEncPackedHeaderType::VAEncPackedHeaderSlice,
    /// Raw data
    RawData = bindings::VAEncPackedHeaderType::VAEncPackedHeaderRawData,
}

/// Abstraction over `EncPackedHeaderParameterBuffer` types we support
pub struct EncPackedHeaderParameter(Box<bindings::VAEncPackedHeaderParameterBuffer>);

impl EncPackedHeaderParameter {
    /// Creates a new `EncPackedHeaderParameter` from the given `VAEncPackedHeaderParameterBuffer`.
    pub fn new(type_: EncPackedHeaderType, length_in_bits: u32, has_emulation: bool) -> Self {
        Self(Box::new(bindings::VAEncPackedHeaderParameterBuffer {
            type_: type_ as _,
            bit_length: length_in_bits,
            has_emulation_bytes: has_emulation as u8,
            ..Default::default()
        }))
    }

    /// Returns a mutable reference to the inner `VAEncPackedHeaderParameterBuffer`.
    pub fn inner_mut(&mut self) -> &mut bindings::VAEncPackedHeaderParameterBuffer {
        &mut self.0
    }
}
