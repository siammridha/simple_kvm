//! Implements a lightweight and safe interface over `libva`, trimmed to the
//! core plumbing needed by simple_kvm's GPU encoding pipeline.
//!
//! This crate is a purpose-built port of [cros-libva](https://github.com/chromeos/cros-libva),
//! trimmed down in scope (see the crate's build.rs / README-equivalent notes for what was left
//! out) and with two deliberate deviations from cros-libva's design:
//!
//! 1. [`Surface`] is not generic over a memory descriptor: it is always driver-allocated, and the
//!    DRM-PRIME external-buffer-import machinery has been dropped entirely.
//! 2. [`Surface::display`] is a public accessor (in cros-libva it is `pub(crate)`).
//!
//! The starting point to using this crate is to open a [`Display`], from which a [`Context`] and
//! [`Surface`]s can be allocated and used for doing actual work.
//!
//! [`buffer`] has the codec-agnostic buffer plumbing (`Buffer`, `EncCodedBuffer`,
//! `MappedCodedBuffer`, `EncPackedHeaderParameter`, `BufferType`/`BorrowedBufferType`) plus the
//! H.264-encode and VPP buffer-type wrappers this crate supports (`buffer::h264`,
//! `buffer::proc_pipeline`, `buffer::enc_misc`). Only H.264 encode and VPP are in scope; H.264
//! decode and every other codec are not implemented.

mod bindings;
pub mod buffer;
mod config;
mod context;
mod display;
mod generic_value;
mod image;
mod picture;
mod surface;
mod usage_hint;

pub use bindings::*;
pub use buffer::*;
pub use config::*;
pub use context::*;
pub use display::*;
pub use generic_value::*;
pub use image::*;
pub use picture::*;
pub use surface::*;
pub use usage_hint::*;

use std::num::NonZeroI32;

/// A `VAStatus` that is guaranteed to not be `VA_STATUS_SUCCESS`.
#[derive(Debug)]
pub struct VaError(NonZeroI32);

impl VaError {
    /// Returns the `VAStatus` of this error.
    pub fn va_status(&self) -> VAStatus {
        self.0.get() as VAStatus
    }
}

impl std::fmt::Display for VaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use std::ffi::CStr;

        // Safe because `vaErrorStr` will return a pointer to a statically allocated, null
        // terminated C string. The pointer is guaranteed to never be null.
        let err_str = unsafe { CStr::from_ptr(bindings::vaErrorStr(self.0.get())) }
            .to_str()
            .unwrap();
        f.write_str(err_str)
    }
}

impl std::error::Error for VaError {}

/// Checks a VA return value and returns a `VaError` if it is not `VA_STATUS_SUCCESS`.
///
/// This can be used on the return value of any VA function returning `VAStatus` in order to
/// convert it to a proper Rust `Result`.
fn va_check(code: VAStatus) -> Result<(), VaError> {
    match code as u32 {
        bindings::VA_STATUS_SUCCESS => Ok(()),
        _ => Err(VaError(unsafe { NonZeroI32::new_unchecked(code) })),
    }
}
