//! Round 4 integration test: HEVC IDR end-to-end decode.
//!
//! Mirrors the H.264 round-3 test shape: skip-friendly when no X
//! server / no NVIDIA driver is reachable, hard-asserts when those
//! prerequisites are met.

#![cfg(target_os = "linux")]

use oxideav_vdpau::{sys, Display, HevcVdpauDecoder, VdpDevice};

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
            // Box-leak the display: VdpDevice's lifetime is tied to
            // the X connection at the driver layer.
            let _ = Box::leak(Box::new(display));
            Some(d)
        }
        Err(e) => {
            eprintln!("{name}: skipping (no VDPAU backend) — {e}");
            None
        }
    }
}

const FIXTURE: &[u8] = include_bytes!("fixtures/hevc_main_320x240_1frame.h265");

/// On NVIDIA hardware VDPAU advertises support for HEVC Main; check
/// that `VdpDecoderQueryCapabilities` reports `supported = true` on
/// this host.
#[test]
fn decoder_caps_hevc_main_supported_on_nvidia() {
    let device = match open_device("decoder_caps_hevc_main_supported_on_nvidia") {
        Some(d) => d,
        None => return,
    };
    let info = device.information_string().unwrap_or_default();
    if !info.contains("NVIDIA") {
        eprintln!(
            "decoder_caps_hevc_main_supported_on_nvidia: skipping — non-NVIDIA driver ({info})"
        );
        return;
    }
    let caps = match device.decoder_caps(sys::VDP_DECODER_PROFILE_HEVC_MAIN) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("decoder_caps_hevc_main_supported_on_nvidia: skipping ({e})");
            return;
        }
    };
    eprintln!(
        "HEVC_MAIN caps: supported={} max_level={} max_macroblocks={} max={}x{}",
        caps.supported, caps.max_level, caps.max_macroblocks, caps.max_width, caps.max_height
    );
    assert!(
        caps.supported,
        "expected NVIDIA VDPAU to advertise HEVC Main support, got {caps:?}"
    );
}

/// Build a `HevcVdpauDecoder` from the bundled IDR fixture. Verifies
/// that VPS/SPS/PPS parsing recovers the encoded dimensions and that
/// `VdpDecoderCreate` accepts the result.
#[test]
fn hevc_vdpau_decoder_parses_fixture_and_creates_decoder() {
    let device = match open_device("hevc_vdpau_decoder_parses_fixture_and_creates_decoder") {
        Some(d) => d,
        None => return,
    };
    let info = device.information_string().unwrap_or_default();
    if !info.contains("NVIDIA") {
        eprintln!(
            "hevc_vdpau_decoder_parses_fixture_and_creates_decoder: skipping — \
             driver is not NVIDIA ({info})"
        );
        return;
    }
    let decoder = HevcVdpauDecoder::new(&device, FIXTURE)
        .expect("HevcVdpauDecoder::new on bundled 320x240 Main-profile IDR");
    assert_eq!(decoder.width(), 320);
    assert_eq!(decoder.height(), 240);
}

/// Decode the bundled single-IDR HEVC fixture end-to-end and assert
/// the luma plane has real variability (not all-zero / all-FF).
#[test]
fn hevc_idr_decode_yields_non_trivial_luma() {
    let device = match open_device("hevc_idr_decode_yields_non_trivial_luma") {
        Some(d) => d,
        None => return,
    };
    let info = device.information_string().unwrap_or_default();
    if !info.contains("NVIDIA") {
        eprintln!(
            "hevc_idr_decode_yields_non_trivial_luma: skipping — driver is not NVIDIA ({info})"
        );
        return;
    }
    let decoder = HevcVdpauDecoder::new(&device, FIXTURE)
        .expect("HevcVdpauDecoder::new on bundled 320x240 Main-profile IDR");
    let frame = match decoder.decode_idr(&device, FIXTURE) {
        Ok(f) => f,
        Err(e) => {
            panic!("HevcVdpauDecoder::decode_idr failed: {e}");
        }
    };
    assert_eq!(frame.width, 320);
    assert_eq!(frame.height, 240);
    assert_eq!(frame.y.len(), 320 * 240);
    assert_eq!(frame.u.len(), 160 * 120);
    assert_eq!(frame.v.len(), 160 * 120);

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
        "hevc_idr_decode_yields_non_trivial_luma: unique luma values={unique}, range=[{lo},{hi}]"
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

/// Cross-validate the HEVC decode against an ffmpeg reference YUV.
/// Generated on-the-fly via `ffmpeg -i fixture -f rawvideo -pix_fmt yuv420p`.
/// Skipped if `ffmpeg` is not on PATH. Asserts mean abs diff < 20/255.
#[test]
fn hevc_idr_decode_matches_ffmpeg_reference() {
    let device = match open_device("hevc_idr_decode_matches_ffmpeg_reference") {
        Some(d) => d,
        None => return,
    };
    let info = device.information_string().unwrap_or_default();
    if !info.contains("NVIDIA") {
        eprintln!(
            "hevc_idr_decode_matches_ffmpeg_reference: skipping — non-NVIDIA driver ({info})"
        );
        return;
    }

    // Generate the reference YUV via ffmpeg subprocess.
    let fixture_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hevc_main_320x240_1frame.h265"
    );
    let out_path = std::env::temp_dir().join("oxideav_vdpau_hevc_ref.yuv");
    let _ = std::fs::remove_file(&out_path);
    let status = match std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-loglevel",
            "error",
            "-i",
            fixture_path,
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&out_path)
        .status()
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "hevc_idr_decode_matches_ffmpeg_reference: skipping — ffmpeg not invokable ({e})"
            );
            return;
        }
    };
    if !status.success() {
        eprintln!("hevc_idr_decode_matches_ffmpeg_reference: skipping — ffmpeg exit {status}");
        return;
    }
    let ref_yuv = match std::fs::read(&out_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("hevc_idr_decode_matches_ffmpeg_reference: skipping — read ref ({e})");
            return;
        }
    };
    let _ = std::fs::remove_file(&out_path);
    let expected_len = 320 * 240 + 2 * (160 * 120);
    assert_eq!(ref_yuv.len(), expected_len, "ffmpeg reference YUV size");

    let decoder = HevcVdpauDecoder::new(&device, FIXTURE).expect("HevcVdpauDecoder::new");
    let frame = decoder.decode_idr(&device, FIXTURE).expect("decode");
    let (ref_y, rest) = ref_yuv.split_at(320 * 240);
    let (ref_u, ref_v) = rest.split_at(160 * 120);

    let mean_abs = |a: &[u8], b: &[u8]| -> f64 {
        assert_eq!(a.len(), b.len());
        let mut sum: u64 = 0;
        for (&p, &q) in a.iter().zip(b.iter()) {
            sum += (p as i32 - q as i32).unsigned_abs() as u64;
        }
        sum as f64 / a.len() as f64
    };
    let dy = mean_abs(&frame.y, ref_y);
    let du = mean_abs(&frame.u, ref_u);
    let dv = mean_abs(&frame.v, ref_v);
    eprintln!(
        "hevc_idr_decode_matches_ffmpeg_reference: mean abs diff Y={dy:.3} U={du:.3} V={dv:.3}"
    );
    assert!(dy < 20.0, "luma diff too large: {dy}");
    assert!(du < 20.0, "Cb diff too large: {du}");
    assert!(dv < 20.0, "Cr diff too large: {dv}");
}
