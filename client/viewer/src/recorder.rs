//! Recording of a session: the H.264 frames received from the agent go into MP4
//! files (see `mp4.rs`) as they are, without encoding them again.
//!
//! A recording starts at a keyframe. When the remote resolution changes (another
//! screen, all the screens, a remote resolution change) the file is closed and the
//! next part starts, "name (2).mp4" and so on: MP4 players do not cope well with a
//! track whose picture size changes. Frames lost on the way (datagrams) break the
//! stream until the next keyframe: they are skipped, and the picture stays still.
//!
//! The remote computer's sound, when the session has it, goes into a second track.
//! Its packets carry no time of their own (and silence is not sent): each is placed
//! by when it arrived here, moved to the agent's clock with the difference measured
//! on the video frames, and packets that follow one another stay 20 ms apart.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use nuvira_proto::v1::VideoFrame;
use tracing::{info, warn};

use crate::mp4::{Mp4Writer, NAL_PPS, NAL_SPS, nal_type, nal_units, parse_sps};

struct Part {
    writer: Mp4Writer<BufWriter<File>>,
    path: PathBuf,
    size: (u32, u32),
    /// Agent clock (µs) of the first frame.
    start_us: u64,
    /// Agent clock (µs) of the last frame written, and when it arrived here: the
    /// last picture lasts until the file is closed.
    last_us: u64,
    last_at: std::time::Instant,
}

pub struct Recorder {
    /// NuviraHub Pro: every closed file is sent to the server.
    upload: Option<crate::upload::Target>,
    /// An Opus packet of silence when the session has sound: the files get a sound track.
    silence: Option<Vec<u8>>,
    /// Arrival here minus the agent's capture time of the video frames (µs): the
    /// lowest seen, slowly let up so that the two clocks drifting apart are followed.
    clock_offset: Option<i64>,
    /// The last audio packet written.
    audio_seq: Option<u32>,
    dir: PathBuf,
    stem: String,
    part: Option<Part>,
    parts: u32,
    last_seq: Option<u64>,
    waiting_key: bool,
    /// Files written, closed ones included.
    files: Vec<PathBuf>,
}

impl Recorder {
    /// Files go in `dir` (created if missing), named after `stem`; with `audio` they
    /// have a sound track.
    pub fn new(dir: PathBuf, stem: String, upload: Option<crate::upload::Target>, audio: bool) -> Self {
        let silence = audio
            .then(|| silence_packet().inspect_err(|e| warn!("recording without sound: {e:#}")).ok())
            .flatten();
        Self {
            upload,
            silence,
            clock_offset: None,
            audio_seq: None,
            dir,
            stem,
            part: None,
            parts: 0,
            last_seq: None,
            waiting_key: true,
            files: Vec::new(),
        }
    }

    /// Adds a frame, received here at `received_us`. Errors (disk full, folder not
    /// writable) end the recording: the caller stops it and says why.
    pub fn push(&mut self, frame: &VideoFrame, received_us: u64) -> Result<()> {
        let offset = received_us as i64 - frame.capture_us as i64;
        self.clock_offset = Some(match self.clock_offset {
            Some(o) if o <= offset => o + (offset - o) / 1000,
            _ => offset,
        });
        let gap = self.last_seq.is_some_and(|last| frame.seq != last + 1);
        self.last_seq = Some(frame.seq);
        if gap && !frame.keyframe {
            self.waiting_key = true;
        }
        if frame.keyframe {
            self.waiting_key = false;
            self.on_keyframe(frame)?;
        }
        if self.waiting_key {
            return Ok(());
        }
        let Some(part) = self.part.as_mut() else { return Ok(()) };
        // 90 kHz ticks from the first frame of the part.
        let time = frame.capture_us.saturating_sub(part.start_us) * 9 / 100;
        part.last_us = part.last_us.max(frame.capture_us);
        part.last_at = std::time::Instant::now();
        part.writer.push(&frame.data, time, frame.keyframe)
    }

    /// Adds a packet of the remote computer's sound, received here at `received_us`.
    pub fn push_audio(&mut self, seq: u32, packet: &[u8], received_us: u64) -> Result<()> {
        let (Some(offset), Some(part)) = (self.clock_offset, self.part.as_mut()) else { return Ok(()) };
        let Some(next) = part.writer.next_audio_slot() else { return Ok(()) };
        // Late or repeated.
        if self.audio_seq.is_some_and(|s| seq.wrapping_sub(s).wrapping_sub(1) > u32::MAX / 2) {
            return Ok(());
        }
        let follows = self.audio_seq.is_some_and(|s| seq == s.wrapping_add(1));
        self.audio_seq = Some(seq);
        let since = received_us as i64 - offset - part.start_us as i64;
        if since < 0 {
            return Ok(());
        }
        let estimate = since as u64 / 20_000;
        // One packet after the other: the next slot, unless the time says otherwise by
        // more than 200 ms (the sound stopped and started again, the clocks moved).
        let slot = if follows && estimate.abs_diff(next) <= 10 { next } else { estimate.max(next) };
        part.writer.push_audio(packet, slot)?;
        Ok(())
    }

    /// A keyframe carries the parameter sets: a new part when the size changed.
    fn on_keyframe(&mut self, frame: &VideoFrame) -> Result<()> {
        let sps = nal_units(&frame.data).find(|n| nal_type(n) == NAL_SPS);
        let pps = nal_units(&frame.data).find(|n| nal_type(n) == NAL_PPS);
        let (Some(sps), Some(pps)) = (sps, pps) else {
            warn!("keyframe without parameter sets: not recorded");
            self.waiting_key = true;
            return Ok(());
        };
        let info = parse_sps(sps)?;
        let size = (info.width, info.height);
        if self.part.as_ref().is_some_and(|p| p.size == size) {
            return Ok(());
        }
        // A recording started during the session begins with the keyframe asked for,
        // still at the reduced size of the window, and the full-size one follows a
        // moment later: that first bit is dropped instead of becoming a file of its own.
        let first_moment = self.parts == 1
            && self
                .part
                .as_ref()
                .is_some_and(|p| p.writer.duration() < u64::from(crate::mp4::TIMESCALE) * 3 / 2);
        if first_moment && let Some(part) = self.part.take() {
            drop(part.writer);
            let _ = std::fs::remove_file(&part.path);
            self.files.retain(|f| *f != part.path);
            self.parts = 0;
            info!("first {}x{} frames dropped: the full size follows", part.size.0, part.size.1);
        }
        self.close_part()?;
        std::fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        self.parts += 1;
        let path = free_path(&self.dir, &self.stem, self.parts);
        let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        let writer =
            Mp4Writer::create(BufWriter::with_capacity(1 << 20, file), sps, pps, self.silence.clone())?;
        self.audio_seq = None;
        info!(w = size.0, h = size.1, "recording to {}", path.display());
        self.files.push(path.clone());
        self.part = Some(Part {
            writer,
            path,
            size,
            start_us: frame.capture_us,
            last_us: frame.capture_us,
            last_at: std::time::Instant::now(),
        });
        Ok(())
    }

    fn close_part(&mut self) -> Result<()> {
        if let Some(part) = self.part.take() {
            // Now, on the agent's clock: the still picture since the last frame counts.
            let now_us = part.last_us + part.last_at.elapsed().as_micros() as u64;
            let end = now_us.saturating_sub(part.start_us) * 9 / 100;
            let seconds = part.writer.duration().max(end) / u64::from(crate::mp4::TIMESCALE);
            part.writer.finish_at(end).with_context(|| format!("closing {}", part.path.display()))?;
            info!(seconds, "recording saved: {}", part.path.display());
            // For the NuviraHub app, which reads the output (also while the window closes).
            println!("@recorded\t{}", part.path.display());
            if let Some(target) = &self.upload {
                crate::upload::spawn(target, &part.path);
            }
        }
        Ok(())
    }

    /// Closes the file being written; returns all the files of the recording.
    pub fn finish(mut self) -> Result<Vec<PathBuf>> {
        self.close_part()?;
        Ok(self.files)
    }
}

/// An Opus packet of 20 ms of silence, for the moments without sound.
fn silence_packet() -> Result<Vec<u8>> {
    let mut encoder = nuvira_audio::opus::Encoder::new()?;
    let zeros = vec![0f32; nuvira_audio::FRAME * nuvira_audio::CHANNELS];
    // The first packets of an encoder carry its start; the third is plain silence.
    let mut packet = Vec::new();
    for _ in 0..3 {
        packet = encoder.encode(&zeros)?;
    }
    Ok(packet)
}

/// The sound packets received while a recording runs, taken by the decoding thread
/// (which writes the files): (seq, packet, received at, µs).
#[derive(Default)]
pub struct AudioTap {
    on: AtomicBool,
    packets: Mutex<Vec<(u32, Vec<u8>, u64)>>,
}

impl AudioTap {
    pub fn push(&self, seq: u32, packet: &[u8]) {
        if self.on.load(Ordering::Relaxed) {
            let mut packets = self.packets.lock().expect("audio tap");
            // A few minutes at most, if nobody takes them.
            if packets.len() < 10_000 {
                packets.push((seq, packet.to_vec(), nuvira_proto::now_us()));
            }
        }
    }

    pub fn set_on(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
        if !on {
            self.packets.lock().expect("audio tap").clear();
        }
    }

    pub fn take(&self) -> Vec<(u32, Vec<u8>, u64)> {
        std::mem::take(&mut *self.packets.lock().expect("audio tap"))
    }
}

/// What the window wants recorded, read by the decoding thread (which writes the
/// files): a recording to the folder with the name, or none. Every start has its own
/// number, so that a stop and a new start between two frames are not missed.
#[derive(Default)]
pub struct Switch(Mutex<(u64, Option<(PathBuf, String)>)>, Mutex<Option<crate::upload::Target>>);

impl Switch {
    /// Where the files of the current session go besides the folder (NuviraHub Pro).
    pub fn set_upload(&self, target: Option<crate::upload::Target>) {
        *self.1.lock().expect("recording switch") = target;
    }

    pub fn upload(&self) -> Option<crate::upload::Target> {
        self.1.lock().expect("recording switch").clone()
    }

    pub fn start(&self, dir: PathBuf, stem: String) {
        let mut s = self.0.lock().expect("recording switch");
        s.0 += 1;
        s.1 = Some((dir, stem));
    }

    pub fn stop(&self) {
        self.0.lock().expect("recording switch").1 = None;
    }

    /// The recording wanted now, with its number.
    pub fn wanted(&self) -> Option<(u64, PathBuf, String)> {
        let s = self.0.lock().expect("recording switch");
        s.1.as_ref().map(|(dir, stem)| (s.0, dir.clone(), stem.clone()))
    }
}

/// `stem.mp4`, `stem (2).mp4` for the second part..., never over an existing file.
fn free_path(dir: &Path, stem: &str, part: u32) -> PathBuf {
    let name = |n: u32| match n {
        1 => format!("{stem}.mp4"),
        n => format!("{stem} ({n}).mp4"),
    };
    (part..).map(|n| dir.join(name(n))).find(|p| !p.exists()).expect("a free name")
}

/// File name of a recording: date and time, and the remote computer, e.g.
/// "NuviraHub 2026-11-07 10.15.03 PC-UFFICIO". Characters not allowed in file names
/// are replaced.
pub fn file_stem(device: &str) -> String {
    let (y, mo, d, h, mi, s) = local_time();
    let device: String = device
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .take(60)
        .collect();
    let device = device.trim().trim_end_matches('.');
    let mut stem = format!("NuviraHub {y:04}-{mo:02}-{d:02} {h:02}.{mi:02}.{s:02}");
    if !device.is_empty() {
        stem.push(' ');
        stem.push_str(device);
    }
    stem
}

/// The user's Videos folder (Movies on macOS) + NuviraHub.
pub fn default_dir() -> PathBuf {
    videos_dir().join("NuviraHub")
}

#[cfg(windows)]
fn videos_dir() -> PathBuf {
    use windows::Win32::UI::Shell::{FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    // SAFETY: the returned string is freed with CoTaskMemFree.
    unsafe {
        match SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None) {
            Ok(p) => {
                let path = PathBuf::from(p.to_string().unwrap_or_default());
                windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as _));
                path
            }
            Err(_) => std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default().join("Videos"),
        }
    }
}

#[cfg(target_os = "macos")]
fn videos_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join("Movies")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn videos_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    // XDG_VIDEOS_DIR="$HOME/Video" in ~/.config/user-dirs.dirs (localised names).
    let config =
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    std::fs::read_to_string(config.join("user-dirs.dirs"))
        .ok()
        .and_then(|text| {
            text.lines().find_map(|l| {
                let value = l.trim().strip_prefix("XDG_VIDEOS_DIR=")?.trim_matches('"');
                let path = match value.strip_prefix("$HOME") {
                    Some(rest) => home.join(rest.trim_start_matches('/')),
                    None => PathBuf::from(value),
                };
                (path != home).then_some(path)
            })
        })
        .unwrap_or_else(|| home.join("Videos"))
}

/// Year, month, day, hour, minute, second of the local clock.
#[cfg(windows)]
fn local_time() -> (u16, u16, u16, u16, u16, u16) {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    (t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

#[cfg(unix)]
fn local_time() -> (u16, u16, u16, u16, u16, u16) {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    (
        (tm.tm_year + 1900) as u16,
        (tm.tm_mon + 1) as u16,
        tm.tm_mday as u16,
        tm.tm_hour as u16,
        tm.tm_min as u16,
        tm.tm_sec as u16,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(w: u32, h: u32, n: usize, first_seq: u64, start_us: u64) -> Vec<VideoFrame> {
        let mut encoder = nuvira_codec::OpenH264Encoder::new(Default::default()).unwrap();
        let mut bgra = vec![90u8; (w * h * 4) as usize];
        (0..n)
            .filter_map(|i| {
                bgra[i * 4..i * 4 + 64].fill((i as u8).wrapping_mul(3));
                let image =
                    nuvira_codec::BgraImage { width: w, height: h, stride: (w * 4) as usize, data: &bgra };
                nuvira_codec::VideoEncoder::encode(&mut encoder, &image, i == 0).unwrap()
            })
            .enumerate()
            .map(|(i, f)| VideoFrame {
                seq: first_seq + i as u64,
                capture_us: start_us + i as u64 * 33_333,
                keyframe: f.keyframe,
                data: f.data,
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn parts_follow_the_resolution() {
        let dir = std::env::temp_dir().join(format!("nuvira-rec-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut rec = Recorder::new(dir.clone(), "test".into(), None, false);
        // It starts at a keyframe: the frames before are skipped.
        let mut first = frames(320, 240, 20, 1, 0);
        first.remove(0);
        // 2 s, then another size.
        let mut later = frames(320, 240, 60, 100, 1_000_000);
        let other = frames(640, 360, 20, 160, 3_100_000);
        for f in first.iter().chain(&later).chain(&other) {
            rec.push(f, f.capture_us + 20_000).unwrap();
        }
        // A lost frame (datagrams): nothing until the next keyframe.
        later = frames(640, 360, 5, 250, 4_000_000);
        for f in &later[1..] {
            rec.push(f, f.capture_us + 20_000).unwrap();
        }
        let files = rec.finish().unwrap();
        assert_eq!(files, [dir.join("test.mp4"), dir.join("test (2).mp4")]);
        for f in &files {
            let data = std::fs::read(f).unwrap();
            assert!(data.windows(4).any(|w| w == b"mfra"), "{} not closed", f.display());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn first_moment_at_another_size_is_dropped() {
        let dir = std::env::temp_dir().join(format!("nuvira-rec-test-first-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut rec = Recorder::new(dir.clone(), "test".into(), None, false);
        // 0.3 s at the window's size, then the full size.
        for f in frames(320, 240, 10, 1, 0).iter().chain(&frames(640, 480, 20, 11, 333_330)) {
            rec.push(f, f.capture_us + 20_000).unwrap();
        }
        assert_eq!(rec.finish().unwrap(), [dir.join("test.mp4")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sound_goes_into_a_second_track() {
        let dir = std::env::temp_dir().join(format!("nuvira-rec-test-audio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut rec = Recorder::new(dir.clone(), "test".into(), None, true);
        // 3 s of video (the agent's clock 5 s behind this one, 30 ms on the way) and
        // a 440 Hz tone, with 0.5 s of silence not sent from 1 s (packets 50 to 74
        // missing) and the arrival a bit irregular.
        let video = frames(320, 240, 90, 1, 1_000_000);
        let mut encoder = nuvira_audio::opus::Encoder::new().unwrap();
        let mut packets = Vec::new();
        for i in 0..150u64 {
            let pcm: Vec<f32> = (0..nuvira_audio::FRAME)
                .flat_map(|n| {
                    let t = (i as usize * nuvira_audio::FRAME + n) as f32 / 48_000.0;
                    let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.3;
                    [v, v]
                })
                .collect();
            let packet = encoder.encode(&pcm).unwrap();
            if !(50..75).contains(&i) {
                packets.push((i, packet));
            }
        }
        let offset = 5_000_000 + 30_000;
        let mut seq = 0u32;
        let mut audio = packets.into_iter().peekable();
        for f in &video {
            rec.push(f, f.capture_us + offset).unwrap();
            while let Some((i, packet)) = audio.next_if(|(i, _)| 1_000_000 + i * 20_000 <= f.capture_us) {
                let jitter = (i % 3) * 7_000;
                rec.push_audio(seq, &packet, 1_000_000 + i * 20_000 + offset + 20_000 + jitter).unwrap();
                seq += 1;
            }
        }
        let files = rec.finish().unwrap();
        assert_eq!(files.len(), 1);
        let data = std::fs::read(&files[0]).unwrap();
        let count = |kind: &[u8]| data.windows(4).filter(|x| *x == kind).count();
        assert_eq!(count(b"trak"), 2);
        assert_eq!(count(b"dOps"), 1);
        // The audio fragments: 1 s each.
        assert!(count(b"moof") >= 5, "{} fragments", count(b"moof"));
        if let Some(keep) = std::env::var_os("NUVIRA_KEEP_TEST_MP4") {
            std::fs::copy(&files[0], keep).unwrap();
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stem_is_a_valid_file_name() {
        let stem = file_stem("PC: \"Ufficio\"/2.");
        assert!(stem.starts_with("NuviraHub 20"));
        assert!(stem.ends_with("PC_ _Ufficio__2"), "{stem}");
    }
}
