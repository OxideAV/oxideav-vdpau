//! Minimal VP9 IVF/uncompressed-header parser and a single-keyframe
//! VDPAU decode glue layer (Round 4).
//!
//! # Scope
//!
//! Like the H.264 / HEVC siblings, this is **not** a full VP9 parser.
//! We extract exactly the fields `VdpPictureInfoVP9` requires for
//! VDPAU's `VdpDecoderRender` to decode a single keyframe into a
//! `VdpVideoSurface`. Inter-frame prediction, frame-context management,
//! show_existing_frame, and superframes are out of scope.
//!
//! VDPAU's VP9 picture-info struct is **uncompressed-header-driven**:
//! VDPAU expects the uncompressed-header bytes parsed into
//! `VdpPictureInfoVP9` and the full frame payload (including the
//! uncompressed header bytes) handed back to the GPU via
//! `VdpBitstreamBuffer`.
//!
//! IVF is a thin two-tier container:
//!   - 32-byte file header (`DKIF` magic, codec FourCC, dimensions,
//!     framerate, frame count)
//!   - per-frame: 12-byte header (4-byte LE size + 8-byte LE timestamp),
//!     followed by the raw VP9 frame payload.
//!
//! # References
//!
//! VP9 Bitstream & Decoding Process Specification (2016-12, Adrian
//! Grange, Peter de Rivaz, Jonathan Hunt, Google) — public spec
//! shipped by the WebM project. Section 6.2 (uncompressed_header),
//! 6.2.1 (frame_size), 6.2.2 (render_size), 6.2.3 (frame_size_with_refs),
//! 6.2.4 (loop_filter_params), 6.2.5 (quantization_params), 6.2.6
//! (segmentation_params), 6.2.7 (tile_info).

use std::ffi::c_void;

use crate::device::{VdpDecoder, VdpDevice, VdpError};
use crate::h264::{DecodedFrame, get_bits_nv12_as_i420};
use crate::sys::{
    VDP_BITSTREAM_BUFFER_VERSION, VDP_CHROMA_TYPE_420, VDP_DECODER_PROFILE_VP9_PROFILE_0,
    VDP_INVALID_HANDLE, VdpBitstreamBuffer, VdpPictureInfoVP9,
};

// ─────────────────────────── IVF parsing ────────────────────────────────────

/// Parsed IVF file header — the 32-byte preamble at the start of an
/// IVF file. We only carry the fields we use.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IvfHeader {
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct IvfFrame<'a> {
    pub payload: &'a [u8],
    pub _timestamp: u64,
}

/// Parse the IVF file header. Returns the header plus a slice
/// pointing at the first per-frame block.
pub(crate) fn parse_ivf(buf: &[u8]) -> Result<(IvfHeader, &[u8]), VdpError> {
    if buf.len() < 32 {
        return Err(VdpError::other("IVF file shorter than 32-byte header"));
    }
    if &buf[0..4] != b"DKIF" {
        return Err(VdpError::other(format!(
            "IVF magic mismatch: {:?}",
            &buf[0..4]
        )));
    }
    let version = u16::from_le_bytes([buf[4], buf[5]]);
    if version != 0 {
        return Err(VdpError::other(format!("IVF version {version} != 0")));
    }
    let header_len = u16::from_le_bytes([buf[6], buf[7]]) as usize;
    if header_len != 32 {
        return Err(VdpError::other(format!(
            "IVF header_len {header_len} != 32"
        )));
    }
    if &buf[8..12] != b"VP90" {
        return Err(VdpError::other(format!(
            "IVF codec FourCC {:?} != VP90",
            &buf[8..12]
        )));
    }
    let width = u16::from_le_bytes([buf[12], buf[13]]);
    let height = u16::from_le_bytes([buf[14], buf[15]]);
    Ok((IvfHeader { width, height }, &buf[32..]))
}

/// Parse the next IVF frame. Returns `Ok(None)` at EOF.
pub(crate) fn parse_ivf_frame(buf: &[u8]) -> Result<Option<(IvfFrame<'_>, &[u8])>, VdpError> {
    if buf.is_empty() {
        return Ok(None);
    }
    if buf.len() < 12 {
        return Err(VdpError::other("truncated IVF frame header"));
    }
    let size = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let ts = u64::from_le_bytes([
        buf[4], buf[5], buf[6], buf[7], buf[8], buf[9], buf[10], buf[11],
    ]);
    if buf.len() < 12 + size {
        return Err(VdpError::other("truncated IVF frame payload"));
    }
    let payload = &buf[12..12 + size];
    let rest = &buf[12 + size..];
    Ok(Some((
        IvfFrame {
            payload,
            _timestamp: ts,
        },
        rest,
    )))
}

// ─────────────────────────── Bit reader (MSB-first, byte-aligned) ───────────

struct BitReader<'a> {
    bytes: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit_pos: 0 }
    }

    fn byte_offset(&self) -> usize {
        // Round up to next whole byte.
        (self.bit_pos + 7) / 8
    }

    fn bit_offset(&self) -> usize {
        self.bit_pos
    }

    fn f(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        let mut value: u32 = 0;
        for _ in 0..n {
            let byte_idx = self.bit_pos / 8;
            let shift = 7 - (self.bit_pos % 8) as u32;
            let bit = if byte_idx < self.bytes.len() {
                ((self.bytes[byte_idx] >> shift) & 1) as u32
            } else {
                0
            };
            value = (value << 1) | bit;
            self.bit_pos += 1;
        }
        value
    }

    fn s(&mut self, n: u32) -> i32 {
        let value = self.f(n) as i32;
        let sign = self.f(1);
        if sign == 1 { -value } else { value }
    }
}

// ─────────────────────────── Parsed uncompressed header ─────────────────────

#[derive(Debug, Clone, Default)]
pub(crate) struct Vp9UncompressedHeader {
    pub profile: u8,
    pub show_existing_frame: u8,
    pub frame_type: u8, // 0 = KEY_FRAME, 1 = NON_KEY_FRAME
    pub show_frame: u8,
    pub error_resilient_mode: u8,
    pub bit_depth: u8, // 8, 10, or 12
    pub color_space: u8,
    pub color_range: u8,
    pub subsampling_x: u8,
    pub subsampling_y: u8,
    pub width: u32,
    pub height: u32,
    pub intra_only: u8,
    pub reset_frame_context: u8,
    pub refresh_frame_flags: u8,
    pub allow_high_precision_mv: u8,
    pub interpolation_filter: u8,
    pub refresh_frame_context: u8,
    pub frame_parallel_decoding_mode: u8,
    pub frame_context_idx: u8,
    pub loop_filter_level: u8,
    pub loop_filter_sharpness: u8,
    pub loop_filter_mode_ref_delta_enabled: u8,
    pub loop_filter_ref_deltas: [i32; 4],
    pub loop_filter_mode_deltas: [i32; 2],
    pub base_q_idx: i32,
    pub delta_q_y_dc: i32,
    pub delta_q_uv_dc: i32,
    pub delta_q_uv_ac: i32,
    pub segmentation_enabled: u8,
    pub segmentation_update_map: u8,
    pub segmentation_temporal_update: u8,
    pub segmentation_update_data: u8,
    pub segmentation_abs_or_delta_update: u8,
    pub segment_feature_enable: [[u8; 4]; 8],
    pub segment_feature_data: [[i16; 4]; 8],
    pub mb_segment_tree_probs: [u8; 7],
    pub segment_pred_probs: [u8; 3],
    pub log2_tile_cols: u8,
    pub log2_tile_rows: u8,
    pub uncompressed_header_size: u32,
    pub compressed_header_size: u32,
    /// Reference frame indices [0..3] for LAST / GOLDEN / ALTREF refs.
    pub _ref_frame_idx: [u8; 3],
    pub ref_frame_sign_bias: [u8; 4],
}

const KEY_FRAME: u8 = 0;
const VP9_SYNC_CODE: u32 = 0x49_8342;

fn read_color_config(r: &mut BitReader<'_>, h: &mut Vp9UncompressedHeader) -> Result<(), VdpError> {
    if h.profile >= 2 {
        let ten_or_twelve_bit_depth = r.f(1);
        h.bit_depth = if ten_or_twelve_bit_depth != 0 { 12 } else { 10 };
    } else {
        h.bit_depth = 8;
    }
    h.color_space = r.f(3) as u8;
    if h.color_space != 7 {
        // 7 = SRGB
        h.color_range = r.f(1) as u8;
        if h.profile == 1 || h.profile == 3 {
            h.subsampling_x = r.f(1) as u8;
            h.subsampling_y = r.f(1) as u8;
            let _reserved = r.f(1);
        } else {
            h.subsampling_x = 1;
            h.subsampling_y = 1;
        }
    } else {
        h.color_range = 1;
        if h.profile == 1 || h.profile == 3 {
            let _reserved = r.f(1);
        }
        h.subsampling_x = 0;
        h.subsampling_y = 0;
    }
    Ok(())
}

fn read_frame_size(r: &mut BitReader<'_>, h: &mut Vp9UncompressedHeader) {
    h.width = r.f(16) + 1;
    h.height = r.f(16) + 1;
    let render_and_frame_size_different = r.f(1);
    if render_and_frame_size_different != 0 {
        let _render_width_minus_1 = r.f(16);
        let _render_height_minus_1 = r.f(16);
    }
}

fn read_loop_filter_params(r: &mut BitReader<'_>, h: &mut Vp9UncompressedHeader) {
    h.loop_filter_level = r.f(6) as u8;
    h.loop_filter_sharpness = r.f(3) as u8;
    let mode_ref_delta_enabled = r.f(1);
    h.loop_filter_mode_ref_delta_enabled = mode_ref_delta_enabled as u8;
    if mode_ref_delta_enabled != 0 {
        let mode_ref_delta_update = r.f(1);
        if mode_ref_delta_update != 0 {
            for i in 0..4 {
                let update_ref_delta = r.f(1);
                if update_ref_delta != 0 {
                    h.loop_filter_ref_deltas[i] = r.s(6);
                }
            }
            for i in 0..2 {
                let update_mode_delta = r.f(1);
                if update_mode_delta != 0 {
                    h.loop_filter_mode_deltas[i] = r.s(6);
                }
            }
        }
    }
}

fn read_delta_q(r: &mut BitReader<'_>) -> i32 {
    let delta_coded = r.f(1);
    if delta_coded != 0 { r.s(4) } else { 0 }
}

fn read_quantization_params(r: &mut BitReader<'_>, h: &mut Vp9UncompressedHeader) {
    h.base_q_idx = r.f(8) as i32;
    h.delta_q_y_dc = read_delta_q(r);
    h.delta_q_uv_dc = read_delta_q(r);
    h.delta_q_uv_ac = read_delta_q(r);
}

const SEG_LVL_MAX: usize = 4;
const SEGMENT_FEATURE_BITS: [u32; 4] = [8, 6, 2, 0];
const SEGMENT_FEATURE_SIGNED: [bool; 4] = [true, true, false, false];

fn read_segmentation_params(r: &mut BitReader<'_>, h: &mut Vp9UncompressedHeader) {
    h.segmentation_enabled = r.f(1) as u8;
    if h.segmentation_enabled != 0 {
        let update_map = r.f(1);
        h.segmentation_update_map = update_map as u8;
        if update_map != 0 {
            for i in 0..7 {
                let prob_coded = r.f(1);
                h.mb_segment_tree_probs[i] = if prob_coded != 0 { r.f(8) as u8 } else { 255 };
            }
            let temporal_update = r.f(1);
            h.segmentation_temporal_update = temporal_update as u8;
            for i in 0..3 {
                let prob_coded = if temporal_update != 0 { r.f(1) } else { 0 };
                h.segment_pred_probs[i] = if prob_coded != 0 { r.f(8) as u8 } else { 255 };
            }
        }
        let update_data = r.f(1);
        h.segmentation_update_data = update_data as u8;
        if update_data != 0 {
            h.segmentation_abs_or_delta_update = r.f(1) as u8;
            for i in 0..8 {
                for j in 0..SEG_LVL_MAX {
                    let feature_enabled = r.f(1);
                    h.segment_feature_enable[i][j] = feature_enabled as u8;
                    if feature_enabled != 0 {
                        let bits = SEGMENT_FEATURE_BITS[j];
                        let signed = SEGMENT_FEATURE_SIGNED[j];
                        let raw = r.f(bits) as i32;
                        let val = if signed {
                            let sign = r.f(1);
                            if sign != 0 { -raw } else { raw }
                        } else {
                            raw
                        };
                        h.segment_feature_data[i][j] = val as i16;
                    }
                }
            }
        }
    }
}

fn calc_min_log2_tile_cols(sb64_cols: u32) -> u32 {
    let mut min = 0u32;
    while (64 << min) < sb64_cols {
        min += 1;
    }
    min
}

fn calc_max_log2_tile_cols(sb64_cols: u32) -> u32 {
    let mut max = 0u32;
    while (sb64_cols >> (max + 1)) >= 4 {
        max += 1;
    }
    max
}

fn read_tile_info(r: &mut BitReader<'_>, h: &mut Vp9UncompressedHeader) {
    let mi_cols = (h.width + 7) / 8;
    let sb64_cols = (mi_cols + 7) / 8;
    let min_log2 = calc_min_log2_tile_cols(sb64_cols);
    let max_log2 = calc_max_log2_tile_cols(sb64_cols);
    let mut log2_cols = min_log2;
    while log2_cols < max_log2 {
        let increment = r.f(1);
        if increment == 0 {
            break;
        }
        log2_cols += 1;
    }
    h.log2_tile_cols = log2_cols as u8;
    let log2_rows = r.f(1);
    h.log2_tile_rows = if log2_rows != 0 {
        let r2 = r.f(1);
        if r2 != 0 { 2 } else { 1 }
    } else {
        0
    };
}

/// Parse the uncompressed VP9 header (key frame only).
pub(crate) fn parse_uncompressed_header(
    payload: &[u8],
) -> Result<Vp9UncompressedHeader, VdpError> {
    if payload.len() < 8 {
        return Err(VdpError::other("VP9 frame shorter than 8 bytes"));
    }
    let mut r = BitReader::new(payload);
    let mut h = Vp9UncompressedHeader::default();
    let frame_marker = r.f(2);
    if frame_marker != 2 {
        return Err(VdpError::other(format!(
            "VP9 frame_marker {frame_marker} != 2"
        )));
    }
    let profile_low = r.f(1);
    let profile_high = r.f(1);
    h.profile = ((profile_high << 1) | profile_low) as u8;
    if h.profile == 3 {
        let _reserved = r.f(1);
    }
    h.show_existing_frame = r.f(1) as u8;
    if h.show_existing_frame != 0 {
        return Err(VdpError::other(
            "VP9: show_existing_frame=1 not supported in single-keyframe path",
        ));
    }
    h.frame_type = r.f(1) as u8;
    h.show_frame = r.f(1) as u8;
    h.error_resilient_mode = r.f(1) as u8;

    if h.frame_type == KEY_FRAME {
        let sync = r.f(24);
        if sync != VP9_SYNC_CODE {
            return Err(VdpError::other(format!(
                "VP9 frame_sync_code mismatch: 0x{sync:06x} != 0x498342"
            )));
        }
        read_color_config(&mut r, &mut h)?;
        read_frame_size(&mut r, &mut h);
        h.refresh_frame_flags = 0xff;
        // VP9 spec doesn't set intra_only in the keyframe path —
        // intra_only is only relevant for non-keyframes.
        h.intra_only = 0;
        h.frame_context_idx = 0;
        // Keyframes always reset entropy.
        h.refresh_frame_context = 1;
    } else {
        return Err(VdpError::other(
            "VP9: only KEY_FRAME supported in single-keyframe path",
        ));
    }

    // loop_filter_params, quantization_params, segmentation_params, tile_info
    read_loop_filter_params(&mut r, &mut h);
    read_quantization_params(&mut r, &mut h);
    read_segmentation_params(&mut r, &mut h);
    read_tile_info(&mut r, &mut h);

    // first_partition_size = 16 bits.
    let first_partition_size = r.f(16);

    // Round up to next byte for the uncompressed-header size (the
    // remaining bits of the current byte are the byte-alignment padding).
    let uncompressed_size = r.byte_offset();
    h.uncompressed_header_size = uncompressed_size as u32;
    h.compressed_header_size = first_partition_size;

    let _ = r.bit_offset();
    Ok(h)
}

// ─────────────────────────── Vp9VdpauDecoder ────────────────────────────────

pub struct Vp9VdpauDecoder {
    header: Vp9UncompressedHeader,
    decoder: VdpDecoder,
    width: u32,
    height: u32,
}

impl Vp9VdpauDecoder {
    /// Parse the IVF wrapper and create a decoder sized for the
    /// embedded keyframe.
    pub fn new(device: &VdpDevice, ivf: &[u8]) -> Result<Self, VdpError> {
        let (ivf_hdr, body) = parse_ivf(ivf)?;
        let frame = match parse_ivf_frame(body)? {
            Some((f, _)) => f,
            None => return Err(VdpError::other("IVF has no frames")),
        };
        let header = parse_uncompressed_header(frame.payload)?;
        if header.frame_type != KEY_FRAME {
            return Err(VdpError::other("first VP9 frame is not a keyframe"));
        }
        if header.profile != 0 {
            return Err(VdpError::other(format!(
                "VP9 profile {} not supported (only profile 0)",
                header.profile
            )));
        }
        // Use the IVF-advertised dimensions for VdpDecoderCreate; they
        // should match the uncompressed header's frame size.
        debug_assert_eq!(ivf_hdr.width as u32, header.width);
        debug_assert_eq!(ivf_hdr.height as u32, header.height);
        let width = header.width;
        let height = header.height;
        let max_refs = 8u32;
        let decoder = device.create_decoder(VDP_DECODER_PROFILE_VP9_PROFILE_0, width, height, max_refs)?;
        Ok(Self {
            header,
            decoder,
            width,
            height,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn decode_keyframe(&self, device: &VdpDevice, ivf: &[u8]) -> Result<DecodedFrame, VdpError> {
        let (_ivf_hdr, body) = parse_ivf(ivf)?;
        let frame = match parse_ivf_frame(body)? {
            Some((f, _)) => f,
            None => return Err(VdpError::other("IVF has no frames")),
        };
        let pic_info = self.build_pic_info();

        let surface = device.create_video_surface(VDP_CHROMA_TYPE_420, self.width, self.height)?;

        let bs = [VdpBitstreamBuffer {
            struct_version: VDP_BITSTREAM_BUFFER_VERSION,
            bitstream: frame.payload.as_ptr() as *const c_void,
            bitstream_bytes: frame.payload.len() as u32,
        }];

        // SAFETY: pic_info matches the decoder's VP9 profile;
        // frame.payload outlives this call (lives in the input buffer).
        unsafe {
            self.decoder.render(
                &surface,
                &pic_info as *const VdpPictureInfoVP9 as *const c_void,
                &bs,
            )?;
        }
        let _ = pic_info; // keep struct alive across the unsafe call
        get_bits_nv12_as_i420(&surface, self.width, self.height)
    }

    fn build_pic_info(&self) -> VdpPictureInfoVP9 {
        let h = &self.header;
        // Quantization: Y_DC and UV planes derived from base_q_idx +
        // delta_q_*. NVIDIA expects raw QP indices (0..255) clamped.
        let qp_y_ac = h.base_q_idx;
        let qp_y_dc = (h.base_q_idx + h.delta_q_y_dc).clamp(0, 255);
        let qp_ch_dc = (h.base_q_idx + h.delta_q_uv_dc).clamp(0, 255);
        let qp_ch_ac = (h.base_q_idx + h.delta_q_uv_ac).clamp(0, 255);

        VdpPictureInfoVP9 {
            width: self.width,
            height: self.height,
            // Keyframe → no refs.
            last_reference: VDP_INVALID_HANDLE,
            golden_reference: VDP_INVALID_HANDLE,
            alt_reference: VDP_INVALID_HANDLE,
            color_space: h.color_space,
            profile: h.profile as u16,
            frame_context_idx: h.frame_context_idx as u16,
            key_frame: 1,
            show_frame: h.show_frame as u16,
            error_resilient: h.error_resilient_mode as u16,
            frame_parallel_decoding: h.frame_parallel_decoding_mode as u16,
            sub_sampling_x: h.subsampling_x as u16,
            sub_sampling_y: h.subsampling_y as u16,
            intra_only: h.intra_only as u16,
            allow_high_precision_mv: h.allow_high_precision_mv as u16,
            refresh_entropy_probs: h.refresh_frame_context as u16,
            ref_frame_sign_bias: h.ref_frame_sign_bias,
            bit_depth_minus8_luma: h.bit_depth.saturating_sub(8),
            bit_depth_minus8_chroma: h.bit_depth.saturating_sub(8),
            loop_filter_level: h.loop_filter_level,
            loop_filter_sharpness: h.loop_filter_sharpness,
            mode_ref_lf_enabled: h.loop_filter_mode_ref_delta_enabled,
            log2_tile_columns: h.log2_tile_cols,
            log2_tile_rows: h.log2_tile_rows,
            segment_enabled: h.segmentation_enabled,
            segment_map_update: h.segmentation_update_map,
            segment_map_temporal_update: h.segmentation_temporal_update,
            segment_feature_mode: h.segmentation_abs_or_delta_update,
            segment_feature_enable: h.segment_feature_enable,
            segment_feature_data: h.segment_feature_data,
            mb_segment_tree_probs: h.mb_segment_tree_probs,
            segment_pred_probs: h.segment_pred_probs,
            reserved_segment_16_bits: [0u8; 2],
            qp_y_ac,
            qp_y_dc,
            qp_ch_dc,
            qp_ch_ac,
            // Keyframe: no active refs, but VDPAU still wants the
            // table populated.
            active_ref_idx: [0u32; 3],
            reset_frame_context: h.reset_frame_context as u32,
            mcomp_filter_type: h.interpolation_filter as u32,
            mb_ref_lf_delta: [
                h.loop_filter_ref_deltas[0] as u32,
                h.loop_filter_ref_deltas[1] as u32,
                h.loop_filter_ref_deltas[2] as u32,
                h.loop_filter_ref_deltas[3] as u32,
            ],
            mb_mode_lf_delta: [
                h.loop_filter_mode_deltas[0] as u32,
                h.loop_filter_mode_deltas[1] as u32,
            ],
            uncompressed_header_size: h.uncompressed_header_size,
            // NOTE: VP9 spec says first_partition_size is the size of
            // the compressed header. NVIDIA's VDPAU appears to expect
            // a different value here — TODO investigate.
            compressed_header_size: h.compressed_header_size,
        }
    }
}

// ─────────────────────────── Tests ──────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_uncompressed_header_extracts_dimensions_and_qp() {
        const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/vp9_320x240_1frame.ivf");
        let (_h, body) = parse_ivf(FIXTURE).unwrap();
        let (frame, _rest) = parse_ivf_frame(body).unwrap().unwrap();
        let hdr = parse_uncompressed_header(frame.payload).unwrap();
        assert_eq!(hdr.profile, 0);
        assert_eq!(hdr.frame_type, 0);
        assert_eq!(hdr.width, 320);
        assert_eq!(hdr.height, 240);
        assert_eq!(hdr.bit_depth, 8);
        assert_eq!(hdr.base_q_idx, 28);
        assert_eq!(hdr.loop_filter_level, 48);
        assert_eq!(hdr.loop_filter_sharpness, 1);
        // Segmentation enabled, update_map and update_data both off.
        assert_eq!(hdr.segmentation_enabled, 1);
        assert_eq!(hdr.segmentation_update_map, 0);
        assert_eq!(hdr.segmentation_update_data, 0);
        assert_eq!(hdr.log2_tile_cols, 0);
        assert_eq!(hdr.log2_tile_rows, 1);
        assert_eq!(hdr.uncompressed_header_size, 14);
        assert_eq!(hdr.compressed_header_size, 3596);
    }

    #[test]
    fn parse_ivf_recovers_dimensions() {
        // Minimal IVF with VP90, 320x240, no frames.
        let mut buf = Vec::new();
        buf.extend_from_slice(b"DKIF");
        buf.extend_from_slice(&0u16.to_le_bytes()); // version
        buf.extend_from_slice(&32u16.to_le_bytes()); // header_len
        buf.extend_from_slice(b"VP90");
        buf.extend_from_slice(&320u16.to_le_bytes());
        buf.extend_from_slice(&240u16.to_le_bytes());
        // framerate denom/num/length/unused — pad to 32 bytes.
        buf.extend_from_slice(&[0u8; 16]);
        let (h, body) = parse_ivf(&buf).expect("ivf parses");
        assert_eq!(h.width, 320);
        assert_eq!(h.height, 240);
        assert_eq!(body.len(), 0);
    }
}
