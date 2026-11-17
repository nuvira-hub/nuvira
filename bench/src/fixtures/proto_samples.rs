//! Protocol samples (plan 5.3), built in code from fixed values.
//!
//! The manifest stores the SHA-256 of `framing::encode(msg)` of each sample: a
//! different encoding is a wire-compatibility break and fails the run regardless
//! of speed (gate `exact`). Every literal sets only fields that exist at the tag
//! and ends with `..Default::default()`, so fields added later (unset, hence not
//! encoded) keep both the code compiling and the bytes identical. Values that the
//! real viewer takes from the build (protocol version, app version) are fixed
//! literals here for the same reason.

// Struct updates kept on purpose: fields added later stay unset.
#![allow(clippy::needless_update)]

use nuvira_proto::framing;
use nuvira_proto::v1::{
    self, ControlMessage, CursorPosition, CursorShape, Feedback, FileChunk, FsDownload, FsEntry, FsRequest,
    FsResponse, FsUploadOffset, Hello, InputEvent, Key, MouseButton, MouseMove, MouseWheel, Ping, Pong, Text,
    VideoFrame, ViewSize, control_message::Msg, fs_request::Op, input_event::Event,
};

use crate::rng::Xoshiro256;

const PROTO_SEED: u64 = 0x5052_4f54_4f53_4d50; // "PROTOSMP"

/// `PROTOCOL_VERSION` at the tag (ProtocolVersion::ConfirmationPending = 2).
pub const HELLO_PROTOCOL_VERSION: u32 = 2;

/// A sample, typed by the channel it travels on.
#[derive(Clone, Debug, PartialEq)]
pub enum Sample {
    /// `video` uni stream (or a datagram payload, `prost::Message::encode_to_vec`).
    Video(VideoFrame),
    /// `input` uni stream.
    Input(InputEvent),
    /// bidi `control` stream.
    Control(ControlMessage),
    /// file uni streams.
    Chunk(FileChunk),
}

impl Sample {
    /// Length-delimited encoding, as on the streams.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Sample::Video(m) => framing::encode(m),
            Sample::Input(m) => framing::encode(m),
            Sample::Control(m) => framing::encode(m),
            Sample::Chunk(m) => framing::encode(m),
        }
    }

    /// Plain encoding without length (datagram path).
    pub fn encode_raw(&self) -> Vec<u8> {
        use prost::Message;
        match self {
            Sample::Video(m) => m.encode_to_vec(),
            Sample::Input(m) => m.encode_to_vec(),
            Sample::Control(m) => m.encode_to_vec(),
            Sample::Chunk(m) => m.encode_to_vec(),
        }
    }

    pub fn channel(&self) -> &'static str {
        match self {
            Sample::Video(_) => "video",
            Sample::Input(_) => "input",
            Sample::Control(_) => "control",
            Sample::Chunk(_) => "file",
        }
    }

    /// Decodes `bytes` (length-delimited) as the same message type.
    pub fn decode_same(&self, bytes: &[u8]) -> anyhow::Result<Sample> {
        let mut d = framing::Decoder::default();
        d.push(bytes);
        let none = || anyhow::anyhow!("incomplete message");
        Ok(match self {
            Sample::Video(_) => Sample::Video(d.next_message()?.ok_or_else(none)?),
            Sample::Input(_) => Sample::Input(d.next_message()?.ok_or_else(none)?),
            Sample::Control(_) => Sample::Control(d.next_message()?.ok_or_else(none)?),
            Sample::Chunk(_) => Sample::Chunk(d.next_message()?.ok_or_else(none)?),
        })
    }
}

fn control(msg: Msg) -> Sample {
    Sample::Control(ControlMessage { msg: Some(msg), ..Default::default() })
}

fn input(event: Event) -> Sample {
    Sample::Input(InputEvent { event: Some(event), ..Default::default() })
}

/// Video payload sizes: 2 KiB, 50 KiB, 500 KiB, 2 MiB.
pub const VIDEO_SIZES: [(&str, usize); 4] =
    [("2k", 2 << 10), ("50k", 50 << 10), ("500k", 500 << 10), ("2m", 2 << 20)];

/// Every sample, in a fixed order, with a unique name.
pub fn samples() -> Vec<(&'static str, Sample)> {
    let mut out: Vec<(&'static str, Sample)> = Vec::new();
    const VIDEO_NAMES: [[&str; 2]; 4] = [
        ["video/2k/key", "video/2k/delta"],
        ["video/50k/key", "video/50k/delta"],
        ["video/500k/key", "video/500k/delta"],
        ["video/2m/key", "video/2m/delta"],
    ];
    for (i, (_, size)) in VIDEO_SIZES.iter().enumerate() {
        for (j, keyframe) in [true, false].into_iter().enumerate() {
            let seq = 1000 + (i * 2 + j) as u64;
            let mut rng = Xoshiro256::new(PROTO_SEED ^ seq);
            out.push((
                VIDEO_NAMES[i][j],
                Sample::Video(VideoFrame {
                    seq,
                    capture_us: 1_763_300_000_000_000 + seq * 16_667,
                    encode_us: 4_321,
                    keyframe,
                    data: rng.bytes(*size),
                    encoded_us: 1_763_300_000_004_321 + seq * 16_667,
                    ..Default::default()
                }),
            ));
        }
    }
    out.push((
        "input/mouse_move",
        input(Event::MouseMove(MouseMove { x: 0.25, y: 0.75, ..Default::default() })),
    ));
    out.push((
        "input/mouse_button",
        input(Event::MouseButton(MouseButton {
            button: v1::Button::Left as i32,
            down: true,
            ..Default::default()
        })),
    ));
    out.push((
        "input/mouse_wheel",
        input(Event::MouseWheel(MouseWheel { dx: 0, dy: -120, ..Default::default() })),
    ));
    out.push(("input/key", input(Event::Key(Key { code: "KeyA".into(), down: true, ..Default::default() }))));
    out.push(("input/text", input(Event::Text(Text { text: "hello".into(), ..Default::default() }))));
    // Hello as the viewer sends it (viewer/src/net.rs at the tag), desktop session.
    out.push((
        "control/hello",
        control(Msg::Hello(Hello {
            protocol_version: HELLO_PROTOCOL_VERSION,
            device_name: "BENCH-PC".into(),
            os: "windows".into(),
            app_version: "0.28.0".into(),
            video_datagrams: false,
            terminal_only: false,
            print: true,
            recording: true,
            audio: false,
            cursor: true,
            cursor_cache: true,
            clipboard_files: true,
            handover: true,
            switch_sides: false,
            two_factor: true,
            microphone: false,
            ..Default::default()
        })),
    ));
    out.push((
        "control/feedback",
        control(Msg::Feedback(Feedback {
            latency_ms_p50: 23,
            latency_ms_min: 17,
            frames: 15,
            window_ms: 250,
            transit_ms_p50: Some(4),
            transit_ms_min: Some(2),
            shards: Some(180),
            shards_lost: Some(3),
            frames_lost: Some(0),
            ..Default::default()
        })),
    ));
    out.push((
        "control/ping",
        control(Msg::Ping(Ping { sent_us: 1_763_300_000_123_456, ..Default::default() })),
    ));
    out.push((
        "control/pong",
        control(Msg::Pong(Pong {
            ping_sent_us: 1_763_300_000_123_456,
            agent_us: 1_763_300_000_125_000,
            ..Default::default()
        })),
    ));
    out.push((
        "control/cursor_position",
        control(Msg::CursorPosition(CursorPosition {
            x: 960,
            y: 540,
            visible: true,
            serial: 7,
            ..Default::default()
        })),
    ));
    out.push((
        "control/cursor_shape",
        control(Msg::CursorShape(CursorShape {
            serial: 7,
            width: 64,
            height: 64,
            hot_x: 1,
            hot_y: 1,
            png: cursor_png(),
            ..Default::default()
        })),
    ));
    out.push((
        "control/view_size",
        control(Msg::ViewSize(ViewSize { width: 1600, height: 900, ..Default::default() })),
    ));
    out.push((
        "control/fs_request_list",
        control(Msg::FsRequest(FsRequest {
            id: 1,
            op: Some(Op::List("/home/bench/Documents".into())),
            ..Default::default()
        })),
    ));
    out.push((
        "control/fs_request_download",
        control(Msg::FsRequest(FsRequest {
            id: 2,
            op: Some(Op::Download(FsDownload {
                path: "/home/bench/Documents/report.pdf".into(),
                offset: 1 << 20,
                ..Default::default()
            })),
            ..Default::default()
        })),
    ));
    out.push((
        "control/fs_request_upload_offset",
        control(Msg::FsRequest(FsRequest {
            id: 3,
            op: Some(Op::UploadOffset(FsUploadOffset {
                dir: "/home/bench/Downloads".into(),
                name: "big.iso".into(),
                ..Default::default()
            })),
            ..Default::default()
        })),
    ));
    let mut rng = Xoshiro256::new(PROTO_SEED ^ 0xf5);
    let entries = (0..1000u32)
        .map(|i| FsEntry {
            name: format!("file-{i:04}.dat"),
            path: format!("/home/bench/Documents/file-{i:04}.dat"),
            dir: i % 10 == 0,
            size: if i % 10 == 0 { 0 } else { rng.below(1 << 32) },
            modified: 1_763_300_000 - i as i64 * 3600,
            ..Default::default()
        })
        .collect();
    out.push((
        "control/fs_response_1000",
        control(Msg::FsResponse(FsResponse {
            id: 1,
            error: String::new(),
            path: "/home/bench/Documents".into(),
            entries,
            offset: 0,
            ..Default::default()
        })),
    ));
    out.push((
        "file/chunk_256k",
        Sample::Chunk(FileChunk {
            name: "bench.bin".into(),
            size: 1 << 30,
            data: Xoshiro256::new(PROTO_SEED ^ 0xc4).bytes(256 << 10),
            dir: "/home/bench/Downloads".into(),
            offset: 0,
            transfer_id: 7,
            ..Default::default()
        }),
    ));
    out
}

/// Every sample's length-delimited encoding.
pub fn all() -> Vec<(&'static str, Vec<u8>)> {
    samples().into_iter().map(|(n, s)| (n, s.encode())).collect()
}

// --- a 64x64 RGBA PNG written by hand (stored deflate), so its bytes never depend on
// a PNG library version.

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// Uncompressed PNG of an RGBA image (rows of `width * 4` bytes).
pub fn png_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((width as usize * 4 + 1) * height as usize);
    for row in rgba.chunks_exact(width as usize * 4) {
        raw.push(0); // filter: none
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65535).peekable();
    if blocks.peek().is_none() {
        z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(b) = blocks.next() {
        z.push(u8::from(blocks.peek().is_none()));
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut chunk = |kind: &[u8; 4], data: &[u8]| {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend_from_slice(data);
        png.extend_from_slice(&c);
        png.extend_from_slice(&crc32(&c).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8 bit, RGBA, deflate, adaptive, no interlace
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    png
}

/// A 64x64 arrow-like pointer: black outline, white fill, transparent elsewhere.
fn cursor_png() -> Vec<u8> {
    let mut rgba = vec![0u8; 64 * 64 * 4];
    for y in 0..64usize {
        for x in 0..64usize {
            let inside = x <= y / 2 + 1 && y < 48;
            let edge = inside && (x == 0 || x == y / 2 + 1 || y == 47);
            let p = &mut rgba[(y * 64 + x) * 4..][..4];
            if edge {
                p.copy_from_slice(&[0, 0, 0, 255]);
            } else if inside {
                p.copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    png_rgba(64, 64, &rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_unique_and_roundtrip() {
        let s = samples();
        let names: std::collections::HashSet<_> = s.iter().map(|(n, _)| *n).collect();
        assert_eq!(names.len(), s.len());
        for (name, sample) in &s {
            let bytes = sample.encode();
            assert_eq!(&sample.decode_same(&bytes).unwrap(), sample, "{name}");
        }
        assert_eq!(all(), all());
    }

    #[test]
    fn crc_and_adler_known_values() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
        let png = cursor_png();
        assert_eq!(&png[1..4], b"PNG");
    }
}
