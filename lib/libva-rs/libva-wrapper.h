// Wrapper header fed to bindgen. Ported from cros-libva's lib/libva-wrapper.h,
// with the protected-content conditional include dropped (out of scope for
// this crate).

#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>
