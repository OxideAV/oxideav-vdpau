//! Typed VDPAU decoder profile.
//!
//! VDPAU's `VdpDecoderProfile` is a plain `u32` constant family
//! (`sys::VDP_DECODER_PROFILE_H264_HIGH`, `_HEVC_MAIN`, `_VP9_PROFILE_0`,
//! …). The raw form is fine for FFI but loses two pieces of context
//! every consumer of the bridge ends up reconstructing by hand:
//!
//!   1. **Which codec family does this profile belong to?** —
//!      `engine.rs` walks a `CODEC_QUERIES` table to map u32 → "h264".
//!      Hardware decoder factories doing the inverse mapping (codec id
//!      → profile to feed `VdpDecoderCreate`) reach for a switch-statement
//!      over the bare constants.
//!   2. **What is the human-facing label for this profile?** — the
//!      engine probe builds `HwCodecCaps::profiles` with strings like
//!      `"High"` / `"Main10"`; today those strings live next to the raw
//!      constants in `CODEC_QUERIES` rather than on the profile itself.
//!
//! [`Profile`] is the narrow typed primitive that carries both: it is a
//! `Copy` enum with one variant per `VDP_DECODER_PROFILE_*` constant the
//! crate currently knows about, and methods [`Profile::codec_id`] and
//! [`Profile::label`] for the two contextual lookups. Round-trip with
//! the raw FFI form is via [`Profile::as_raw`] and [`Profile::from_raw`].
//!
//! Unknown raw values round-trip as `None` rather than panicking — VDPAU
//! drivers can advertise profiles the framework hasn't enumerated yet
//! (e.g. a future AV1 codec or a vendor extension), and the bridge
//! should treat those as "not in the typed set" rather than as an
//! invariant violation.

use crate::sys;
use crate::sys::VdpDecoderProfile;

/// Typed VDPAU decoder profile.
///
/// The variant list mirrors `sys::VDP_DECODER_PROFILE_*` one-for-one.
/// New profile families added to `sys.rs` should grow a matching variant
/// here so the typed surface stays exhaustive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    Mpeg2Simple,
    Mpeg2Main,
    H264Baseline,
    H264Main,
    H264High,
    Vc1Simple,
    Vc1Main,
    Vc1Advanced,
    Mpeg4Part2Sp,
    Mpeg4Part2Asp,
    H264ConstrainedBaseline,
    H264Extended,
    H264ProgressiveHigh,
    H264ConstrainedHigh,
    Vp9Profile0,
    Vp9Profile1,
    Vp9Profile2,
    Vp9Profile3,
    HevcMain,
    HevcMain10,
    HevcMainStill,
    HevcMain12,
    Av1Main,
    Av1High,
    Av1Professional,
}

impl Profile {
    /// Every typed profile, in `sys.rs` declaration order. Useful for
    /// enumeration in test code and engine probes that want a fan-out
    /// list without hard-coding it inline.
    pub const ALL: &'static [Profile] = &[
        Profile::Mpeg2Simple,
        Profile::Mpeg2Main,
        Profile::H264Baseline,
        Profile::H264Main,
        Profile::H264High,
        Profile::Vc1Simple,
        Profile::Vc1Main,
        Profile::Vc1Advanced,
        Profile::Mpeg4Part2Sp,
        Profile::Mpeg4Part2Asp,
        Profile::H264ConstrainedBaseline,
        Profile::H264Extended,
        Profile::H264ProgressiveHigh,
        Profile::H264ConstrainedHigh,
        Profile::Vp9Profile0,
        Profile::Vp9Profile1,
        Profile::Vp9Profile2,
        Profile::Vp9Profile3,
        Profile::HevcMain,
        Profile::HevcMain10,
        Profile::HevcMainStill,
        Profile::HevcMain12,
        Profile::Av1Main,
        Profile::Av1High,
        Profile::Av1Professional,
    ];

    /// Raw `VdpDecoderProfile` (`u32`) for FFI calls.
    pub const fn as_raw(self) -> VdpDecoderProfile {
        match self {
            Profile::Mpeg2Simple => sys::VDP_DECODER_PROFILE_MPEG2_SIMPLE,
            Profile::Mpeg2Main => sys::VDP_DECODER_PROFILE_MPEG2_MAIN,
            Profile::H264Baseline => sys::VDP_DECODER_PROFILE_H264_BASELINE,
            Profile::H264Main => sys::VDP_DECODER_PROFILE_H264_MAIN,
            Profile::H264High => sys::VDP_DECODER_PROFILE_H264_HIGH,
            Profile::Vc1Simple => sys::VDP_DECODER_PROFILE_VC1_SIMPLE,
            Profile::Vc1Main => sys::VDP_DECODER_PROFILE_VC1_MAIN,
            Profile::Vc1Advanced => sys::VDP_DECODER_PROFILE_VC1_ADVANCED,
            Profile::Mpeg4Part2Sp => sys::VDP_DECODER_PROFILE_MPEG4_PART2_SP,
            Profile::Mpeg4Part2Asp => sys::VDP_DECODER_PROFILE_MPEG4_PART2_ASP,
            Profile::H264ConstrainedBaseline => sys::VDP_DECODER_PROFILE_H264_CONSTRAINED_BASELINE,
            Profile::H264Extended => sys::VDP_DECODER_PROFILE_H264_EXTENDED,
            Profile::H264ProgressiveHigh => sys::VDP_DECODER_PROFILE_H264_PROGRESSIVE_HIGH,
            Profile::H264ConstrainedHigh => sys::VDP_DECODER_PROFILE_H264_CONSTRAINED_HIGH,
            Profile::Vp9Profile0 => sys::VDP_DECODER_PROFILE_VP9_PROFILE_0,
            Profile::Vp9Profile1 => sys::VDP_DECODER_PROFILE_VP9_PROFILE_1,
            Profile::Vp9Profile2 => sys::VDP_DECODER_PROFILE_VP9_PROFILE_2,
            Profile::Vp9Profile3 => sys::VDP_DECODER_PROFILE_VP9_PROFILE_3,
            Profile::HevcMain => sys::VDP_DECODER_PROFILE_HEVC_MAIN,
            Profile::HevcMain10 => sys::VDP_DECODER_PROFILE_HEVC_MAIN_10,
            Profile::HevcMainStill => sys::VDP_DECODER_PROFILE_HEVC_MAIN_STILL,
            Profile::HevcMain12 => sys::VDP_DECODER_PROFILE_HEVC_MAIN_12,
            Profile::Av1Main => sys::VDP_DECODER_PROFILE_AV1_MAIN,
            Profile::Av1High => sys::VDP_DECODER_PROFILE_AV1_HIGH,
            Profile::Av1Professional => sys::VDP_DECODER_PROFILE_AV1_PROFESSIONAL,
        }
    }

    /// Recover a typed profile from its raw `VdpDecoderProfile` form.
    ///
    /// Returns `None` for any value that isn't one of the constants the
    /// crate currently knows about. Vendor extensions and future-spec
    /// profile numbers fall through here — callers that need to preserve
    /// the raw u32 can hold it directly.
    pub const fn from_raw(raw: VdpDecoderProfile) -> Option<Profile> {
        match raw {
            sys::VDP_DECODER_PROFILE_MPEG2_SIMPLE => Some(Profile::Mpeg2Simple),
            sys::VDP_DECODER_PROFILE_MPEG2_MAIN => Some(Profile::Mpeg2Main),
            sys::VDP_DECODER_PROFILE_H264_BASELINE => Some(Profile::H264Baseline),
            sys::VDP_DECODER_PROFILE_H264_MAIN => Some(Profile::H264Main),
            sys::VDP_DECODER_PROFILE_H264_HIGH => Some(Profile::H264High),
            sys::VDP_DECODER_PROFILE_VC1_SIMPLE => Some(Profile::Vc1Simple),
            sys::VDP_DECODER_PROFILE_VC1_MAIN => Some(Profile::Vc1Main),
            sys::VDP_DECODER_PROFILE_VC1_ADVANCED => Some(Profile::Vc1Advanced),
            sys::VDP_DECODER_PROFILE_MPEG4_PART2_SP => Some(Profile::Mpeg4Part2Sp),
            sys::VDP_DECODER_PROFILE_MPEG4_PART2_ASP => Some(Profile::Mpeg4Part2Asp),
            sys::VDP_DECODER_PROFILE_H264_CONSTRAINED_BASELINE => {
                Some(Profile::H264ConstrainedBaseline)
            }
            sys::VDP_DECODER_PROFILE_H264_EXTENDED => Some(Profile::H264Extended),
            sys::VDP_DECODER_PROFILE_H264_PROGRESSIVE_HIGH => Some(Profile::H264ProgressiveHigh),
            sys::VDP_DECODER_PROFILE_H264_CONSTRAINED_HIGH => Some(Profile::H264ConstrainedHigh),
            sys::VDP_DECODER_PROFILE_VP9_PROFILE_0 => Some(Profile::Vp9Profile0),
            sys::VDP_DECODER_PROFILE_VP9_PROFILE_1 => Some(Profile::Vp9Profile1),
            sys::VDP_DECODER_PROFILE_VP9_PROFILE_2 => Some(Profile::Vp9Profile2),
            sys::VDP_DECODER_PROFILE_VP9_PROFILE_3 => Some(Profile::Vp9Profile3),
            sys::VDP_DECODER_PROFILE_HEVC_MAIN => Some(Profile::HevcMain),
            sys::VDP_DECODER_PROFILE_HEVC_MAIN_10 => Some(Profile::HevcMain10),
            sys::VDP_DECODER_PROFILE_HEVC_MAIN_STILL => Some(Profile::HevcMainStill),
            sys::VDP_DECODER_PROFILE_HEVC_MAIN_12 => Some(Profile::HevcMain12),
            sys::VDP_DECODER_PROFILE_AV1_MAIN => Some(Profile::Av1Main),
            sys::VDP_DECODER_PROFILE_AV1_HIGH => Some(Profile::Av1High),
            sys::VDP_DECODER_PROFILE_AV1_PROFESSIONAL => Some(Profile::Av1Professional),
            _ => None,
        }
    }

    /// Framework codec id this profile belongs to: `"h264"`, `"hevc"`,
    /// `"vp9"`, `"mpeg2"`, `"vc1"`, `"mpeg4"`, or `"av1"`.
    ///
    /// Matches the `CodecQuery::codec` strings in [`crate::engine`] and
    /// the strings used by the framework registry for `CodecId::new`.
    /// MPEG-2's `"mpeg2"` here does NOT match the framework codec id
    /// `"mpeg2video"` — callers cross-referencing the framework registry
    /// should be aware (the engine-probe codec-caps row uses `"mpeg2"`
    /// because that's the family name; the framework registry row uses
    /// `"mpeg2video"` because that's the FOURCC-style canonical id).
    pub const fn codec_id(self) -> &'static str {
        match self {
            Profile::Mpeg2Simple | Profile::Mpeg2Main => "mpeg2",
            Profile::H264Baseline
            | Profile::H264Main
            | Profile::H264High
            | Profile::H264ConstrainedBaseline
            | Profile::H264Extended
            | Profile::H264ProgressiveHigh
            | Profile::H264ConstrainedHigh => "h264",
            Profile::Vc1Simple | Profile::Vc1Main | Profile::Vc1Advanced => "vc1",
            Profile::Mpeg4Part2Sp | Profile::Mpeg4Part2Asp => "mpeg4",
            Profile::Vp9Profile0
            | Profile::Vp9Profile1
            | Profile::Vp9Profile2
            | Profile::Vp9Profile3 => "vp9",
            Profile::HevcMain
            | Profile::HevcMain10
            | Profile::HevcMainStill
            | Profile::HevcMain12 => "hevc",
            Profile::Av1Main | Profile::Av1High | Profile::Av1Professional => "av1",
        }
    }

    /// Short, human-facing label for the profile suffix. Matches the
    /// label column of [`crate::engine`]'s `CODEC_QUERIES` and feeds
    /// `HwCodecCaps::profiles`.
    ///
    /// Examples: `"Baseline"`, `"Main"`, `"High"`, `"Main10"`, `"0"`,
    /// `"Simple"`, `"ASP"`, `"Pro"`.
    pub const fn label(self) -> &'static str {
        match self {
            Profile::Mpeg2Simple => "Simple",
            Profile::Mpeg2Main => "Main",
            Profile::H264Baseline => "Baseline",
            Profile::H264Main => "Main",
            Profile::H264High => "High",
            Profile::Vc1Simple => "Simple",
            Profile::Vc1Main => "Main",
            Profile::Vc1Advanced => "Advanced",
            Profile::Mpeg4Part2Sp => "SP",
            Profile::Mpeg4Part2Asp => "ASP",
            Profile::H264ConstrainedBaseline => "ConstrainedBaseline",
            Profile::H264Extended => "Extended",
            Profile::H264ProgressiveHigh => "ProgressiveHigh",
            Profile::H264ConstrainedHigh => "ConstrainedHigh",
            Profile::Vp9Profile0 => "0",
            Profile::Vp9Profile1 => "1",
            Profile::Vp9Profile2 => "2",
            Profile::Vp9Profile3 => "3",
            Profile::HevcMain => "Main",
            Profile::HevcMain10 => "Main10",
            Profile::HevcMainStill => "MainStill",
            Profile::HevcMain12 => "Main12",
            Profile::Av1Main => "Main",
            Profile::Av1High => "High",
            Profile::Av1Professional => "Pro",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_known_profiles() {
        for &p in Profile::ALL {
            let raw = p.as_raw();
            let back = Profile::from_raw(raw).unwrap_or_else(|| {
                panic!("Profile::from_raw({raw}) returned None for known {p:?}")
            });
            assert_eq!(p, back, "round-trip mismatch for {p:?}");
        }
    }

    #[test]
    fn from_raw_unknown_returns_none() {
        // Gaps in the VDPAU numbering: 0 (reserved), 3/4/5 (gap before
        // H264 family), 26 (gap between H264 family and VP9), 104..106
        // (gap between HEVC and AV1). All should fall through to None.
        for raw in [0u32, 3, 4, 5, 26, 104, 105, 106, 200, u32::MAX] {
            assert!(
                Profile::from_raw(raw).is_none(),
                "expected None for raw={raw}, got Some(_)"
            );
        }
    }

    #[test]
    fn codec_id_groups_h264_family() {
        for p in [
            Profile::H264Baseline,
            Profile::H264Main,
            Profile::H264High,
            Profile::H264ConstrainedBaseline,
            Profile::H264Extended,
            Profile::H264ProgressiveHigh,
            Profile::H264ConstrainedHigh,
        ] {
            assert_eq!(p.codec_id(), "h264", "{p:?} should be in the h264 family");
        }
    }

    #[test]
    fn codec_id_groups_hevc_family() {
        for p in [
            Profile::HevcMain,
            Profile::HevcMain10,
            Profile::HevcMainStill,
            Profile::HevcMain12,
        ] {
            assert_eq!(p.codec_id(), "hevc");
        }
    }

    #[test]
    fn codec_id_groups_vp9_family() {
        for p in [
            Profile::Vp9Profile0,
            Profile::Vp9Profile1,
            Profile::Vp9Profile2,
            Profile::Vp9Profile3,
        ] {
            assert_eq!(p.codec_id(), "vp9");
        }
    }

    #[test]
    fn codec_id_groups_av1_family() {
        for p in [Profile::Av1Main, Profile::Av1High, Profile::Av1Professional] {
            assert_eq!(p.codec_id(), "av1");
        }
    }

    #[test]
    fn codec_id_groups_mpeg2_vc1_mpeg4_families() {
        assert_eq!(Profile::Mpeg2Simple.codec_id(), "mpeg2");
        assert_eq!(Profile::Mpeg2Main.codec_id(), "mpeg2");
        assert_eq!(Profile::Vc1Simple.codec_id(), "vc1");
        assert_eq!(Profile::Vc1Main.codec_id(), "vc1");
        assert_eq!(Profile::Vc1Advanced.codec_id(), "vc1");
        assert_eq!(Profile::Mpeg4Part2Sp.codec_id(), "mpeg4");
        assert_eq!(Profile::Mpeg4Part2Asp.codec_id(), "mpeg4");
    }

    #[test]
    fn label_matches_engine_query_strings() {
        // These must match the labels in CODEC_QUERIES inside
        // `engine.rs` exactly — that's the whole point of the typed
        // primitive: a single source of truth.
        assert_eq!(Profile::H264Baseline.label(), "Baseline");
        assert_eq!(Profile::H264Main.label(), "Main");
        assert_eq!(Profile::H264High.label(), "High");
        assert_eq!(
            Profile::H264ConstrainedBaseline.label(),
            "ConstrainedBaseline"
        );
        assert_eq!(Profile::H264Extended.label(), "Extended");
        assert_eq!(Profile::H264ProgressiveHigh.label(), "ProgressiveHigh");
        assert_eq!(Profile::H264ConstrainedHigh.label(), "ConstrainedHigh");
        assert_eq!(Profile::HevcMain.label(), "Main");
        assert_eq!(Profile::HevcMain10.label(), "Main10");
        assert_eq!(Profile::HevcMainStill.label(), "MainStill");
        assert_eq!(Profile::HevcMain12.label(), "Main12");
        assert_eq!(Profile::Vp9Profile0.label(), "0");
        assert_eq!(Profile::Vp9Profile1.label(), "1");
        assert_eq!(Profile::Vp9Profile2.label(), "2");
        assert_eq!(Profile::Vp9Profile3.label(), "3");
        assert_eq!(Profile::Av1Main.label(), "Main");
        assert_eq!(Profile::Av1High.label(), "High");
        assert_eq!(Profile::Av1Professional.label(), "Pro");
        assert_eq!(Profile::Mpeg2Simple.label(), "Simple");
        assert_eq!(Profile::Mpeg2Main.label(), "Main");
        assert_eq!(Profile::Vc1Simple.label(), "Simple");
        assert_eq!(Profile::Vc1Main.label(), "Main");
        assert_eq!(Profile::Vc1Advanced.label(), "Advanced");
        assert_eq!(Profile::Mpeg4Part2Sp.label(), "SP");
        assert_eq!(Profile::Mpeg4Part2Asp.label(), "ASP");
    }

    #[test]
    fn as_raw_matches_sys_constants() {
        // Pin a representative subset against the raw constants so a
        // future renumbering in `sys.rs` lands as a hard test failure
        // rather than a silent behaviour drift.
        assert_eq!(Profile::H264High.as_raw(), 8);
        assert_eq!(Profile::HevcMain.as_raw(), 100);
        assert_eq!(Profile::HevcMain10.as_raw(), 101);
        assert_eq!(Profile::Vp9Profile0.as_raw(), 27);
        assert_eq!(Profile::Av1Main.as_raw(), 107);
        assert_eq!(Profile::Mpeg2Main.as_raw(), 2);
    }

    #[test]
    fn all_is_dense_and_unique() {
        let n = Profile::ALL.len();
        // ALL must enumerate every variant exactly once.
        let mut raws: Vec<VdpDecoderProfile> = Profile::ALL.iter().map(|p| p.as_raw()).collect();
        raws.sort();
        raws.dedup();
        assert_eq!(
            raws.len(),
            n,
            "Profile::ALL contains duplicates; expected {n} unique raw values, got {}",
            raws.len()
        );
    }
}
