use bytes::Bytes;
use smelter_render::{Framerate, Resolution};

const H264_PROFILE_MAIN: u8 = 77;
const H264_LEVEL_4_0: u8 = 40;
const LOG2_MAX_FRAME_NUM_MINUS4: u32 = 12;
const LOG2_MAX_PIC_ORDER_CNT_LSB_MINUS4: u32 = 12;

pub(crate) fn h264_main_parameter_sets(
    resolution: Resolution,
    framerate: Framerate,
) -> Bytes {
    let coded_width = align_to_macroblock(resolution.width as u32);
    let coded_height = align_to_macroblock(resolution.height as u32);
    let width_mbs = coded_width / 16;
    let height_mbs = coded_height / 16;
    let crop_right = (coded_width - resolution.width as u32) / 2;
    let crop_bottom = (coded_height - resolution.height as u32) / 2;

    let mut out = Vec::new();
    out.extend_from_slice(&annexb_nal(
        0x67,
        sps_rbsp(width_mbs, height_mbs, crop_right, crop_bottom, framerate),
    ));
    out.extend_from_slice(&annexb_nal(0x68, pps_rbsp()));
    out.into()
}

fn sps_rbsp(
    width_mbs: u32,
    height_mbs: u32,
    crop_right: u32,
    crop_bottom: u32,
    framerate: Framerate,
) -> Vec<u8> {
    let mut bits = BitWriter::new();
    bits.bits(H264_PROFILE_MAIN.into(), 8);
    bits.bits(0, 8);
    bits.bits(H264_LEVEL_4_0.into(), 8);
    bits.ue(0);
    bits.ue(1);
    bits.ue(0);
    bits.ue(0);
    bits.bit(false);
    bits.bit(false);
    bits.ue(LOG2_MAX_FRAME_NUM_MINUS4);
    bits.ue(0);
    bits.ue(LOG2_MAX_PIC_ORDER_CNT_LSB_MINUS4);
    bits.ue(1);
    bits.bit(false);
    bits.ue(width_mbs - 1);
    bits.ue(height_mbs - 1);
    bits.bit(true);
    bits.bit(true);
    bits.bit(crop_right > 0 || crop_bottom > 0);
    if crop_right > 0 || crop_bottom > 0 {
        bits.ue(0);
        bits.ue(crop_right);
        bits.ue(0);
        bits.ue(crop_bottom);
    }
    bits.bit(true);
    write_vui(&mut bits, framerate);
    bits.finish_rbsp()
}

fn write_vui(bits: &mut BitWriter, framerate: Framerate) {
    bits.bit(true);
    bits.bits(1, 8);
    bits.bit(false);
    bits.bit(false);
    bits.bit(false);
    bits.bit(true);
    bits.bits(framerate.den.max(1), 32);
    bits.bits(framerate.num.max(1).saturating_mul(2), 32);
    bits.bit(true);
    bits.bit(false);
    bits.bit(false);
    bits.bit(false);
    bits.bit(false);
}

fn pps_rbsp() -> Vec<u8> {
    let mut bits = BitWriter::new();
    bits.ue(0);
    bits.ue(0);
    bits.bit(false);
    bits.bit(false);
    bits.ue(0);
    bits.ue(0);
    bits.ue(0);
    bits.bit(false);
    bits.bits(0, 2);
    bits.se(0);
    bits.se(0);
    bits.se(0);
    bits.bit(true);
    bits.bit(false);
    bits.bit(false);
    bits.finish_rbsp()
}

fn annexb_nal(header: u8, rbsp: Vec<u8>) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 1, header];
    out.extend_from_slice(&escape_rbsp(&rbsp));
    out
}

fn escape_rbsp(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len());
    let mut zero_count = 0;
    for &byte in rbsp {
        if zero_count >= 2 && byte <= 3 {
            out.push(3);
            zero_count = 0;
        }
        out.push(byte);
        zero_count = if byte == 0 { zero_count + 1 } else { 0 };
    }
    out
}

fn align_to_macroblock(value: u32) -> u32 {
    value.next_multiple_of(16)
}

struct BitWriter {
    bytes: Vec<u8>,
    byte: u8,
    used: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self { bytes: Vec::new(), byte: 0, used: 0 }
    }

    fn bit(&mut self, value: bool) {
        self.byte = (self.byte << 1) | u8::from(value);
        self.used += 1;
        if self.used == 8 {
            self.bytes.push(self.byte);
            self.byte = 0;
            self.used = 0;
        }
    }

    fn bits(&mut self, value: u32, count: u8) {
        for shift in (0..count).rev() {
            self.bit(((value >> shift) & 1) != 0);
        }
    }

    fn ue(&mut self, value: u32) {
        let code = value + 1;
        let bits = u32::BITS - code.leading_zeros();
        for _ in 0..bits - 1 {
            self.bit(false);
        }
        self.bits(code, bits as u8);
    }

    fn se(&mut self, value: i32) {
        let code = if value <= 0 { (-value as u32) * 2 } else { value as u32 * 2 - 1 };
        self.ue(code);
    }

    fn finish_rbsp(mut self) -> Vec<u8> {
        self.bit(true);
        while self.used != 0 {
            self.bit(false);
        }
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use crate::pipeline::utils::build_avc_decoder_config;

    use super::*;

    #[test]
    fn parameter_sets_build_avc_config() {
        let parameter_sets = h264_main_parameter_sets(
            Resolution { width: 1920, height: 1080 },
            Framerate { num: 30, den: 1 },
        );
        let config = build_avc_decoder_config(&parameter_sets).unwrap();
        assert_eq!(&config[..4], &[1, H264_PROFILE_MAIN, 0, H264_LEVEL_4_0]);
    }

    #[test]
    fn parameter_sets_use_annexb_start_codes() {
        let parameter_sets = h264_main_parameter_sets(
            Resolution { width: 1280, height: 720 },
            Framerate { num: 60, den: 1 },
        );
        assert!(parameter_sets.starts_with(&[0, 0, 0, 1, 0x67]));
        assert!(parameter_sets.windows(5).any(|window| window == [0, 0, 0, 1, 0x68]));
    }
}
