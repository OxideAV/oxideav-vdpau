//! Round 4 integration test: MPEG-2 I-frame end-to-end decode.

#![cfg(target_os = "linux")]

use oxideav_vdpau::{Display, Mpeg2VdpauDecoder, VdpDevice, sys};

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
            let _ = Box::leak(Box::new(display));
            Some(d)
        }
        Err(e) => {
            eprintln!("{name}: skipping (no VDPAU backend) — {e}");
            None
        }
    }
}

const FIXTURE: &[u8] = include_bytes!("fixtures/mpeg2_main_320x240_1frame.m2v");

/// On NVIDIA hardware VDPAU advertises MPEG-2 Main support universally
/// — it's the codec VDPAU was originally designed around.
#[test]
fn decoder_caps_mpeg2_main_supported_on_nvidia() {
    let device = match open_device("decoder_caps_mpeg2_main_supported_on_nvidia") {
        Some(d) => d,
        None => return,
    };
    let info = device.information_string().unwrap_or_default();
    if !info.contains("NVIDIA") {
        eprintln!("decoder_caps_mpeg2_main_supported_on_nvidia: skipping — non-NVIDIA ({info})");
        return;
    }
    let caps = match device.decoder_caps(sys::VDP_DECODER_PROFILE_MPEG2_MAIN) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("decoder_caps_mpeg2_main_supported_on_nvidia: skipping ({e})");
            return;
        }
    };
    eprintln!(
        "MPEG2_MAIN caps: supported={} max_level={} max_macroblocks={} max={}x{}",
        caps.supported, caps.max_level, caps.max_macroblocks, caps.max_width, caps.max_height
    );
    assert!(caps.supported, "expected NVIDIA VDPAU to advertise MPEG-2 Main support");
}

#[test]
fn mpeg2_vdpau_decoder_parses_fixture_and_creates_decoder() {
    let device = match open_device("mpeg2_vdpau_decoder_parses_fixture_and_creates_decoder") {
        Some(d) => d,
        None => return,
    };
    let info = device.information_string().unwrap_or_default();
    if !info.contains("NVIDIA") {
        eprintln!(
            "mpeg2_vdpau_decoder_parses_fixture_and_creates_decoder: skipping — non-NVIDIA ({info})"
        );
        return;
    }
    let decoder = Mpeg2VdpauDecoder::new(&device, FIXTURE)
        .expect("Mpeg2VdpauDecoder::new on bundled 320x240 Main-profile I-frame");
    assert_eq!(decoder.width(), 320);
    assert_eq!(decoder.height(), 240);
}

#[test]
fn mpeg2_iframe_decode_yields_non_trivial_luma() {
    let device = match open_device("mpeg2_iframe_decode_yields_non_trivial_luma") {
        Some(d) => d,
        None => return,
    };
    let info = device.information_string().unwrap_or_default();
    if !info.contains("NVIDIA") {
        eprintln!("mpeg2_iframe_decode_yields_non_trivial_luma: skipping — non-NVIDIA ({info})");
        return;
    }
    let decoder = Mpeg2VdpauDecoder::new(&device, FIXTURE)
        .expect("Mpeg2VdpauDecoder::new on bundled 320x240 Main-profile I-frame");
    let frame = match decoder.decode_iframe(&device, FIXTURE) {
        Ok(f) => f,
        Err(e) => panic!("Mpeg2VdpauDecoder::decode_iframe failed: {e}"),
    };
    assert_eq!(frame.width, 320);
    assert_eq!(frame.height, 240);
    let mut seen = [false; 256];
    let (mut lo, mut hi) = (255u8, 0u8);
    for &v in &frame.y {
        seen[v as usize] = true;
        if v < lo { lo = v; }
        if v > hi { hi = v; }
    }
    let unique = seen.iter().filter(|b| **b).count();
    eprintln!(
        "mpeg2_iframe_decode_yields_non_trivial_luma: unique luma values={unique}, range=[{lo},{hi}]"
    );
    assert!(unique >= 16, "luma should span at least 16 distinct values, got {unique}");
    assert!(hi as u32 - lo as u32 >= 64, "luma range too narrow: [{lo},{hi}]");
}

/// Cross-validate against ffmpeg reference.
#[test]
fn mpeg2_iframe_decode_matches_ffmpeg_reference() {
    let device = match open_device("mpeg2_iframe_decode_matches_ffmpeg_reference") {
        Some(d) => d,
        None => return,
    };
    let info = device.information_string().unwrap_or_default();
    if !info.contains("NVIDIA") {
        eprintln!("mpeg2_iframe_decode_matches_ffmpeg_reference: skipping — non-NVIDIA ({info})");
        return;
    }

    let fixture_path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mpeg2_main_320x240_1frame.m2v");
    let out_path = std::env::temp_dir().join("oxideav_vdpau_mpeg2_ref.yuv");
    let _ = std::fs::remove_file(&out_path);
    let status = match std::process::Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-i", fixture_path, "-frames:v", "1",
               "-f", "rawvideo", "-pix_fmt", "yuv420p"])
        .arg(&out_path)
        .status()
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mpeg2_iframe_decode_matches_ffmpeg_reference: skipping — ffmpeg ({e})");
            return;
        }
    };
    if !status.success() {
        eprintln!("mpeg2_iframe_decode_matches_ffmpeg_reference: skipping — ffmpeg exit {status}");
        return;
    }
    let ref_yuv = match std::fs::read(&out_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("mpeg2_iframe_decode_matches_ffmpeg_reference: skipping — read ({e})");
            return;
        }
    };
    let _ = std::fs::remove_file(&out_path);
    let expected_len = 320 * 240 + 2 * (160 * 120);
    assert_eq!(ref_yuv.len(), expected_len, "ffmpeg reference YUV size");

    let decoder = Mpeg2VdpauDecoder::new(&device, FIXTURE).expect("decoder");
    let frame = decoder.decode_iframe(&device, FIXTURE).expect("decode");
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
        "mpeg2_iframe_decode_matches_ffmpeg_reference: mean abs diff Y={dy:.3} U={du:.3} V={dv:.3}"
    );
    assert!(dy < 20.0, "luma diff too large: {dy}");
    assert!(du < 20.0, "Cb diff too large: {du}");
    assert!(dv < 20.0, "Cr diff too large: {dv}");
}
