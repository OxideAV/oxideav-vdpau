//! Round 7 integration test: `validate_device_index()` enforces the
//! `CodecParameters::device_index` contract introduced by
//! `oxideav-core` Phase 1.
//!
//! VDPAU is per-X-display: on a single-display box the only valid
//! index is `0`. These tests exercise the boundary cases (None /
//! Some(0) are accepted, anything else errors) and skip cleanly when
//! no VDPAU device is reachable on this host.

#![cfg(target_os = "linux")]
#![cfg(feature = "registry")]

#[test]
fn validate_device_index_zero_ok() {
    if oxideav_vdpau::engine_info().is_empty() {
        eprintln!("no VDPAU device — skip");
        return;
    }
    let r = oxideav_vdpau::validate_device_index(0);
    assert!(r.is_ok(), "device_index=0 should be valid: {r:?}");
}

#[test]
fn validate_device_index_out_of_range_errors() {
    if oxideav_vdpau::engine_info().is_empty() {
        eprintln!("no VDPAU device — skip");
        return;
    }
    let r = oxideav_vdpau::validate_device_index(99);
    assert!(r.is_err(), "device_index=99 should be rejected");
    let msg = r.unwrap_err().to_string();
    assert!(
        msg.contains("out of range"),
        "error message mentions out-of-range: got {msg:?}"
    );
}

#[test]
fn h264_with_params_default_device_ok() {
    use oxideav_core::{CodecId, CodecParameters};

    let devs = oxideav_vdpau::engine_info();
    if devs.is_empty() {
        eprintln!("no VDPAU device — skip");
        return;
    }

    // Open a real device — this is the same path engine_info just
    // walked, so on a host where engine_info reports a device the open
    // here should succeed too.
    let display = match oxideav_vdpau::Display::open_from_env() {
        Ok(d) => d,
        Err(_) => {
            eprintln!("display open failed — skip");
            return;
        }
    };
    let device = match display.create_vdp_device() {
        Ok(d) => d,
        Err(_) => {
            eprintln!("vdp_device_create_x11 failed — skip");
            return;
        }
    };

    // Load the same H.264 IDR fixture round 3 exercises.
    let annex_b: &[u8] = include_bytes!("fixtures/h264_high_320x240_1frame.h264");

    // device_index=None → default device (index 0). Should construct.
    let params = CodecParameters::video(CodecId::new("h264"));
    if let Err(e) = oxideav_vdpau::H264VdpauDecoder::with_params(&device, &params, annex_b) {
        panic!("with_params(default) should construct: {e}");
    }

    // device_index=Some(0) → explicitly the only valid device on this
    // single-display box. Should also construct.
    let params0 = CodecParameters::video(CodecId::new("h264")).with_device_index(0);
    if let Err(e) = oxideav_vdpau::H264VdpauDecoder::with_params(&device, &params0, annex_b) {
        panic!("with_params(device_index=Some(0)) should construct: {e}");
    }
}

#[test]
fn h264_with_params_out_of_range_errors() {
    use oxideav_core::{CodecId, CodecParameters};

    if oxideav_vdpau::engine_info().is_empty() {
        eprintln!("no VDPAU device — skip");
        return;
    }
    let display = match oxideav_vdpau::Display::open_from_env() {
        Ok(d) => d,
        Err(_) => {
            eprintln!("display open failed — skip");
            return;
        }
    };
    let device = match display.create_vdp_device() {
        Ok(d) => d,
        Err(_) => {
            eprintln!("vdp_device_create_x11 failed — skip");
            return;
        }
    };

    // The validator runs *before* any bitstream parsing — empty buffer
    // is fine here, the function should fail on the index check first.
    let params = CodecParameters::video(CodecId::new("h264")).with_device_index(7);
    match oxideav_vdpau::H264VdpauDecoder::with_params(&device, &params, &[]) {
        Ok(_) => panic!("device_index=7 should be rejected"),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("out of range"),
                "error message mentions out-of-range: got {msg:?}"
            );
        }
    }
}
