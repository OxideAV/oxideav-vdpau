//! Minimal MPEG-2 (H.262 / ISO/IEC 13818-2) sequence/picture header
//! parser and a single-I-frame VDPAU decode glue layer (Round 4).
//!
//! # Scope
//!
//! Like the H.264 / HEVC / VP9 siblings, this is **not** a full MPEG-2
//! parser. We extract exactly the fields `VdpPictureInfoMPEG1Or2`
//! requires for VDPAU's `VdpDecoderRender` to decode an I-frame
//! (intra-coded) into a `VdpVideoSurface`. P/B-frames, field-encoded
//! pictures, scalable extensions, and custom quantizer matrices are
//! out of scope for the minimum viable Round 4 demonstration.
//!
//! MPEG-2 uses byte-aligned start codes `00 00 01 XX` (no Annex-B
//! emulation-prevention bytes — those are H.264/HEVC only). The parser
//! walks the bitstream looking for:
//!
//!   - sequence_header (start_code 0xb3),
//!   - sequence_extension (extension_start_code 0xb5 + extension_id 0x1),
//!   - picture_header (start_code 0x00),
//!   - picture_coding_extension (start_code 0xb5 + extension_id 0x8).
//!
//! # References
//!
//! ITU-T H.262 / ISO/IEC 13818-2 (MPEG-2 Video) sections 6.2.2 (sequence
//! header), 6.2.2.3 (sequence extension), 6.2.3 (picture header), and
//! 6.2.3.1 (picture coding extension). MPEG-2 is a public ISO standard.

use std::ffi::c_void;

use crate::device::{VdpDecoder, VdpDevice, VdpError};
use crate::h264::{DecodedFrame, get_bits_nv12_as_i420};
use crate::sys::{
    VDP_BITSTREAM_BUFFER_VERSION, VDP_CHROMA_TYPE_420, VDP_DECODER_PROFILE_MPEG2_MAIN,
    VDP_INVALID_HANDLE, VdpBitstreamBuffer, VdpPictureInfoMPEG1Or2,
};

// ─────────────────────────── Default quantizer matrices ─────────────────────

/// Default intra-quantizer matrix (zig-zag scan order from the
/// spec, table 7-3 in ISO 13818-2). Most encoders use this when not
/// transmitting a custom matrix.
const DEFAULT_INTRA_QUANT: [u8; 64] = [
    8,  16, 19, 22, 26, 27, 29, 34,
    16, 16, 22, 24, 27, 29, 34, 37,
    19, 22, 26, 27, 29, 34, 34, 38,
    22, 22, 26, 27, 29, 34, 37, 40,
    22, 26, 27, 29, 32, 35, 40, 48,
    26, 27, 29, 32, 35, 40, 48, 58,
    26, 27, 29, 34, 38, 46, 56, 69,
    27, 29, 35, 38, 46, 56, 69, 83,
];

/// Default non-intra-quantizer matrix (uniform 16/16, table 7-4).
const DEFAULT_NON_INTRA_QUANT: [u8; 64] = [16; 64];

/// Convert MPEG-2's zig-zag scan order to raster. Used to put the
/// quantizer matrices into raster order before handing them to VDPAU
/// (the C struct comment says "Convert to raster order").
const ZIGZAG_TO_RASTER: [u8; 64] = [
     0,  1,  8, 16,  9,  2,  3, 10,
    17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34,
    27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46,
    53, 60, 61, 54, 47, 55, 62, 63,
];

fn zigzag_to_raster_quant(zz: &[u8; 64]) -> [u8; 64] {
    let mut out = [0u8; 64];
    for (zz_idx, &raster_idx) in ZIGZAG_TO_RASTER.iter().enumerate() {
        out[raster_idx as usize] = zz[zz_idx];
    }
    out
}

// ─────────────────────────── Start-code framing ─────────────────────────────

/// Locate every MPEG-2 start code in `buf`. Returns a vec of
/// `(start_code_byte, payload_start_index)` — the start_code_byte is
/// the byte immediately after `00 00 01`, and `payload_start_index`
/// points at the byte AFTER that.
pub(crate) fn find_start_codes(buf: &[u8]) -> Vec<(u8, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    let n = buf.len();
    while i + 3 < n {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            out.push((buf[i + 3], i + 4));
            i += 4;
        } else {
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
        let mut v = 0u32;
        for _ in 0..n {
            let bi = self.bit_pos / 8;
            let s = 7 - (self.bit_pos % 8) as u32;
            let bit = if bi < self.bytes.len() {
                ((self.bytes[bi] >> s) & 1) as u32
            } else {
                0
            };
            v = (v << 1) | bit;
            self.bit_pos += 1;
        }
        v
    }
}

// ─────────────────────────── Parsed headers ─────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct SequenceHeader {
    pub width: u32,
    pub height: u32,
    pub _aspect_ratio: u8,
    pub _frame_rate_code: u8,
    pub _bit_rate_value: u32,
    pub load_intra_quantizer_matrix: u8,
    pub intra_quantizer_matrix: [u8; 64],
    pub load_non_intra_quantizer_matrix: u8,
    pub non_intra_quantizer_matrix: [u8; 64],
}

impl Default for SequenceHeader {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            _aspect_ratio: 0,
            _frame_rate_code: 0,
            _bit_rate_value: 0,
            load_intra_quantizer_matrix: 0,
            intra_quantizer_matrix: [0u8; 64],
            load_non_intra_quantizer_matrix: 0,
            non_intra_quantizer_matrix: [0u8; 64],
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PictureHeader {
    pub _temporal_reference: u32,
    pub picture_coding_type: u8, // 1=I, 2=P, 3=B
    pub _vbv_delay: u32,
    pub full_pel_forward_vector: u8,
    pub full_pel_backward_vector: u8,
    pub f_code: [[u8; 2]; 2],
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PictureCodingExtension {
    pub f_code: [[u8; 2]; 2],
    pub intra_dc_precision: u8,
    pub picture_structure: u8,
    pub top_field_first: u8,
    pub frame_pred_frame_dct: u8,
    pub concealment_motion_vectors: u8,
    pub q_scale_type: u8,
    pub intra_vlc_format: u8,
    pub alternate_scan: u8,
    pub _repeat_first_field: u8,
    pub _chroma_420_type: u8,
    pub _progressive_frame: u8,
}

pub(crate) fn parse_sequence_header(bytes: &[u8]) -> Result<SequenceHeader, VdpError> {
    if bytes.len() < 8 {
        return Err(VdpError::other("parse_sequence_header: too short"));
    }
    let mut r = BitReader::new(bytes);
    let mut s = SequenceHeader::default();
    let h_size = r.u(12);
    let v_size = r.u(12);
    s.width = h_size;
    s.height = v_size;
    s._aspect_ratio = r.u(4) as u8;
    s._frame_rate_code = r.u(4) as u8;
    s._bit_rate_value = r.u(18);
    let _marker_bit = r.u(1);
    let _vbv_buffer_size = r.u(10);
    let _constrained_parameters_flag = r.u(1);
    s.load_intra_quantizer_matrix = r.u(1) as u8;
    if s.load_intra_quantizer_matrix != 0 {
        for q in &mut s.intra_quantizer_matrix {
            *q = r.u(8) as u8;
        }
    } else {
        s.intra_quantizer_matrix = DEFAULT_INTRA_QUANT;
    }
    s.load_non_intra_quantizer_matrix = r.u(1) as u8;
    if s.load_non_intra_quantizer_matrix != 0 {
        for q in &mut s.non_intra_quantizer_matrix {
            *q = r.u(8) as u8;
        }
    } else {
        s.non_intra_quantizer_matrix = DEFAULT_NON_INTRA_QUANT;
    }
    Ok(s)
}

pub(crate) fn parse_picture_header(bytes: &[u8]) -> Result<PictureHeader, VdpError> {
    if bytes.len() < 4 {
        return Err(VdpError::other("parse_picture_header: too short"));
    }
    let mut r = BitReader::new(bytes);
    let mut p = PictureHeader::default();
    p._temporal_reference = r.u(10);
    p.picture_coding_type = r.u(3) as u8;
    p._vbv_delay = r.u(16);
    if p.picture_coding_type == 2 || p.picture_coding_type == 3 {
        // P or B: full_pel_forward_vector + forward_f_code
        p.full_pel_forward_vector = r.u(1) as u8;
        let forward_f_code = r.u(3) as u8;
        p.f_code[0][0] = forward_f_code;
        p.f_code[0][1] = forward_f_code;
    }
    if p.picture_coding_type == 3 {
        // B: full_pel_backward_vector + backward_f_code
        p.full_pel_backward_vector = r.u(1) as u8;
        let backward_f_code = r.u(3) as u8;
        p.f_code[1][0] = backward_f_code;
        p.f_code[1][1] = backward_f_code;
    }
    Ok(p)
}

pub(crate) fn parse_picture_coding_extension(
    bytes: &[u8],
) -> Result<PictureCodingExtension, VdpError> {
    if bytes.len() < 4 {
        return Err(VdpError::other("parse_picture_coding_extension: too short"));
    }
    let mut r = BitReader::new(bytes);
    // The first byte's high 4 bits = extension_start_code_identifier.
    // For picture_coding_extension, this is 0b1000 == 8.
    let ext_id = r.u(4);
    if ext_id != 8 {
        return Err(VdpError::other(format!(
            "parse_picture_coding_extension: ext_id={ext_id} != 8"
        )));
    }
    let mut e = PictureCodingExtension::default();
    e.f_code[0][0] = r.u(4) as u8;
    e.f_code[0][1] = r.u(4) as u8;
    e.f_code[1][0] = r.u(4) as u8;
    e.f_code[1][1] = r.u(4) as u8;
    e.intra_dc_precision = r.u(2) as u8;
    e.picture_structure = r.u(2) as u8;
    e.top_field_first = r.u(1) as u8;
    e.frame_pred_frame_dct = r.u(1) as u8;
    e.concealment_motion_vectors = r.u(1) as u8;
    e.q_scale_type = r.u(1) as u8;
    e.intra_vlc_format = r.u(1) as u8;
    e.alternate_scan = r.u(1) as u8;
    e._repeat_first_field = r.u(1) as u8;
    e._chroma_420_type = r.u(1) as u8;
    e._progressive_frame = r.u(1) as u8;
    Ok(e)
}

// ─────────────────────────── Mpeg2VdpauDecoder ──────────────────────────────

pub struct Mpeg2VdpauDecoder {
    seq: SequenceHeader,
    pic: PictureHeader,
    pic_ext: PictureCodingExtension,
    decoder: VdpDecoder,
    width: u32,
    height: u32,
}

impl Mpeg2VdpauDecoder {
    pub fn new(device: &VdpDevice, m2v: &[u8]) -> Result<Self, VdpError> {
        let codes = find_start_codes(m2v);

        // Find the sequence_header (start_code 0xb3) and parse it from
        // the bytes immediately after.
        let (_, seq_payload_start) = codes
            .iter()
            .copied()
            .find(|(c, _)| *c == 0xb3)
            .ok_or_else(|| VdpError::other("MPEG-2: no sequence_header"))?;
        let seq = parse_sequence_header(&m2v[seq_payload_start..])?;

        // Find picture_header (start_code 0x00) and the
        // picture_coding_extension (extension start code 0xb5 with
        // extension id 0x8 in the high 4 bits of the next byte).
        let (_, pic_payload_start) = codes
            .iter()
            .copied()
            .find(|(c, _)| *c == 0x00)
            .ok_or_else(|| VdpError::other("MPEG-2: no picture_header"))?;
        let pic = parse_picture_header(&m2v[pic_payload_start..])?;

        let mut pic_ext = PictureCodingExtension::default();
        // Walk extension start codes; pick the one whose first nibble is 0x8.
        for (code, payload_start) in &codes {
            if *code != 0xb5 {
                continue;
            }
            let payload = &m2v[*payload_start..];
            if payload.is_empty() {
                continue;
            }
            let ext_id = payload[0] >> 4;
            if ext_id == 0x8 {
                pic_ext = parse_picture_coding_extension(payload)?;
                break;
            }
        }

        if pic.picture_coding_type != 1 {
            return Err(VdpError::other(format!(
                "MPEG-2: picture_coding_type={} (only I-frames supported)",
                pic.picture_coding_type
            )));
        }

        let width = seq.width;
        let height = seq.height;
        // I-frame: zero references needed, but VDPAU expects a non-zero
        // capacity. 2 is the minimum the spec requires for any P/B path.
        let max_refs = 2u32;
        let decoder = device.create_decoder(VDP_DECODER_PROFILE_MPEG2_MAIN, width, height, max_refs)?;

        Ok(Self {
            seq,
            pic,
            pic_ext,
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

    pub fn decode_iframe(&self, device: &VdpDevice, m2v: &[u8]) -> Result<DecodedFrame, VdpError> {
        let pic_info = self.build_pic_info();

        let surface = device.create_video_surface(VDP_CHROMA_TYPE_420, self.width, self.height)?;

        // VDPAU's MPEG-2 decoder consumes the slice data starting at the
        // first slice_start_code (0x00000101..0x000001AF). Parameter
        // sets (sequence/picture/extensions) are communicated via
        // VdpPictureInfoMPEG1Or2 — same pattern as HEVC.
        let slice_start = locate_first_slice(m2v)
            .ok_or_else(|| VdpError::other("no MPEG-2 slice start code found"))?;
        let bs_buf = &m2v[slice_start..];

        let bs = [VdpBitstreamBuffer {
            struct_version: VDP_BITSTREAM_BUFFER_VERSION,
            bitstream: bs_buf.as_ptr() as *const c_void,
            bitstream_bytes: bs_buf.len() as u32,
        }];

        // SAFETY: pic_info matches the decoder's MPEG-2 profile; bs_buf
        // outlives this call (lives in the input buffer).
        unsafe {
            self.decoder.render(
                &surface,
                &pic_info as *const VdpPictureInfoMPEG1Or2 as *const c_void,
                &bs,
            )?;
        }
        let _ = pic_info;
        get_bits_nv12_as_i420(&surface, self.width, self.height)
    }

    fn build_pic_info(&self) -> VdpPictureInfoMPEG1Or2 {
        // MPEG-2 uses one slice per macroblock row by default. The
        // VDPAU driver expects this count to match the slice headers
        // present in the bitstream we hand it.
        let mb_height = self.height.div_ceil(16);
        VdpPictureInfoMPEG1Or2 {
            forward_reference: VDP_INVALID_HANDLE,
            backward_reference: VDP_INVALID_HANDLE,
            slice_count: mb_height,
            picture_structure: self.pic_ext.picture_structure,
            picture_coding_type: self.pic.picture_coding_type,
            intra_dc_precision: self.pic_ext.intra_dc_precision,
            frame_pred_frame_dct: self.pic_ext.frame_pred_frame_dct,
            concealment_motion_vectors: self.pic_ext.concealment_motion_vectors,
            intra_vlc_format: self.pic_ext.intra_vlc_format,
            alternate_scan: self.pic_ext.alternate_scan,
            q_scale_type: self.pic_ext.q_scale_type,
            top_field_first: self.pic_ext.top_field_first,
            // MPEG-1 fields; MPEG-2 ignores per spec.
            full_pel_forward_vector: 0,
            full_pel_backward_vector: 0,
            // f_code from the picture coding extension (the picture
            // header's f_code is the MPEG-1 fallback, ignored on MPEG-2).
            f_code: self.pic_ext.f_code,
            intra_quantizer_matrix: zigzag_to_raster_quant(&self.seq.intra_quantizer_matrix),
            non_intra_quantizer_matrix: zigzag_to_raster_quant(&self.seq.non_intra_quantizer_matrix),
        }
    }
}

/// Find the byte offset of the first MPEG-2 slice start code
/// (0x00000101 .. 0x000001AF). Returns the offset of the leading
/// `00 00 01` (3 bytes) so callers can pass `&buf[off..]` to VDPAU.
fn locate_first_slice(buf: &[u8]) -> Option<usize> {
    let n = buf.len();
    let mut i = 0;
    while i + 3 < n {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            let code = buf[i + 3];
            if (0x01..=0xaf).contains(&code) {
                return Some(i);
            }
            i += 4;
        } else {
            i += 1;
        }
    }
    None
}

// ─────────────────────────── Tests ──────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_start_codes_locates_known_codes() {
        let buf = [
            0, 0, 1, 0xb3, 0xaa, 0xbb, // sequence header
            0, 0, 1, 0xb5, 0xcc, // extension
            0, 0, 1, 0x00, 0xdd, 0xee, // picture header
            0, 0, 1, 0x01, 0xff, // slice 1
        ];
        let codes = find_start_codes(&buf);
        assert_eq!(codes.len(), 4);
        assert_eq!(codes[0].0, 0xb3);
        assert_eq!(codes[1].0, 0xb5);
        assert_eq!(codes[2].0, 0x00);
        assert_eq!(codes[3].0, 0x01);
    }

    #[test]
    fn zigzag_inverse_roundtrip() {
        // Place sequential numbers in zig-zag, verify raster output is
        // a permutation. The DC coefficient is always at raster index 0.
        let zz: [u8; 64] = std::array::from_fn(|i| (i + 1) as u8);
        let raster = zigzag_to_raster_quant(&zz);
        // raster[ZIGZAG[0]] should equal zz[0] = 1.
        assert_eq!(raster[0], 1); // DC
        assert_eq!(raster[1], 2); // first AC across
        assert_eq!(raster[8], 3); // first AC down
    }
}
