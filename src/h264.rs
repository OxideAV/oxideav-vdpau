//! Minimal H.264 SPS/PPS parser and a single-IDR-frame VDPAU decode
//! glue layer (Tier B of Round 3).
//!
//! # Scope
//!
//! This is **not** a full H.264 parser. We extract exactly the fields
//! `VdpPictureInfoH264` requires for VDPAU's `VdpDecoderRender` to
//! decode an IDR (intra-coded keyframe) into a `VdpVideoSurface`. No
//! P/B-frame DPB management, no frame reordering, no MMCOs. The
//! parser handles:
//!
//!   - Annex-B start-code framing,
//!   - SPS through `frame_cropping_*` (skipping VUI),
//!   - PPS through the second-cqp/transform-8x8/scaling-list block,
//!   - skipping `seq_scaling_matrix_present` / `pic_scaling_matrix_present`
//!     scaling lists (we always submit flat 16/16 to the GPU, which
//!     is the encoder default when those flags are unset — the
//!     fixture in this crate's tests is encoded that way).
//!
//! # References
//!
//! ITU-T H.264 (V13) sections 7.3.2.1, 7.3.2.2, 7.3.2.3 (RBSP), 9.1
//! (Exp-Golomb), and 7.4.5 (slice header). The H.264 specification is
//! a public ITU-T standard; using its bitstream syntax is the same
//! shape as following an RFC.

use std::ffi::c_void;

use crate::device::{VdpDecoder, VdpDevice, VdpError, VdpVideoSurface};
use crate::sys::{
    VDP_BITSTREAM_BUFFER_VERSION, VDP_CHROMA_TYPE_420, VDP_DECODER_PROFILE_H264_HIGH,
    VDP_INVALID_HANDLE, VDP_YCBCR_FORMAT_NV12, VdpBitstreamBuffer, VdpPictureInfoH264,
    VdpReferenceFrameH264,
};

// ─────────────────────────── Annex-B framing ─────────────────────────────────

/// Locate every NAL unit in an Annex-B bitstream and return slices
/// pointing at the RBSP payload (start-code stripped, but emulation
/// bytes still in place — VDPAU consumes Annex-B with start codes
/// directly via `VdpBitstreamBuffer`, so this helper is for parsing
/// only).
pub(crate) fn split_nal_units(buf: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    let n = buf.len();
    let mut last_payload_start: Option<usize> = None;
    while i < n {
        // Match 00 00 00 01 or 00 00 01.
        let four = i + 3 < n
            && buf[i] == 0
            && buf[i + 1] == 0
            && buf[i + 2] == 0
            && buf[i + 3] == 1;
        let three = !four && i + 2 < n && buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1;
        if four || three {
            // Close out previous NAL.
            if let Some(start) = last_payload_start.take() {
                out.push(&buf[start..i]);
            }
            i += if four { 4 } else { 3 };
            last_payload_start = Some(i);
            continue;
        }
        i += 1;
    }
    if let Some(start) = last_payload_start.take() {
        out.push(&buf[start..n]);
    }
    out
}

/// Strip H.264 emulation-prevention `0x03` bytes from an RBSP payload.
/// The spec inserts them after any `00 00 0x` to keep start codes
/// unique inside the payload; we strip them before bit-level parsing.
fn strip_emulation_prevention(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut i = 0;
    let n = nal.len();
    while i < n {
        if i + 2 < n && nal[i] == 0 && nal[i + 1] == 0 && nal[i + 2] == 3 {
            out.push(0);
            out.push(0);
            i += 3;
        } else {
            out.push(nal[i]);
            i += 1;
        }
    }
    out
}

// ─────────────────────────── Bit reader ──────────────────────────────────────

struct BitReader<'a> {
    bytes: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit_pos: 0 }
    }

    fn u(&mut self, n: u32) -> u32 {
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

    /// Unsigned Exp-Golomb (`ue(v)`), per H.264 9.1.
    fn ue(&mut self) -> u32 {
        let mut leading_zeros = 0u32;
        while self.bit_pos / 8 < self.bytes.len() && self.u(1) == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return 0;
            }
        }
        if leading_zeros == 0 {
            0
        } else {
            (1u32 << leading_zeros) - 1 + self.u(leading_zeros)
        }
    }

    /// Signed Exp-Golomb (`se(v)`), per H.264 9.1.1.
    fn se(&mut self) -> i32 {
        let code = self.ue();
        if code == 0 {
            0
        } else if code & 1 == 1 {
            code.div_ceil(2) as i32
        } else {
            -((code / 2) as i32)
        }
    }
}

// ─────────────────────────── SPS / PPS structs ───────────────────────────────

/// Sequence parameter set fields needed to populate `VdpPictureInfoH264`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Sps {
    pub _profile_idc: u8,
    pub _constraint_flags: u8,
    pub _level_idc: u8,
    pub _seq_parameter_set_id: u32,
    pub _chroma_format_idc: u32,
    pub _bit_depth_luma_minus8: u32,
    pub _bit_depth_chroma_minus8: u32,
    pub log2_max_frame_num_minus4: u32,
    pub pic_order_cnt_type: u32,
    pub log2_max_pic_order_cnt_lsb_minus4: u32,
    pub delta_pic_order_always_zero_flag: u8,
    pub max_num_ref_frames: u32,
    pub _gaps_in_frame_num_value_allowed_flag: u8,
    pub pic_width_in_mbs_minus1: u32,
    pub pic_height_in_map_units_minus1: u32,
    pub frame_mbs_only_flag: u8,
    pub mb_adaptive_frame_field_flag: u8,
    pub direct_8x8_inference_flag: u8,
}

/// Picture parameter set fields needed to populate `VdpPictureInfoH264`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Pps {
    pub _pic_parameter_set_id: u32,
    pub _seq_parameter_set_id: u32,
    pub entropy_coding_mode_flag: u8,
    pub pic_order_present_flag: u8,
    pub num_slice_groups_minus1: u32,
    pub num_ref_idx_l0_active_minus1: u32,
    pub num_ref_idx_l1_active_minus1: u32,
    pub weighted_pred_flag: u8,
    pub weighted_bipred_idc: u8,
    pub pic_init_qp_minus26: i32,
    pub _pic_init_qs_minus26: i32,
    pub chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present_flag: u8,
    pub constrained_intra_pred_flag: u8,
    pub redundant_pic_cnt_present_flag: u8,
    /// Only present if the High-profile extension block was emitted.
    pub transform_8x8_mode_flag: u8,
    pub second_chroma_qp_index_offset: i32,
}

/// Parse a SPS NAL (excluding NAL header). The `nal` slice is the
/// raw NAL with NAL header byte at index 0; this function strips the
/// header and emulation-prevention bytes itself.
pub(crate) fn parse_sps(nal: &[u8]) -> Result<Sps, VdpError> {
    if nal.is_empty() {
        return Err(VdpError::other("parse_sps: empty NAL"));
    }
    if nal[0] & 0x1f != 7 {
        return Err(VdpError::other(format!(
            "parse_sps: not a SPS NAL (type={})",
            nal[0] & 0x1f
        )));
    }
    let rbsp = strip_emulation_prevention(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    if rbsp.len() < 3 {
        return Err(VdpError::other("parse_sps: SPS shorter than 3 bytes"));
    }
    let profile_idc = r.u(8) as u8;
    let constraint_flags = r.u(8) as u8;
    let level_idc = r.u(8) as u8;
    let mut sps = Sps {
        _profile_idc: profile_idc,
        _constraint_flags: constraint_flags,
        _level_idc: level_idc,
        ..Sps::default()
    };
    sps._seq_parameter_set_id = r.ue();

    // Profiles that carry the High-profile extension block.
    let has_high_ext = matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    );
    if has_high_ext {
        sps._chroma_format_idc = r.ue();
        if sps._chroma_format_idc == 3 {
            let _separate_colour_plane_flag = r.u(1);
        }
        sps._bit_depth_luma_minus8 = r.ue();
        sps._bit_depth_chroma_minus8 = r.ue();
        let _qpprime_y_zero_transform_bypass_flag = r.u(1);
        let seq_scaling_matrix_present_flag = r.u(1);
        if seq_scaling_matrix_present_flag != 0 {
            // Skip scaling lists. Reading them precisely requires
            // implementing the scaling-list parser. For our test
            // fixture the flag is 0 (encoder default with -preset
            // medium / cqm=0); refuse if it's actually set so we
            // don't silently produce garbage picture info.
            return Err(VdpError::other(
                "parse_sps: seq_scaling_matrix_present_flag=1 not supported by minimal parser",
            ));
        }
    } else {
        sps._chroma_format_idc = 1; // 4:2:0
    }

    sps.log2_max_frame_num_minus4 = r.ue();
    sps.pic_order_cnt_type = r.ue();
    if sps.pic_order_cnt_type == 0 {
        sps.log2_max_pic_order_cnt_lsb_minus4 = r.ue();
    } else if sps.pic_order_cnt_type == 1 {
        sps.delta_pic_order_always_zero_flag = r.u(1) as u8;
        let _offset_for_non_ref_pic = r.se();
        let _offset_for_top_to_bottom_field = r.se();
        let num_ref_frames_in_pic_order_cnt_cycle = r.ue();
        for _ in 0..num_ref_frames_in_pic_order_cnt_cycle {
            let _offset_for_ref_frame = r.se();
        }
    }
    sps.max_num_ref_frames = r.ue();
    sps._gaps_in_frame_num_value_allowed_flag = r.u(1) as u8;
    sps.pic_width_in_mbs_minus1 = r.ue();
    sps.pic_height_in_map_units_minus1 = r.ue();
    sps.frame_mbs_only_flag = r.u(1) as u8;
    if sps.frame_mbs_only_flag == 0 {
        sps.mb_adaptive_frame_field_flag = r.u(1) as u8;
    }
    sps.direct_8x8_inference_flag = r.u(1) as u8;
    // The remaining fields (frame_cropping, VUI) don't feed VdpPictureInfoH264.
    Ok(sps)
}

/// Parse a PPS NAL. The High-profile extension block (after
/// `redundant_pic_cnt_present_flag`) is only present when `more_rbsp_data()`
/// returns true; we approximate that by checking whether more than
/// one bit of payload remains beyond the trailing-bit marker.
pub(crate) fn parse_pps(nal: &[u8]) -> Result<Pps, VdpError> {
    if nal.is_empty() {
        return Err(VdpError::other("parse_pps: empty NAL"));
    }
    if nal[0] & 0x1f != 8 {
        return Err(VdpError::other(format!(
            "parse_pps: not a PPS NAL (type={})",
            nal[0] & 0x1f
        )));
    }
    let rbsp = strip_emulation_prevention(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let mut pps = Pps::default();
    pps._pic_parameter_set_id = r.ue();
    pps._seq_parameter_set_id = r.ue();
    pps.entropy_coding_mode_flag = r.u(1) as u8;
    pps.pic_order_present_flag = r.u(1) as u8;
    pps.num_slice_groups_minus1 = r.ue();
    if pps.num_slice_groups_minus1 != 0 {
        // FMO. Fixture doesn't use it; reject so we don't silently
        // mis-parse.
        return Err(VdpError::other(
            "parse_pps: num_slice_groups_minus1>0 (FMO) not supported by minimal parser",
        ));
    }
    pps.num_ref_idx_l0_active_minus1 = r.ue();
    pps.num_ref_idx_l1_active_minus1 = r.ue();
    pps.weighted_pred_flag = r.u(1) as u8;
    pps.weighted_bipred_idc = r.u(2) as u8;
    pps.pic_init_qp_minus26 = r.se();
    pps._pic_init_qs_minus26 = r.se();
    pps.chroma_qp_index_offset = r.se();
    pps.deblocking_filter_control_present_flag = r.u(1) as u8;
    pps.constrained_intra_pred_flag = r.u(1) as u8;
    pps.redundant_pic_cnt_present_flag = r.u(1) as u8;

    // Detect whether more_rbsp_data() is true — i.e. there's at least
    // one more meaningful bit before the rbsp_trailing_bits marker.
    if more_rbsp_data(&r) {
        pps.transform_8x8_mode_flag = r.u(1) as u8;
        let pic_scaling_matrix_present_flag = r.u(1);
        if pic_scaling_matrix_present_flag != 0 {
            return Err(VdpError::other(
                "parse_pps: pic_scaling_matrix_present_flag=1 not supported by minimal parser",
            ));
        }
        pps.second_chroma_qp_index_offset = r.se();
    } else {
        // Field absent → treat second_chroma_qp_index_offset as a
        // copy of the first per H.264 7.4.2.2.
        pps.second_chroma_qp_index_offset = pps.chroma_qp_index_offset;
    }
    Ok(pps)
}

/// Approximate `more_rbsp_data()` — H.264 7.2: there is more RBSP data
/// if the current bit position is not at the rbsp_trailing_bits marker
/// (a `1` followed by 0..7 zero bits at byte alignment).
fn more_rbsp_data(r: &BitReader<'_>) -> bool {
    let total_bits = r.bytes.len() * 8;
    if r.bit_pos >= total_bits {
        return false;
    }
    // Scan: if any '1' bit exists strictly *after* the next '1' bit
    // from the current position (which would be the trailing marker),
    // then there is more RBSP data.
    let mut saw_one = false;
    for p in r.bit_pos..total_bits {
        let b = (r.bytes[p / 8] >> (7 - (p % 8))) & 1;
        if !saw_one {
            if b == 1 {
                saw_one = true;
            }
            // else still scanning towards the trailing marker
        } else if b == 1 {
            return true;
        }
    }
    // We saw the trailing '1' (or nothing); no more data.
    false
}

// ─────────────────────────── Decoded-frame container ─────────────────────────

/// Decoded I420 frame extracted from a `VdpVideoSurface`.
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// Y plane, `width * height` bytes, row-major.
    pub y: Vec<u8>,
    /// Cb (U) plane, `(width/2) * (height/2)` bytes, row-major.
    pub u: Vec<u8>,
    /// Cr (V) plane, `(width/2) * (height/2)` bytes, row-major.
    pub v: Vec<u8>,
}

// ─────────────────────────── H264VdpauDecoder ────────────────────────────────

/// One-shot single-IDR decoder for the Round 3 fixture. Not designed
/// for streams with P/B frames; callers wanting more should write a
/// proper decoder on top of `VdpDevice::create_decoder` and the
/// raw types in `crate::sys`.
pub struct H264VdpauDecoder {
    sps: Sps,
    pps: Pps,
    decoder: VdpDecoder,
    width: u32,
    height: u32,
}

impl H264VdpauDecoder {
    /// Create a decoder configured from the SPS+PPS embedded in
    /// `annex_b`. Allocates the underlying `VdpDecoder` for H.264
    /// High at the SPS-derived dimensions.
    pub fn new(device: &VdpDevice, annex_b: &[u8]) -> Result<Self, VdpError> {
        let nals = split_nal_units(annex_b);
        let sps_nal = nals
            .iter()
            .find(|n| !n.is_empty() && (n[0] & 0x1f) == 7)
            .ok_or_else(|| VdpError::other("H.264 fixture has no SPS NAL"))?;
        let pps_nal = nals
            .iter()
            .find(|n| !n.is_empty() && (n[0] & 0x1f) == 8)
            .ok_or_else(|| VdpError::other("H.264 fixture has no PPS NAL"))?;
        let sps = parse_sps(sps_nal)?;
        let pps = parse_pps(pps_nal)?;

        // Derive coded dimensions: pic_width_in_mbs * 16 and
        // pic_height_in_map_units * 16 * (2 - frame_mbs_only_flag).
        let width = (sps.pic_width_in_mbs_minus1 + 1) * 16;
        let height = (sps.pic_height_in_map_units_minus1 + 1)
            * 16
            * (2 - sps.frame_mbs_only_flag as u32);
        let max_refs = sps.max_num_ref_frames.max(1);

        let decoder =
            device.create_decoder(VDP_DECODER_PROFILE_H264_HIGH, width, height, max_refs)?;

        Ok(Self {
            sps,
            pps,
            decoder,
            width,
            height,
        })
    }

    /// Coded width derived from the SPS.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Coded height derived from the SPS.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Decode the full Annex-B fixture (assumed to contain exactly
    /// one IDR access unit) and return the resulting NV12-deinterleaved
    /// I420 frame.
    pub fn decode_idr(
        &self,
        device: &VdpDevice,
        annex_b: &[u8],
    ) -> Result<DecodedFrame, VdpError> {
        // Build the picture-info struct for an IDR with no references.
        let pic_info = self.build_idr_picture_info();

        // Allocate the target surface.
        let surface = device.create_video_surface(VDP_CHROMA_TYPE_420, self.width, self.height)?;

        // Hand VDPAU the entire Annex-B buffer in one bitstream
        // entry. NVIDIA's driver accepts Annex-B with start codes for
        // H.264 (it parses NAL boundaries internally).
        let bs = [VdpBitstreamBuffer {
            struct_version: VDP_BITSTREAM_BUFFER_VERSION,
            bitstream: annex_b.as_ptr() as *const c_void,
            bitstream_bytes: annex_b.len() as u32,
        }];

        // SAFETY: `pic_info` matches the decoder's H.264 profile;
        // `bs` references `annex_b` which outlives this call.
        unsafe {
            self.decoder.render(
                &surface,
                &pic_info as *const VdpPictureInfoH264 as *const c_void,
                &bs,
            )?;
        }

        // Pull the result back as NV12 (NVIDIA's natural output
        // format) and convert to I420 for the public API.
        get_bits_nv12_as_i420(&surface, self.width, self.height)
    }

    fn build_idr_picture_info(&self) -> VdpPictureInfoH264 {
        let unused_ref = VdpReferenceFrameH264 {
            surface: VDP_INVALID_HANDLE,
            is_long_term: 0,
            top_is_reference: 0,
            bottom_is_reference: 0,
            field_order_cnt: [0, 0],
            frame_idx: 0,
        };
        VdpPictureInfoH264 {
            slice_count: 1,
            field_order_cnt: [0, 0],
            is_reference: 1,
            frame_num: 0,
            field_pic_flag: 0,
            bottom_field_flag: 0,
            num_ref_frames: self.sps.max_num_ref_frames as u8,
            mb_adaptive_frame_field_flag: self.sps.mb_adaptive_frame_field_flag,
            constrained_intra_pred_flag: self.pps.constrained_intra_pred_flag,
            weighted_pred_flag: self.pps.weighted_pred_flag,
            weighted_bipred_idc: self.pps.weighted_bipred_idc,
            frame_mbs_only_flag: self.sps.frame_mbs_only_flag,
            transform_8x8_mode_flag: self.pps.transform_8x8_mode_flag,
            chroma_qp_index_offset: self.pps.chroma_qp_index_offset as i8,
            second_chroma_qp_index_offset: self.pps.second_chroma_qp_index_offset as i8,
            pic_init_qp_minus26: self.pps.pic_init_qp_minus26 as i8,
            num_ref_idx_l0_active_minus1: self.pps.num_ref_idx_l0_active_minus1 as u8,
            num_ref_idx_l1_active_minus1: self.pps.num_ref_idx_l1_active_minus1 as u8,
            log2_max_frame_num_minus4: self.sps.log2_max_frame_num_minus4 as u8,
            pic_order_cnt_type: self.sps.pic_order_cnt_type as u8,
            log2_max_pic_order_cnt_lsb_minus4: self.sps.log2_max_pic_order_cnt_lsb_minus4 as u8,
            delta_pic_order_always_zero_flag: self.sps.delta_pic_order_always_zero_flag,
            direct_8x8_inference_flag: self.sps.direct_8x8_inference_flag,
            entropy_coding_mode_flag: self.pps.entropy_coding_mode_flag,
            pic_order_present_flag: self.pps.pic_order_present_flag,
            deblocking_filter_control_present_flag: self.pps.deblocking_filter_control_present_flag,
            redundant_pic_cnt_present_flag: self.pps.redundant_pic_cnt_present_flag,
            scaling_lists_4x4: [[16u8; 16]; 6],
            scaling_lists_8x8: [[16u8; 64]; 2],
            reference_frames: [unused_ref; 16],
        }
    }
}

/// Read NV12 bytes from `surface` and split the interleaved UV plane
/// into separate U/V buffers (I420 layout).
fn get_bits_nv12_as_i420(
    surface: &VdpVideoSurface,
    width: u32,
    height: u32,
) -> Result<DecodedFrame, VdpError> {
    let w = width as usize;
    let h = height as usize;
    let cw = w / 2;
    let ch = h / 2;
    let mut y_plane = vec![0u8; w * h];
    let mut uv_plane = vec![0u8; cw * 2 * ch];

    let dest_planes: [*mut c_void; 2] = [
        y_plane.as_mut_ptr() as *mut c_void,
        uv_plane.as_mut_ptr() as *mut c_void,
    ];
    let dest_pitches: [u32; 2] = [w as u32, (cw * 2) as u32];

    // SAFETY: dest_planes / dest_pitches describe two correctly sized
    // buffers for NV12 at width × height.
    unsafe {
        surface.get_bits_ycbcr(VDP_YCBCR_FORMAT_NV12, &dest_planes, &dest_pitches)?;
    }

    // Deinterleave UV → U + V.
    let mut u_plane = vec![0u8; cw * ch];
    let mut v_plane = vec![0u8; cw * ch];
    for row in 0..ch {
        for col in 0..cw {
            let src = row * (cw * 2) + col * 2;
            let dst = row * cw + col;
            u_plane[dst] = uv_plane[src];
            v_plane[dst] = uv_plane[src + 1];
        }
    }

    Ok(DecodedFrame {
        width,
        height,
        y: y_plane,
        u: u_plane,
        v: v_plane,
    })
}

// ─────────────────────────── Tests ───────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_finds_three_nal_starts_in_synthetic_buffer() {
        let buf = [
            0, 0, 0, 1, 0x67, 0xaa, // NAL 1
            0, 0, 1, 0x68, 0xbb, // NAL 2
            0, 0, 0, 1, 0x65, 0xcc, 0xdd, // NAL 3
        ];
        let nals = split_nal_units(&buf);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0x67, 0xaa]);
        assert_eq!(nals[1], &[0x68, 0xbb]);
        assert_eq!(nals[2], &[0x65, 0xcc, 0xdd]);
    }

    #[test]
    fn ue_decodes_known_values() {
        // Bit stream: 1 010 011 00100
        //  ue=0  -> "1"
        //  ue=1  -> "010"
        //  ue=2  -> "011"
        //  ue=3  -> "00100"
        // 13 bits packed into 2 bytes: 10100110 01000000.
        let bytes = [0b1010_0110, 0b0100_0000];
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.ue(), 0);
        assert_eq!(r.ue(), 1);
        assert_eq!(r.ue(), 2);
        assert_eq!(r.ue(), 3);
    }
}
