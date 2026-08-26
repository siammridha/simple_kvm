// Ported from cros-libva's lib/bindgen_gen.rs, with the allowlist trimmed to
// drop MPEG2/VP8/VP9/HEVC/AV1/JPEG/PRIME/ExternalBuffers/protected-content
// types entirely. VAProc*/VAEncPacked*/VAEncMisc*/H264* raw struct types are
// pulled in for the H.264-encode and VPP safe wrappers in src/buffer/h264.rs,
// src/buffer/proc_pipeline.rs, and src/buffer/enc_misc.rs (this crate only
// supports H.264 encode + VPP, so decode-only and other-codec raw types are
// still out of scope and stay out of this allowlist).
//
// Judgment call / deviation from the brief's literal example string: added
// `VASurfaceDecodeMBErrors|VADecodeErrorType`, which the brief's example
// allowlist omitted but which `surface.rs`'s `query_error`/`DecodeErrorType`
// (ported unchanged, as instructed) need to compile - those two types are
// only reachable through `vaQuerySurfaceError`'s `void *` output parameter,
// so bindgen does not pull them in automatically the way it does for types
// used directly in function signatures.
const ALLOW_LIST_TYPE: &str =
    "VACodedBufferSegment|.*VAProc.*|VAEncPacked.*|VAEncMisc.*|.*H264.*|\
    VASurfaceDecodeMBErrors|VADecodeErrorType";

// The common bindgen builder for VA-API.
pub fn vaapi_gen_builder(builder: bindgen::Builder) -> bindgen::Builder {
    builder
        .derive_default(true)
        .derive_eq(true)
        .layout_tests(false)
        .constified_enum_module("VA.*")
        .allowlist_var("VA.*")
        .allowlist_function("va.*")
        .allowlist_type(ALLOW_LIST_TYPE)
}
