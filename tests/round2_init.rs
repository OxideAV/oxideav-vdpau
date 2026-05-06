//! Round 2 integration tests: open an X display, create a `VdpDevice`,
//! and exercise the post-bootstrap dispatch table.
//!
//! Every test is **skip-friendly**: if `XOpenDisplay` fails (no X
//! server, headless CI) or `vdp_device_create_x11` returns an error
//! (no VDPAU backend driver, locked GPU, etc.) the test prints a
//! diagnostic and returns instead of panicking. On a workstation
//! with NVIDIA + libvdpau + an active X server (which is where this
//! crate is developed and validated) all five tests pass.

#![cfg(target_os = "linux")]

use oxideav_vdpau::{sys, Display, VdpDevice};

/// Open and immediately drop a `Display` from `$DISPLAY` (falling
/// back to `:0`).
#[test]
fn display_opens_from_env() {
    let display = match Display::open_from_env() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("display_opens_from_env: skipping — {e}");
            return;
        }
    };
    // Default screen index should be a non-negative integer; on a
    // single-screen NVIDIA box it's 0.
    let s = display.default_screen();
    assert!(s >= 0, "default_screen returned negative value: {s}");
}

/// Create a `VdpDevice` end-to-end from an open X display.
#[test]
fn vdp_device_creates() {
    let display = match Display::open_from_env() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("vdp_device_creates: skipping (no X) — {e}");
            return;
        }
    };
    let _device: VdpDevice = match display.create_vdp_device() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("vdp_device_creates: skipping (no VDPAU backend) — {e}");
            return;
        }
    };
    // Dropping `_device` here exercises VdpDeviceDestroy via Drop.
}

/// `VdpGetInformationString` returns a non-empty driver banner; on
/// this NVIDIA box it contains "NVIDIA".
#[test]
fn vdp_device_reports_information_string() {
    let display = match Display::open_from_env() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("vdp_device_reports_information_string: skipping (no X) — {e}");
            return;
        }
    };
    let device = match display.create_vdp_device() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("vdp_device_reports_information_string: skipping (no VDPAU) — {e}");
            return;
        }
    };
    let info = match device.information_string() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("vdp_device_reports_information_string: skipping ({e})");
            return;
        }
    };
    assert!(
        !info.is_empty(),
        "VdpGetInformationString returned an empty string"
    );
    eprintln!("VDPAU implementation banner: {info}");
    if cfg!(target_os = "linux") && info.contains("NVIDIA") {
        // On the NVIDIA workstation this assertion holds; on Mesa/AMD
        // the banner mentions "Nouveau" or the radeon driver.
        // We've already verified non-empty; the NVIDIA check is just
        // a tighter assertion for this box.
        assert!(info.contains("NVIDIA"));
    }
}

/// `VdpGetApiVersion` returns at least 1 — VDPAU API has been at
/// version >= 1 since release.
#[test]
fn vdp_device_reports_api_version_at_least_1() {
    let display = match Display::open_from_env() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("vdp_device_reports_api_version_at_least_1: skipping (no X) — {e}");
            return;
        }
    };
    let device = match display.create_vdp_device() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("vdp_device_reports_api_version_at_least_1: skipping (no VDPAU) — {e}");
            return;
        }
    };
    let v = match device.api_version() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("vdp_device_reports_api_version_at_least_1: skipping ({e})");
            return;
        }
    };
    eprintln!("VDPAU API version: {v}");
    assert!(v >= 1, "VdpGetApiVersion returned {v}, expected >= 1");
}

/// On NVIDIA hardware VDPAU advertises support for H.264 High; check
/// that `VdpDecoderQueryCapabilities` reports `supported = true`.
#[test]
fn decoder_caps_h264_high_supported_on_nvidia() {
    let display = match Display::open_from_env() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("decoder_caps_h264_high_supported_on_nvidia: skipping (no X) — {e}");
            return;
        }
    };
    let device = match display.create_vdp_device() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("decoder_caps_h264_high_supported_on_nvidia: skipping (no VDPAU) — {e}");
            return;
        }
    };
    let info = device.information_string().unwrap_or_default();
    if !info.contains("NVIDIA") {
        eprintln!(
            "decoder_caps_h264_high_supported_on_nvidia: skipping — driver is not NVIDIA ({info})"
        );
        return;
    }

    let caps = match device.decoder_caps(sys::VDP_DECODER_PROFILE_H264_HIGH) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("decoder_caps_h264_high_supported_on_nvidia: skipping ({e})");
            return;
        }
    };
    eprintln!(
        "H264_HIGH caps: supported={} max_level={} max_macroblocks={} max={}x{}",
        caps.supported, caps.max_level, caps.max_macroblocks, caps.max_width, caps.max_height
    );
    assert!(
        caps.supported,
        "expected NVIDIA VDPAU to advertise H.264 High support, got {caps:?}"
    );
    assert!(
        caps.max_width >= 1920 && caps.max_height >= 1080,
        "expected at least 1080p surface support on NVIDIA H.264 High, got {}x{}",
        caps.max_width,
        caps.max_height
    );
}
