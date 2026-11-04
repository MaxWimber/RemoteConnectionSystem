//! Minimal H.264 bitstream reading for reports: NAL units of an Annex B access unit and the SPS with its VUI.

/// Iterates over NAL unit payloads (without start codes) of an Annex B buffer.
pub fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&s| {
            // Trailing zeros before the next start code belong to it (four-byte start codes).
            let mut end = s - 3;
            while end > 0 && data[end - 1] == 0 {
                end -= 1;
            }
            end
        })
        .chain(std::iter::once(data.len()))
        .collect();
    starts
        .into_iter()
        .zip(ends)
        .filter(|(s, e)| s < e)
        .map(move |(s, e)| &data[s..e])
}

pub fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1f)
}

pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

/// Removes emulation prevention bytes (`00 00 03`).
fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(u32::from(bit))
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    fn flag(&mut self) -> Option<bool> {
        self.bit().map(|b| b == 1)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + u64::from(self.bits(zeros)?)) as u32)
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k % 2 == 1 {
            k.div_ceil(2) as i32
        } else {
            -((k / 2) as i32)
        })
    }
}

/// Fields of the sequence parameter set that matter for the stream and the browser.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sps {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub chroma_format_idc: u32,
    pub bit_depth_luma: u32,
    pub pic_order_cnt_type: u32,
    pub max_num_ref_frames: u32,
    pub width: u32,
    pub height: u32,
    pub frame_mbs_only: bool,
    pub vui: Option<Vui>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Vui {
    pub video_full_range: Option<bool>,
    pub colour_primaries: Option<u8>,
    pub transfer_characteristics: Option<u8>,
    pub matrix_coefficients: Option<u8>,
    pub timing: Option<(u32, u32, bool)>,
    /// From the bitstream restriction; 0 lets a decoder output every frame at once.
    pub max_num_reorder_frames: Option<u32>,
    pub max_dec_frame_buffering: Option<u32>,
}

fn skip_scaling_list(b: &mut Bits<'_>, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..size {
        if next != 0 {
            next = (last + b.se()? + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

fn skip_hrd(b: &mut Bits<'_>) -> Option<()> {
    let count = b.ue()? + 1;
    b.bits(8)?;
    for _ in 0..count {
        b.ue()?;
        b.ue()?;
        b.flag()?;
    }
    b.bits(20)?;
    Some(())
}

/// Returns the VUI and the bit position of `video_signal_type_present_flag` in the RBSP.
fn parse_vui(b: &mut Bits<'_>) -> Option<(Vui, usize)> {
    let mut vui = Vui::default();
    if b.flag()? {
        let idc = b.bits(8)?;
        if idc == 255 {
            b.bits(32)?;
        }
    }
    if b.flag()? {
        b.flag()?;
    }
    let signal_bit = b.pos;
    if b.flag()? {
        b.bits(3)?;
        vui.video_full_range = Some(b.flag()?);
        if b.flag()? {
            vui.colour_primaries = Some(b.bits(8)? as u8);
            vui.transfer_characteristics = Some(b.bits(8)? as u8);
            vui.matrix_coefficients = Some(b.bits(8)? as u8);
        }
    }
    if b.flag()? {
        b.ue()?;
        b.ue()?;
    }
    if b.flag()? {
        let units = b.bits(32)?;
        let scale = b.bits(32)?;
        vui.timing = Some((units, scale, b.flag()?));
    }
    let nal_hrd = b.flag()?;
    if nal_hrd {
        skip_hrd(b)?;
    }
    let vcl_hrd = b.flag()?;
    if vcl_hrd {
        skip_hrd(b)?;
    }
    if nal_hrd || vcl_hrd {
        b.flag()?;
    }
    b.flag()?;
    if b.flag()? {
        b.flag()?;
        for _ in 0..4 {
            b.ue()?;
        }
        vui.max_num_reorder_frames = Some(b.ue()?);
        vui.max_dec_frame_buffering = Some(b.ue()?);
    }
    Some((vui, signal_bit))
}

/// Parses an SPS NAL unit, header byte included.
pub fn parse_sps(nal: &[u8]) -> Option<Sps> {
    parse_sps_inner(nal).map(|(sps, _, _)| sps)
}

/// The SPS, the bit position of `video_signal_type_present_flag` if there is a VUI, and the RBSP.
fn parse_sps_inner(nal: &[u8]) -> Option<(Sps, Option<usize>, Vec<u8>)> {
    if nal_type(nal)? != NAL_SPS {
        return None;
    }
    let data = rbsp(&nal[1..]);
    let b = &mut Bits {
        data: &data,
        pos: 0,
    };
    let mut sps = Sps {
        profile_idc: b.bits(8)? as u8,
        constraint_flags: b.bits(8)? as u8,
        level_idc: b.bits(8)? as u8,
        chroma_format_idc: 1,
        bit_depth_luma: 8,
        ..Default::default()
    };
    b.ue()?;
    if matches!(
        sps.profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        sps.chroma_format_idc = b.ue()?;
        if sps.chroma_format_idc == 3 {
            b.flag()?;
        }
        sps.bit_depth_luma = b.ue()? + 8;
        b.ue()?;
        b.flag()?;
        if b.flag()? {
            let lists = if sps.chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if b.flag()? {
                    skip_scaling_list(b, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    b.ue()?;
    sps.pic_order_cnt_type = b.ue()?;
    match sps.pic_order_cnt_type {
        0 => {
            b.ue()?;
        }
        1 => {
            b.flag()?;
            b.se()?;
            b.se()?;
            for _ in 0..b.ue()? {
                b.se()?;
            }
        }
        _ => {}
    }
    sps.max_num_ref_frames = b.ue()?;
    b.flag()?;
    let width_mbs = b.ue()? + 1;
    let height_units = b.ue()? + 1;
    sps.frame_mbs_only = b.flag()?;
    if !sps.frame_mbs_only {
        b.flag()?;
    }
    b.flag()?;
    let (mut crop_x, mut crop_y) = (0, 0);
    if b.flag()? {
        let (left, right, top, bottom) = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
        // 4:2:0 crops in units of two pixels in both directions.
        let unit_y = if sps.frame_mbs_only { 2 } else { 4 };
        crop_x = (left + right) * 2;
        crop_y = (top + bottom) * unit_y;
    }
    let frame_height_mbs = if sps.frame_mbs_only {
        height_units
    } else {
        height_units * 2
    };
    sps.width = width_mbs * 16 - crop_x;
    sps.height = frame_height_mbs * 16 - crop_y;
    let mut signal_bit = None;
    if b.flag()?
        && let Some((vui, bit)) = parse_vui(b)
    {
        sps.vui = Some(vui);
        signal_bit = Some(bit);
    }
    Some((sps, signal_bit, data))
}

/// Bit writer into a reusable buffer.
struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    bits: usize,
}

impl BitWriter<'_> {
    fn put(&mut self, value: u32, n: u32) {
        for i in (0..n).rev() {
            if self.bits.is_multiple_of(8) {
                self.out.push(0);
            }
            let bit = ((value >> i) & 1) as u8;
            *self.out.last_mut().expect("pushed above") |= bit << (7 - self.bits % 8);
            self.bits += 1;
        }
    }
}

fn bit_at(data: &[u8], pos: usize) -> u32 {
    u32::from((data[pos / 8] >> (7 - pos % 8)) & 1)
}

/// Colour signal to write into the VUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColourDescription {
    pub full_range: bool,
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
}

/// BT.709 primaries, transfer and matrix in limited range.
pub const BT709_LIMITED: ColourDescription = ColourDescription {
    full_range: false,
    primaries: 1,
    transfer: 1,
    matrix: 1,
};

/// Writes an SPS with `video_signal_type` set to `colour` into `out`, header byte and emulation prevention
/// included. Every other bit is copied. Returns false and leaves `out` empty if the SPS has no VUI or does not parse.
pub fn rewrite_sps_colour(
    nal: &[u8],
    colour: ColourDescription,
    rbsp_scratch: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> bool {
    out.clear();
    let Some((_, Some(signal_bit), data)) = parse_sps_inner(nal) else {
        return false;
    };
    // The RBSP ends with a stop bit and zero padding; the copy of the tail stops before the stop bit.
    let Some(stop_bit) = (0..data.len() * 8).rev().find(|&p| bit_at(&data, p) == 1) else {
        return false;
    };
    let mut r = Bits {
        data: &data,
        pos: signal_bit,
    };
    let (video_format, tail_start) = match r.flag() {
        Some(true) => {
            let format = r.bits(3);
            let _ = r.flag();
            let described = r.flag();
            if described == Some(true) {
                let _ = r.bits(24);
            }
            match (format, described) {
                (Some(format), Some(_)) => (format, r.pos),
                _ => return false,
            }
        }
        Some(false) => (5, r.pos),
        None => return false,
    };
    if tail_start > stop_bit {
        return false;
    }

    rbsp_scratch.clear();
    let mut w = BitWriter {
        out: rbsp_scratch,
        bits: 0,
    };
    for p in 0..signal_bit {
        w.put(bit_at(&data, p), 1);
    }
    w.put(1, 1);
    w.put(video_format, 3);
    w.put(u32::from(colour.full_range), 1);
    w.put(1, 1);
    w.put(u32::from(colour.primaries), 8);
    w.put(u32::from(colour.transfer), 8);
    w.put(u32::from(colour.matrix), 8);
    for p in tail_start..stop_bit {
        w.put(bit_at(&data, p), 1);
    }
    w.put(1, 1);
    while !w.bits.is_multiple_of(8) {
        w.put(0, 1);
    }

    out.push(nal[0]);
    let mut zeros = 0;
    for &b in rbsp_scratch.iter() {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    true
}

/// Reusable buffers for `set_sps_colour`.
#[derive(Debug, Default)]
pub struct SpsRewriter {
    rbsp: Vec<u8>,
    sps: Vec<u8>,
    frame: Vec<u8>,
}

impl SpsRewriter {
    /// Replaces every SPS in an Annex B access unit with one that carries `colour` in its VUI.
    /// Returns the number of SPS units rewritten.
    pub fn set_sps_colour(
        &mut self,
        access_unit: &mut Vec<u8>,
        colour: ColourDescription,
    ) -> usize {
        let base = access_unit.as_ptr() as usize;
        let mut ranges = [(0usize, 0usize); 4];
        let mut count = 0;
        for nal in nal_units(access_unit) {
            if nal_type(nal) == Some(NAL_SPS) && count < ranges.len() {
                let start = nal.as_ptr() as usize - base;
                ranges[count] = (start, start + nal.len());
                count += 1;
            }
        }
        if count == 0 {
            return 0;
        }
        self.frame.clear();
        let mut copied = 0;
        let mut rewritten = 0;
        for &(start, end) in &ranges[..count] {
            if !rewrite_sps_colour(
                &access_unit[start..end],
                colour,
                &mut self.rbsp,
                &mut self.sps,
            ) {
                continue;
            }
            self.frame.extend_from_slice(&access_unit[copied..start]);
            self.frame.extend_from_slice(&self.sps);
            copied = end;
            rewritten += 1;
        }
        if rewritten > 0 {
            self.frame.extend_from_slice(&access_unit[copied..]);
            std::mem::swap(access_unit, &mut self.frame);
        }
        rewritten
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes bits the way an encoder does, to build test SPS units.
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        bits: u32,
    }

    impl Writer {
        fn put(&mut self, value: u32, n: u32) {
            for i in (0..n).rev() {
                if self.bits.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                let bit = ((value >> i) & 1) as u8;
                *self.bytes.last_mut().unwrap() |= bit << (7 - self.bits % 8);
                self.bits += 1;
            }
        }
        fn ue(&mut self, v: u32) {
            let x = v + 1;
            let len = 32 - x.leading_zeros();
            self.put(0, len - 1);
            self.put(x, len);
        }
        fn finish(mut self) -> Vec<u8> {
            self.put(1, 1);
            // Emulation prevention, as in a real stream.
            let mut out = vec![0x67];
            let mut zeros = 0;
            for b in self.bytes {
                if zeros >= 2 && b <= 3 {
                    out.push(3);
                    zeros = 0;
                }
                zeros = if b == 0 { zeros + 1 } else { 0 };
                out.push(b);
            }
            out
        }
    }

    /// High profile 1920x1080 with BT.709 limited range and no reordering, like the AMF Low Latency stream.
    fn high_1080p() -> Vec<u8> {
        let mut w = Writer::default();
        w.put(100, 8);
        w.put(0, 8);
        w.put(42, 8);
        w.ue(0); // sps id
        w.ue(1); // 4:2:0
        w.ue(0);
        w.ue(0);
        w.put(0, 1);
        w.put(0, 1); // no scaling matrix
        w.ue(0); // log2_max_frame_num - 4
        w.ue(2); // poc type 2
        w.ue(1); // refs
        w.put(0, 1);
        w.ue(119); // 120 MBs
        w.ue(67); // 68 MBs
        w.put(1, 1); // frame_mbs_only
        w.put(1, 1); // direct_8x8
        w.put(1, 1); // cropping
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(4); // 8 rows off the bottom
        w.put(1, 1); // vui
        w.put(0, 1); // aspect
        w.put(0, 1); // overscan
        w.put(1, 1); // video signal
        w.put(5, 3);
        w.put(0, 1); // limited range
        w.put(1, 1);
        w.put(1, 8);
        w.put(1, 8);
        w.put(1, 8);
        w.put(0, 1); // chroma loc
        w.put(1, 1); // timing
        w.put(1, 32);
        w.put(120, 32);
        w.put(1, 1);
        w.put(0, 1); // nal hrd
        w.put(0, 1); // vcl hrd
        w.put(0, 1); // pic struct
        w.put(1, 1); // bitstream restriction
        w.put(1, 1);
        w.ue(2);
        w.ue(1);
        w.ue(16);
        w.ue(16);
        w.ue(0); // max_num_reorder_frames
        w.ue(1);
        w.finish()
    }

    #[test]
    fn parses_a_1080p_high_sps_with_vui() {
        let sps = parse_sps(&high_1080p()).unwrap();
        assert_eq!((sps.profile_idc, sps.level_idc), (100, 42));
        assert_eq!((sps.width, sps.height), (1920, 1080));
        assert_eq!(sps.pic_order_cnt_type, 2);
        let vui = sps.vui.unwrap();
        assert_eq!(vui.video_full_range, Some(false));
        assert_eq!(
            (
                vui.colour_primaries,
                vui.transfer_characteristics,
                vui.matrix_coefficients
            ),
            (Some(1), Some(1), Some(1))
        );
        assert_eq!(vui.timing, Some((1, 120, true)));
        assert_eq!(vui.max_num_reorder_frames, Some(0));
    }

    #[test]
    fn splits_annex_b_with_three_and_four_byte_start_codes() {
        let sps = high_1080p();
        let mut stream = vec![0, 0, 0, 1, 0x09, 0x10];
        stream.extend_from_slice(&[0, 0, 0, 1]);
        stream.extend_from_slice(&sps);
        stream.extend_from_slice(&[0, 0, 1, 0x65, 0x88, 0x80]);
        let nals: Vec<&[u8]> = nal_units(&stream).collect();
        assert_eq!(nals.len(), 3);
        assert_eq!(nal_type(nals[0]), Some(NAL_AUD));
        assert_eq!(nals[1], &sps[..]);
        assert_eq!(nal_type(nals[2]), Some(NAL_IDR));
    }

    #[test]
    fn rewrites_the_rx560_sps_without_touching_other_fields() {
        let before = parse_sps(AMF_RX560_SPS).unwrap();
        let vui = before.vui.clone().unwrap();
        assert_eq!(vui.video_full_range, Some(false));
        assert_eq!(vui.colour_primaries, None);

        let (mut rbsp, mut out) = (Vec::new(), Vec::new());
        assert!(rewrite_sps_colour(
            AMF_RX560_SPS,
            BT709_LIMITED,
            &mut rbsp,
            &mut out
        ));
        let after = parse_sps(&out).unwrap();
        let new_vui = after.vui.clone().unwrap();
        assert_eq!(
            (
                new_vui.colour_primaries,
                new_vui.transfer_characteristics,
                new_vui.matrix_coefficients
            ),
            (Some(1), Some(1), Some(1))
        );
        assert_eq!(new_vui.video_full_range, Some(false));
        // Everything else, including the timing and the bitstream restriction after the signal type, is unchanged.
        let mut expected = before.clone();
        expected.vui = Some(Vui {
            colour_primaries: Some(1),
            transfer_characteristics: Some(1),
            matrix_coefficients: Some(1),
            ..vui
        });
        assert_eq!(after, expected);
    }

    #[test]
    fn replaces_an_existing_colour_description() {
        let (mut rbsp, mut out) = (Vec::new(), Vec::new());
        let full = ColourDescription {
            full_range: true,
            primaries: 9,
            transfer: 16,
            matrix: 9,
        };
        assert!(rewrite_sps_colour(&high_1080p(), full, &mut rbsp, &mut out));
        let mut again = Vec::new();
        assert!(rewrite_sps_colour(
            &out,
            BT709_LIMITED,
            &mut rbsp,
            &mut again
        ));
        assert_eq!(parse_sps(&again), parse_sps(&high_1080p()));
    }

    #[test]
    fn swaps_the_sps_inside_an_access_unit() {
        let mut frame = vec![0, 0, 0, 1, 0x09, 0x10, 0, 0, 0, 1];
        frame.extend_from_slice(AMF_RX560_SPS);
        let tail = [
            0, 0, 0, 1, 0x68, 0xee, 0x38, 0x80, 0, 0, 0, 1, 0x65, 0x88, 0x80,
        ];
        frame.extend_from_slice(&tail);
        let mut rewriter = SpsRewriter::default();
        assert_eq!(rewriter.set_sps_colour(&mut frame, BT709_LIMITED), 1);
        assert!(frame.starts_with(&[0, 0, 0, 1, 0x09, 0x10, 0, 0, 0, 1, 0x67]));
        assert!(frame.ends_with(&tail));
        let sps = nal_units(&frame).find_map(parse_sps).unwrap();
        assert_eq!(sps.vui.unwrap().colour_primaries, Some(1));
        // No SPS, no change.
        let mut p_frame = tail[10..].to_vec();
        assert_eq!(rewriter.set_sps_colour(&mut p_frame, BT709_LIMITED), 0);
        assert_eq!(p_frame, &tail[10..]);
    }

    /// SPS of the AMF Low Latency stream from the RX 560 server, 4 November 2026, runtime 1.4.31.
    const AMF_RX560_SPS: &[u8] = &[
        0x67, 0x64, 0x00, 0x2a, 0xac, 0x23, 0x28, 0x0f, 0x00, 0x44, 0xfc, 0xbf, 0xf8, 0x00, 0x08,
        0x00, 0x0a, 0x04, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x01, 0xe2, 0x3c, 0x22,
        0x11, 0x96,
    ];

    #[test]
    fn rejects_other_units_and_truncated_data() {
        assert_eq!(parse_sps(&[0x68, 0xce]), None);
        assert_eq!(parse_sps(&high_1080p()[..4]), None);
    }
}
