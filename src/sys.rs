//! Runtime-loaded VDPAU + libX11 library handles.
//!
//! Loaded once via `OnceLock` on first use and cached for the process
//! lifetime. If the dlopen fails the cache stores the error so
//! subsequent calls don't repeatedly hammer the dynamic linker.
//!
//! Libraries needed for the bridge:
//!
//! | Library         | Purpose                                            |
//! |-----------------|----------------------------------------------------|
//! | libvdpau.so.1   | VDPAU dispatch (`vdp_device_create_x11`)           |
//! | libX11.so.6     | X server connection (`XOpenDisplay`, …)            |
//!
//! VDPAU's bootstrap is a single symbol — `vdp_device_create_x11`. It
//! returns a `VdpDevice` plus a pointer to a `VdpGetProcAddress`
//! function. Every other VDPAU function (decoder query, decode,
//! mixer, output surface, …) is reached through that GetProcAddress
//! indexed by `VdpFuncId` constants. We resolve the post-bootstrap
//! entries lazily in `device.rs`.
//!
//! # Style note
//!
//! Type aliases and constants in this module are spelled exactly as
//! they appear in `<vdpau/vdpau.h>` and `<X11/Xlib.h>` (snake-cased
//! at the macro level, kept verbatim where they're already snake).
//! Those headers are platform vendor headers and may be referenced
//! freely under the workspace's clean-room policy — they're the
//! equivalent of `<stdio.h>`.

use libloading::Library;
use std::ffi::c_void;
use std::sync::OnceLock;

// ─────────────────────────── opaque VDPAU types ──────────────────────────────

/// VDPAU device handle. 32-bit opaque ID populated by
/// `vdp_device_create_x11`.
pub type VdpDevice = u32;

/// VDPAU decoder handle. 32-bit opaque ID returned by
/// `VdpDecoderCreate` (resolved post-bootstrap via VdpGetProcAddress).
pub type VdpDecoder = u32;

/// VdpStatus — return code for almost every VDPAU entry point.
pub type VdpStatus = i32;

/// VdpFuncId — index into the dispatch table looked up via
/// `VdpGetProcAddress`. Each VDPAU function has a stable ID.
pub type VdpFuncId = u32;

/// VdpBool — `int` in the C header. `0` is false, non-zero true.
pub type VdpBool = i32;

/// VdpDecoderProfile — `uint32_t` in the C header. Identifies a
/// codec profile (H.264 High, HEVC Main, …).
pub type VdpDecoderProfile = u32;

/// Success status: `VDP_STATUS_OK == 0`.
pub const VDP_STATUS_OK: VdpStatus = 0;

// ─────────────────────────── VDPAU function IDs ──────────────────────────────
//
// Stable ABI constants from `<vdpau/vdpau.h>`. The full list is huge;
// we expose the subset Round 2 needs.

pub const VDP_FUNC_ID_GET_API_VERSION: VdpFuncId = 2;
pub const VDP_FUNC_ID_GET_INFORMATION_STRING: VdpFuncId = 4;
pub const VDP_FUNC_ID_DEVICE_DESTROY: VdpFuncId = 5;
pub const VDP_FUNC_ID_DECODER_QUERY_CAPABILITIES: VdpFuncId = 36;
pub const VDP_FUNC_ID_DECODER_CREATE: VdpFuncId = 37;
pub const VDP_FUNC_ID_DECODER_DESTROY: VdpFuncId = 38;
pub const VDP_FUNC_ID_DECODER_RENDER: VdpFuncId = 40;

// ─────────────────────────── VDPAU decoder profiles ──────────────────────────

pub const VDP_DECODER_PROFILE_MPEG2_SIMPLE: VdpDecoderProfile = 1;
pub const VDP_DECODER_PROFILE_MPEG2_MAIN: VdpDecoderProfile = 2;
pub const VDP_DECODER_PROFILE_H264_BASELINE: VdpDecoderProfile = 6;
pub const VDP_DECODER_PROFILE_H264_MAIN: VdpDecoderProfile = 7;
pub const VDP_DECODER_PROFILE_H264_HIGH: VdpDecoderProfile = 8;
pub const VDP_DECODER_PROFILE_VC1_SIMPLE: VdpDecoderProfile = 9;
pub const VDP_DECODER_PROFILE_VC1_MAIN: VdpDecoderProfile = 10;
pub const VDP_DECODER_PROFILE_VC1_ADVANCED: VdpDecoderProfile = 11;
pub const VDP_DECODER_PROFILE_MPEG4_PART2_SP: VdpDecoderProfile = 12;
pub const VDP_DECODER_PROFILE_MPEG4_PART2_ASP: VdpDecoderProfile = 13;
pub const VDP_DECODER_PROFILE_H264_CONSTRAINED_BASELINE: VdpDecoderProfile = 22;
pub const VDP_DECODER_PROFILE_H264_EXTENDED: VdpDecoderProfile = 23;
pub const VDP_DECODER_PROFILE_H264_PROGRESSIVE_HIGH: VdpDecoderProfile = 24;
pub const VDP_DECODER_PROFILE_H264_CONSTRAINED_HIGH: VdpDecoderProfile = 25;
pub const VDP_DECODER_PROFILE_VP9_PROFILE_0: VdpDecoderProfile = 27;
pub const VDP_DECODER_PROFILE_VP9_PROFILE_1: VdpDecoderProfile = 28;
pub const VDP_DECODER_PROFILE_VP9_PROFILE_2: VdpDecoderProfile = 29;
pub const VDP_DECODER_PROFILE_VP9_PROFILE_3: VdpDecoderProfile = 30;
pub const VDP_DECODER_PROFILE_HEVC_MAIN: VdpDecoderProfile = 100;
pub const VDP_DECODER_PROFILE_HEVC_MAIN_10: VdpDecoderProfile = 101;
pub const VDP_DECODER_PROFILE_HEVC_MAIN_STILL: VdpDecoderProfile = 102;
pub const VDP_DECODER_PROFILE_HEVC_MAIN_12: VdpDecoderProfile = 103;
pub const VDP_DECODER_PROFILE_AV1_MAIN: VdpDecoderProfile = 107;
pub const VDP_DECODER_PROFILE_AV1_HIGH: VdpDecoderProfile = 108;
pub const VDP_DECODER_PROFILE_AV1_PROFESSIONAL: VdpDecoderProfile = 109;

// ─────────────────────────── VDPAU function pointer types ────────────────────

/// `VdpGetProcAddress` — the post-bootstrap entry-point table. Calling
/// `vdp_device_create_x11` writes back a pointer to this. Every other
/// VDPAU entry is resolved through it.
pub type VdpGetProcAddress = unsafe extern "C" fn(
    device: VdpDevice,
    function_id: VdpFuncId,
    function_pointer: *mut *mut c_void,
) -> VdpStatus;

/// `vdp_device_create_x11(display, screen, device_out, get_proc_address_out)`
/// — the only VDPAU symbol exported as a normal dynamic-linker entry.
/// `display` is an `XDisplay*` returned by `XOpenDisplay`.
///
/// Note on the last parameter: the C signature is
/// `VdpGetProcAddress ** get_proc_address` where `VdpGetProcAddress`
/// is a *function type* (not a pointer). So `VdpGetProcAddress *`
/// in C is a function pointer, and `VdpGetProcAddress **` is a
/// pointer to a function pointer. In Rust `VdpGetProcAddress` is
/// already an `unsafe extern "C" fn(...)` (a function pointer), so
/// the matching FFI type is `*mut VdpGetProcAddress`.
pub type FnVdpDeviceCreateX11 = unsafe extern "C" fn(
    display: *mut c_void,
    screen: i32,
    device: *mut VdpDevice,
    get_proc_address: *mut VdpGetProcAddress,
) -> VdpStatus;

/// `VdpGetApiVersion(*api_version_out)` — resolved via VdpGetProcAddress.
pub type FnVdpGetApiVersion = unsafe extern "C" fn(api_version: *mut u32) -> VdpStatus;

/// `VdpGetInformationString(*info_string_out)` — resolved via VdpGetProcAddress.
/// The returned C string is statically allocated; do not free.
pub type FnVdpGetInformationString =
    unsafe extern "C" fn(information_string: *mut *const std::os::raw::c_char) -> VdpStatus;

/// `VdpDeviceDestroy(device)` — resolved via VdpGetProcAddress.
pub type FnVdpDeviceDestroy = unsafe extern "C" fn(device: VdpDevice) -> VdpStatus;

/// `VdpDecoderQueryCapabilities(device, profile, *is_supported_out,
/// *max_level_out, *max_macroblocks_out, *max_width_out,
/// *max_height_out)` — resolved via VdpGetProcAddress.
pub type FnVdpDecoderQueryCapabilities = unsafe extern "C" fn(
    device: VdpDevice,
    profile: VdpDecoderProfile,
    is_supported: *mut VdpBool,
    max_level: *mut u32,
    max_macroblocks: *mut u32,
    max_width: *mut u32,
    max_height: *mut u32,
) -> VdpStatus;

// ─────────────────────────── X11 opaque types ────────────────────────────────

/// `Display*` from libX11. Opaque pointer; we never deref the struct.
pub type XDisplay = c_void;

// ─────────────────────────── X11 function pointer types ──────────────────────

/// `XOpenDisplay(display_name)` — connects to the X server. Returns
/// NULL on failure. `display_name` may be NULL, in which case the
/// `$DISPLAY` env var is consulted (we always pass an explicit C
/// string from Rust).
pub type FnXOpenDisplay = unsafe extern "C" fn(display_name: *const u8) -> *mut XDisplay;

/// `XCloseDisplay(display)` — closes the X connection.
pub type FnXCloseDisplay = unsafe extern "C" fn(display: *mut XDisplay) -> i32;

/// `XDefaultScreen(display)` — returns the default screen index for a
/// display.
pub type FnXDefaultScreen = unsafe extern "C" fn(display: *mut XDisplay) -> i32;

// ─────────────────────────── Vtables ─────────────────────────────────────────

/// Resolved function pointers for the bootstrap VDPAU symbol set
/// (currently exactly one symbol — `vdp_device_create_x11`) plus the
/// libX11 entry points needed to obtain a `Display*`.
///
/// All fields are `unsafe extern "C" fn(...)` pointer types — callers
/// are responsible for the FFI invariants (correct argument types,
/// device lifetime, `VdpStatus` checking).
pub struct Vtable {
    // VDPAU bootstrap
    pub vdp_device_create_x11: FnVdpDeviceCreateX11,
    // X11
    pub x_open_display: FnXOpenDisplay,
    pub x_close_display: FnXCloseDisplay,
    pub x_default_screen: FnXDefaultScreen,
    // Keep libraries alive
    _libvdpau: Library,
    _libx11: Library,
}

/// Smoke-test wrapper used by tests + by the pre-flight load check
/// in `register()`. Holds the raw `Library` handles so callers can
/// assert that dlopen succeeded without paying the full dlsym tour.
pub struct FrameworkSmoke {
    pub libvdpau: Library,
    pub libx11: Library,
}

// ─────────────────────────── Caches ──────────────────────────────────────────

static VTABLE: OnceLock<Result<Vtable, String>> = OnceLock::new();
static FRAMEWORK: OnceLock<Result<FrameworkSmoke, String>> = OnceLock::new();

/// Get (or load) the fully-resolved vtable. Returns the cached `Err`
/// if a previous load attempt failed.
pub fn vtable() -> Result<&'static Vtable, &'static str> {
    VTABLE
        .get_or_init(load_vtable)
        .as_ref()
        .map_err(|s| s.as_str())
}

/// Cheap framework-load check used by `register()`. Resolves the
/// libraries but does no dlsym work.
pub fn framework() -> Result<&'static FrameworkSmoke, &'static str> {
    FRAMEWORK
        .get_or_init(load_smoke)
        .as_ref()
        .map_err(|s| s.as_str())
}

fn load_smoke() -> Result<FrameworkSmoke, String> {
    Ok(FrameworkSmoke {
        libvdpau: open("libvdpau.so.1")?,
        libx11: open("libX11.so.6")?,
    })
}

fn load_vtable() -> Result<Vtable, String> {
    let libvdpau = open("libvdpau.so.1")?;
    let libx11 = open("libX11.so.6")?;

    macro_rules! sym {
        ($lib:expr, $name:expr, $ty:ty) => {{
            let s: libloading::Symbol<$ty> = unsafe {
                $lib.get(concat!($name, "\0").as_bytes())
                    .map_err(|e| format!("dlsym {}: {}", $name, e))?
            };
            *s
        }};
    }

    Ok(Vtable {
        vdp_device_create_x11: sym!(libvdpau, "vdp_device_create_x11", FnVdpDeviceCreateX11),
        x_open_display: sym!(libx11, "XOpenDisplay", FnXOpenDisplay),
        x_close_display: sym!(libx11, "XCloseDisplay", FnXCloseDisplay),
        x_default_screen: sym!(libx11, "XDefaultScreen", FnXDefaultScreen),
        _libvdpau: libvdpau,
        _libx11: libx11,
    })
}

fn open(path: &str) -> Result<Library, String> {
    // SAFETY: dlopen on a soname with no init callbacks; equivalent to
    // a normal program startup load.
    unsafe { Library::new(path) }.map_err(|e| format!("dlopen {path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: libvdpau.so.1 + libX11.so.6 on this machine load
    /// cleanly.
    #[test]
    fn frameworks_load() {
        let fw = framework().expect("framework load");
        // Confirm the bootstrap entry point is present.
        let _: libloading::Symbol<unsafe extern "C" fn()> = unsafe {
            fw.libvdpau
                .get(b"vdp_device_create_x11\0")
                .expect("vdp_device_create_x11 symbol")
        };
        let _: libloading::Symbol<unsafe extern "C" fn()> = unsafe {
            fw.libx11
                .get(b"XOpenDisplay\0")
                .expect("XOpenDisplay symbol")
        };
    }

    /// Verify the vtable resolves all symbols.
    #[test]
    fn vtable_resolves() {
        vtable().expect("vtable load");
    }
}
