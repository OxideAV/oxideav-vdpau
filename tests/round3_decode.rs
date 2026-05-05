//! Round 3 integration tests: allocate a `VdpVideoSurface` and a
//! `VdpDecoder` end-to-end on the live driver.
//!
//! Skip-friendly like Round 2 — no X server / no VDPAU backend / no
//! NVIDIA driver is treated as "skip with diagnostic, don't fail the
//! suite". Failures from the actual VDPAU calls *are* asserted.

#![cfg(target_os = "linux")]

use oxideav_vdpau::{Display, H264VdpauDecoder, VdpDevice, sys};

/// Open a Display and a VdpDevice or skip the test cleanly. Returns
/// `None` if the environment can't run real VDPAU.
fn open_device(name: &str) -> Option<VdpDevice> {
    let display = match Display::open_from_env() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{name}: skipping (no X) — {e}");
            return None;
        }
    };
    match display.create_vdp_device() {
        Ok(d) => {
            // Keep the device by leaking the Display alongside it —
            // VdpDevice is bound to the X connection at the driver
            // level, so we must not drop Display before the device.
            // The simplest correct lifetime for a unit test is to
            // box-leak the display.
            let _ = Box::leak(Box::new(display));
            Some(d)
        }
        Err(e) => {
            eprintln!("{name}: skipping (no VDPAU backend) — {e}");
            None
        }
    }
}

/// 1920×1088 4:2:0 video surface allocates and drops cleanly.
#[test]
fn video_surface_creates() {
    let device = match open_device("video_surface_creates") {
        Some(d) => d,
        None => return,
    };
    let surface = device
        .create_video_surface(sys::VDP_CHROMA_TYPE_420, 1920, 1088)
        .expect("VdpVideoSurfaceCreate(1920x1088, 4:2:0)");
    assert_eq!(surface.width(), 1920);
    assert_eq!(surface.height(), 1088);
    assert_eq!(surface.chroma_type(), sys::VDP_CHROMA_TYPE_420);
    // Surface dropped here — exercises VdpVideoSurfaceDestroy.
}

/// H.264 High decoder allocates for a 1080p stream with up to 16 refs.
#[test]
fn h264_high_decoder_creates() {
    let device = match open_device("h264_high_decoder_creates") {
        Some(d) => d,
        None => return,
    };
    let info = device.information_string().unwrap_or_default();
    if !info.contains("NVIDIA") {
        eprintln!(
            "h264_high_decoder_creates: skipping — driver is not NVIDIA ({info}); H.264 High \
             support varies on Mesa front-ends"
        );
        return;
    }
    let decoder = device
        .create_decoder(sys::VDP_DECODER_PROFILE_H264_HIGH, 1920, 1088, 16)
        .expect("VdpDecoderCreate(H264_HIGH, 1920x1088, 16 refs)");
    assert_eq!(decoder.profile(), sys::VDP_DECODER_PROFILE_H264_HIGH);
    assert_eq!(decoder.width(), 1920);
    assert_eq!(decoder.height(), 1088);
    // Decoder dropped here — exercises VdpDecoderDestroy.
}

/// `VdpDecoderCreate` must reject a request that exceeds the
/// driver-advertised maxima for the profile (16384×16384 is well
/// past the H.264 4096×4096 cap reported by Round 2). The driver
/// returns a non-OK `VdpStatus`; we assert that the wrapper
/// surfaces it as `Err`.
#[test]
fn decoder_creation_fails_for_oversized_request() {
    let device = match open_device("decoder_creation_fails_for_oversized_request") {
        Some(d) => d,
        None => return,
    };
    let info = device.information_string().unwrap_or_default();
    if !info.contains("NVIDIA") {
        eprintln!(
            "decoder_creation_fails_for_oversized_request: skipping — non-NVIDIA driver may have \
             different caps ({info})"
        );
        return;
    }
    match device.create_decoder(sys::VDP_DECODER_PROFILE_H264_HIGH, 16384, 16384, 16) {
        Ok(_) => panic!(
            "expected VdpDecoderCreate(H264_HIGH, 16384x16384, 16) to fail — driver advertised \
             4096x4096 max in Round 2"
        ),
        Err(e) => {
            eprintln!("decoder_creation_fails_for_oversized_request: got expected error — {e}");
            assert_ne!(
                e.status,
                sys::VDP_STATUS_OK,
                "expected non-OK VdpStatus on oversized request"
            );
        }
    }
}

// ─────────────────────────── Tier B: end-to-end H.264 decode ─────────────────

const FIXTURE: &[u8] = include_bytes!("fixtures/h264_high_320x240_1frame.h264");

/// Construct an `H264VdpauDecoder` from the bundled IDR fixture.
/// Verifies that SPS/PPS parsing recovers the encoded dimensions and
/// that `VdpDecoderCreate` accepts the result.
#[test]
fn h264_vdpau_decoder_parses_fixture_and_creates_decoder() {
    let device = match open_device("h264_vdpau_decoder_parses_fixture_and_creates_decoder") {
        Some(d) => d,
        None => return,
    };
    let info = device.information_string().unwrap_or_default();
    if !info.contains("NVIDIA") {
        eprintln!(
            "h264_vdpau_decoder_parses_fixture_and_creates_decoder: skipping — \
             driver is not NVIDIA ({info})"
        );
        return;
    }
    let decoder = H264VdpauDecoder::new(&device, FIXTURE)
        .expect("H264VdpauDecoder::new on bundled 320x240 High-profile IDR");
    assert_eq!(decoder.width(), 320);
    assert_eq!(decoder.height(), 240);
}

/// Decode the bundled single-IDR Annex-B fixture end-to-end:
/// SPS/PPS parse → `VdpDecoderCreate` → `VdpVideoSurfaceCreate` →
/// `VdpDecoderRender` → `VdpVideoSurfaceGetBitsYCbCr(NV12)` →
/// I420 deinterleave. Asserts dimensions and that the luma plane has
/// real variability (not all-zero / all-one), which is what we expect
/// from `testsrc2` (gradient + text overlays).
#[test]
fn h264_idr_decode_yields_non_trivial_luma() {
    let device = match open_device("h264_idr_decode_yields_non_trivial_luma") {
        Some(d) => d,
        None => return,
    };
    let info = device.information_string().unwrap_or_default();
    if !info.contains("NVIDIA") {
        eprintln!(
            "h264_idr_decode_yields_non_trivial_luma: skipping — driver is not NVIDIA ({info})"
        );
        return;
    }
    let decoder = H264VdpauDecoder::new(&device, FIXTURE)
        .expect("H264VdpauDecoder::new on bundled 320x240 High-profile IDR");
    let frame = match decoder.decode_idr(&device, FIXTURE) {
        Ok(f) => f,
        Err(e) => {
            // A real failure path in a real test — surface it.
            panic!("H264VdpauDecoder::decode_idr failed: {e}");
        }
    };
    assert_eq!(frame.width, 320);
    assert_eq!(frame.height, 240);
    assert_eq!(frame.y.len(), 320 * 240);
    assert_eq!(frame.u.len(), 160 * 120);
    assert_eq!(frame.v.len(), 160 * 120);

    // Variability check on luma. testsrc2 has many distinct
    // luminance levels (color bars, chequerboard, text). Demand at
    // least 16 unique byte values and a span of at least 64 between
    // min and max — comfortably within reach for any non-trivial
    // image and impossible for an all-zero / all-FF buffer.
    let mut seen = [false; 256];
    let (mut lo, mut hi) = (255u8, 0u8);
    for &v in &frame.y {
        seen[v as usize] = true;
        if v < lo {
            lo = v;
        }
        if v > hi {
            hi = v;
        }
    }
    let unique = seen.iter().filter(|b| **b).count();
    eprintln!(
        "h264_idr_decode_yields_non_trivial_luma: unique luma values={unique}, range=[{lo},{hi}]"
    );
    assert!(
        unique >= 16,
        "luma should span at least 16 distinct values, got {unique}"
    );
    assert!(
        hi as u32 - lo as u32 >= 64,
        "luma range too narrow: [{lo},{hi}] (decode probably failed silently)"
    );
}
