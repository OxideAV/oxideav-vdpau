//! Safe wrappers around the X11 `Display` connection and the VDPAU
//! `VdpDevice` it is needed to bootstrap.
//!
//! Lifecycle:
//!
//! 1. [`Display::open`] (or [`Display::open_from_env`]) connects to
//!    the X server via `XOpenDisplay`.
//! 2. [`Display::create_vdp_device`] calls `vdp_device_create_x11`,
//!    which returns a `VdpDevice` plus a `VdpGetProcAddress` function
//!    pointer; we resolve the post-bootstrap dispatch table
//!    (`VdpDeviceDestroy`, `VdpGetApiVersion`,
//!    `VdpGetInformationString`, `VdpDecoderQueryCapabilities`)
//!    immediately.
//! 3. Methods on [`VdpDevice`] (the safe wrapper, not the raw
//!    `u32` ID alias in `sys`) call those resolved entries.
//! 4. `Drop for VdpDevice` calls `VdpDeviceDestroy`. `Drop for
//!    Display` calls `XCloseDisplay`.

use std::ffi::{CStr, CString, c_void};
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::Arc;

use crate::sys::{
    self, FnVdpDecoderCreate, FnVdpDecoderDestroy, FnVdpDecoderQueryCapabilities,
    FnVdpDecoderRender, FnVdpDeviceDestroy, FnVdpGetApiVersion, FnVdpGetInformationString,
    FnVdpVideoSurfaceCreate, FnVdpVideoSurfaceDestroy, FnVdpVideoSurfaceGetBitsYCbCr,
    VDP_FUNC_ID_DECODER_CREATE, VDP_FUNC_ID_DECODER_DESTROY, VDP_FUNC_ID_DECODER_QUERY_CAPABILITIES,
    VDP_FUNC_ID_DECODER_RENDER, VDP_FUNC_ID_DEVICE_DESTROY, VDP_FUNC_ID_GET_API_VERSION,
    VDP_FUNC_ID_GET_INFORMATION_STRING, VDP_FUNC_ID_VIDEO_SURFACE_CREATE,
    VDP_FUNC_ID_VIDEO_SURFACE_DESTROY, VDP_FUNC_ID_VIDEO_SURFACE_GET_BITS_Y_CB_CR, VDP_STATUS_OK,
    VdpBitstreamBuffer, VdpChromaType, VdpDecoderProfile, VdpGetProcAddress, VdpStatus,
    VdpYCbCrFormat, XDisplay,
};

// ─────────────────────────── Error ───────────────────────────────────────────

/// Error type for the VDPAU bridge. Wraps a `VdpStatus` code and a
/// human-readable message describing where the failure occurred.
#[derive(Debug, Clone)]
pub struct VdpError {
    /// The VDPAU status code, or 0 when the failure happened before
    /// VDPAU was reached (X11 connect, dlsym, …).
    pub status: VdpStatus,
    /// Human-readable description.
    pub message: String,
}

impl VdpError {
    pub(crate) fn other(message: impl Into<String>) -> Self {
        Self {
            status: 0,
            message: message.into(),
        }
    }

    pub(crate) fn vdp(status: VdpStatus, where_: &str) -> Self {
        Self {
            status,
            message: format!("{where_}: VdpStatus={status}"),
        }
    }
}

impl std::fmt::Display for VdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.status != 0 {
            write!(f, "{} (VdpStatus={})", self.message, self.status)
        } else {
            write!(f, "{}", self.message)
        }
    }
}

impl std::error::Error for VdpError {}

// ─────────────────────────── Display ─────────────────────────────────────────

/// X11 `Display*` connection. Owns the pointer; closes it on drop.
///
/// `Display` is `Send` but **not** `Sync`: Xlib without
/// `XInitThreads` is not thread-safe even for read-only access. The
/// bridge uses one Display per thread or external synchronisation.
pub struct Display {
    raw: *mut XDisplay,
}

// SAFETY: Xlib explicitly allows transferring a Display* between
// threads as long as access is serialised. We don't expose `&Display`
// methods that perform concurrent writes on the same connection.
unsafe impl Send for Display {}

impl Display {
    /// Open an X server connection. `name` should look like `":0"` or
    /// `":0.0"` (an empty string also works and means "use $DISPLAY").
    pub fn open(name: &str) -> Result<Self, VdpError> {
        let vt = sys::vtable().map_err(VdpError::other)?;
        let cstr = CString::new(name)
            .map_err(|e| VdpError::other(format!("display name has interior NUL: {e}")))?;
        let raw = unsafe { (vt.x_open_display)(cstr.as_ptr() as *const u8) };
        if raw.is_null() {
            return Err(VdpError::other(format!(
                "XOpenDisplay({name:?}) returned NULL — is the X server reachable?"
            )));
        }
        Ok(Self { raw })
    }

    /// Open the display indicated by `$DISPLAY`, or `:0` if unset.
    pub fn open_from_env() -> Result<Self, VdpError> {
        let name = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
        Self::open(&name)
    }

    /// Default screen index for this display.
    pub fn default_screen(&self) -> i32 {
        // SAFETY: vtable is initialised because we constructed via
        // `open` which already required it, and self.raw is a valid
        // Display* until Drop runs.
        let vt = sys::vtable().expect("vtable already loaded");
        unsafe { (vt.x_default_screen)(self.raw) }
    }

    /// Create a `VdpDevice` from this X display.
    pub fn create_vdp_device(&self) -> Result<VdpDevice, VdpError> {
        let vt = sys::vtable().map_err(VdpError::other)?;
        let screen = self.default_screen();

        let mut device: sys::VdpDevice = 0;
        // VDPAU writes the function pointer into our `MaybeUninit`
        // slot. Using MaybeUninit here is just paranoia — the C call
        // is guaranteed to write the slot on success — but it's the
        // most defensible way to express "out parameter".
        let mut get_proc: MaybeUninit<VdpGetProcAddress> = MaybeUninit::uninit();

        // SAFETY: self.raw is a valid Display* obtained from
        // XOpenDisplay; `device` and the get_proc slot are stack
        // locations the callee writes to; no further aliasing.
        let status = unsafe {
            (vt.vdp_device_create_x11)(self.raw, screen, &mut device, get_proc.as_mut_ptr())
        };
        if status != VDP_STATUS_OK {
            return Err(VdpError::vdp(status, "vdp_device_create_x11"));
        }
        // SAFETY: VDPAU initialised `get_proc` on success. The
        // documented VDPAU contract is that the callee writes a
        // valid function pointer.
        let get_proc: VdpGetProcAddress = unsafe { get_proc.assume_init() };

        // Resolve the post-bootstrap dispatch table.
        let dispatch = Arc::new(Dispatch::resolve(device, get_proc)?);

        Ok(VdpDevice {
            handle: device,
            dispatch,
        })
    }

    /// Raw `XDisplay*`. Escape hatch for code that needs to call
    /// libX11 directly via `sys::vtable()`.
    pub fn as_raw(&self) -> *mut XDisplay {
        self.raw
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // Best-effort close. If the vtable somehow failed to load,
        // we leak the connection rather than panic in drop.
        if let Ok(vt) = sys::vtable() {
            // SAFETY: self.raw was allocated by XOpenDisplay; we own
            // it and have not handed out aliasing pointers.
            unsafe {
                (vt.x_close_display)(self.raw);
            }
        }
    }
}

// ─────────────────────────── Dispatch table ──────────────────────────────────

/// Post-bootstrap dispatch table resolved through `VdpGetProcAddress`.
///
/// Every entry is `unsafe extern "C" fn(...)` — the safe wrappers in
/// `VdpDevice` are responsible for type-checking arguments and
/// converting `VdpStatus` to `Result`.
pub(crate) struct Dispatch {
    device_destroy: FnVdpDeviceDestroy,
    get_api_version: FnVdpGetApiVersion,
    get_information_string: FnVdpGetInformationString,
    decoder_query_capabilities: FnVdpDecoderQueryCapabilities,
    pub(crate) video_surface_create: FnVdpVideoSurfaceCreate,
    pub(crate) video_surface_destroy: FnVdpVideoSurfaceDestroy,
    pub(crate) video_surface_get_bits_ycbcr: FnVdpVideoSurfaceGetBitsYCbCr,
    pub(crate) decoder_create: FnVdpDecoderCreate,
    pub(crate) decoder_destroy: FnVdpDecoderDestroy,
    pub(crate) decoder_render: FnVdpDecoderRender,
}

impl Dispatch {
    fn resolve(device: sys::VdpDevice, get_proc: VdpGetProcAddress) -> Result<Self, VdpError> {
        // Helper: ask VDPAU for the function pointer for `id` and
        // transmute to the right Rust signature.
        unsafe fn fetch<F: Copy>(
            device: sys::VdpDevice,
            get_proc: VdpGetProcAddress,
            id: sys::VdpFuncId,
            name: &str,
        ) -> Result<F, VdpError> {
            assert_eq!(
                std::mem::size_of::<F>(),
                std::mem::size_of::<*mut c_void>(),
                "fetched FnPtr type must be pointer-sized"
            );
            let mut out: *mut c_void = ptr::null_mut();
            // SAFETY: get_proc is the dispatch table entry-point
            // returned by vdp_device_create_x11; out is a stack slot.
            let status = unsafe { get_proc(device, id, &mut out) };
            if status != VDP_STATUS_OK {
                return Err(VdpError::vdp(status, &format!("VdpGetProcAddress({name})")));
            }
            if out.is_null() {
                return Err(VdpError::other(format!(
                    "VdpGetProcAddress({name}) returned NULL"
                )));
            }
            // SAFETY: VDPAU returns a real function pointer of the
            // documented signature; F is chosen to match per call
            // site, and the size assert above guards against accidental
            // mis-typing on exotic ABIs.
            Ok(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&out) })
        }

        // SAFETY: see `fetch`.
        unsafe {
            Ok(Self {
                device_destroy: fetch(device, get_proc, VDP_FUNC_ID_DEVICE_DESTROY, "DEVICE_DESTROY")?,
                get_api_version: fetch(device, get_proc, VDP_FUNC_ID_GET_API_VERSION, "GET_API_VERSION")?,
                get_information_string: fetch(
                    device,
                    get_proc,
                    VDP_FUNC_ID_GET_INFORMATION_STRING,
                    "GET_INFORMATION_STRING",
                )?,
                decoder_query_capabilities: fetch(
                    device,
                    get_proc,
                    VDP_FUNC_ID_DECODER_QUERY_CAPABILITIES,
                    "DECODER_QUERY_CAPABILITIES",
                )?,
                video_surface_create: fetch(
                    device,
                    get_proc,
                    VDP_FUNC_ID_VIDEO_SURFACE_CREATE,
                    "VIDEO_SURFACE_CREATE",
                )?,
                video_surface_destroy: fetch(
                    device,
                    get_proc,
                    VDP_FUNC_ID_VIDEO_SURFACE_DESTROY,
                    "VIDEO_SURFACE_DESTROY",
                )?,
                video_surface_get_bits_ycbcr: fetch(
                    device,
                    get_proc,
                    VDP_FUNC_ID_VIDEO_SURFACE_GET_BITS_Y_CB_CR,
                    "VIDEO_SURFACE_GET_BITS_Y_CB_CR",
                )?,
                decoder_create: fetch(
                    device,
                    get_proc,
                    VDP_FUNC_ID_DECODER_CREATE,
                    "DECODER_CREATE",
                )?,
                decoder_destroy: fetch(
                    device,
                    get_proc,
                    VDP_FUNC_ID_DECODER_DESTROY,
                    "DECODER_DESTROY",
                )?,
                decoder_render: fetch(
                    device,
                    get_proc,
                    VDP_FUNC_ID_DECODER_RENDER,
                    "DECODER_RENDER",
                )?,
            })
        }
    }
}

// ─────────────────────────── VdpDevice ───────────────────────────────────────

/// Safe wrapper around a VDPAU `VdpDevice` handle plus the
/// post-bootstrap dispatch table resolved at creation time.
///
/// Drops invoke `VdpDeviceDestroy`. The wrapper is `Send` (the device
/// handle is just a `u32`, and the dispatch table is read-only after
/// resolve) but **not** `Sync` — concurrent VDPAU calls on the same
/// device are not guaranteed safe across drivers.
pub struct VdpDevice {
    handle: sys::VdpDevice,
    dispatch: Arc<Dispatch>,
}

// SAFETY: handle is `u32`; Dispatch holds only `unsafe extern "C" fn`
// pointers which are themselves Send. Do not implement Sync.
unsafe impl Send for VdpDevice {}

impl VdpDevice {
    /// Raw VDPAU device handle. Escape hatch for code that wants to
    /// call entry points the wrapper does not yet expose.
    pub fn raw(&self) -> sys::VdpDevice {
        self.handle
    }

    /// Driver-supplied implementation/version string. On NVIDIA this
    /// looks like `"NVIDIA VDPAU Driver Shared Library  580.95.05  …"`.
    pub fn information_string(&self) -> Result<String, VdpError> {
        let mut s: *const std::os::raw::c_char = ptr::null();
        // SAFETY: dispatch entry was resolved via VdpGetProcAddress;
        // `s` is a stack slot the callee writes to.
        let status = unsafe { (self.dispatch.get_information_string)(&mut s) };
        if status != VDP_STATUS_OK {
            return Err(VdpError::vdp(status, "VdpGetInformationString"));
        }
        if s.is_null() {
            return Err(VdpError::other(
                "VdpGetInformationString returned NULL pointer",
            ));
        }
        // SAFETY: the C string is statically allocated by the driver
        // and remains valid for the process lifetime; we copy out.
        let cstr = unsafe { CStr::from_ptr(s) };
        Ok(cstr.to_string_lossy().into_owned())
    }

    /// VDPAU API version implemented by the loaded backend driver.
    pub fn api_version(&self) -> Result<u32, VdpError> {
        let mut v: u32 = 0;
        // SAFETY: dispatch entry was resolved via VdpGetProcAddress.
        let status = unsafe { (self.dispatch.get_api_version)(&mut v) };
        if status != VDP_STATUS_OK {
            return Err(VdpError::vdp(status, "VdpGetApiVersion"));
        }
        Ok(v)
    }

    /// Allocate a new `VdpVideoSurface` of `chroma_type`, sized
    /// `width × height`. The returned wrapper releases the surface
    /// in its `Drop`.
    pub fn create_video_surface(
        &self,
        chroma_type: VdpChromaType,
        width: u32,
        height: u32,
    ) -> Result<VdpVideoSurface, VdpError> {
        let mut surface: sys::VdpVideoSurface = 0;
        // SAFETY: dispatch entry resolved via VdpGetProcAddress; the
        // out-pointer is a stack slot.
        let status = unsafe {
            (self.dispatch.video_surface_create)(
                self.handle,
                chroma_type,
                width,
                height,
                &mut surface,
            )
        };
        if status != VDP_STATUS_OK {
            return Err(VdpError::vdp(status, "VdpVideoSurfaceCreate"));
        }
        Ok(VdpVideoSurface {
            handle: surface,
            width,
            height,
            chroma_type,
            dispatch: Arc::clone(&self.dispatch),
        })
    }

    /// Allocate a new `VdpDecoder` for `profile`, sized
    /// `width × height` with `max_references` DPB slots. The returned
    /// wrapper releases the decoder in its `Drop`.
    pub fn create_decoder(
        &self,
        profile: VdpDecoderProfile,
        width: u32,
        height: u32,
        max_references: u32,
    ) -> Result<VdpDecoder, VdpError> {
        let mut decoder: sys::VdpDecoder = 0;
        // SAFETY: dispatch entry resolved via VdpGetProcAddress; the
        // out-pointer is a stack slot.
        let status = unsafe {
            (self.dispatch.decoder_create)(
                self.handle,
                profile,
                width,
                height,
                max_references,
                &mut decoder,
            )
        };
        if status != VDP_STATUS_OK {
            return Err(VdpError::vdp(status, "VdpDecoderCreate"));
        }
        Ok(VdpDecoder {
            handle: decoder,
            profile,
            width,
            height,
            dispatch: Arc::clone(&self.dispatch),
        })
    }

    /// Query whether `profile` is supported by this device, plus the
    /// driver's advertised maxima.
    pub fn decoder_caps(&self, profile: VdpDecoderProfile) -> Result<DecoderCaps, VdpError> {
        let mut supported: sys::VdpBool = 0;
        let mut max_level: u32 = 0;
        let mut max_macroblocks: u32 = 0;
        let mut max_width: u32 = 0;
        let mut max_height: u32 = 0;
        // SAFETY: dispatch entry resolved via VdpGetProcAddress; all
        // out-pointers are stack slots.
        let status = unsafe {
            (self.dispatch.decoder_query_capabilities)(
                self.handle,
                profile,
                &mut supported,
                &mut max_level,
                &mut max_macroblocks,
                &mut max_width,
                &mut max_height,
            )
        };
        if status != VDP_STATUS_OK {
            return Err(VdpError::vdp(status, "VdpDecoderQueryCapabilities"));
        }
        Ok(DecoderCaps {
            supported: supported != 0,
            max_level,
            max_macroblocks,
            max_width,
            max_height,
        })
    }
}

impl Drop for VdpDevice {
    fn drop(&mut self) {
        // Best-effort destroy. Errors are ignored — we have no
        // sensible way to surface them from drop, and the X
        // connection close (in Display::drop) will tear down any
        // remaining resources anyway.
        // SAFETY: handle was returned by vdp_device_create_x11 and
        // has not been destroyed previously (Drop only runs once).
        unsafe {
            (self.dispatch.device_destroy)(self.handle);
        }
    }
}

// ─────────────────────────── DecoderCaps ─────────────────────────────────────

/// Decoder capability query result, mirroring the five out-parameters
/// of `VdpDecoderQueryCapabilities`.
#[derive(Debug, Clone, Copy)]
pub struct DecoderCaps {
    /// Whether the queried profile is supported on this device.
    pub supported: bool,
    /// Maximum codec-level value supported (driver-defined units).
    pub max_level: u32,
    /// Maximum number of macroblocks per frame.
    pub max_macroblocks: u32,
    /// Maximum supported surface width, in pixels.
    pub max_width: u32,
    /// Maximum supported surface height, in pixels.
    pub max_height: u32,
}

// ─────────────────────────── VdpVideoSurface ─────────────────────────────────

/// Safe wrapper around a `VdpVideoSurface` handle. Drops invoke
/// `VdpVideoSurfaceDestroy`.
///
/// The wrapper holds an `Arc<Dispatch>` so the dispatch table outlives
/// the surface even if the parent `VdpDevice` was dropped first
/// (VDPAU permits child handles to outlive the device handle from a
/// pure refcount perspective, though it's bad form; the Arc keeps the
/// function pointer table alive for our `Drop`).
pub struct VdpVideoSurface {
    handle: sys::VdpVideoSurface,
    width: u32,
    height: u32,
    chroma_type: VdpChromaType,
    dispatch: Arc<Dispatch>,
}

// SAFETY: handle is `u32`; Arc<Dispatch> is Send because Dispatch is
// read-only after construction.
unsafe impl Send for VdpVideoSurface {}

impl VdpVideoSurface {
    /// Raw VDPAU surface handle.
    pub fn raw(&self) -> sys::VdpVideoSurface {
        self.handle
    }

    /// Surface width in pixels (as requested at creation).
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Surface height in pixels (as requested at creation).
    pub fn height(&self) -> u32 {
        self.height
    }

    /// `VdpChromaType` the surface was created with.
    pub fn chroma_type(&self) -> VdpChromaType {
        self.chroma_type
    }

    /// Copy out the surface contents as YCbCr in `format`. The
    /// caller-supplied `dest_planes` are pointers to plane
    /// buffers (one per plane required by `format`) and
    /// `dest_pitches` carries the row stride in bytes for each
    /// plane.
    ///
    /// # Safety
    ///
    /// Caller must ensure each destination plane is sized large
    /// enough for the surface dimensions and the chosen format
    /// (`pitch * height_in_rows`). For NV12 there are 2 planes (Y
    /// full-res, UV half-res interleaved); for YV12 there are 3
    /// planes (Y, V, U at half-res each).
    pub unsafe fn get_bits_ycbcr(
        &self,
        format: VdpYCbCrFormat,
        dest_planes: &[*mut c_void],
        dest_pitches: &[u32],
    ) -> Result<(), VdpError> {
        if dest_planes.len() != dest_pitches.len() {
            return Err(VdpError::other(format!(
                "get_bits_ycbcr: plane/pitch length mismatch ({} vs {})",
                dest_planes.len(),
                dest_pitches.len()
            )));
        }
        // SAFETY: caller upholds that dest_planes / dest_pitches
        // describe valid buffers for the chosen format; dispatch
        // entry resolved via VdpGetProcAddress.
        let status = unsafe {
            (self.dispatch.video_surface_get_bits_ycbcr)(
                self.handle,
                format,
                dest_planes.as_ptr(),
                dest_pitches.as_ptr(),
            )
        };
        if status != VDP_STATUS_OK {
            return Err(VdpError::vdp(status, "VdpVideoSurfaceGetBitsYCbCr"));
        }
        Ok(())
    }
}

impl Drop for VdpVideoSurface {
    fn drop(&mut self) {
        // SAFETY: handle was returned by VdpVideoSurfaceCreate; we
        // own it. Best-effort destroy.
        unsafe {
            (self.dispatch.video_surface_destroy)(self.handle);
        }
    }
}

// ─────────────────────────── VdpDecoder ──────────────────────────────────────

/// Safe wrapper around a `VdpDecoder` handle. Drops invoke
/// `VdpDecoderDestroy`.
pub struct VdpDecoder {
    handle: sys::VdpDecoder,
    profile: VdpDecoderProfile,
    width: u32,
    height: u32,
    dispatch: Arc<Dispatch>,
}

// SAFETY: handle is `u32`; Arc<Dispatch> is Send because Dispatch is
// read-only after construction.
unsafe impl Send for VdpDecoder {}

impl VdpDecoder {
    /// Raw VDPAU decoder handle.
    pub fn raw(&self) -> sys::VdpDecoder {
        self.handle
    }

    /// Decoder profile this instance was created for.
    pub fn profile(&self) -> VdpDecoderProfile {
        self.profile
    }

    /// Decoder width (matches the target surface width the decoder
    /// was created for).
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Decoder height.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Submit a picture for decode. The picture-info pointer must
    /// be the codec-specific struct corresponding to the decoder's
    /// profile (e.g. `VdpPictureInfoH264` for H.264 profiles).
    ///
    /// # Safety
    ///
    /// `picture_info` must point to a valid struct matching the
    /// decoder's profile, and the inner `bitstream` pointers in
    /// `bitstream_buffers` must reference live buffer memory for the
    /// duration of the call.
    pub unsafe fn render(
        &self,
        target: &VdpVideoSurface,
        picture_info: *const c_void,
        bitstream_buffers: &[VdpBitstreamBuffer],
    ) -> Result<(), VdpError> {
        // SAFETY: dispatch entry resolved via VdpGetProcAddress;
        // caller upholds `picture_info` validity per the docstring.
        let status = unsafe {
            (self.dispatch.decoder_render)(
                self.handle,
                target.raw(),
                picture_info,
                bitstream_buffers.len() as u32,
                bitstream_buffers.as_ptr(),
            )
        };
        if status != VDP_STATUS_OK {
            return Err(VdpError::vdp(status, "VdpDecoderRender"));
        }
        Ok(())
    }
}

impl Drop for VdpDecoder {
    fn drop(&mut self) {
        // SAFETY: handle was returned by VdpDecoderCreate; we own
        // it. Best-effort destroy.
        unsafe {
            (self.dispatch.decoder_destroy)(self.handle);
        }
    }
}
