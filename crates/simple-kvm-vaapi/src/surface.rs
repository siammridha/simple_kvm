// Ported from cros-libva's lib/src/surface.rs, with deviation 1 applied: `Surface` is no longer
// generic over a `SurfaceMemoryDescriptor`. It always wraps driver-allocated memory. This drops
// the `SurfaceMemoryDescriptor`/`ExternalBufferDescriptor` traits, `MemoryType`,
// `SurfaceExternalDescriptor`, and the whole DRM-PRIME export path (`export_prime`,
// `DrmPrimeSurfaceDescriptor`, etc.) entirely.
//
// Deviation 2 is also applied here: `display()` is `pub` instead of `pub(crate)`.

use std::os::raw::c_void;
use std::rc::Rc;

use crate::bindings;
use crate::display::Display;
use crate::va_check;
use crate::UsageHint;
use crate::VASurfaceID;
use crate::VaError;

/// Decode error type aka `VADecodeErrorType`
#[repr(u32)]
#[derive(Debug)]
pub enum DecodeErrorType {
    SliceMissing = bindings::VADecodeErrorType::VADecodeSliceMissing,
    MBError = bindings::VADecodeErrorType::VADecodeMBError,
    #[cfg(libva_1_20_or_higher)]
    Reset = bindings::VADecodeErrorType::VADecodeReset,
}

/// Decode error details extracted from `VASurfaceDecodeMBErrors`, result of vaQuerySurfaceError.
#[derive(Debug)]
pub struct SurfaceDecodeMBError {
    /// Start mb address with errors
    pub start_mb: u32,

    /// End mb address with errors
    pub end_mb: u32,

    pub decode_error_type: DecodeErrorType,

    /// Number of mbs with errors
    pub num_mb: u32,
}

/// An owned, driver-allocated VA surface that is tied to a particular `Display`.
pub struct Surface {
    display: Rc<Display>,
    id: bindings::VASurfaceID,
    width: u32,
    height: u32,
}

impl From<i32> for bindings::VAGenericValue {
    fn from(i: i32) -> Self {
        Self {
            type_: bindings::VAGenericValueType::VAGenericValueTypeInteger,
            value: bindings::_VAGenericValue__bindgen_ty_1 { i },
        }
    }
}

impl From<f32> for bindings::VAGenericValue {
    fn from(f: f32) -> Self {
        Self {
            type_: bindings::VAGenericValueType::VAGenericValueTypeFloat,
            value: bindings::_VAGenericValue__bindgen_ty_1 { f },
        }
    }
}

impl From<*mut c_void> for bindings::VAGenericValue {
    fn from(p: *mut c_void) -> Self {
        Self {
            type_: bindings::VAGenericValueType::VAGenericValueTypePointer,
            value: bindings::_VAGenericValue__bindgen_ty_1 { p },
        }
    }
}

/// Helpers to build valid `VASurfaceAttrib`s.
impl bindings::VASurfaceAttrib {
    pub fn new_pixel_format(fourcc: u32) -> Self {
        Self {
            type_: bindings::VASurfaceAttribType::VASurfaceAttribPixelFormat,
            flags: bindings::VA_SURFACE_ATTRIB_SETTABLE,
            value: bindings::VAGenericValue::from(fourcc as i32),
        }
    }

    pub fn new_usage_hint(usage_hint: UsageHint) -> Self {
        Self {
            type_: bindings::VASurfaceAttribType::VASurfaceAttribUsageHint,
            flags: bindings::VA_SURFACE_ATTRIB_SETTABLE,
            value: bindings::VAGenericValue::from(usage_hint.bits() as i32),
        }
    }
}

impl Surface {
    /// Create `Surfaces` by wrapping around `vaCreateSurfaces` calls (one per surface). This is
    /// just a helper for [`Display::create_surfaces`](crate::Display::create_surfaces).
    pub(crate) fn new(
        display: Rc<Display>,
        rt_format: u32,
        va_fourcc: Option<u32>,
        width: u32,
        height: u32,
        usage_hint: Option<UsageHint>,
        num_surfaces: usize,
    ) -> Result<Vec<Self>, VaError> {
        let mut surfaces = vec![];

        for _ in 0..num_surfaces {
            let mut attrs = vec![];

            if let Some(usage_hint) = usage_hint {
                attrs.push(bindings::VASurfaceAttrib::new_usage_hint(usage_hint));
            }

            if let Some(fourcc) = va_fourcc {
                attrs.push(bindings::VASurfaceAttrib::new_pixel_format(fourcc));
            }

            let mut surface_id: VASurfaceID = 0;

            // Safe because `display` represents a valid VADisplay. The `attrs` vector is properly
            // initialized and a valid size is passed to the C function, so it is impossible to
            // write past the end of its storage by mistake.
            match va_check(unsafe {
                bindings::vaCreateSurfaces(
                    display.handle(),
                    rt_format,
                    width,
                    height,
                    &mut surface_id,
                    1,
                    attrs.as_mut_ptr(),
                    attrs.len() as u32,
                )
            }) {
                Ok(()) => surfaces.push(Self {
                    display: Rc::clone(&display),
                    id: surface_id,
                    width,
                    height,
                }),
                Err(e) => return Err(e),
            }
        }

        Ok(surfaces)
    }

    /// Returns a shared reference to the [`Display`] this surface was created from.
    pub fn display(&self) -> &Rc<Display> {
        &self.display
    }

    /// Wrapper around `vaSyncSurface` that blocks until all pending operations on the render
    /// target have been completed.
    ///
    /// Upon return it
    /// is safe to use the render target for a different picture.
    pub fn sync(&self) -> Result<(), VaError> {
        // Safe because `self` represents a valid VASurface.
        va_check(unsafe { bindings::vaSyncSurface(self.display.handle(), self.id) })
    }

    /// Convenience function to return a VASurfaceID vector. Useful to interface with the C API
    /// where a surface array might be needed.
    pub fn as_id_vec(surfaces: &[Self]) -> Vec<bindings::VASurfaceID> {
        surfaces.iter().map(|surface| surface.id).collect()
    }

    /// Wrapper over `vaQuerySurfaceStatus` to find out any pending ops on the render target.
    pub fn query_status(&self) -> Result<bindings::VASurfaceStatus::Type, VaError> {
        let mut status: bindings::VASurfaceStatus::Type = 0;
        // Safe because `self` represents a valid VASurface.
        va_check(unsafe {
            bindings::vaQuerySurfaceStatus(self.display.handle(), self.id, &mut status)
        })?;

        Ok(status)
    }

    pub fn query_error(&self) -> Result<Vec<SurfaceDecodeMBError>, VaError> {
        let mut raw: *const bindings::VASurfaceDecodeMBErrors = std::ptr::null();

        // Safe because `self` represents a valid VASurface.
        va_check(unsafe {
            bindings::vaQuerySurfaceError(
                self.display.handle(),
                self.id,
                bindings::VA_STATUS_ERROR_DECODING_ERROR as i32,
                (&mut raw) as *mut _ as *mut _,
            )
        })?;

        let mut errors = vec![];

        while !raw.is_null() {
            // Safe because raw is a valid pointer
            let error = unsafe { *raw };
            if error.status == -1 {
                break;
            }

            let type_ = match error.decode_error_type {
                bindings::VADecodeErrorType::VADecodeSliceMissing => DecodeErrorType::SliceMissing,
                bindings::VADecodeErrorType::VADecodeMBError => DecodeErrorType::MBError,
                #[cfg(libva_1_20_or_higher)]
                bindings::VADecodeErrorType::VADecodeReset => DecodeErrorType::Reset,
                _ => {
                    log::warn!(
                        "Unrecognized `decode_error_type` value ({})",
                        error.decode_error_type
                    );

                    // Safe because status != -1
                    raw = unsafe { raw.offset(1) };
                    continue;
                }
            };

            errors.push(SurfaceDecodeMBError {
                start_mb: error.start_mb,
                end_mb: error.end_mb,
                decode_error_type: type_,
                num_mb: error.num_mb,
            });

            // Safe because status != -1
            raw = unsafe { raw.offset(1) };
        }

        Ok(errors)
    }

    /// Returns the ID of this surface.
    pub fn id(&self) -> bindings::VASurfaceID {
        self.id
    }

    /// Returns the dimensions of this surface.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // Safe because `self` represents a valid VASurface.
        unsafe { bindings::vaDestroySurfaces(self.display.handle(), &mut self.id, 1) };
    }
}
