//! Fragmented MP4 (ISO BMFF) writer for the H.264 stream of a session.
//!
//! The frames are stored as they arrive from the agent, without encoding them
//! again: Annex B NAL units become length-prefixed samples of an `avc1` track.
//! The file is fragmented (`moov` at the start, then a `moof` + `mdat` about every
//! second, each starting at a keyframe when there is one), so that a crash or a full
//! disk loses at most the last second. At the end the total duration is written
//! into `mehd` and an `mfra` index of the keyframes is appended for seeking.

use std::io::{Seek, SeekFrom, Write};

use anyhow::{Context, Result, bail};

/// Ticks per second of the video track.
pub const TIMESCALE: u32 = 90_000;
/// Ticks per second of the movie (`mvhd`, `mehd`).
const MOVIE_TIMESCALE: u32 = 1_000;
/// A fragment is closed at a keyframe, or when it holds this much video.
const FRAGMENT_TICKS: u64 = TIMESCALE as u64;
/// Duration of the last sample, when nothing follows it (1/30 s).
const LAST_SAMPLE_TICKS: u32 = TIMESCALE / 30;

/// What the writer needs to know from a sequence parameter set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpsInfo {
    pub width: u32,
    pub height: u32,
    profile: u8,
    compatibility: u8,
    level: u8,
    chroma_format: u32,
    bit_depth_luma: u32,
    bit_depth_chroma: u32,
}

/// NAL units of an Annex B buffer (start codes removed).
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
            // The start code of the next unit, with its optional fourth zero byte.
            let mut end = s - 3;
            while end > 0 && data[end - 1] == 0 {
                end -= 1;
            }
            end
        })
        .chain(std::iter::once(data.len()))
        .collect();
    starts.into_iter().zip(ends).filter(|(s, e)| s < e).map(move |(s, e)| &data[s..e])
}

pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

/// Reads the bits of a NAL unit's payload (emulation prevention bytes removed).
struct Bits {
    data: Vec<u8>,
    pos: usize,
}

impl Bits {
    fn new(nal: &[u8]) -> Self {
        let mut data = Vec::with_capacity(nal.len());
        let mut zeros = 0;
        for &b in nal {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            data.push(b);
        }
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Result<u32> {
        let byte = *self.data.get(self.pos / 8).context("SPS too short")?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(bit.into())
    }

    fn bits(&mut self, n: u32) -> Result<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }

    /// Unsigned Exp-Golomb.
    fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                bail!("bad Exp-Golomb code");
            }
        }
        Ok(((1u64 << zeros) - 1 + u64::from(self.bits(zeros)?)) as u32)
    }

    /// Signed Exp-Golomb (only skipped).
    fn se(&mut self) -> Result<()> {
        self.ue().map(|_| ())
    }
}

/// Width, height, profile and level of an SPS NAL unit (with its header byte).
pub fn parse_sps(nal: &[u8]) -> Result<SpsInfo> {
    if nal_type(nal) != NAL_SPS || nal.len() < 4 {
        bail!("not an SPS");
    }
    let mut r = Bits::new(&nal[1..]);
    let profile = r.bits(8)? as u8;
    let compatibility = r.bits(8)? as u8;
    let level = r.bits(8)? as u8;
    r.ue()?; // seq_parameter_set_id
    let (mut chroma_format, mut bit_depth_luma, mut bit_depth_chroma) = (1, 8, 8);
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma_format = r.ue()?;
        if chroma_format == 3 {
            r.bit()?; // separate_colour_plane_flag
        }
        bit_depth_luma = r.ue()? + 8;
        bit_depth_chroma = r.ue()? + 8;
        r.bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.bit()? == 1 {
            // seq_scaling_matrix_present_flag: the lists are skipped.
            for i in 0..if chroma_format == 3 { 12 } else { 8 } {
                if r.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            let code = r.ue()?;
                            let delta =
                                if code % 2 == 1 { (code as i32 + 1) / 2 } else { -(code as i32 / 2) };
                            next = (last + delta + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bit()?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = r.ue()? + 1;
    let height_units = r.ue()? + 1;
    let frame_mbs_only = r.bit()?;
    if frame_mbs_only == 0 {
        r.bit()?; // mb_adaptive_frame_field_flag
    }
    r.bit()?; // direct_8x8_inference_flag
    let mut width = width_mbs * 16;
    let mut height = (2 - frame_mbs_only) * height_units * 16;
    if r.bit()? == 1 {
        let (left, right, top, bottom) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        let (unit_x, unit_y) = match chroma_format {
            0 | 3 => (1, 2 - frame_mbs_only),
            2 => (2, 2 - frame_mbs_only),
            _ => (2, 2 * (2 - frame_mbs_only)),
        };
        width = width.saturating_sub((left + right) * unit_x);
        height = height.saturating_sub((top + bottom) * unit_y);
    }
    if width == 0 || height == 0 {
        bail!("SPS with an empty picture");
    }
    Ok(SpsInfo {
        width,
        height,
        profile,
        compatibility,
        level,
        chroma_format,
        bit_depth_luma,
        bit_depth_chroma,
    })
}

struct Sample {
    data: Vec<u8>,
    time: u64,
    key: bool,
}

/// One MP4 file being written.
pub struct Mp4Writer<W: Write + Seek> {
    out: W,
    sps: Vec<u8>,
    pps: Vec<u8>,
    pending: Vec<Sample>,
    /// Duration of the last sample written, for the very last one.
    last_duration: u32,
    sequence: u32,
    /// Position of the `mehd` duration (patched at the end).
    mehd_at: u64,
    /// Start time and position of the fragments that begin with a keyframe.
    keyframes: Vec<(u64, u64)>,
    end_time: u64,
}

impl<W: Write + Seek> Mp4Writer<W> {
    /// Writes the header for a stream described by `sps` and `pps` (NAL units with
    /// their header byte, no start code).
    pub fn create(mut out: W, sps: &[u8], pps: &[u8]) -> Result<Self> {
        let info = parse_sps(sps)?;
        let start = out.stream_position()?;
        let ftyp = boxed(b"ftyp", |b| {
            b.extend_from_slice(b"isom");
            put32(b, 0x200);
            for brand in [b"isom", b"iso6", b"avc1", b"mp41"] {
                b.extend_from_slice(brand);
            }
        });
        let moov = moov(&info, sps, pps);
        // The mehd duration is the last 8 bytes of the mehd box, found by its name.
        let mehd = find(&moov, b"mehd").context("mehd")?;
        out.write_all(&ftyp)?;
        out.write_all(&moov)?;
        Ok(Self {
            out,
            sps: sps.to_vec(),
            pps: pps.to_vec(),
            pending: Vec::new(),
            last_duration: LAST_SAMPLE_TICKS,
            sequence: 0,
            mehd_at: start + ftyp.len() as u64 + mehd as u64 + 12,
            keyframes: Vec::new(),
            end_time: 0,
        })
    }

    /// Adds a frame (Annex B) shown at `time` ticks from the start of the file.
    /// Parameter sets equal to those of the header are left out of the sample.
    pub fn push(&mut self, frame: &[u8], time: u64, key: bool) -> Result<()> {
        let mut data = Vec::with_capacity(frame.len());
        for nal in nal_units(frame) {
            let skip = match nal_type(nal) {
                NAL_AUD => true,
                NAL_SPS => nal == self.sps.as_slice(),
                NAL_PPS => nal == self.pps.as_slice(),
                _ => false,
            };
            if !skip {
                put32(&mut data, nal.len() as u32);
                data.extend_from_slice(nal);
            }
        }
        if data.is_empty() {
            return Ok(());
        }
        // Times only go forward.
        let time = match self.pending.last() {
            Some(prev) if time <= prev.time => prev.time + 1,
            _ => time,
        };
        let first = self.pending.first().map(|s| s.time);
        if first.is_some_and(|f| key || time - f >= FRAGMENT_TICKS) {
            self.flush(time)?;
        }
        self.pending.push(Sample { data, time, key });
        Ok(())
    }

    /// Writes the pending samples as one fragment; `next` is the time of the sample
    /// that follows them (the duration of the last one).
    fn flush(&mut self, next: u64) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.sequence += 1;
        let samples = std::mem::take(&mut self.pending);
        let durations: Vec<u32> = samples
            .windows(2)
            .map(|w| (w[1].time - w[0].time).min(u32::MAX as u64) as u32)
            .chain(std::iter::once(
                (next.saturating_sub(samples[samples.len() - 1].time)).clamp(1, u32::MAX as u64) as u32,
            ))
            .collect();
        self.last_duration = durations[durations.len() - 1];
        let base = samples[0].time;
        let mut moof = moof(self.sequence, base, &samples, &durations, 0);
        // The data offset points just past the mdat header, after the moof.
        let offset = moof.len() as u32 + 8;
        moof = moof_with_offset(moof, offset);
        let position = self.out.stream_position()?;
        if samples[0].key {
            self.keyframes.push((base, position));
        }
        self.out.write_all(&moof)?;
        let size: usize = samples.iter().map(|s| s.data.len()).sum();
        put32_to(&mut self.out, (size + 8) as u32)?;
        self.out.write_all(b"mdat")?;
        for s in &samples {
            self.out.write_all(&s.data)?;
        }
        self.out.flush()?;
        self.end_time = next;
        Ok(())
    }

    /// Duration written so far, in ticks.
    pub fn duration(&self) -> u64 {
        self.pending.last().map_or(self.end_time, |s| s.time)
    }

    /// Writes what is left, the total duration and the keyframe index.
    pub fn finish(mut self) -> Result<W> {
        if let Some(last) = self.pending.last() {
            let next = last.time + u64::from(self.last_duration.max(1));
            self.flush(next)?;
        }
        let mfra = mfra(&self.keyframes);
        self.out.write_all(&mfra)?;
        let end = self.out.stream_position()?;
        self.out.seek(SeekFrom::Start(self.mehd_at))?;
        let ms = self.end_time * u64::from(MOVIE_TIMESCALE) / u64::from(TIMESCALE);
        self.out.write_all(&ms.to_be_bytes())?;
        self.out.seek(SeekFrom::Start(end))?;
        self.out.flush()?;
        Ok(self.out)
    }
}

fn put16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_be_bytes());
}

fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_be_bytes());
}

fn put64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_be_bytes());
}

fn put32_to(w: &mut impl Write, v: u32) -> std::io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

/// A box: size, type and the content written by `body`.
fn boxed(kind: &[u8; 4], body: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut b = vec![0; 4];
    b.extend_from_slice(kind);
    body(&mut b);
    let size = b.len() as u32;
    b[..4].copy_from_slice(&size.to_be_bytes());
    b
}

/// A full box (version and flags after the type).
fn full(kind: &[u8; 4], version: u8, flags: u32, body: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    boxed(kind, |b| {
        put32(b, (u32::from(version) << 24) | (flags & 0xff_ffff));
        body(b);
    })
}

/// Offset of the first box named `kind` inside `data` (searching nested boxes).
fn find(data: &[u8], kind: &[u8; 4]) -> Option<usize> {
    data.windows(4).position(|w| w == kind).map(|p| p - 4)
}

const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn moov(info: &SpsInfo, sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mvhd = full(b"mvhd", 0, 0, |b| {
        put32(b, 0);
        put32(b, 0);
        put32(b, MOVIE_TIMESCALE);
        put32(b, 0);
        put32(b, 0x0001_0000);
        put16(b, 0x0100);
        b.extend_from_slice(&[0; 10]);
        MATRIX.iter().for_each(|&m| put32(b, m));
        b.extend_from_slice(&[0; 24]);
        put32(b, 2);
    });
    let tkhd = full(b"tkhd", 0, 3, |b| {
        put32(b, 0);
        put32(b, 0);
        put32(b, 1);
        put32(b, 0);
        put32(b, 0);
        b.extend_from_slice(&[0; 8]);
        put16(b, 0);
        put16(b, 0);
        put16(b, 0);
        put16(b, 0);
        MATRIX.iter().for_each(|&m| put32(b, m));
        put32(b, info.width << 16);
        put32(b, info.height << 16);
    });
    let mdhd = full(b"mdhd", 0, 0, |b| {
        put32(b, 0);
        put32(b, 0);
        put32(b, TIMESCALE);
        put32(b, 0);
        put16(b, 0x55c4); // "und"
        put16(b, 0);
    });
    let hdlr = full(b"hdlr", 0, 0, |b| {
        put32(b, 0);
        b.extend_from_slice(b"vide");
        b.extend_from_slice(&[0; 12]);
        b.extend_from_slice(b"NuviraHub\0");
    });
    let avcc = boxed(b"avcC", |b| {
        b.extend_from_slice(&[1, info.profile, info.compatibility, info.level, 0xff, 0xe1]);
        put16(b, sps.len() as u16);
        b.extend_from_slice(sps);
        b.push(1);
        put16(b, pps.len() as u16);
        b.extend_from_slice(pps);
        if matches!(info.profile, 100 | 110 | 122 | 144) {
            b.push(0xfc | info.chroma_format as u8);
            b.push(0xf8 | (info.bit_depth_luma - 8) as u8);
            b.push(0xf8 | (info.bit_depth_chroma - 8) as u8);
            b.push(0);
        }
    });
    let avc1 = boxed(b"avc1", |b| {
        b.extend_from_slice(&[0; 6]);
        put16(b, 1);
        b.extend_from_slice(&[0; 16]);
        put16(b, info.width as u16);
        put16(b, info.height as u16);
        put32(b, 0x0048_0000);
        put32(b, 0x0048_0000);
        put32(b, 0);
        put16(b, 1);
        let mut name = [0u8; 32];
        let text = b"NuviraHub";
        name[0] = text.len() as u8;
        name[1..=text.len()].copy_from_slice(text);
        b.extend_from_slice(&name);
        put16(b, 0x18);
        put16(b, 0xffff);
        b.extend_from_slice(&avcc);
    });
    let stbl = boxed(b"stbl", |b| {
        b.extend(full(b"stsd", 0, 0, |b| {
            put32(b, 1);
            b.extend_from_slice(&avc1);
        }));
        b.extend(full(b"stts", 0, 0, |b| put32(b, 0)));
        b.extend(full(b"stsc", 0, 0, |b| put32(b, 0)));
        b.extend(full(b"stsz", 0, 0, |b| {
            put32(b, 0);
            put32(b, 0);
        }));
        b.extend(full(b"stco", 0, 0, |b| put32(b, 0)));
    });
    let minf = boxed(b"minf", |b| {
        b.extend(full(b"vmhd", 0, 1, |b| b.extend_from_slice(&[0; 8])));
        b.extend(boxed(b"dinf", |b| {
            b.extend(full(b"dref", 0, 0, |b| {
                put32(b, 1);
                b.extend(full(b"url ", 0, 1, |_| {}));
            }));
        }));
        b.extend(stbl);
    });
    let trak = boxed(b"trak", |b| {
        b.extend(tkhd);
        b.extend(boxed(b"mdia", |b| {
            b.extend(mdhd);
            b.extend(hdlr);
            b.extend(minf);
        }));
    });
    let mvex = boxed(b"mvex", |b| {
        b.extend(full(b"mehd", 1, 0, |b| put64(b, 0)));
        b.extend(full(b"trex", 0, 0, |b| {
            put32(b, 1);
            put32(b, 1);
            put32(b, 0);
            put32(b, 0);
            put32(b, 0);
        }));
    });
    boxed(b"moov", |b| {
        b.extend(mvhd);
        b.extend(trak);
        b.extend(mvex);
    })
}

/// Sample flags: a keyframe depends on nothing; the others on previous frames and
/// are not sync samples.
fn sample_flags(key: bool) -> u32 {
    if key { 0x0200_0000 } else { 0x0101_0000 }
}

fn moof(sequence: u32, base: u64, samples: &[Sample], durations: &[u32], data_offset: u32) -> Vec<u8> {
    boxed(b"moof", |b| {
        b.extend(full(b"mfhd", 0, 0, |b| put32(b, sequence)));
        b.extend(boxed(b"traf", |b| {
            // default-base-is-moof
            b.extend(full(b"tfhd", 0, 0x02_0000, |b| put32(b, 1)));
            b.extend(full(b"tfdt", 1, 0, |b| put64(b, base)));
            // data offset, duration, size and flags for every sample
            b.extend(full(b"trun", 0, 0x701, |b| {
                put32(b, samples.len() as u32);
                put32(b, data_offset);
                for (s, d) in samples.iter().zip(durations) {
                    put32(b, *d);
                    put32(b, s.data.len() as u32);
                    put32(b, sample_flags(s.key));
                }
            }));
        }));
    })
}

/// The same moof with the trun data offset set (it does not change the size).
fn moof_with_offset(mut moof: Vec<u8>, offset: u32) -> Vec<u8> {
    let trun = find(&moof, b"trun").expect("trun");
    // size, type, version/flags, sample count, then the offset.
    moof[trun + 16..trun + 20].copy_from_slice(&offset.to_be_bytes());
    moof
}

fn mfra(keyframes: &[(u64, u64)]) -> Vec<u8> {
    let tfra = full(b"tfra", 1, 0, |b| {
        put32(b, 1);
        // One byte each for the traf, trun and sample numbers.
        put32(b, 0);
        put32(b, keyframes.len() as u32);
        for &(time, offset) in keyframes {
            put64(b, time);
            put64(b, offset);
            b.extend_from_slice(&[1, 1, 1]);
        }
    });
    let size = (8 + tfra.len() + 16) as u32;
    boxed(b"mfra", |b| {
        b.extend(tfra);
        b.extend(full(b"mfro", 0, 0, |b| put32(b, size)));
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames of a real stream: OpenH264, `count` frames of `w`x`h`, keyframe when asked.
    fn encode(w: u32, h: u32, keys: &[bool]) -> Vec<Vec<u8>> {
        let mut encoder = nuvira_codec::OpenH264Encoder::new(Default::default()).unwrap();
        let mut bgra = vec![128u8; (w * h * 4) as usize];
        keys.iter()
            .enumerate()
            .filter_map(|(i, &key)| {
                bgra[i * 4..i * 4 + 400].fill((i * 7) as u8);
                let image =
                    nuvira_codec::BgraImage { width: w, height: h, stride: (w * 4) as usize, data: &bgra };
                nuvira_codec::VideoEncoder::encode(&mut encoder, &image, key).unwrap().map(|f| f.data)
            })
            .collect()
    }

    fn param_sets(frame: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let find = |t| nal_units(frame).find(|n| nal_type(n) == t).unwrap().to_vec();
        (find(NAL_SPS), find(NAL_PPS))
    }

    #[test]
    fn splits_annex_b() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5];
        let units: Vec<&[u8]> = nal_units(&data).collect();
        assert_eq!(units, [&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4, 5]]);
    }

    #[test]
    fn reads_sps_size() {
        // Sizes that are not multiples of 16 use cropping.
        for (w, h) in [(1920, 1080), (330, 202), (1366, 768)] {
            let frames = encode(w, h, &[true]);
            let info = parse_sps(&param_sets(&frames[0]).0).unwrap();
            assert_eq!((info.width, info.height), (w, h));
        }
    }

    #[test]
    fn writes_fragments_and_index() {
        // 3 s at 10 fps, keyframe every 2 s: fragments by time and by keyframe.
        let keys: Vec<bool> = (0..30).map(|i| i % 20 == 0).collect();
        let frames = encode(320, 240, &keys);
        assert_eq!(frames.len(), 30);
        let (sps, pps) = param_sets(&frames[0]);
        let mut w = Mp4Writer::create(std::io::Cursor::new(Vec::new()), &sps, &pps).unwrap();
        for (i, frame) in frames.iter().enumerate() {
            w.push(frame, i as u64 * 9000, keys[i]).unwrap();
        }
        let file = w.finish().unwrap().into_inner();
        let count = |kind: &[u8]| file.windows(4).filter(|x| *x == kind).count();
        assert_eq!(count(b"moov"), 1);
        // 0-0.9 s, 1-1.9 s, 2-2.9 s (keyframe at 2 s).
        assert_eq!(count(b"moof"), 3);
        assert_eq!(count(b"mfra"), 1);
        // The parameter sets are only in the header.
        assert_eq!(file.windows(sps.len()).filter(|x| *x == sps.as_slice()).count(), 1);
        // mehd: 30 samples of 0.1 s.
        let mehd = find(&file, b"mehd").unwrap();
        assert_eq!(u64::from_be_bytes(file[mehd + 12..mehd + 20].try_into().unwrap()), 3000);
        // tfra: the two keyframe fragments.
        let tfra = find(&file, b"tfra").unwrap();
        assert_eq!(u32::from_be_bytes(file[tfra + 20..tfra + 24].try_into().unwrap()), 2);
        // mfra size in mfro.
        let size = u32::from_be_bytes(file[file.len() - 4..].try_into().unwrap()) as usize;
        assert_eq!(&file[file.len() - size + 4..file.len() - size + 8], b"mfra");
    }
}
