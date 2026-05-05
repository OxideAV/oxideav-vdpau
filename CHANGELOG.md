# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added — Round 2

- `libX11.so.6` is now dlopened alongside `libvdpau.so.1`; the
  `Vtable` resolves `XOpenDisplay`, `XCloseDisplay`, and
  `XDefaultScreen` so the bridge no longer needs a compile-time
  Xlib link dependency.
- VDPAU function-ID constants (`VDP_FUNC_ID_GET_API_VERSION`,
  `_GET_INFORMATION_STRING`, `_DEVICE_DESTROY`,
  `_DECODER_QUERY_CAPABILITIES`, `_DECODER_CREATE`, `_DECODER_DESTROY`,
  `_DECODER_RENDER`) and the common `VdpDecoderProfile` constants
  (H.264 Baseline/Main/High + variants, MPEG-2, VC-1, MPEG-4 Pt 2,
  VP9 0..3, HEVC Main/Main10/Still/Main12, AV1 Main/High/Professional)
  are exposed in `sys`.
- VDPAU function pointer typedefs for the post-bootstrap entries:
  `FnVdpGetApiVersion`, `FnVdpGetInformationString`,
  `FnVdpDeviceDestroy`, `FnVdpDecoderQueryCapabilities`.
- New `device` module with safe wrappers:
  - `Display` owns an `XDisplay*`; opens via `Display::open` /
    `Display::open_from_env`; `Drop` calls `XCloseDisplay`.
  - `VdpDevice` owns a `VdpDevice` handle plus the resolved
    post-bootstrap dispatch table; created via
    `Display::create_vdp_device`; `Drop` calls `VdpDeviceDestroy`.
    Methods: `information_string()`, `api_version()`,
    `decoder_caps(profile)`.
  - `DecoderCaps` carries the five outputs of
    `VdpDecoderQueryCapabilities` (supported bool + max level /
    macroblocks / width / height).
  - `VdpError` wraps `VdpStatus` with a contextual message.
- Integration test suite `tests/round2_init.rs` — five skip-friendly
  end-to-end tests: display opens, device creates, info string +
  API version, H.264 High capability query.

### Added — Round 1

- Initial scaffolding: `#![cfg(target_os = "linux")]` crate that
  dlopens `libvdpau.so.1` via `libloading` on first use.
- `sys.rs` exposes opaque type aliases (`VdpDevice`, `VdpDecoder`,
  `VdpStatus`, `VdpFuncId`, `VdpGetProcAddress`) and a resolved
  `Vtable` covering the single bootstrap symbol VDPAU exports
  directly (`vdp_device_create_x11`); every other VDPAU entry is
  reached via the `VdpGetProcAddress` returned by device creation,
  to be wired up in Round 2.
- Process-wide `OnceLock<Result<Vtable, String>>` cache so the
  dlopen + dlsym round-trip happens at most once per process.
- Unified `register(&mut RuntimeContext)` entry point. Round 1: the
  function confirms the library loads and returns; no codec
  factories are wired up yet. If load fails (no libvdpau, no X
  display, etc.) the function logs and returns — the pure-Rust
  codec path remains the only resolution candidate.
- Standalone-friendly `registry` feature (default-on) gates the
  `oxideav-core` + `linkme` deps.
- README coverage roadmap and priority explanation.
- Smoke tests: `frameworks_load` and `vtable_resolves` confirm
  symbol resolution on Linux machines that have the VDPAU driver
  stack installed.
