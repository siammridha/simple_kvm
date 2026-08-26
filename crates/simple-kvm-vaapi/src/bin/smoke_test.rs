//! Smoke test proving the ported plumbing works against the real driver.
//!
//! Opens a `Display`, then creates a `Config`+`Context` for both an H.264 encode entrypoint
//! (preferring `VAEntrypointEncSliceLP`, falling back to `VAEntrypointEncSlice`) and the VPP
//! entrypoint (`VAEntrypointVideoProc` / `VAProfileNone`), the same way simple_kvm's existing GPU
//! encoding pipeline does. Also creates a surface for each context and exercises
//! `Surface::display()` (the public accessor added as one of this crate's two deliberate
//! deviations from cros-libva).

use simple_kvm_vaapi::{
    self as va, Display, UsageHint, VAConfigAttrib, VAConfigAttribType, VAEntrypoint, VAProfile,
    VA_RT_FORMAT_YUV420,
};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 64;

fn main() {
    println!("== simple-kvm-vaapi smoke test ==");

    let display = Display::open().expect("Display::open() failed: no usable DRM/VAAPI device");
    println!("Display::open() succeeded");

    let vendor = display
        .query_vendor_string()
        .expect("query_vendor_string failed");
    println!("vendor string: {}", vendor);

    // --- H.264 encode config/context, preferring the low-power entrypoint ---
    let h264_profile = VAProfile::VAProfileH264Main;
    let entrypoints = display
        .query_config_entrypoints(h264_profile)
        .expect("query_config_entrypoints(VAProfileH264Main) failed");
    println!("VAProfileH264Main entrypoints: {:?}", entrypoints);

    let enc_entrypoint = if entrypoints.contains(&VAEntrypoint::VAEntrypointEncSliceLP) {
        println!("using VAEntrypointEncSliceLP (low-power)");
        VAEntrypoint::VAEntrypointEncSliceLP
    } else if entrypoints.contains(&VAEntrypoint::VAEntrypointEncSlice) {
        println!("VAEntrypointEncSliceLP not supported by this driver, falling back to VAEntrypointEncSlice");
        VAEntrypoint::VAEntrypointEncSlice
    } else {
        panic!("driver supports neither VAEntrypointEncSliceLP nor VAEntrypointEncSlice for VAProfileH264Main");
    };

    let mut enc_attrs = vec![VAConfigAttrib {
        type_: VAConfigAttribType::VAConfigAttribRTFormat,
        value: 0,
    }];
    display
        .get_config_attributes(h264_profile, enc_entrypoint, &mut enc_attrs)
        .expect("get_config_attributes (H.264 encode) failed");
    assert!(
        enc_attrs[0].value != va::VA_ATTRIB_NOT_SUPPORTED,
        "driver did not report a supported RT format for H.264 encode"
    );

    let enc_config = display
        .create_config(enc_attrs, h264_profile, enc_entrypoint)
        .expect("create_config (H.264 encode) failed");
    println!("H.264 encode Config created");

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
    println!(
        "H.264 encode Surface created: id={}, size={:?}",
        enc_surfaces[0].id(),
        enc_surfaces[0].size()
    );
    // Exercise the public Surface::display() accessor (deviation 2).
    assert!(std::ptr::eq(
        enc_surfaces[0].display().as_ref(),
        display.as_ref()
    ));

    let enc_context = display
        .create_context(&enc_config, WIDTH, HEIGHT, Some(&enc_surfaces), true)
        .expect("create_context (H.264 encode) failed");
    println!("H.264 encode Context created");
    drop(enc_context);
    drop(enc_surfaces);
    drop(enc_config);

    // --- VPP config/context ---
    let vpp_profile = VAProfile::VAProfileNone;
    let vpp_entrypoints = display
        .query_config_entrypoints(vpp_profile)
        .expect("query_config_entrypoints(VAProfileNone) failed");
    println!("VAProfileNone entrypoints: {:?}", vpp_entrypoints);
    assert!(
        vpp_entrypoints.contains(&VAEntrypoint::VAEntrypointVideoProc),
        "driver does not support VAEntrypointVideoProc on VAProfileNone"
    );

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
    println!("VPP Config created");

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
    println!(
        "VPP Surface created: id={}, size={:?}",
        vpp_surfaces[0].id(),
        vpp_surfaces[0].size()
    );

    let vpp_context = display
        .create_context(&vpp_config, WIDTH, HEIGHT, Some(&vpp_surfaces), true)
        .expect("create_context (VPP) failed");
    println!("VPP Context created");
    drop(vpp_context);
    drop(vpp_surfaces);
    drop(vpp_config);

    println!("== smoke test PASSED ==");
}
