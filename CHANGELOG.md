# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added — Round 8

- `register()` now pushes four `CodecInfo` entries into
  `RuntimeContext::codecs` — one each for `h264`, `hevc`, `vp9`, and
  `mpeg2video`. Every entry advertises:
    - `CodecCapabilities::video("<codec>_vdpau")` with `with_decode()`,
      `with_lossy(true)`, `with_hardware(true)`, `with_priority(15)`
      (slightly higher / worse than VA-API's 10 because VA-API has the
      better Linux driver story today), and `with_max_size(8192, 8192)`
      as a generous pre-filter bound — the per-profile live maxima
      still come from `engine_info()` on demand;
    - Container tags so the registry resolver picks the VDPAU bridge
      up from FourCC and Matroska codec-id lookups — H.264 (`H264` /
      `h264` / `AVC1` / `avc1` / `X264` / `V_MPEG4/ISO/AVC`), HEVC
      (`hvc1` / `hev1` / `HEVC` / `H265` / `V_MPEGH/ISO/HEVC`), VP9
      (`VP90` / `vp09` / `V_VP9`), MPEG-2 (`MPG2` / `mpg2` / `M2V ` /
      `V_MPEG2`);
    - `with_engine_id("vdpau")` + `with_engine_probe(engine_info)` so
      `oxideav info` groups the rows by VDPAU device and the CLI can
      dedupe the probe call by engine id.
- Framework-load pre-flight: if `libvdpau.so.1` or `libX11.so.6` cannot
  be loaded, `register()` logs once and returns without pushing any
  rows — pure-Rust fallbacks then remain the only resolution candidates.
- `decoder_factory` is intentionally still unset on every row: the
  existing direct constructors (`H264VdpauDecoder::with_params`,
  `HevcVdpauDecoder::with_params`, `Vp9VdpauDecoder::with_params`,
  `Mpeg2VdpauDecoder::with_params`) honour
  `CodecParameters::device_index`, but adapting them into streaming
  `dyn oxideav_core::Decoder` impls (`send_packet` / `receive_frame`
  with cached SPS-PPS / per-codec parser state) lands per-codec in
  follow-up rounds rather than as one omnibus drop. The follow-up will
  read `device_index` from `CodecParameters` via
  `validate_device_index`, then thread through the matching
  `with_params` constructor.
- Integration test `tests/round8_codec_registry.rs`: skip-friendly
  tests that confirm each of the four codec ids exposes exactly one
  `*_vdpau` implementation row, that the capability flags on the
  H.264 row match the registration (decode/hw/priority/max_size), and
  that every row carries the expected `<codec>_vdpau` implementation
  suffix. All tests skip cleanly with a diagnostic `eprintln!` when
  the framework load fails (no driver on the runner).

## [0.0.2](https://github.com/OxideAV/oxideav-vdpau/compare/v0.0.1...v0.0.2) - 2026-05-06

### Other

- apply rustfmt layout + Default::default field-assign + div_ceil + redundant-cast
- skip frameworks_load + vtable_resolves on hosts without the driver
- validate CodecParameters::device_index against engine_info() count
- implement engine_info() — enumerate per-codec VDPAU decoder caps

### Added — Round 7

- New helper `pub fn validate_device_index(index: u32) -> Result<(), VdpError>`
  in the `engine` module (re-exported at the crate root). Validates a
  caller-supplied device index against the live host enumeration: empty
  `engine_info()` → `"no VDPAU device available"`; index past the last
  enumerated device → `"vdpau: device_index N out of range (0..M)"`. VDPAU
  exposes one device per X display, so on a single-display host only
  index `0` passes.
- Public decoder constructors honour `oxideav_core::CodecParameters::device_index`:
  every decoder (`H264VdpauDecoder`, `HevcVdpauDecoder`, `Vp9VdpauDecoder`,
  `Mpeg2VdpauDecoder`) now exposes a `with_params(&VdpDevice, &CodecParameters,
  &[u8]) -> Result<Self, VdpError>` overload alongside the existing `new`.
  It reads `params.device_index.unwrap_or(0)`, runs `validate_device_index`,
  and delegates to `new`. `None` (default) and `Some(0)` are accepted; any
  other value yields a clean error before any VDPAU FFI is touched. The
  pre-existing `new(&VdpDevice, &[u8])` API is unchanged — `with_params`
  is purely additive so existing callers don't churn.
- Integration test `tests/round7_device_index.rs`: skip-friendly tests
  that `validate_device_index(0)` is `Ok` and `validate_device_index(99)`
  errors with `"out of range"`; that `H264VdpauDecoder::with_params`
  constructs successfully with `device_index = None` and
  `device_index = Some(0)`; and that `device_index = Some(7)` is
  rejected before any bitstream parsing happens.

Wiring `make_*` factories into `oxideav-core::CodecRegistry` is deferred
to a follow-up round — the crate's `register()` body still doesn't push
`CodecInfo` entries (decoders are exposed as direct structs). Once the
registry side lands, the factories will read `device_index` from
`CodecParameters` via the same helper.

### Added — Round 6

- New `engine` module exposing `pub fn engine_info() -> Vec<HwDeviceInfo>`,
  the on-demand `oxideav_core::EngineProbeFn` for this backend. Re-exported
  at the crate root as `oxideav_vdpau::engine_info`.
- The probe opens an X display via `Display::open_from_env()`, creates a
  `VdpDevice`, queries `VdpGetInformationString` + `VdpGetApiVersion`, and
  walks `VdpDecoderQueryCapabilities` for the seven codec families VDPAU
  exposes (h264, hevc, vp9, av1, mpeg2, vc1, mpeg4). Each family reports
  the representative profile's `max_width` / `max_height` / `max_level` /
  `max_macroblocks`, plus the fan-out list of supported profile labels
  (e.g. H.264 → `["Baseline", "Main", "High", "ConstrainedBaseline",
  "Extended", "ProgressiveHigh", "ConstrainedHigh"]` if all are
  supported). VDPAU has no encode side, so every `HwCodecCaps::encode` is
  `false`; `total_memory_bytes` is `None` (VDPAU doesn't expose
  per-device memory); the full driver banner is included verbatim under
  `extra["information_string"]`.
- The probe is skip-friendly — every error path (no `$DISPLAY`, X
  unreachable, libvdpau missing, `vdp_device_create_x11` failed, dispatch
  table resolve failed) returns `vec![]`. VDPAU is per-X-display, not
  per-GPU, so the probe never returns more than one entry on this driver.
- The probe parses the marketing name + version number out of the NVIDIA
  banner: `parse_device_name` takes the first banner line trimmed,
  `parse_driver_version` picks the first whitespace-separated digits-and-
  dots token (e.g. `"580.95.05"`).
- Integration test `tests/round6_engine_info.rs`: asserts that on a host
  with VDPAU reachable the probe reports a non-empty name, an API
  version, and an `h264` codec entry with `decode = true`,
  `max_width >= 1920`, `encode = false`. Skips cleanly on hosts without
  VDPAU.

### Changed — Round 5

- Migrated the H.264 SPS / PPS / slice-header parser to the new shared
  `oxideav-bitstream` sibling crate. `src/h264.rs` is now just the
  VDPAU-specific glue (`H264VdpauDecoder`, `DecodedFrame`,
  `get_bits_nv12_as_i420`, `From<BitstreamError> for VdpError`) — the
  Annex-B framer, RBSP emulation-prevention stripper, Exp-Golomb bit
  reader, SPS / PPS / slice-header structs and `parse_*` functions all
  moved to `oxideav_bitstream::h264`.
- Added `oxideav-bitstream = "0.0"` as a Linux-only dependency. The
  workspace `[patch.crates-io]` block redirects to
  `crates/oxideav-bitstream/` for local builds.
- HEVC + VP9 + MPEG-2 stay inline pending bitstream coverage of those
  codecs. The HEVC migration is blocked on `oxideav-bitstream` not
  carrying the full HEVC PPS — see the in-source note at the top of
  `src/hevc.rs`'s splitter for details.

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
