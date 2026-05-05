//! Runtime-loaded VDPAU library handle.
//!
//! Loaded once via `OnceLock` on first use and cached for the process
//! lifetime. If the dlopen fails the cache stores the error so
//! subsequent calls don't repeatedly hammer the dynamic linker.
//!
//! Library needed for the bridge:
//!
//! | Library         | Purpose                               |
//! |-----------------|---------------------------------------|
//! | libvdpau.so.1   | dispatch (`vdp_device_create_x11`)    |
//!
//! VDPAU's bootstrap is a single symbol — `vdp_device_create_x11`. It
//! returns a `VdpDevice` plus a `VdpGetProcAddress` function pointer;
//! every other VDPAU function (decoder query, decode, mixer, output
//! surface, …) is reached through that GetProcAddress. Round 1 only
//! wires up the bootstrap; Round 2 will populate the post-create
//! dispatch table.

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

/// Success status: `VDP_STATUS_OK == 0`.
pub const VDP_STATUS_OK: VdpStatus = 0;

/// `VdpGetProcAddress` — the post-bootstrap entry-point table. Calling
/// `vdp_device_create_x11` populates a pointer to this. Round 2 will
/// resolve every other VDPAU entry through it.
pub type VdpGetProcAddress = unsafe extern "C" fn(
    device: VdpDevice,
    function_id: VdpFuncId,
    function_pointer: *mut *mut c_void,
) -> VdpStatus;

// ─────────────────────────── function pointer types ──────────────────────────

/// `vdp_device_create_x11(display, screen, device_out, get_proc_address_out)`
/// — the only VDPAU symbol exported as a normal dynamic-linker entry.
/// `display` is an `XDisplay*`. We model it as `*mut c_void` here
/// because Round 1 doesn't open an X display (the smoke test only
/// resolves the symbol).
pub type FnVdpDeviceCreateX11 = unsafe extern "C" fn(
    display: *mut c_void,
    screen: i32,
    device: *mut VdpDevice,
    get_proc_address: *mut VdpGetProcAddress,
) -> VdpStatus;

// ─────────────────────────── Vtable ───────────────────────────────────────────

/// Resolved function pointers for the bootstrap VDPAU symbol set.
///
/// All fields are `unsafe extern "C" fn(...)` pointer types — callers
/// are responsible for the FFI invariants (correct argument types,
/// device lifetime, `VdpStatus` checking).
pub struct Vtable {
    pub vdp_device_create_x11: FnVdpDeviceCreateX11,
    // Keep library alive
    _libvdpau: Library,
}

/// Smoke-test wrapper used by tests + by the pre-flight load check
/// in `register()`. Holds the raw `Library` handle so callers can
/// assert that dlopen succeeded without paying the full dlsym tour.
pub struct FrameworkSmoke {
    pub libvdpau: Library,
}

// ─────────────────────────── Caches ───────────────────────────────────────────

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
/// library but does no dlsym work.
pub fn framework() -> Result<&'static FrameworkSmoke, &'static str> {
    FRAMEWORK
        .get_or_init(load_smoke)
        .as_ref()
        .map_err(|s| s.as_str())
}

fn load_smoke() -> Result<FrameworkSmoke, String> {
    Ok(FrameworkSmoke {
        libvdpau: open("libvdpau.so.1")?,
    })
}

fn load_vtable() -> Result<Vtable, String> {
    let libvdpau = open("libvdpau.so.1")?;

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
        vdp_device_create_x11: sym!(
            libvdpau,
            "vdp_device_create_x11",
            FnVdpDeviceCreateX11
        ),
        _libvdpau: libvdpau,
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

    /// Smoke test: libvdpau.so.1 on this machine loads cleanly.
    #[test]
    fn frameworks_load() {
        let fw = framework().expect("framework load");
        // Confirm the bootstrap entry point is present.
        let _: libloading::Symbol<unsafe extern "C" fn()> = unsafe {
            fw.libvdpau
                .get(b"vdp_device_create_x11\0")
                .expect("vdp_device_create_x11 symbol")
        };
    }

    /// Verify the vtable resolves all symbols.
    #[test]
    fn vtable_resolves() {
        vtable().expect("vtable load");
    }
}
