//! Round 8 integration test: `register()` pushes four `CodecInfo`
//! entries (h264 / hevc / vp9 / mpeg2video) into the framework's
//! `RuntimeContext::codecs`, each tagged with `engine_id = "vdpau"` and
//! the `engine_info` on-demand probe.
//!
//! This test is **skip-friendly**: on a host without libvdpau /
//! libX11, the framework load fails inside `register()` and the
//! function logs + returns without pushing anything. We treat that as
//! "no `*_vdpau` rows registered" and skip.
//!
//! The test reads through the stable `oxideav-core` 0.1.x registry
//! introspection surface — `RuntimeContext::default()`,
//! `CodecRegistry::implementations(&CodecId)`, and the per-impl
//! `caps.implementation` string — so it stays green against the
//! published producer crate.

#![cfg(target_os = "linux")]
#![cfg(feature = "registry")]

use oxideav_core::{CodecCapabilities, CodecId, RuntimeContext};

/// Vdpau rows are the per-impl rows whose `caps.implementation` ends in
/// `_vdpau` (e.g. `h264_vdpau`, `hevc_vdpau`). The naming convention
/// lets the test identify our backend's rows without reaching for the
/// 0.2-only `engine_id` field on `CodecImplementation`.
fn count_vdpau_rows<'a>(ctx: &'a RuntimeContext, id: &CodecId) -> Vec<&'a CodecCapabilities> {
    ctx.codecs
        .implementations(id)
        .iter()
        .map(|i| &i.caps)
        .filter(|c| c.implementation.ends_with("_vdpau"))
        .collect()
}

fn register_once() -> RuntimeContext {
    let mut ctx = RuntimeContext::default();
    oxideav_vdpau::register(&mut ctx);
    ctx
}

/// `register()` pushes the expected four codecs when VDPAU is
/// reachable. Skip-friendly: if the framework can't load (no driver)
/// the registry stays empty for the vdpau subset and we return.
#[test]
fn register_pushes_four_codecs() {
    let ctx = register_once();

    let h264 = count_vdpau_rows(&ctx, &CodecId::new("h264")).len();
    let hevc = count_vdpau_rows(&ctx, &CodecId::new("hevc")).len();
    let vp9 = count_vdpau_rows(&ctx, &CodecId::new("vp9")).len();
    let mpeg2 = count_vdpau_rows(&ctx, &CodecId::new("mpeg2video")).len();
    let total = h264 + hevc + vp9 + mpeg2;
    if total == 0 {
        eprintln!(
            "register_pushes_four_codecs: skipping — framework load failed \
             (no libvdpau/libX11 on this host)"
        );
        return;
    }
    assert_eq!(h264, 1, "expected exactly one h264 vdpau row, got {h264}");
    assert_eq!(hevc, 1, "expected exactly one hevc vdpau row, got {hevc}");
    assert_eq!(vp9, 1, "expected exactly one vp9 vdpau row, got {vp9}");
    assert_eq!(
        mpeg2, 1,
        "expected exactly one mpeg2video vdpau row, got {mpeg2}"
    );
}

/// Confirm each registered row sets the expected capability flags:
/// `decode = true`, `hardware_accelerated = true`, `priority = 15`,
/// max_size 8192x8192, `implementation = "<codec>_vdpau"`.
#[test]
fn registered_capability_flags_match() {
    let ctx = register_once();

    let cid = CodecId::new("h264");
    let rows = count_vdpau_rows(&ctx, &cid);
    if rows.is_empty() {
        eprintln!("registered_capability_flags_match: skipping — framework load failed");
        return;
    }
    let caps = rows[0];
    assert!(caps.decode, "vdpau h264 row should advertise decode");
    assert!(
        !caps.encode,
        "vdpau has no encode side; encode flag should be false"
    );
    assert!(
        caps.hardware_accelerated,
        "vdpau is a HW backend; flag should be set"
    );
    assert_eq!(caps.priority, 15, "vdpau priority is 15 by convention");
    assert_eq!(caps.max_width, Some(8192));
    assert_eq!(caps.max_height, Some(8192));
    assert_eq!(caps.implementation, "h264_vdpau");
}

/// Confirm every registered codec advertises the same `_vdpau`
/// implementation suffix — so consumers grouping by suffix don't pick
/// up unrelated rows.
#[test]
fn registered_codecs_share_vdpau_suffix() {
    let ctx = register_once();

    for codec_id_str in ["h264", "hevc", "vp9", "mpeg2video"] {
        let cid = CodecId::new(codec_id_str);
        let rows = count_vdpau_rows(&ctx, &cid);
        if rows.is_empty() {
            // Skip-friendly: this test only meaningful with a live
            // framework. Don't fail when it's not present.
            eprintln!("registered_codecs_share_vdpau_suffix: skipping — framework load failed");
            return;
        }
        assert_eq!(rows.len(), 1);
        let expected_short = match codec_id_str {
            "mpeg2video" => "mpeg2",
            other => other,
        };
        assert_eq!(
            rows[0].implementation,
            format!("{expected_short}_vdpau"),
            "{codec_id_str}: implementation string mismatch"
        );
        assert!(
            rows[0].hardware_accelerated,
            "{codec_id_str}: hardware_accelerated should be true"
        );
    }
}
