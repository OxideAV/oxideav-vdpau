#![cfg(target_os = "linux")]
//! Linux VDPAU hardware decode bridge.
//!
//! This crate is a **runtime-loaded** bridge to the
//! [VDPAU](https://www.freedesktop.org/wiki/Software/VDPAU/) library.
//! It uses [`libloading`] to `dlopen` `libvdpau.so.1` on first use,
//! so:
//!
//! * Linux builds have **no compile-time link dependency** on
//!   libvdpau; if the library can't be loaded, the registered
//!   factories return `Error::Unsupported` and the framework registry
//!   falls back to the pure-Rust codec implementation.
//! * No bindgen, no `*-sys` crate. VDPAU is a C API; symbol resolution
//!   is plain dlsym.
//!
//! The crate is gated to `cfg(target_os = "linux")` at the source
//! level: on macOS / Windows the entire crate compiles to an empty
//! rlib, and consumers (umbrella `oxideav`) gate the `register` call
//! behind the same cfg.
//!
//! # Programming model
//!
//! VDPAU exports exactly one normal symbol — `vdp_device_create_x11`.
//! Every other VDPAU function is reached via the `VdpGetProcAddress`
//! function pointer that `vdp_device_create_x11` writes back as part
//! of device creation, indexed by `VdpFuncId` constants. So the
//! bootstrap vtable is intentionally tiny; Round 2 will add the
//! full post-create dispatch surface.
//!
//! # Status
//!
//! Round 1 (this commit): scaffolding only. The framework load is
//! verified via `sys::framework()`; no codec factories are wired up
//! yet. Round 2 will resolve the post-create dispatch table via
//! `VdpGetProcAddress` and add H.264 + HEVC decoders.
//!
//! # Workspace policy
//!
//! Calling a system OS / driver API via FFI is the same shape as
//! calling `libc::malloc` — it's the platform, not a copied
//! algorithm. The workspace's clean-room rule (no embedding source
//! from libvpx, libwebp, libjxl, etc.) doesn't apply here.

pub mod sys;

/// Confirm the VDPAU framework loads, but do not register any codec
/// factories yet (Round 1 scaffolding).
///
/// If `libvdpau.so.1` cannot be loaded (no NVIDIA / Mesa VDPAU stack,
/// sandboxed environment, etc.) the function logs and returns — the
/// runtime falls back to the pure-Rust impls.
#[cfg(feature = "registry")]
pub fn register(_ctx: &mut oxideav_core::RuntimeContext) {
    match sys::framework() {
        Ok(_) => {
            // Round 1: framework loads. No factories wired up yet.
        }
        Err(e) => {
            eprintln!("oxideav-vdpau: library unavailable, skipping registration: {e}");
        }
    }
}

#[cfg(feature = "registry")]
oxideav_core::register!("vdpau", register);
