//! Round 6 integration test: `engine_info()` enumerates the VDPAU
//! device + per-codec decode capabilities on this host.
//!
//! Skip-friendly: when no VDPAU device is reachable (no X server, no
//! NVIDIA / Mesa driver, sandboxed CI) the probe returns an empty
//! `Vec` and the test prints a diagnostic and returns. On the
//! NVIDIA workstation this crate targets, every assertion below
//! holds.

#![cfg(target_os = "linux")]

#[cfg(feature = "registry")]
#[test]
fn engine_info_reports_nvidia_vdpau_or_skips() {
    let devs = oxideav_vdpau::engine_info();
    if devs.is_empty() {
        eprintln!("No VDPAU device — skip");
        return;
    }
    let dev = &devs[0];
    eprintln!("device 0: {:?}", dev);
    assert!(!dev.name.is_empty(), "name non-empty");
    assert!(dev.api_version.is_some(), "API version reported");
    let h264 = dev.codecs.iter().find(|c| c.codec == "h264");
    assert!(h264.is_some(), "h264 entry");
    let h264 = h264.unwrap();
    assert!(h264.decode, "h264 decode supported");
    assert!(
        h264.max_width.unwrap_or(0) >= 1920,
        "h264 max_width >= 1920"
    );
    assert!(!h264.encode, "vdpau has no encode");
}
