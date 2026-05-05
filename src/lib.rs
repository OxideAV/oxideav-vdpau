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
//! Round 2: post-bootstrap dispatch table is wired up. We can open an
//! X display, create a `VdpDevice`, query the implementation string
//! and API version, and run `VdpDecoderQueryCapabilities` for any
//! `VdpDecoderProfile`. Codec factories (H.264, HEVC, …) come in
//! Round 3 once we wire actual decode through `VdpDecoderRender`.
//!
//! # Workspace policy
//!
//! Calling a system OS / driver API via FFI is the same shape as
//! calling `libc::malloc` — it's the platform, not a copied
//! algorithm. The workspace's clean-room rule (no embedding source
//! from libvpx, libwebp, libjxl, etc.) doesn't apply here.

pub mod device;
pub mod sys;

pub use device::{DecoderCaps, Display, VdpDevice, VdpError};

/// Confirm the VDPAU + libX11 frameworks load, but do not register
/// any codec factories yet (Round 2 — codec factories come in Round 3).
///
/// If `libvdpau.so.1` or `libX11.so.6` cannot be loaded (no NVIDIA /
/// Mesa VDPAU stack, headless / sandboxed environment, etc.) the
/// function logs and returns — the runtime falls back to the
/// pure-Rust impls.
#[cfg(feature = "registry")]
pub fn register(_ctx: &mut oxideav_core::RuntimeContext) {
    match sys::framework() {
        Ok(_) => {
            // Round 2: framework loads, dispatch table is reachable
            // via Display::create_vdp_device. Codec factories will
            // land in Round 3 with the first VdpDecoderRender call.
        }
        Err(e) => {
            eprintln!("oxideav-vdpau: library unavailable, skipping registration: {e}");
        }
    }
}

#[cfg(feature = "registry")]
oxideav_core::register!("vdpau", register);
