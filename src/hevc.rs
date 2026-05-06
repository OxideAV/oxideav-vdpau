//! Minimal HEVC (H.265) VPS/SPS/PPS parser and a single-IDR-frame VDPAU
//! decode glue layer (Round 4).
//!
//! # Scope
//!
//! Like the H.264 sibling in [`crate::h264`], this is **not** a full
//! HEVC parser. We extract exactly the fields `VdpPictureInfoHEVC`
//! requires for VDPAU's `VdpDecoderRender` to decode an IDR (intra-coded
//! random-access point) into a `VdpVideoSurface`. No P/B-frame DPB
//! management, no frame reordering, no scaling lists. The parser
//! handles:
//!
//!   - Annex-B start-code framing (shared format with H.264, two-byte
//!     NAL header instead of one),
//!   - VPS through `vps_max_dec_pic_buffering_minus1` (we only need
//!     the high-level info),
//!   - SPS through `strong_intra_smoothing_enabled_flag` (skipping VUI
//!     and SPS extensions),
//!   - PPS through `slice_segment_header_extension_present_flag`,
//!   - The minimal slice-segment header bits required to set
//!     `IDRPicFlag`, `RAPPicFlag`, `CurrPicOrderCntVal=0` for the IDR.
//!
//! Scaling lists are submitted to the GPU as flat 16/16 — fine for
//! HEVC streams that don't enable them. `scaling_list_enabled_flag=0`
//! is asserted at parse time; if the SPS opts in, we refuse rather
//! than silently corrupt output.
//!
//! # References
//!
//! ITU-T H.265 (V14) sections 7.3.2 (VPS/SPS/PPS/slice syntax), 7.3.4
//! (RBSP), 9.2 (Exp-Golomb). The HEVC specification is a public ITU-T
//! standard.

use std::ffi::c_void;

use crate::device::{VdpDecoder, VdpDevice, VdpError};
use crate::h264::{DecodedFrame, get_bits_nv12_as_i420};
use crate::sys::{
    VDP_BITSTREAM_BUFFER_VERSION, VDP_CHROMA_TYPE_420, VDP_DECODER_PROFILE_HEVC_MAIN,
    VDP_INVALID_HANDLE, VdpBitstreamBuffer, VdpPictureInfoHEVC,
};

// ─────────────────────────── Annex-B framing (HEVC) ─────────────────────────

/// HEVC Annex-B uses the same 0x000001 / 0x00000001 start-code framing
/// as H.264. We can reuse the H.264 splitter, but HEVC NAL headers are
/// **two bytes** so callers should check the type via bits 1..7 of the
/// first byte (`(byte0 >> 1) & 0x3f`).
///
/// (Round 5 note: the h264 module's local `split_nal_units` was deleted
/// when its parsing path migrated to `oxideav-bitstream`. HEVC keeps an
/// inline parser pending bitstream coverage of the full HEVC PPS, so we
/// need our own splitter here too.)
pub(crate) fn split_nal_units(buf: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    let n = buf.len();
    let mut last_payload_start: Option<usize> = None;
    while i < n {
        let four = i + 3 < n
            && buf[i] == 0
            && buf[i + 1] == 0
            && buf[i + 2] == 0
            && buf[i + 3] == 1;
        let three = !four && i + 2 < n && buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1;
        if four || three {
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

/// Strip emulation-prevention `0x03` bytes from an HEVC RBSP payload.
/// Same rule as H.264: any `00 00 03` triple is collapsed to `00 00`.
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

/// HEVC NAL unit type, decoded from the 6 bits at byte 0 bits 1..7.
fn nal_type(nal: &[u8]) -> u8 {
    if nal.is_empty() {
        return 0xff;
    }
    (nal[0] >> 1) & 0x3f
}

#[allow(dead_code)] pub(crate) const NAL_TRAIL_N: u8 = 0;
#[allow(dead_code)] pub(crate) const NAL_TRAIL_R: u8 = 1;
pub(crate) const NAL_BLA_W_LP: u8 = 16;
#[allow(dead_code)] pub(crate) const NAL_BLA_W_RADL: u8 = 17;
#[allow(dead_code)] pub(crate) const NAL_BLA_N_LP: u8 = 18;
pub(crate) const NAL_IDR_W_RADL: u8 = 19;
pub(crate) const NAL_IDR_N_LP: u8 = 20;
#[allow(dead_code)] pub(crate) const NAL_CRA_NUT: u8 = 21;
#[allow(dead_code)] pub(crate) const NAL_RSV_IRAP_VCL22: u8 = 22;
pub(crate) const NAL_RSV_IRAP_VCL23: u8 = 23;
pub(crate) const NAL_VPS_NUT: u8 = 32;
pub(crate) const NAL_SPS_NUT: u8 = 33;
pub(crate) const NAL_PPS_NUT: u8 = 34;

fn is_idr(t: u8) -> bool {
    t == NAL_IDR_W_RADL || t == NAL_IDR_N_LP
}
fn is_rap(t: u8) -> bool {
    (NAL_BLA_W_LP..=NAL_RSV_IRAP_VCL23).contains(&t)
}

/// Walk `buf` looking for the first start code (`00 00 01` / `00 00 00 01`)
/// whose following NAL header byte decodes to an IDR-class type. Returns
/// the byte index of the start-code prefix (so `buf[idx..]` begins with
/// the start code), or `None` if no such NAL is found.
fn locate_idr_start(buf: &[u8]) -> Option<usize> {
    let n = buf.len();
    let mut i = 0;
    while i + 3 < n {
        let four = buf[i] == 0
            && buf[i + 1] == 0
            && buf[i + 2] == 0
            && buf[i + 3] == 1
            && i + 4 < n;
        let three = !four && buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1;
        if four || three {
            let header_idx = if four { i + 4 } else { i + 3 };
            if header_idx < n {
                let t = (buf[header_idx] >> 1) & 0x3f;
                if is_idr(t) {
                    return Some(i);
                }
            }
            i = header_idx;
        } else {
            i += 1;
        }
    }
    None
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

// ─────────────────────────── Parsed parameter sets ──────────────────────────

#[derive(Debug, Clone, Default)]
pub(crate) struct Vps {
    pub _vps_id: u32,
    pub _max_layers_minus1: u32,
    pub _max_sub_layers_minus1: u32,
    pub vps_max_dec_pic_buffering_minus1: u32,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Sps {
    pub _vps_id: u32,
    pub _max_sub_layers_minus1: u32,
    pub _sps_id: u32,
    pub chroma_format_idc: u32,
    pub separate_colour_plane_flag: u8,
    pub pic_width_in_luma_samples: u32,
    pub pic_height_in_luma_samples: u32,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub log2_max_pic_order_cnt_lsb_minus4: u32,
    pub sps_max_dec_pic_buffering_minus1: u32,
    pub log2_min_luma_coding_block_size_minus3: u32,
    pub log2_diff_max_min_luma_coding_block_size: u32,
    pub log2_min_transform_block_size_minus2: u32,
    pub log2_diff_max_min_transform_block_size: u32,
    pub max_transform_hierarchy_depth_inter: u32,
    pub max_transform_hierarchy_depth_intra: u32,
    pub scaling_list_enabled_flag: u8,
    pub amp_enabled_flag: u8,
    pub sample_adaptive_offset_enabled_flag: u8,
    pub pcm_enabled_flag: u8,
    pub num_short_term_ref_pic_sets: u32,
    pub long_term_ref_pics_present_flag: u8,
    pub num_long_term_ref_pics_sps: u32,
    pub sps_temporal_mvp_enabled_flag: u8,
    pub strong_intra_smoothing_enabled_flag: u8,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Pps {
    pub _pps_id: u32,
    pub _sps_id: u32,
    pub dependent_slice_segments_enabled_flag: u8,
    pub output_flag_present_flag: u8,
    pub num_extra_slice_header_bits: u32,
    pub sign_data_hiding_enabled_flag: u8,
    pub cabac_init_present_flag: u8,
    pub num_ref_idx_l0_default_active_minus1: u32,
    pub num_ref_idx_l1_default_active_minus1: u32,
    pub init_qp_minus26: i32,
    pub constrained_intra_pred_flag: u8,
    pub transform_skip_enabled_flag: u8,
    pub cu_qp_delta_enabled_flag: u8,
    pub diff_cu_qp_delta_depth: u32,
    pub pps_cb_qp_offset: i32,
    pub pps_cr_qp_offset: i32,
    pub pps_slice_chroma_qp_offsets_present_flag: u8,
    pub weighted_pred_flag: u8,
    pub weighted_bipred_flag: u8,
    pub transquant_bypass_enabled_flag: u8,
    pub tiles_enabled_flag: u8,
    pub entropy_coding_sync_enabled_flag: u8,
    pub pps_loop_filter_across_slices_enabled_flag: u8,
    pub deblocking_filter_control_present_flag: u8,
    pub deblocking_filter_override_enabled_flag: u8,
    pub pps_deblocking_filter_disabled_flag: u8,
    pub pps_beta_offset_div2: i32,
    pub pps_tc_offset_div2: i32,
    pub lists_modification_present_flag: u8,
    pub log2_parallel_merge_level_minus2: u32,
    pub slice_segment_header_extension_present_flag: u8,
}

/// Parse the PTL (profile_tier_level) block. Layout from H.265 7.3.3.
/// Returns the number of bits consumed (not used externally; the
/// reader's bit_pos advances).
fn parse_profile_tier_level(r: &mut BitReader<'_>, max_sub_layers_minus1: u32) {
    // general_profile_space (2), general_tier_flag (1), general_profile_idc (5)
    let _ = r.u(2);
    let _ = r.u(1);
    let _ = r.u(5);
    // general_profile_compatibility_flag[32]
    let _ = r.u(32);
    // general_progressive_source_flag, general_interlaced_source_flag,
    // general_non_packed_constraint_flag, general_frame_only_constraint_flag
    let _ = r.u(4);
    // 43 bits of constraint flags + 1 bit reserved
    let _ = r.u(32);
    let _ = r.u(11);
    let _ = r.u(1);
    // general_level_idc (8)
    let _ = r.u(8);

    // sub_layer_profile_present_flag[i] (1) and sub_layer_level_present_flag[i] (1)
    let mut sub_layer_profile_present = [0u32; 8];
    let mut sub_layer_level_present = [0u32; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        sub_layer_profile_present[i] = r.u(1);
        sub_layer_level_present[i] = r.u(1);
    }
    if max_sub_layers_minus1 > 0 {
        // reserved_zero_2bits[i] for i in max_sub_layers_minus1 .. 7
        for _ in max_sub_layers_minus1..8 {
            let _ = r.u(2);
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if sub_layer_profile_present[i] != 0 {
            let _ = r.u(2);
            let _ = r.u(1);
            let _ = r.u(5);
            let _ = r.u(32);
            let _ = r.u(4);
            let _ = r.u(32);
            let _ = r.u(11);
            let _ = r.u(1);
        }
        if sub_layer_level_present[i] != 0 {
            let _ = r.u(8);
        }
    }
}

/// Skip a `scaling_list_data()` block. We never *use* scaling lists
/// (the parser refuses streams that have `scaling_list_enabled_flag=1`)
/// but the SPS / PPS may still emit it via `*_scaling_list_data_present_flag`.
fn skip_scaling_list_data(r: &mut BitReader<'_>) {
    for size_id in 0..4 {
        let n_matrix = if size_id == 3 { 2 } else { 6 };
        for matrix_id in 0..n_matrix {
            let pred = r.u(1);
            if pred == 0 {
                let _delta = r.ue();
            } else {
                if size_id > 1 {
                    let _dc_coef_minus8 = r.se();
                }
                let coef_num = std::cmp::min(64, 1 << (4 + (size_id << 1)));
                for _ in 0..coef_num {
                    let _delta = r.se();
                }
                let _ = matrix_id;
            }
        }
    }
}

/// Skip a `st_ref_pic_set(idx)` block. Used in SPS for the
/// `num_short_term_ref_pic_sets` array. For an IDR with
/// `num_short_term_ref_pic_sets=0`, we never enter this — but the SPS
/// often sets it to a small number even when the stream is intra-only.
///
/// Returns `Err` if we would have to parse a non-trivial RPS for our
/// IDR-only fixture.
fn skip_st_ref_pic_set(
    r: &mut BitReader<'_>,
    st_rps_idx: u32,
    num_short_term_ref_pic_sets: u32,
    prev_num_delta_pocs: u32,
) -> Result<u32, VdpError> {
    let inter_rps_pred = if st_rps_idx != 0 { r.u(1) } else { 0 };
    if inter_rps_pred != 0 {
        let delta_idx_m1 = if st_rps_idx == num_short_term_ref_pic_sets {
            r.ue()
        } else {
            0
        };
        let _delta_rps_sign = r.u(1);
        let _abs_delta_rps_minus1 = r.ue();
        let ref_rps_idx = st_rps_idx as i64 - 1 - delta_idx_m1 as i64;
        if ref_rps_idx < 0 {
            return Err(VdpError::other(
                "skip_st_ref_pic_set: negative RefRpsIdx",
            ));
        }
        // For each j in 0..NumDeltaPocs[RefRpsIdx]: used_by_curr_pic_flag,
        // and conditionally use_delta_flag.
        for _ in 0..=prev_num_delta_pocs {
            let used_by_curr_pic_flag = r.u(1);
            if used_by_curr_pic_flag == 0 {
                let _use_delta_flag = r.u(1);
            }
        }
        Ok(prev_num_delta_pocs)
    } else {
        let num_negative_pics = r.ue();
        let num_positive_pics = r.ue();
        for _ in 0..num_negative_pics {
            let _delta_poc_s0_minus1 = r.ue();
            let _used_by_curr_pic_s0_flag = r.u(1);
        }
        for _ in 0..num_positive_pics {
            let _delta_poc_s1_minus1 = r.ue();
            let _used_by_curr_pic_s1_flag = r.u(1);
        }
        Ok(num_negative_pics + num_positive_pics)
    }
}

pub(crate) fn parse_vps(nal: &[u8]) -> Result<Vps, VdpError> {
    if nal.len() < 2 {
        return Err(VdpError::other("parse_vps: NAL too short"));
    }
    if nal_type(nal) != NAL_VPS_NUT {
        return Err(VdpError::other(format!(
            "parse_vps: not a VPS NAL (type={})",
            nal_type(nal)
        )));
    }
    let rbsp = strip_emulation_prevention(&nal[2..]);
    let mut r = BitReader::new(&rbsp);
    let mut vps = Vps {
        _vps_id: r.u(4),
        ..Vps::default()
    };
    let _base_layer_internal_flag = r.u(1);
    let _base_layer_available_flag = r.u(1);
    vps._max_layers_minus1 = r.u(6);
    vps._max_sub_layers_minus1 = r.u(3);
    let _temporal_id_nesting_flag = r.u(1);
    let _ = r.u(16); // vps_reserved_0xffff_16bits
    parse_profile_tier_level(&mut r, vps._max_sub_layers_minus1);
    let sub_layer_ordering_info_present_flag = r.u(1);
    let start_sub = if sub_layer_ordering_info_present_flag != 0 {
        0
    } else {
        vps._max_sub_layers_minus1
    };
    for i in start_sub..=vps._max_sub_layers_minus1 {
        let max_dec_pic_buffering_minus1 = r.ue();
        let _max_num_reorder_pics = r.ue();
        let _max_latency_increase_plus1 = r.ue();
        if i == vps._max_sub_layers_minus1 {
            vps.vps_max_dec_pic_buffering_minus1 = max_dec_pic_buffering_minus1;
        }
    }
    Ok(vps)
}

pub(crate) fn parse_sps(nal: &[u8]) -> Result<Sps, VdpError> {
    if nal.len() < 2 {
        return Err(VdpError::other("parse_sps: NAL too short"));
    }
    if nal_type(nal) != NAL_SPS_NUT {
        return Err(VdpError::other(format!(
            "parse_sps: not a SPS NAL (type={})",
            nal_type(nal)
        )));
    }
    let rbsp = strip_emulation_prevention(&nal[2..]);
    let mut r = BitReader::new(&rbsp);
    let mut sps = Sps::default();
    sps._vps_id = r.u(4);
    sps._max_sub_layers_minus1 = r.u(3);
    let _temporal_id_nesting_flag = r.u(1);
    parse_profile_tier_level(&mut r, sps._max_sub_layers_minus1);
    sps._sps_id = r.ue();
    sps.chroma_format_idc = r.ue();
    if sps.chroma_format_idc == 3 {
        sps.separate_colour_plane_flag = r.u(1) as u8;
    }
    sps.pic_width_in_luma_samples = r.ue();
    sps.pic_height_in_luma_samples = r.ue();
    let conformance_window_flag = r.u(1);
    if conformance_window_flag != 0 {
        let _ = r.ue();
        let _ = r.ue();
        let _ = r.ue();
        let _ = r.ue();
    }
    sps.bit_depth_luma_minus8 = r.ue();
    sps.bit_depth_chroma_minus8 = r.ue();
    sps.log2_max_pic_order_cnt_lsb_minus4 = r.ue();
    let sub_layer_ordering_info_present_flag = r.u(1);
    let start_sub = if sub_layer_ordering_info_present_flag != 0 {
        0
    } else {
        sps._max_sub_layers_minus1
    };
    for i in start_sub..=sps._max_sub_layers_minus1 {
        let max_dec_pic_buffering_minus1 = r.ue();
        let _max_num_reorder_pics = r.ue();
        let _max_latency_increase_plus1 = r.ue();
        if i == sps._max_sub_layers_minus1 {
            sps.sps_max_dec_pic_buffering_minus1 = max_dec_pic_buffering_minus1;
        }
    }
    sps.log2_min_luma_coding_block_size_minus3 = r.ue();
    sps.log2_diff_max_min_luma_coding_block_size = r.ue();
    sps.log2_min_transform_block_size_minus2 = r.ue();
    sps.log2_diff_max_min_transform_block_size = r.ue();
    sps.max_transform_hierarchy_depth_inter = r.ue();
    sps.max_transform_hierarchy_depth_intra = r.ue();
    sps.scaling_list_enabled_flag = r.u(1) as u8;
    if sps.scaling_list_enabled_flag != 0 {
        let sps_scaling_list_data_present_flag = r.u(1);
        if sps_scaling_list_data_present_flag != 0 {
            // We don't extract the lists — just skip past them.
            // Then refuse the stream below.
            skip_scaling_list_data(&mut r);
        }
        return Err(VdpError::other(
            "parse_sps: scaling_list_enabled_flag=1 not supported by minimal parser",
        ));
    }
    sps.amp_enabled_flag = r.u(1) as u8;
    sps.sample_adaptive_offset_enabled_flag = r.u(1) as u8;
    sps.pcm_enabled_flag = r.u(1) as u8;
    if sps.pcm_enabled_flag != 0 {
        return Err(VdpError::other(
            "parse_sps: pcm_enabled_flag=1 not supported by minimal parser",
        ));
    }
    sps.num_short_term_ref_pic_sets = r.ue();
    let mut prev_num_delta_pocs = 0u32;
    for i in 0..sps.num_short_term_ref_pic_sets {
        prev_num_delta_pocs = skip_st_ref_pic_set(
            &mut r,
            i,
            sps.num_short_term_ref_pic_sets,
            prev_num_delta_pocs,
        )?;
    }
    sps.long_term_ref_pics_present_flag = r.u(1) as u8;
    if sps.long_term_ref_pics_present_flag != 0 {
        sps.num_long_term_ref_pics_sps = r.ue();
        for _ in 0..sps.num_long_term_ref_pics_sps {
            let _ = r.u((sps.log2_max_pic_order_cnt_lsb_minus4 + 4) as u32);
            let _ = r.u(1);
        }
    }
    sps.sps_temporal_mvp_enabled_flag = r.u(1) as u8;
    sps.strong_intra_smoothing_enabled_flag = r.u(1) as u8;
    Ok(sps)
}

pub(crate) fn parse_pps(nal: &[u8]) -> Result<Pps, VdpError> {
    if nal.len() < 2 {
        return Err(VdpError::other("parse_pps: NAL too short"));
    }
    if nal_type(nal) != NAL_PPS_NUT {
        return Err(VdpError::other(format!(
            "parse_pps: not a PPS NAL (type={})",
            nal_type(nal)
        )));
    }
    let rbsp = strip_emulation_prevention(&nal[2..]);
    let mut r = BitReader::new(&rbsp);
    let mut pps = Pps::default();
    pps._pps_id = r.ue();
    pps._sps_id = r.ue();
    pps.dependent_slice_segments_enabled_flag = r.u(1) as u8;
    pps.output_flag_present_flag = r.u(1) as u8;
    pps.num_extra_slice_header_bits = r.u(3);
    pps.sign_data_hiding_enabled_flag = r.u(1) as u8;
    pps.cabac_init_present_flag = r.u(1) as u8;
    pps.num_ref_idx_l0_default_active_minus1 = r.ue();
    pps.num_ref_idx_l1_default_active_minus1 = r.ue();
    pps.init_qp_minus26 = r.se();
    pps.constrained_intra_pred_flag = r.u(1) as u8;
    pps.transform_skip_enabled_flag = r.u(1) as u8;
    pps.cu_qp_delta_enabled_flag = r.u(1) as u8;
    if pps.cu_qp_delta_enabled_flag != 0 {
        pps.diff_cu_qp_delta_depth = r.ue();
    }
    pps.pps_cb_qp_offset = r.se();
    pps.pps_cr_qp_offset = r.se();
    pps.pps_slice_chroma_qp_offsets_present_flag = r.u(1) as u8;
    pps.weighted_pred_flag = r.u(1) as u8;
    pps.weighted_bipred_flag = r.u(1) as u8;
    pps.transquant_bypass_enabled_flag = r.u(1) as u8;
    pps.tiles_enabled_flag = r.u(1) as u8;
    pps.entropy_coding_sync_enabled_flag = r.u(1) as u8;
    if pps.tiles_enabled_flag != 0 {
        // We don't store the tile geometry; the IDR fixture has tiles
        // disabled. Refuse rather than silently mis-parse.
        return Err(VdpError::other(
            "parse_pps: tiles_enabled_flag=1 not supported by minimal parser",
        ));
    }
    pps.pps_loop_filter_across_slices_enabled_flag = r.u(1) as u8;
    pps.deblocking_filter_control_present_flag = r.u(1) as u8;
    if pps.deblocking_filter_control_present_flag != 0 {
        pps.deblocking_filter_override_enabled_flag = r.u(1) as u8;
        pps.pps_deblocking_filter_disabled_flag = r.u(1) as u8;
        if pps.pps_deblocking_filter_disabled_flag == 0 {
            pps.pps_beta_offset_div2 = r.se();
            pps.pps_tc_offset_div2 = r.se();
        }
    }
    let pps_scaling_list_data_present_flag = r.u(1);
    if pps_scaling_list_data_present_flag != 0 {
        return Err(VdpError::other(
            "parse_pps: pps_scaling_list_data_present_flag=1 not supported by minimal parser",
        ));
    }
    pps.lists_modification_present_flag = r.u(1) as u8;
    pps.log2_parallel_merge_level_minus2 = r.ue();
    pps.slice_segment_header_extension_present_flag = r.u(1) as u8;
    Ok(pps)
}

// ─────────────────────────── HevcVdpauDecoder ───────────────────────────────

/// One-shot single-IDR HEVC decoder. Mirrors the H.264 sibling.
pub struct HevcVdpauDecoder {
    _vps: Vps,
    sps: Sps,
    pps: Pps,
    decoder: VdpDecoder,
    width: u32,
    height: u32,
    /// NAL unit type of the IDR slice — used to set IDRPicFlag/RAPPicFlag.
    idr_nal_type: u8,
}

impl HevcVdpauDecoder {
    /// Construct via the framework's [`oxideav_core::CodecParameters`].
    /// Honours `params.device_index` (only `None` / `Some(0)` are valid
    /// on a single-display VDPAU host — see
    /// [`crate::validate_device_index`]) and otherwise delegates to
    /// [`Self::new`].
    #[cfg(feature = "registry")]
    pub fn with_params(
        device: &VdpDevice,
        params: &oxideav_core::CodecParameters,
        annex_b: &[u8],
    ) -> Result<Self, VdpError> {
        let idx = params.device_index.unwrap_or(0);
        crate::engine::validate_device_index(idx)?;
        Self::new(device, annex_b)
    }

    pub fn new(device: &VdpDevice, annex_b: &[u8]) -> Result<Self, VdpError> {
        let nals = split_nal_units(annex_b);
        let vps_nal = nals
            .iter()
            .find(|n| !n.is_empty() && nal_type(n) == NAL_VPS_NUT)
            .ok_or_else(|| VdpError::other("HEVC fixture has no VPS NAL"))?;
        let sps_nal = nals
            .iter()
            .find(|n| !n.is_empty() && nal_type(n) == NAL_SPS_NUT)
            .ok_or_else(|| VdpError::other("HEVC fixture has no SPS NAL"))?;
        let pps_nal = nals
            .iter()
            .find(|n| !n.is_empty() && nal_type(n) == NAL_PPS_NUT)
            .ok_or_else(|| VdpError::other("HEVC fixture has no PPS NAL"))?;
        let idr_nal = nals
            .iter()
            .find(|n| !n.is_empty() && is_idr(nal_type(n)))
            .ok_or_else(|| VdpError::other("HEVC fixture has no IDR slice NAL"))?;

        let vps = parse_vps(vps_nal)?;
        let sps = parse_sps(sps_nal)?;
        let pps = parse_pps(pps_nal)?;
        let idr_nal_type = nal_type(idr_nal);

        let width = sps.pic_width_in_luma_samples;
        let height = sps.pic_height_in_luma_samples;
        let max_refs = sps.sps_max_dec_pic_buffering_minus1 + 1;

        let decoder =
            device.create_decoder(VDP_DECODER_PROFILE_HEVC_MAIN, width, height, max_refs)?;

        Ok(Self {
            _vps: vps,
            sps,
            pps,
            decoder,
            width,
            height,
            idr_nal_type,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn decode_idr(
        &self,
        device: &VdpDevice,
        annex_b: &[u8],
    ) -> Result<DecodedFrame, VdpError> {
        let pic_info = self.build_idr_picture_info();
        let surface = device.create_video_surface(VDP_CHROMA_TYPE_420, self.width, self.height)?;

        // VDPAU's HEVC path expects the slice NAL data only — parameter
        // sets (VPS/SPS/PPS) must be communicated via VdpPictureInfoHEVC.
        // We slice the buffer at the IDR's start-code prefix and pass
        // the IDR onwards (any trailing NALs are part of the same access
        // unit in our single-frame fixture).
        let idr_start = locate_idr_start(annex_b)
            .ok_or_else(|| VdpError::other("no IDR NAL start code found"))?;
        let slice_buf = &annex_b[idr_start..];
        let bs = [VdpBitstreamBuffer {
            struct_version: VDP_BITSTREAM_BUFFER_VERSION,
            bitstream: slice_buf.as_ptr() as *const c_void,
            bitstream_bytes: slice_buf.len() as u32,
        }];

        // SAFETY: pic_info matches the decoder's HEVC profile; bs
        // references annex_b which outlives this call.
        unsafe {
            self.decoder.render(
                &surface,
                &pic_info as *const VdpPictureInfoHEVC as *const c_void,
                &bs,
            )?;
        }

        get_bits_nv12_as_i420(&surface, self.width, self.height)
    }

    fn build_idr_picture_info(&self) -> VdpPictureInfoHEVC {
        let chroma_format_idc = self.sps.chroma_format_idc as u8;
        let mut info = VdpPictureInfoHEVC {
            chroma_format_idc,
            separate_colour_plane_flag: self.sps.separate_colour_plane_flag,
            pic_width_in_luma_samples: self.sps.pic_width_in_luma_samples,
            pic_height_in_luma_samples: self.sps.pic_height_in_luma_samples,
            bit_depth_luma_minus8: self.sps.bit_depth_luma_minus8 as u8,
            bit_depth_chroma_minus8: self.sps.bit_depth_chroma_minus8 as u8,
            log2_max_pic_order_cnt_lsb_minus4: self.sps.log2_max_pic_order_cnt_lsb_minus4 as u8,
            sps_max_dec_pic_buffering_minus1: self.sps.sps_max_dec_pic_buffering_minus1 as u8,
            log2_min_luma_coding_block_size_minus3: self
                .sps
                .log2_min_luma_coding_block_size_minus3
                as u8,
            log2_diff_max_min_luma_coding_block_size: self
                .sps
                .log2_diff_max_min_luma_coding_block_size
                as u8,
            log2_min_transform_block_size_minus2: self
                .sps
                .log2_min_transform_block_size_minus2
                as u8,
            log2_diff_max_min_transform_block_size: self
                .sps
                .log2_diff_max_min_transform_block_size
                as u8,
            max_transform_hierarchy_depth_inter: self.sps.max_transform_hierarchy_depth_inter
                as u8,
            max_transform_hierarchy_depth_intra: self.sps.max_transform_hierarchy_depth_intra
                as u8,
            scaling_list_enabled_flag: self.sps.scaling_list_enabled_flag,
            scaling_list_4x4: [[16u8; 16]; 6],
            scaling_list_8x8: [[16u8; 64]; 6],
            scaling_list_16x16: [[16u8; 64]; 6],
            scaling_list_32x32: [[16u8; 64]; 2],
            scaling_list_dc_coeff_16x16: [16u8; 6],
            scaling_list_dc_coeff_32x32: [16u8; 2],
            amp_enabled_flag: self.sps.amp_enabled_flag,
            sample_adaptive_offset_enabled_flag: self.sps.sample_adaptive_offset_enabled_flag,
            pcm_enabled_flag: self.sps.pcm_enabled_flag,
            pcm_sample_bit_depth_luma_minus1: 0,
            pcm_sample_bit_depth_chroma_minus1: 0,
            log2_min_pcm_luma_coding_block_size_minus3: 0,
            log2_diff_max_min_pcm_luma_coding_block_size: 0,
            pcm_loop_filter_disabled_flag: 0,
            num_short_term_ref_pic_sets: self.sps.num_short_term_ref_pic_sets as u8,
            long_term_ref_pics_present_flag: self.sps.long_term_ref_pics_present_flag,
            num_long_term_ref_pics_sps: self.sps.num_long_term_ref_pics_sps as u8,
            sps_temporal_mvp_enabled_flag: self.sps.sps_temporal_mvp_enabled_flag,
            strong_intra_smoothing_enabled_flag: self.sps.strong_intra_smoothing_enabled_flag,

            dependent_slice_segments_enabled_flag: self.pps.dependent_slice_segments_enabled_flag,
            output_flag_present_flag: self.pps.output_flag_present_flag,
            num_extra_slice_header_bits: self.pps.num_extra_slice_header_bits as u8,
            sign_data_hiding_enabled_flag: self.pps.sign_data_hiding_enabled_flag,
            cabac_init_present_flag: self.pps.cabac_init_present_flag,
            num_ref_idx_l0_default_active_minus1: self.pps.num_ref_idx_l0_default_active_minus1
                as u8,
            num_ref_idx_l1_default_active_minus1: self.pps.num_ref_idx_l1_default_active_minus1
                as u8,
            init_qp_minus26: self.pps.init_qp_minus26 as i8,
            constrained_intra_pred_flag: self.pps.constrained_intra_pred_flag,
            transform_skip_enabled_flag: self.pps.transform_skip_enabled_flag,
            cu_qp_delta_enabled_flag: self.pps.cu_qp_delta_enabled_flag,
            diff_cu_qp_delta_depth: self.pps.diff_cu_qp_delta_depth as u8,
            pps_cb_qp_offset: self.pps.pps_cb_qp_offset as i8,
            pps_cr_qp_offset: self.pps.pps_cr_qp_offset as i8,
            pps_slice_chroma_qp_offsets_present_flag: self
                .pps
                .pps_slice_chroma_qp_offsets_present_flag,
            weighted_pred_flag: self.pps.weighted_pred_flag,
            weighted_bipred_flag: self.pps.weighted_bipred_flag,
            transquant_bypass_enabled_flag: self.pps.transquant_bypass_enabled_flag,
            tiles_enabled_flag: self.pps.tiles_enabled_flag,
            entropy_coding_sync_enabled_flag: self.pps.entropy_coding_sync_enabled_flag,
            num_tile_columns_minus1: 0,
            num_tile_rows_minus1: 0,
            uniform_spacing_flag: 0,
            column_width_minus1: [0u16; 20],
            row_height_minus1: [0u16; 22],
            loop_filter_across_tiles_enabled_flag: 0,
            pps_loop_filter_across_slices_enabled_flag: self
                .pps
                .pps_loop_filter_across_slices_enabled_flag,
            deblocking_filter_control_present_flag: self
                .pps
                .deblocking_filter_control_present_flag,
            deblocking_filter_override_enabled_flag: self
                .pps
                .deblocking_filter_override_enabled_flag,
            pps_deblocking_filter_disabled_flag: self.pps.pps_deblocking_filter_disabled_flag,
            pps_beta_offset_div2: self.pps.pps_beta_offset_div2 as i8,
            pps_tc_offset_div2: self.pps.pps_tc_offset_div2 as i8,
            lists_modification_present_flag: self.pps.lists_modification_present_flag,
            log2_parallel_merge_level_minus2: self.pps.log2_parallel_merge_level_minus2 as u8,
            slice_segment_header_extension_present_flag: self
                .pps
                .slice_segment_header_extension_present_flag,

            idr_pic_flag: if is_idr(self.idr_nal_type) { 1 } else { 0 },
            rap_pic_flag: if is_rap(self.idr_nal_type) { 1 } else { 0 },
            curr_rps_idx: 0,
            num_poc_total_curr: 0,
            num_delta_pocs_of_ref_rps_idx: 0,
            num_short_term_picture_slice_header_bits: 0,
            num_long_term_picture_slice_header_bits: 0,

            curr_pic_order_cnt_val: 0,

            ref_pics: [VDP_INVALID_HANDLE; 16],
            pic_order_cnt_val: [0i32; 16],
            is_long_term: [0u8; 16],
            num_poc_st_curr_before: 0,
            num_poc_st_curr_after: 0,
            num_poc_lt_curr: 0,
            ref_pic_set_st_curr_before: [0u8; 8],
            ref_pic_set_st_curr_after: [0u8; 8],
            ref_pic_set_lt_curr: [0u8; 8],
        };
        // Suppress unused-mut warning when no further fields are mutated.
        let _ = &mut info;
        info
    }
}

// ─────────────────────────── Tests ──────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nal_type_decodes_correctly() {
        // NAL header byte 0x40 = 0100 0000 -> type=(0x40>>1)&0x3f = 32 (VPS)
        assert_eq!(nal_type(&[0x40, 0x01]), NAL_VPS_NUT);
        // 0x42 -> 0100 0010 -> 33 SPS
        assert_eq!(nal_type(&[0x42, 0x01]), NAL_SPS_NUT);
        // 0x44 -> 0100 0100 -> 34 PPS
        assert_eq!(nal_type(&[0x44, 0x01]), NAL_PPS_NUT);
        // 0x28 -> 0010 1000 -> 20 IDR_N_LP
        assert_eq!(nal_type(&[0x28, 0x01]), NAL_IDR_N_LP);
    }
}
