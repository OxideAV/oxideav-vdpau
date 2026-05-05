//! On-demand VDPAU engine probe.
//!
//! [`engine_info`] is the [`oxideav_core::EngineProbeFn`] every codec
//! `CodecInfo` registered by this crate attaches via
//! [`oxideav_core::CodecInfo::with_engine_probe`]. Consumers (CLI
//! `info`, diagnostic tooling) call it on demand to enumerate the
//! VDPAU device this process can dispatch to and the per-codec
//! capabilities the driver advertises.
//!
//! VDPAU is per-X-display, not per-GPU: the entry point
//! `vdp_device_create_x11` returns a single `VdpDevice` for the
//! current `XDisplay*`, regardless of the number of physical GPUs the
//! NVIDIA / Mesa stack sits on. So [`engine_info`] returns at most
//! **one** [`HwDeviceInfo`] entry (a `Vec` is required by the API
//! contract — but our backend never produces more).
//!
//! The probe is **skip-friendly**: any failure to reach the X server,
//! load the VDPAU vtable, create the device, or query the dispatch
//! table returns an empty `Vec`. Consumers treat that as "this engine
//! is unavailable on this host" and move on. The probe is also
//! idempotent and side-effect free — it opens a fresh `Display` +
//! `VdpDevice`, queries everything, and tears them down before
//! returning.

#[cfg(feature = "registry")]
use oxideav_core::{HwCodecCaps, HwDeviceInfo};

use crate::Display;
use crate::device::VdpDevice;
use crate::sys;

/// One codec family we report. Each entry pins one representative
/// VDPAU profile to feed `VdpDecoderQueryCapabilities` for the
/// `max_width` / `max_height` / `max_macroblocks` numbers, plus the
/// fan-out list of profiles to enumerate into `HwCodecCaps::profiles`.
struct CodecQuery {
    codec: &'static str,
    /// Representative profile — its caps drive `max_width`,
    /// `max_height`, `max_level`, `max_macroblocks`, and the top-level
    /// `decode` flag.
    representative: sys::VdpDecoderProfile,
    /// `(profile_constant, profile_label)` pairs. Profiles that the
    /// driver advertises as supported land in `HwCodecCaps::profiles`.
    /// Profiles the driver rejects are silently dropped.
    profiles: &'static [(sys::VdpDecoderProfile, &'static str)],
}

const CODEC_QUERIES: &[CodecQuery] = &[
    CodecQuery {
        codec: "h264",
        representative: sys::VDP_DECODER_PROFILE_H264_HIGH,
        profiles: &[
            (sys::VDP_DECODER_PROFILE_H264_BASELINE, "Baseline"),
            (sys::VDP_DECODER_PROFILE_H264_MAIN, "Main"),
            (sys::VDP_DECODER_PROFILE_H264_HIGH, "High"),
            (
                sys::VDP_DECODER_PROFILE_H264_CONSTRAINED_BASELINE,
                "ConstrainedBaseline",
            ),
            (sys::VDP_DECODER_PROFILE_H264_EXTENDED, "Extended"),
            (
                sys::VDP_DECODER_PROFILE_H264_PROGRESSIVE_HIGH,
                "ProgressiveHigh",
            ),
            (
                sys::VDP_DECODER_PROFILE_H264_CONSTRAINED_HIGH,
                "ConstrainedHigh",
            ),
        ],
    },
    CodecQuery {
        codec: "hevc",
        representative: sys::VDP_DECODER_PROFILE_HEVC_MAIN,
        profiles: &[
            (sys::VDP_DECODER_PROFILE_HEVC_MAIN, "Main"),
            (sys::VDP_DECODER_PROFILE_HEVC_MAIN_10, "Main10"),
            (sys::VDP_DECODER_PROFILE_HEVC_MAIN_STILL, "MainStill"),
            (sys::VDP_DECODER_PROFILE_HEVC_MAIN_12, "Main12"),
        ],
    },
    CodecQuery {
        codec: "vp9",
        representative: sys::VDP_DECODER_PROFILE_VP9_PROFILE_0,
        profiles: &[
            (sys::VDP_DECODER_PROFILE_VP9_PROFILE_0, "0"),
            (sys::VDP_DECODER_PROFILE_VP9_PROFILE_1, "1"),
            (sys::VDP_DECODER_PROFILE_VP9_PROFILE_2, "2"),
            (sys::VDP_DECODER_PROFILE_VP9_PROFILE_3, "3"),
        ],
    },
    CodecQuery {
        codec: "av1",
        representative: sys::VDP_DECODER_PROFILE_AV1_MAIN,
        profiles: &[
            (sys::VDP_DECODER_PROFILE_AV1_MAIN, "Main"),
            (sys::VDP_DECODER_PROFILE_AV1_HIGH, "High"),
            (sys::VDP_DECODER_PROFILE_AV1_PROFESSIONAL, "Pro"),
        ],
    },
    CodecQuery {
        codec: "mpeg2",
        representative: sys::VDP_DECODER_PROFILE_MPEG2_MAIN,
        profiles: &[
            (sys::VDP_DECODER_PROFILE_MPEG2_SIMPLE, "Simple"),
            (sys::VDP_DECODER_PROFILE_MPEG2_MAIN, "Main"),
        ],
    },
    CodecQuery {
        codec: "vc1",
        representative: sys::VDP_DECODER_PROFILE_VC1_ADVANCED,
        profiles: &[
            (sys::VDP_DECODER_PROFILE_VC1_SIMPLE, "Simple"),
            (sys::VDP_DECODER_PROFILE_VC1_MAIN, "Main"),
            (sys::VDP_DECODER_PROFILE_VC1_ADVANCED, "Advanced"),
        ],
    },
    CodecQuery {
        codec: "mpeg4",
        representative: sys::VDP_DECODER_PROFILE_MPEG4_PART2_ASP,
        profiles: &[
            (sys::VDP_DECODER_PROFILE_MPEG4_PART2_SP, "SP"),
            (sys::VDP_DECODER_PROFILE_MPEG4_PART2_ASP, "ASP"),
        ],
    },
];

/// Extract the marketing name (everything up to the first newline,
/// trimmed). NVIDIA's banner is single-line in practice
/// (`"NVIDIA VDPAU Driver Shared Library  580.95.05  Sat Aug 16
/// 03:11:42 UTC 2025"`); on multi-line banners we keep just the first
/// line and trim trailing whitespace + multiple internal spaces stay
/// untouched (the banner double-space is informational and harmless).
fn parse_device_name(info: &str) -> String {
    info.lines().next().unwrap_or("").trim().to_string()
}

/// Pick the version-shaped token out of the banner. NVIDIA stamps the
/// driver version like `580.95.05`; we accept any whitespace-separated
/// token that starts with a digit and contains at least one `.` and
/// only digits / dots. Returns `None` if no such token is present.
fn parse_driver_version(info: &str) -> Option<String> {
    info.split_whitespace().find_map(|tok| {
        match tok.as_bytes().first() {
            Some(b) if b.is_ascii_digit() => {}
            _ => return None,
        }
        if !tok.contains('.') {
            return None;
        }
        if !tok.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
            return None;
        }
        Some(tok.to_string())
    })
}

/// Enumerate the VDPAU engine + its per-codec decode capabilities.
///
/// Returns a single-element `Vec<HwDeviceInfo>` on success, or an
/// empty `Vec` on any failure — no `$DISPLAY`, X unreachable, libvdpau
/// missing, `vdp_device_create_x11` failed, etc. Every error path is
/// silent: this is a probe, not a diagnostic.
///
/// VDPAU has no encode side, so every `HwCodecCaps::encode` is `false`
/// and `max_bit_depth` is left `None` (VDPAU `DecoderQueryCapabilities`
/// doesn't expose bit depth — the per-profile split between e.g.
/// HEVC_MAIN and HEVC_MAIN_10 is the de-facto bit-depth signal).
#[cfg(feature = "registry")]
pub fn engine_info() -> Vec<HwDeviceInfo> {
    // Open Display → device. Both can fail in the headless / no-NVIDIA
    // case; on failure we return an empty Vec.
    let display = match Display::open_from_env() {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let device = match display.create_vdp_device() {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };

    let info_string = device.information_string().unwrap_or_default();
    let api_version_num = device.api_version().ok();
    let name = {
        let n = parse_device_name(&info_string);
        if n.is_empty() {
            "VDPAU device".to_string()
        } else {
            n
        }
    };
    let driver_version = parse_driver_version(&info_string);
    let api_version = api_version_num.map(|v| format!("VDPAU API {v}"));

    let mut extra: Vec<(String, String)> = Vec::new();
    if !info_string.is_empty() {
        extra.push(("information_string".into(), info_string.clone()));
    }

    let codecs = build_codec_caps(&device);

    vec![HwDeviceInfo {
        name,
        driver_version,
        api_version,
        total_memory_bytes: None,
        extra,
        codecs,
    }]
}

/// `cfg(not(feature = "registry"))` stub so the module still compiles
/// without `oxideav-core`. The registry-less crate has no consumer for
/// `HwDeviceInfo` so we expose nothing.
#[cfg(not(feature = "registry"))]
pub fn engine_info() {}

/// Walk `CODEC_QUERIES`, query the representative profile for the
/// top-level numbers, then loop through the fan-out profile list to
/// build the `profiles` field. Profiles the driver rejects (or that
/// the query call errors on) are dropped silently.
#[cfg(feature = "registry")]
fn build_codec_caps(device: &VdpDevice) -> Vec<HwCodecCaps> {
    let mut out = Vec::with_capacity(CODEC_QUERIES.len());
    for q in CODEC_QUERIES {
        let rep_caps = match device.decoder_caps(q.representative) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut profiles: Vec<String> = Vec::new();
        for (p, label) in q.profiles {
            match device.decoder_caps(*p) {
                Ok(c) if c.supported => profiles.push((*label).to_string()),
                _ => {}
            }
        }
        out.push(HwCodecCaps {
            codec: q.codec.to_string(),
            decode: rep_caps.supported,
            encode: false,
            max_width: Some(rep_caps.max_width),
            max_height: Some(rep_caps.max_height),
            max_bit_depth: None,
            profiles,
            extra: vec![
                (
                    "max_macroblocks".to_string(),
                    rep_caps.max_macroblocks.to_string(),
                ),
                ("max_level".to_string(), rep_caps.max_level.to_string()),
            ],
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_device_name_takes_first_line_trimmed() {
        let s = "NVIDIA VDPAU Driver Shared Library  580.95.05  Sat Aug 16 03:11:42 UTC 2025";
        let n = parse_device_name(s);
        assert_eq!(
            n,
            "NVIDIA VDPAU Driver Shared Library  580.95.05  Sat Aug 16 03:11:42 UTC 2025"
        );
    }

    #[test]
    fn parse_device_name_handles_multiline_and_whitespace() {
        let s = "  Acme VDPAU 1.0   \nthe rest is junk\n";
        let n = parse_device_name(s);
        assert_eq!(n, "Acme VDPAU 1.0");
    }

    #[test]
    fn parse_device_name_empty_string_is_empty() {
        assert_eq!(parse_device_name(""), "");
    }

    #[test]
    fn parse_driver_version_picks_dotted_number() {
        let s = "NVIDIA VDPAU Driver Shared Library  580.95.05  Sat Aug 16";
        assert_eq!(parse_driver_version(s).as_deref(), Some("580.95.05"));
    }

    #[test]
    fn parse_driver_version_short_form_works() {
        assert_eq!(parse_driver_version("Mesa VDPAU 1.0").as_deref(), Some("1.0"));
    }

    #[test]
    fn parse_driver_version_returns_none_when_absent() {
        assert!(parse_driver_version("VDPAU placeholder banner").is_none());
        assert!(parse_driver_version("").is_none());
    }

    #[test]
    fn parse_driver_version_skips_non_dot_numbers() {
        // "16" is digits-only — no dot, so not a version.
        assert!(parse_driver_version("foo 16 bar").is_none());
    }
}
