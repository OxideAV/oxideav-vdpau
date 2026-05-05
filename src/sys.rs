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

/// VDPAU video surface handle. 32-bit opaque ID returned by
/// `VdpVideoSurfaceCreate`.
pub type VdpVideoSurface = u32;

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

/// VdpChromaType — `uint32_t` in the C header. Identifies a chroma
/// subsampling pattern for a `VdpVideoSurface`.
pub type VdpChromaType = u32;

/// VdpYCbCrFormat — `uint32_t` in the C header. Identifies a YCbCr
/// pixel layout used for `VdpVideoSurfaceGetBitsYCbCr` /
/// `VdpVideoSurfacePutBitsYCbCr`.
pub type VdpYCbCrFormat = u32;

/// Success status: `VDP_STATUS_OK == 0`.
pub const VDP_STATUS_OK: VdpStatus = 0;

/// Sentinel value used in VDPAU "reference frame" slots that are
/// unused. Defined as `0xffffffffU` in the C header.
pub const VDP_INVALID_HANDLE: u32 = 0xffff_ffff;

/// Required value for `VdpBitstreamBuffer::struct_version`.
pub const VDP_BITSTREAM_BUFFER_VERSION: u32 = 0;

// ─────────────────────────── VDPAU function IDs ──────────────────────────────
//
// Stable ABI constants from `<vdpau/vdpau.h>`. The full list is huge;
// we expose the subset Round 2 needs.

pub const VDP_FUNC_ID_GET_API_VERSION: VdpFuncId = 2;
pub const VDP_FUNC_ID_GET_INFORMATION_STRING: VdpFuncId = 4;
pub const VDP_FUNC_ID_DEVICE_DESTROY: VdpFuncId = 5;
pub const VDP_FUNC_ID_VIDEO_SURFACE_QUERY_CAPABILITIES: VdpFuncId = 7;
pub const VDP_FUNC_ID_VIDEO_SURFACE_CREATE: VdpFuncId = 9;
pub const VDP_FUNC_ID_VIDEO_SURFACE_DESTROY: VdpFuncId = 10;
pub const VDP_FUNC_ID_VIDEO_SURFACE_GET_BITS_Y_CB_CR: VdpFuncId = 12;
pub const VDP_FUNC_ID_DECODER_QUERY_CAPABILITIES: VdpFuncId = 36;
pub const VDP_FUNC_ID_DECODER_CREATE: VdpFuncId = 37;
pub const VDP_FUNC_ID_DECODER_DESTROY: VdpFuncId = 38;
pub const VDP_FUNC_ID_DECODER_RENDER: VdpFuncId = 40;

// ─────────────────────────── Chroma types ────────────────────────────────────
//
// `<vdpau/vdpau.h>` defines these as small integer enum values, not
// fourcc codes. We only carry the three "frame" types used by Round 3.
pub const VDP_CHROMA_TYPE_420: VdpChromaType = 0;
pub const VDP_CHROMA_TYPE_422: VdpChromaType = 1;
pub const VDP_CHROMA_TYPE_444: VdpChromaType = 2;

// ─────────────────────────── YCbCr formats ───────────────────────────────────
//
// Despite their fourcc-looking names these constants are integer enums
// in `<vdpau/vdpau.h>` (`VdpYCbCrFormat`). NV12 = 0, YV12 = 1, etc.
pub const VDP_YCBCR_FORMAT_NV12: VdpYCbCrFormat = 0;
pub const VDP_YCBCR_FORMAT_YV12: VdpYCbCrFormat = 1;
pub const VDP_YCBCR_FORMAT_UYVY: VdpYCbCrFormat = 2;
pub const VDP_YCBCR_FORMAT_YUYV: VdpYCbCrFormat = 3;
pub const VDP_YCBCR_FORMAT_Y8U8V8A8: VdpYCbCrFormat = 4;
pub const VDP_YCBCR_FORMAT_V8U8Y8A8: VdpYCbCrFormat = 5;

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

/// `VdpVideoSurfaceCreate(device, chroma_type, width, height, *surface_out)`
/// — resolved via VdpGetProcAddress.
pub type FnVdpVideoSurfaceCreate = unsafe extern "C" fn(
    device: VdpDevice,
    chroma_type: VdpChromaType,
    width: u32,
    height: u32,
    surface: *mut VdpVideoSurface,
) -> VdpStatus;

/// `VdpVideoSurfaceDestroy(surface)` — resolved via VdpGetProcAddress.
pub type FnVdpVideoSurfaceDestroy =
    unsafe extern "C" fn(surface: VdpVideoSurface) -> VdpStatus;

/// `VdpVideoSurfaceGetBitsYCbCr(surface, dest_format, dest_data, dest_pitches)`
/// — resolved via VdpGetProcAddress. `dest_data` is an array of
/// `void*` per plane and `dest_pitches` is a parallel `uint32_t`
/// array; the count of entries depends on `dest_format` (NV12 → 2,
/// YV12 → 3, packed formats → 1).
pub type FnVdpVideoSurfaceGetBitsYCbCr = unsafe extern "C" fn(
    surface: VdpVideoSurface,
    destination_ycbcr_format: VdpYCbCrFormat,
    destination_data: *const *mut c_void,
    destination_pitches: *const u32,
) -> VdpStatus;

/// `VdpDecoderCreate(device, profile, width, height, max_references, *decoder_out)`
/// — resolved via VdpGetProcAddress.
pub type FnVdpDecoderCreate = unsafe extern "C" fn(
    device: VdpDevice,
    profile: VdpDecoderProfile,
    width: u32,
    height: u32,
    max_references: u32,
    decoder: *mut VdpDecoder,
) -> VdpStatus;

/// `VdpDecoderDestroy(decoder)` — resolved via VdpGetProcAddress.
pub type FnVdpDecoderDestroy = unsafe extern "C" fn(decoder: VdpDecoder) -> VdpStatus;

/// `VdpDecoderRender(decoder, target_surface, picture_info,
/// bitstream_buffer_count, bitstream_buffers)` — resolved via
/// VdpGetProcAddress. `picture_info` points to a codec-specific
/// struct (e.g. `VdpPictureInfoH264`) whose layout matches the
/// profile the decoder was created with.
pub type FnVdpDecoderRender = unsafe extern "C" fn(
    decoder: VdpDecoder,
    target: VdpVideoSurface,
    picture_info: *const c_void,
    bitstream_buffer_count: u32,
    bitstream_buffers: *const VdpBitstreamBuffer,
) -> VdpStatus;

// ─────────────────────────── Bitstream buffer ────────────────────────────────

/// Application data buffer containing compressed video bitstream
/// data. Layout matches `VdpBitstreamBuffer` in `<vdpau/vdpau.h>`:
/// three fields, no implicit padding on either ILP32 or LP64
/// (4-byte struct_version + 8-byte pointer + 4-byte length, total 16
/// or 24 bytes depending on pointer width).
///
/// `struct_version` must be `VDP_BITSTREAM_BUFFER_VERSION` (== 0).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VdpBitstreamBuffer {
    pub struct_version: u32,
    pub bitstream: *const c_void,
    pub bitstream_bytes: u32,
}

// ─────────────────────────── H.264 picture info ──────────────────────────────

/// One slot in `VdpPictureInfoH264::referenceFrames`. Fields copied
/// verbatim from `<vdpau/vdpau.h>`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VdpReferenceFrameH264 {
    /// Surface that holds the reference frame, or `VDP_INVALID_HANDLE`.
    pub surface: VdpVideoSurface,
    /// Long term reference flag (else short term).
    pub is_long_term: VdpBool,
    /// Whether the top field is a reference.
    pub top_is_reference: VdpBool,
    /// Whether the bottom field is a reference.
    pub bottom_is_reference: VdpBool,
    /// `[0]` = top, `[1]` = bottom.
    pub field_order_cnt: [i32; 2],
    /// `frame_num` for short-term refs / `LongTermPicNum` for long-term.
    pub frame_idx: u16,
}

/// Picture-parameter struct passed to `VdpDecoderRender` when the
/// decoder profile is one of the H.264 (non-444) profiles. Layout
/// copied verbatim from `<vdpau/vdpau.h>` `VdpPictureInfoH264`.
///
/// **Note**: the `referenceFrames` array trails 16 entries even when
/// the active DPB is smaller — unused slots must have
/// `surface = VDP_INVALID_HANDLE`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VdpPictureInfoH264 {
    pub slice_count: u32,
    pub field_order_cnt: [i32; 2],
    pub is_reference: VdpBool,

    pub frame_num: u16,
    pub field_pic_flag: u8,
    pub bottom_field_flag: u8,
    pub num_ref_frames: u8,
    pub mb_adaptive_frame_field_flag: u8,
    pub constrained_intra_pred_flag: u8,
    pub weighted_pred_flag: u8,
    pub weighted_bipred_idc: u8,
    pub frame_mbs_only_flag: u8,
    pub transform_8x8_mode_flag: u8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub pic_init_qp_minus26: i8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub log2_max_frame_num_minus4: u8,
    pub pic_order_cnt_type: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub delta_pic_order_always_zero_flag: u8,
    pub direct_8x8_inference_flag: u8,
    pub entropy_coding_mode_flag: u8,
    pub pic_order_present_flag: u8,
    pub deblocking_filter_control_present_flag: u8,
    pub redundant_pic_cnt_present_flag: u8,

    /// 4x4 scaling lists, raster order.
    pub scaling_lists_4x4: [[u8; 16]; 6],
    /// 8x8 scaling lists, raster order.
    pub scaling_lists_8x8: [[u8; 64]; 2],

    /// 16-entry DPB, unused slots set to `VDP_INVALID_HANDLE`.
    pub reference_frames: [VdpReferenceFrameH264; 16],
}

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
