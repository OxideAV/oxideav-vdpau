#![cfg(target_os = "linux")]
//! Linux VDPAU hardware decode bridge.
//!
//! This crate is a **runtime-loaded** bridge to the
//! [VDPAU](https://www.freedesktop.org/wiki/Software/VDPAU/) library.
//! It uses [`libloading`] to `dlopen` `libvdpau.so.1` (and `libX11.so.6`
//! for the `XOpenDisplay` connection VDPAU's bootstrap requires) on
//! first use, so:
//!
//! * Linux builds have **no compile-time link dependency** on
//!   libvdpau or libX11; if either library can't be loaded, the
//!   registered factories return `Error::Unsupported` and the
//!   framework registry falls back to the pure-Rust codec
//!   implementation.
//! * No bindgen, no `*-sys` crate. VDPAU is a C API and X11 is a C
//!   API; symbol resolution is plain dlsym for the bootstrap and
//!   `VdpGetProcAddress` for everything else.
//!
//! The crate is gated to `cfg(target_os = "linux")` at the source
//! level: on macOS / Windows the entire crate compiles to an empty
//! rlib, and consumers (umbrella `oxideav`) gate the `register` call
//! behind the same cfg.
//!
//! # Programming model
//!
//! VDPAU exports exactly one normal symbol — `vdp_device_create_x11`.
//! It returns a `VdpDevice` plus a `VdpGetProcAddress` function
//! pointer; every other VDPAU entry (`VdpGetApiVersion`,
//! `VdpDeviceDestroy`, `VdpDecoderQueryCapabilities`,
//! `VdpDecoderCreate`, …) is reached through that GetProcAddress
//! indexed by `VdpFuncId` constants. See [`device`] for the safe
//! wrappers ([`Display`], [`VdpDevice`], [`DecoderCaps`],
//! [`VdpError`]).
//!
//! # Status
//!
//! Round 8: `register()` now pushes `CodecInfo` entries for the four
//! implemented decoders (`h264`, `hevc`, `vp9`, `mpeg2video`) into
//! `RuntimeContext::codecs`, each tagged with `engine_id = "vdpau"`
//! and the [`engine::engine_info`] probe. Container-level tags
//! (FourCC + Matroska codec ids) ship on every entry so the registry
//! resolver picks the VDPAU bridge up from `oxideav info` and from
//! `make_decoder_with` lookups by tag. Streaming decoder factories
//! that wrap [`h264::H264VdpauDecoder`], [`hevc::HevcVdpauDecoder`],
//! [`vp9::Vp9VdpauDecoder`], and [`mpeg2::Mpeg2VdpauDecoder`] into
//! `dyn oxideav_core::Decoder` impls are deferred to the next round
//! (each one owns a `Display + VdpDevice` and a cached SPS-like
//! parameter set, so the streaming-adapter shape lands per codec
//! rather than as one big drop).
//!
//! # Workspace policy
//!
//! Calling a system OS / driver API via FFI is the same shape as
//! calling `libc::malloc` — it's the platform, not a copied
//! algorithm. The workspace's clean-room rule (no embedding source
//! from libvpx, libwebp, libjxl, etc.) doesn't apply here.

pub mod device;
pub mod engine;
pub mod h264;
pub mod hevc;
pub mod mpeg2;
pub mod profile;
// internal — raw dlopen FFI plumbing; not part of the stable API
#[doc(hidden)]
pub mod sys;
pub mod vp9;

pub use device::{DecoderCaps, Display, VdpDecoder, VdpDevice, VdpError, VdpVideoSurface};
pub use engine::{engine_info, validate_device_index};
pub use h264::{DecodedFrame, H264VdpauDecoder};
pub use hevc::HevcVdpauDecoder;
pub use mpeg2::Mpeg2VdpauDecoder;
pub use profile::Profile;
pub use vp9::Vp9VdpauDecoder;

/// Register the four implemented decoders (h264, hevc, vp9, mpeg2video)
/// as `vdpau` engine entries in the framework's codec registry.
///
/// Each `CodecInfo` is annotated with:
///   * `CodecCapabilities::video(<impl>).with_decode().with_hardware(true)
///     .with_priority(15)` — slightly higher (worse) than VA-API's 10
///     because VA-API generally has the better Linux driver story.
///   * `with_max_size(8192, 8192)` — a generous static upper bound the
///     post-registration `make_decoder_with` walker pre-filters with.
///     The live per-profile maxima come from
///     [`crate::engine::engine_info`] on demand.
///   * Container tags so the registry resolver picks the VDPAU bridge
///     up from FourCC / Matroska codec-id lookups.
///   * `with_engine_id("vdpau")` + `with_engine_probe(engine_info)` so
///     `oxideav info` groups the rows under the VDPAU device and the
///     CLI can dedupe the probe call by engine id.
///
/// No `decoder_factory` is wired yet: the existing direct constructors
/// (`H264VdpauDecoder::with_params`, etc.) honour `device_index`, but
/// adapting them into `dyn oxideav_core::Decoder` impls (streaming,
/// SPS/PPS-cached, `send_packet` / `receive_frame`) lands per codec in
/// a follow-up round.
///
/// If `libvdpau.so.1` or `libX11.so.6` cannot be loaded (no NVIDIA /
/// Mesa VDPAU stack, headless / sandboxed environment, etc.) the
/// function logs and returns — the runtime falls back to the
/// pure-Rust impls.
#[cfg(feature = "registry")]
pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecTag};

    match sys::framework() {
        Ok(_) => {}
        Err(e) => {
            eprintln!("oxideav-vdpau: library unavailable, skipping registration: {e}");
            return;
        }
    }

    // Per-codec maximum size: VDPAU drivers commonly advertise 4096×4096
    // for H.264, 8192×8192 for HEVC/VP9/AV1, and 1920×1080 for MPEG-2.
    // We pre-filter generously at 8192×8192 — the post-registration
    // `make_decoder_with` walker keeps the entry as long as the
    // requested resolution fits, and `engine_info()` gives the live
    // per-profile maxima for callers that want the real number.
    const VDPAU_MAX_W: u32 = 8192;
    const VDPAU_MAX_H: u32 = 8192;
    // Priority lower (worse) than VA-API's 10 — see crate docs.
    const VDPAU_PRIORITY: i32 = 15;

    // ── H.264 decoder ─────────────────────────────────────────────────────
    ctx.codecs.register(
        CodecInfo::new(CodecId::new("h264"))
            .capabilities(
                CodecCapabilities::video("h264_vdpau")
                    .with_decode()
                    .with_lossy(true)
                    .with_hardware(true)
                    .with_priority(VDPAU_PRIORITY)
                    .with_max_size(VDPAU_MAX_W, VDPAU_MAX_H),
            )
            .tags([
                CodecTag::fourcc(b"H264"),
                CodecTag::fourcc(b"h264"),
                CodecTag::fourcc(b"AVC1"),
                CodecTag::fourcc(b"avc1"),
                CodecTag::fourcc(b"X264"),
                CodecTag::matroska("V_MPEG4/ISO/AVC"),
            ])
            .with_engine_id("vdpau")
            .with_engine_probe(engine::engine_info),
    );

    // ── HEVC decoder ──────────────────────────────────────────────────────
    ctx.codecs.register(
        CodecInfo::new(CodecId::new("hevc"))
            .capabilities(
                CodecCapabilities::video("hevc_vdpau")
                    .with_decode()
                    .with_lossy(true)
                    .with_hardware(true)
                    .with_priority(VDPAU_PRIORITY)
                    .with_max_size(VDPAU_MAX_W, VDPAU_MAX_H),
            )
            .tags([
                CodecTag::fourcc(b"hvc1"),
                CodecTag::fourcc(b"hev1"),
                CodecTag::fourcc(b"HEVC"),
                CodecTag::fourcc(b"H265"),
                CodecTag::matroska("V_MPEGH/ISO/HEVC"),
            ])
            .with_engine_id("vdpau")
            .with_engine_probe(engine::engine_info),
    );

    // ── VP9 decoder ───────────────────────────────────────────────────────
    ctx.codecs.register(
        CodecInfo::new(CodecId::new("vp9"))
            .capabilities(
                CodecCapabilities::video("vp9_vdpau")
                    .with_decode()
                    .with_lossy(true)
                    .with_hardware(true)
                    .with_priority(VDPAU_PRIORITY)
                    .with_max_size(VDPAU_MAX_W, VDPAU_MAX_H),
            )
            .tags([
                CodecTag::fourcc(b"VP90"),
                CodecTag::fourcc(b"vp09"),
                CodecTag::matroska("V_VP9"),
            ])
            .with_engine_id("vdpau")
            .with_engine_probe(engine::engine_info),
    );

    // ── MPEG-2 decoder ────────────────────────────────────────────────────
    // MPEG-2 advertises smaller real maxima (1920×1080 on most NVIDIA
    // VDPAU stacks); we still pre-filter at the generous bound and let
    // `engine_info()` carry the actual number to consumers that ask.
    ctx.codecs.register(
        CodecInfo::new(CodecId::new("mpeg2video"))
            .capabilities(
                CodecCapabilities::video("mpeg2_vdpau")
                    .with_decode()
                    .with_lossy(true)
                    .with_hardware(true)
                    .with_priority(VDPAU_PRIORITY)
                    .with_max_size(VDPAU_MAX_W, VDPAU_MAX_H),
            )
            .tags([
                CodecTag::fourcc(b"MPG2"),
                CodecTag::fourcc(b"mpg2"),
                CodecTag::fourcc(b"M2V "),
                CodecTag::matroska("V_MPEG2"),
            ])
            .with_engine_id("vdpau")
            .with_engine_probe(engine::engine_info),
    );
}

#[cfg(feature = "registry")]
oxideav_core::register!("vdpau", register);
