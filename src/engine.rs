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

use crate::device::{VdpDevice, VdpError};
use crate::profile::Profile;
use crate::Display;

/// One codec family we report. Each entry pins one representative
/// VDPAU profile to feed `VdpDecoderQueryCapabilities` for the
/// `max_width` / `max_height` / `max_macroblocks` numbers, plus the
/// fan-out list of profiles to enumerate into `HwCodecCaps::profiles`.
///
/// The labels reported into `HwCodecCaps::profiles` come from
/// [`Profile::label`] — a single source of truth shared with the typed
/// surface (see [`crate::profile`]).
struct CodecQuery {
    codec: &'static str,
    /// Representative profile — its caps drive `max_width`,
    /// `max_height`, `max_level`, `max_macroblocks`, and the top-level
    /// `decode` flag.
    representative: Profile,
    /// Profiles to fan out into `VdpDecoderQueryCapabilities`. Profiles
    /// the driver advertises as supported land in
    /// `HwCodecCaps::profiles` keyed by [`Profile::label`]; profiles the
    /// driver rejects are silently dropped.
    profiles: &'static [Profile],
}

const CODEC_QUERIES: &[CodecQuery] = &[
    CodecQuery {
        codec: "h264",
        representative: Profile::H264High,
        profiles: &[
            Profile::H264Baseline,
            Profile::H264Main,
            Profile::H264High,
            Profile::H264ConstrainedBaseline,
            Profile::H264Extended,
            Profile::H264ProgressiveHigh,
            Profile::H264ConstrainedHigh,
        ],
    },
    CodecQuery {
        codec: "hevc",
        representative: Profile::HevcMain,
        profiles: &[
            Profile::HevcMain,
            Profile::HevcMain10,
            Profile::HevcMainStill,
            Profile::HevcMain12,
        ],
    },
    CodecQuery {
        codec: "vp9",
        representative: Profile::Vp9Profile0,
        profiles: &[
            Profile::Vp9Profile0,
            Profile::Vp9Profile1,
            Profile::Vp9Profile2,
            Profile::Vp9Profile3,
        ],
    },
    CodecQuery {
        codec: "av1",
        representative: Profile::Av1Main,
        profiles: &[Profile::Av1Main, Profile::Av1High, Profile::Av1Professional],
    },
    CodecQuery {
        codec: "mpeg2",
        representative: Profile::Mpeg2Main,
        profiles: &[Profile::Mpeg2Simple, Profile::Mpeg2Main],
    },
    CodecQuery {
        codec: "vc1",
        representative: Profile::Vc1Advanced,
        profiles: &[Profile::Vc1Simple, Profile::Vc1Main, Profile::Vc1Advanced],
    },
    CodecQuery {
        codec: "mpeg4",
        representative: Profile::Mpeg4Part2Asp,
        profiles: &[Profile::Mpeg4Part2Sp, Profile::Mpeg4Part2Asp],
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

/// Validate that `index` is within range for the current host's VDPAU
/// device enumeration.
///
/// VDPAU exposes exactly one device per X display (`vdp_device_create_x11`
/// returns a single `VdpDevice` regardless of the number of physical
/// GPUs the driver sits on), so on a single-display box this means only
/// `0` is valid. The check still goes through [`engine_info`] so that
/// a future multi-display backend (or the no-VDPAU fallback) returns the
/// right error consistently.
///
/// Returns `Err` when:
///   - no VDPAU device is reachable on this host, or
///   - `index` is past the last enumerated device.
///
/// Hardware decoder factories should call this with
/// `params.device_index.unwrap_or(0)` before attempting to bind to a
/// device, so callers that pass a stale or made-up index get a clean
/// error instead of a confusing downstream `VdpDecoderCreate` failure.
#[cfg(feature = "registry")]
pub fn validate_device_index(index: u32) -> Result<(), VdpError> {
    let devices = engine_info();
    if devices.is_empty() {
        return Err(VdpError::other("no VDPAU device available"));
    }
    if (index as usize) >= devices.len() {
        return Err(VdpError::other(format!(
            "vdpau: device_index {index} out of range (0..{})",
            devices.len()
        )));
    }
    Ok(())
}

/// Stub matching [`validate_device_index`] for builds without the
/// `registry` feature: the helper has nothing to validate against (no
/// `engine_info`), so it always reports the index as accepted. The
/// non-registry crate is the raw FFI bridge — callers there don't go
/// through `CodecParameters` and shouldn't be exercising this path.
#[cfg(not(feature = "registry"))]
pub fn validate_device_index(_index: u32) -> Result<(), VdpError> {
    Ok(())
}

/// Walk `CODEC_QUERIES`, query the representative profile for the
/// top-level numbers, then loop through the fan-out profile list to
/// build the `profiles` field. Profiles the driver rejects (or that
/// the query call errors on) are dropped silently.
#[cfg(feature = "registry")]
fn build_codec_caps(device: &VdpDevice) -> Vec<HwCodecCaps> {
    let mut out = Vec::with_capacity(CODEC_QUERIES.len());
    for q in CODEC_QUERIES {
        let rep_caps = match device.decoder_caps(q.representative.as_raw()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut profiles: Vec<String> = Vec::new();
        for p in q.profiles {
            match device.decoder_caps(p.as_raw()) {
                Ok(c) if c.supported => profiles.push(p.label().to_string()),
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
        assert_eq!(
            parse_driver_version("Mesa VDPAU 1.0").as_deref(),
            Some("1.0")
        );
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
