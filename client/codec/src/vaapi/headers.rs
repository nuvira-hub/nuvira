//! H.264 headers written here: the SPS and PPS given to drivers that take packed
//! headers, the level of a frame size, and the NAL unit types of Annex B data.

use std::ffi::c_int;

use super::{LOG2_MAX_FRAME_NUM_MINUS4, PROFILE_CONSTRAINED_BASELINE, PROFILE_HIGH};

/// H.264 level for a frame of `mbs` macroblocks (frame size limit only).
pub(super) fn level(mbs: u32) -> u8 {
    match mbs {
        0..=8192 => 41,
        8193..=36864 => 51,
        _ => 60,
    }
}

/// SPS and PPS matching the parameters given to the driver, as Annex B NAL units
/// (start code, emulation prevention bytes).
pub(super) struct ParameterSets {
    pub(super) profile: c_int,
    pub(super) level: u8,
    pub(super) mbs_w: u32,
    pub(super) mbs_h: u32,
    pub(super) crop_right: u32,
    pub(super) crop_bottom: u32,
    pub(super) time_scale: u32,
    pub(super) cabac: bool,
}

impl ParameterSets {
    pub(super) fn sps(&self) -> Vec<u8> {
        let mut b = Bits::default();
        let (profile_idc, constraints) = match self.profile {
            PROFILE_HIGH => (100, 0),
            PROFILE_CONSTRAINED_BASELINE => (66, 0b11_0000), // constraint_set0 and set1
            _ => (77, 0),
        };
        b.u(8, profile_idc);
        b.u(6, constraints);
        b.u(2, 0);
        b.u(8, u32::from(self.level));
        b.ue(0); // seq_parameter_set_id
        if profile_idc == 100 {
            b.ue(1); // chroma_format_idc 4:2:0
            b.ue(0); // bit_depth_luma_minus8
            b.ue(0); // bit_depth_chroma_minus8
            b.u(1, 0); // qpprime_y_zero_transform_bypass_flag
            b.u(1, 0); // seq_scaling_matrix_present_flag
        }
        b.ue(LOG2_MAX_FRAME_NUM_MINUS4);
        b.ue(2); // pic_order_cnt_type
        b.ue(1); // max_num_ref_frames
        b.u(1, 0); // gaps_in_frame_num_value_allowed_flag
        b.ue(self.mbs_w - 1);
        b.ue(self.mbs_h - 1);
        b.u(1, 1); // frame_mbs_only_flag
        b.u(1, 1); // direct_8x8_inference_flag
        let crop = self.crop_right != 0 || self.crop_bottom != 0;
        b.u(1, u32::from(crop));
        if crop {
            b.ue(0);
            b.ue(self.crop_right);
            b.ue(0);
            b.ue(self.crop_bottom);
        }
        b.u(1, 1); // vui_parameters_present_flag
        b.u(1, 0); // aspect_ratio_info_present_flag
        b.u(1, 0); // overscan_info_present_flag
        b.u(1, 0); // video_signal_type_present_flag
        b.u(1, 0); // chroma_loc_info_present_flag
        b.u(1, 1); // timing_info_present_flag
        b.u(32, 1); // num_units_in_tick
        b.u(32, self.time_scale);
        b.u(1, 0); // fixed_frame_rate_flag
        b.u(1, 0); // nal_hrd_parameters_present_flag
        b.u(1, 0); // vcl_hrd_parameters_present_flag
        b.u(1, 0); // pic_struct_present_flag
        b.u(1, 1); // bitstream_restriction_flag
        b.u(1, 1); // motion_vectors_over_pic_boundaries_flag
        b.ue(0); // max_bytes_per_pic_denom
        b.ue(0); // max_bits_per_mb_denom
        b.ue(15); // log2_max_mv_length_horizontal
        b.ue(15); // log2_max_mv_length_vertical
        b.ue(0); // max_num_reorder_frames
        b.ue(1); // max_dec_frame_buffering
        b.nal(0x67) // nal_ref_idc 3, SPS
    }

    pub(super) fn pps(&self) -> Vec<u8> {
        let mut b = Bits::default();
        b.ue(0); // pic_parameter_set_id
        b.ue(0); // seq_parameter_set_id
        b.u(1, u32::from(self.cabac)); // entropy_coding_mode_flag
        b.u(1, 0); // bottom_field_pic_order_in_frame_present_flag
        b.ue(0); // num_slice_groups_minus1
        b.ue(0); // num_ref_idx_l0_default_active_minus1
        b.ue(0); // num_ref_idx_l1_default_active_minus1
        b.u(1, 0); // weighted_pred_flag
        b.u(2, 0); // weighted_bipred_idc
        b.se(0); // pic_init_qp_minus26 (pic_init_qp 26)
        b.se(0); // pic_init_qs_minus26
        b.se(0); // chroma_qp_index_offset
        b.u(1, 1); // deblocking_filter_control_present_flag
        b.u(1, 0); // constrained_intra_pred_flag
        b.u(1, 0); // redundant_pic_cnt_present_flag
        b.nal(0x68) // nal_ref_idc 3, PPS
    }
}

/// Bit writer for parameter sets (RBSP).
#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    acc: u8,
    n: u32,
}

impl Bits {
    fn u(&mut self, bits: u32, value: u32) {
        for i in (0..bits).rev() {
            self.acc = (self.acc << 1) | ((value >> i) & 1) as u8;
            self.n += 1;
            if self.n == 8 {
                self.bytes.push(self.acc);
                self.acc = 0;
                self.n = 0;
            }
        }
    }

    /// Unsigned Exp-Golomb.
    fn ue(&mut self, value: u32) {
        let v = u64::from(value) + 1;
        let len = 64 - v.leading_zeros();
        self.u(len - 1, 0);
        for i in (0..len).rev() {
            self.u(1, ((v >> i) & 1) as u32);
        }
    }

    /// Signed Exp-Golomb.
    fn se(&mut self, value: i32) {
        let mapped = if value > 0 { 2 * value.unsigned_abs() - 1 } else { 2 * value.unsigned_abs() };
        self.ue(mapped);
    }

    /// Ends the RBSP (stop bit, alignment) and wraps it in a NAL unit with this
    /// header byte: start code and emulation prevention bytes.
    fn nal(mut self, header: u8) -> Vec<u8> {
        self.u(1, 1);
        while self.n != 0 {
            self.u(1, 0);
        }
        let mut out = vec![0, 0, 0, 1, header];
        let mut zeros = 0;
        for byte in self.bytes {
            if zeros == 2 && byte <= 3 {
                out.push(3);
                zeros = 0;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            out.push(byte);
        }
        out
    }
}

/// NAL unit types present in Annex B data.
pub(crate) fn nal_types(data: &[u8]) -> Vec<u8> {
    data.windows(4).filter(|w| w[0] == 0 && w[1] == 0 && w[2] == 1).map(|w| w[3] & 0x1f).collect()
}

#[cfg(test)]
mod tests {
    use super::{Bits, ParameterSets, level, nal_types};
    use crate::vaapi::PROFILE_MAIN;

    #[test]
    fn exp_golomb_and_nal() {
        let mut b = Bits::default();
        b.ue(0); // 1
        b.ue(1); // 010
        b.ue(2); // 011
        b.se(-1); // ue(2) 011
        b.se(1); // ue(1) 010
        assert_eq!(b.bytes, [0b1010_0110]);
        assert_eq!(b.n, 5);
        // Emulation prevention after two zero bytes.
        let mut b = Bits::default();
        b.u(24, 1);
        assert_eq!(b.nal(0x67), [0, 0, 0, 1, 0x67, 0, 0, 3, 1, 0x80]);
    }

    #[test]
    fn sps_of_1080p() {
        let sets = ParameterSets {
            profile: PROFILE_MAIN,
            level: 41,
            mbs_w: 120,
            mbs_h: 68,
            crop_right: 0,
            crop_bottom: 4,
            time_scale: 120,
            cabac: true,
        };
        let sps = sets.sps();
        assert_eq!(&sps[..8], [0, 0, 0, 1, 0x67, 77, 0, 41]);
        assert_eq!(nal_types(&sps), [7]);
        let pps = sets.pps();
        assert_eq!(nal_types(&pps), [8]);
        // pps_id 0, sps_id 0, CABAC: 1 1 1 ...
        assert_eq!(pps[5] >> 5, 0b111);
    }

    #[test]
    fn levels() {
        assert_eq!(level(120 * 68), 41); // 1080p
        assert_eq!(level(240 * 135), 51); // 4K
        assert_eq!(level(320 * 180), 60); // 5K
    }
}
