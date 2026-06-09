# oxideav-vdpau

Linux VDPAU hardware decode bridge for the [oxideav](https://github.com/OxideAV/oxideav) framework.

## Why a bridge crate?

[VDPAU](https://www.freedesktop.org/wiki/Software/VDPAU/) (Video Decode and Presentation API for Unix) is the long-standing NVIDIA-driven HW decode interface on Linux. It pre-dates VA-API and is still the path of least resistance on legacy NVIDIA driver stacks where the proprietary driver does not ship a VA-API shim. AMD/Mesa also ships a VDPAU front-end (driver names: `vdpau-mesa-pcie`, etc.).

This crate is a **thin runtime-loaded bridge** — no compile-time link dependency on `libvdpau`. The library is opened via [`libloading`] on first use.

## Programming model

VDPAU is unusual: only `vdp_device_create_x11` is exported as a normal symbol. **Every other VDPAU function** is reached via the `VdpGetProcAddress` function pointer that `vdp_device_create_x11` writes back as part of device creation, indexed by `VdpFuncId` constants. So this crate's vtable is intentionally small — a single bootstrap symbol — and Round 2 will add the post-create dispatch table generated from `VdpGetProcAddress`.

## Fallback behaviour

Two distinct failure paths fall back automatically to the pure-Rust codec:

1. **Load failure** — `libvdpau.so.1` not installed, or no X server reachable (`vdp_device_create_x11` requires an `XDisplay`). `register()` logs and returns without registering, so the SW codec is the only candidate at dispatch.
2. **Init failure** — `vdp_device_create_x11` returns a non-zero `VdpStatus`, or the requested codec / profile / resolution exceeds what the driver advertises via `vdp_decoder_query_capabilities`. The factory returns `Err`; the registry's `make_decoder_with` retries the next-priority impl.

Pipelines that **require** hardware can opt out of the SW fallback by setting `CodecPreferences { require_hardware: true, .. }`.

## Platform gating

The whole crate is `#![cfg(target_os = "linux")]`. On macOS / Windows it compiles to an empty rlib; the umbrella `oxideav` crate gates the `register` call behind the same cfg. (Solaris / FreeBSD also ship libvdpau but are not yet supported.)

## Priority

Hardware factories register with `CodecCapabilities::with_priority(15)` — slightly higher (worse) than VA-API's 10, because VA-API generally has the better Linux driver story today. Pure-Rust impls remain at priority 100+.

## Opt-out

Users who want to force the pure-Rust path globally can pass `--no-hwaccel` to the `oxideav` CLI; this sets `CodecPreferences { no_hardware: true }`, which the pipeline forwards to `make_decoder_with` so HW factories are skipped at dispatch time. The runtime context still registers VDPAU — `oxideav list` shows the `*_vdpau` rows regardless of the flag.

## Coverage roadmap

| Codec        | Decode |
|--------------|--------|
| H.264        | planned |
| HEVC         | planned (Maxwell GM2+ via NVIDIA, RDNA1+ via Mesa) |
| MPEG-2       | planned |
| MPEG-4 Pt 2  | planned |
| VC-1         | planned |
| VP9          | planned (Pascal+ via NVIDIA) |
| AV1          | planned (Ampere+ via NVIDIA) |

Encode is intentionally absent — VDPAU has no encode counterpart. Encoders go through NVENC (`oxideav-nvidia`) or VA-API (`oxideav-vaapi`).

Round 2 (this commit): the bridge now opens an X server connection (libX11 dlopen — `XOpenDisplay`/`XCloseDisplay`/`XDefaultScreen`), creates a `VdpDevice` via `vdp_device_create_x11`, and resolves the post-bootstrap dispatch table (`VdpDeviceDestroy`, `VdpGetApiVersion`, `VdpGetInformationString`, `VdpDecoderQueryCapabilities`) through the `VdpGetProcAddress` pointer the create call writes back. Safe wrappers — `Display`, `VdpDevice`, `DecoderCaps`, `VdpError` — own the lifecycle (Drop calls `VdpDeviceDestroy` and `XCloseDisplay`). Profile/function-ID constants are exposed in `sys` for callers that want to query specific codecs (`sys::VDP_DECODER_PROFILE_H264_HIGH`, `_HEVC_MAIN`, `_VP9_PROFILE_0`, `_AV1_MAIN`, …). Round 3: codec factories — H.264 + HEVC decode through `VdpDecoderCreate` / `VdpDecoderRender`.

Round 8: `register()` now pushes four `CodecInfo` entries into `RuntimeContext::codecs` — one each for `h264`, `hevc`, `vp9`, and `mpeg2video`. Every entry sets `with_engine_id("vdpau")` + `with_engine_probe(engine_info)`, carries the matching container-tag set (FourCC + Matroska codec id), and advertises `decode = true`, `hardware_accelerated = true`, `priority = 15`, `max_size = 8192 x 8192`. The framework load is still gated up front, so on a host without `libvdpau.so.1` or `libX11.so.6` `register()` logs once and skips registration — the registry then has no `*_vdpau` rows and the pure-Rust fallbacks remain the only candidates. `decoder_factory` is still unset on every row: adapting the existing direct constructors (`H264VdpauDecoder::with_params`, etc.) into streaming `dyn oxideav_core::Decoder` impls (`send_packet` / `receive_frame` / cached SPS-PPS / per-codec state) lands per-codec in follow-up rounds.

Round 9 (this commit): typed `Profile` enum exposed at the crate root (`oxideav_vdpau::Profile`). One variant per `sys::VDP_DECODER_PROFILE_*` constant (25 total — H.264 × 7, HEVC × 4, VP9 × 4, AV1 × 3, MPEG-2 × 2, VC-1 × 3, MPEG-4 Part 2 × 2); `Profile::as_raw` / `Profile::from_raw` round-trip with the raw `VdpDecoderProfile` (`u32`) form so FFI calls stay unchanged. `Profile::codec_id` returns the framework family string (`"h264"`, `"hevc"`, …) and `Profile::label` the human-facing suffix (`"High"`, `"Main10"`, `"0"`, `"Pro"`). The engine probe's `CODEC_QUERIES` table now stores typed `Profile` values rather than `(VdpDecoderProfile, &'static str)` tuples — the labels reported into `HwCodecCaps::profiles` come from `Profile::label()`, one source of truth. Eight unit tests cover the round-trip / unknown-raw / family-grouping / label / `Profile::ALL` density invariants; integration test `tests/round9_profile.rs` pins public reachability and the per-family label set.

## Workspace policy

Calling a system OS / driver API via FFI is the same shape as calling `libc::malloc` — it's the platform, not a copied algorithm. The workspace's clean-room rule (no embedding source from libvpx, libwebp, libjxl, etc.) does not apply to this crate.

## License

MIT.
