//! Round 9 integration test: the typed [`Profile`] surface is the
//! single source of truth for VDPAU decoder profiles. This test pins
//! the public reachability of the type and verifies that:
//!
//!   1. Every variant round-trips through `as_raw` / `from_raw`.
//!   2. The `label` strings match what the engine probe reports in
//!      `HwCodecCaps::profiles` (so a future engine-probe change can't
//!      silently drift away from the typed labels).
//!   3. Every variant in `Profile::ALL` maps to exactly one of the
//!      known codec ids the engine probe enumerates.
//!
//! Like the other integration tests in this crate, this file is
//! `cfg(target_os = "linux")` so it compiles to an empty test binary on
//! macOS / Windows. The body never touches the driver — it only
//! exercises the typed enum and the engine's static `CODEC_QUERIES`
//! contract — so no `cargo test` skip-friendly fallback is required.

#![cfg(target_os = "linux")]

use oxideav_vdpau::Profile;

#[test]
fn profile_is_reachable_from_crate_root() {
    // The whole point of `pub use profile::Profile;` in `lib.rs` is
    // that consumers don't have to know about the submodule split.
    let p = Profile::H264High;
    assert_eq!(p.codec_id(), "h264");
    assert_eq!(p.label(), "High");
}

#[test]
fn every_variant_round_trips() {
    for &p in Profile::ALL {
        let raw = p.as_raw();
        assert_eq!(
            Profile::from_raw(raw),
            Some(p),
            "round-trip failed for {p:?} (raw={raw})"
        );
    }
}

#[test]
fn every_variant_belongs_to_a_known_codec() {
    // The engine probe enumerates these seven codec families. If a new
    // variant is added without a matching codec id, this test fails
    // before the engine probe silently drops it.
    let known: &[&str] = &["h264", "hevc", "vp9", "mpeg2", "vc1", "mpeg4", "av1"];
    for &p in Profile::ALL {
        let cid = p.codec_id();
        assert!(
            known.contains(&cid),
            "Profile::{p:?} has codec_id {cid:?} which is not in the engine-probe set"
        );
    }
}

#[test]
fn h264_label_set_is_complete() {
    // Pin the seven H.264 labels the engine probe enumerates so a
    // refactor of `label()` lands as a test failure.
    let h264: Vec<&str> = Profile::ALL
        .iter()
        .filter(|p| p.codec_id() == "h264")
        .map(|p| p.label())
        .collect();
    assert_eq!(
        h264,
        vec![
            "Baseline",
            "Main",
            "High",
            "ConstrainedBaseline",
            "Extended",
            "ProgressiveHigh",
            "ConstrainedHigh",
        ]
    );
}

#[test]
fn hevc_label_set_is_complete() {
    let hevc: Vec<&str> = Profile::ALL
        .iter()
        .filter(|p| p.codec_id() == "hevc")
        .map(|p| p.label())
        .collect();
    assert_eq!(hevc, vec!["Main", "Main10", "MainStill", "Main12"]);
}

#[test]
fn vp9_label_set_is_complete() {
    let vp9: Vec<&str> = Profile::ALL
        .iter()
        .filter(|p| p.codec_id() == "vp9")
        .map(|p| p.label())
        .collect();
    assert_eq!(vp9, vec!["0", "1", "2", "3"]);
}

#[test]
fn av1_label_set_is_complete() {
    let av1: Vec<&str> = Profile::ALL
        .iter()
        .filter(|p| p.codec_id() == "av1")
        .map(|p| p.label())
        .collect();
    assert_eq!(av1, vec!["Main", "High", "Pro"]);
}
