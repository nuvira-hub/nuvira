//! Video received from the agent: on its stream or as datagrams with error correction,
//! decoded on a thread of its own (also writing the recordings), and the pointer shapes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nuvira_proto::v1::VideoFrame;
use tokio::sync::mpsc;
use tracing::{info, warn};
use winit::event_loop::EventLoopProxy;

use super::{DecodedFrame, FrameSlot, NetEvent};

/// Losses of the video as datagrams since the last feedback.
#[derive(Default)]
pub(super) struct VideoLoss {
    pub(super) shards: AtomicU64,
    pub(super) shards_lost: AtomicU64,
    pub(super) frames_lost: AtomicU64,
}

impl VideoLoss {
    pub(super) fn fill(&self, feedback: &mut nuvira_proto::v1::Feedback) {
        let take = |n: &AtomicU64| Some(n.swap(0, Ordering::Relaxed).min(u32::MAX as u64) as u32);
        feedback.shards = take(&self.shards);
        feedback.shards_lost = take(&self.shards_lost);
        feedback.frames_lost = take(&self.frames_lost);
    }
}

/// Hands a frame received to decoding; `false` when decoding ended. With the statistics
/// log on, also counts it and the time the full queue held the network task.
pub(super) fn to_decoder(
    frames: &std::sync::mpsc::SyncSender<(VideoFrame, u64)>,
    frame: VideoFrame,
    stats: Option<&crate::session_stats::ViewerStats>,
) -> bool {
    let received_us = nuvira_proto::now_us();
    let Some(s) = stats else { return frames.send((frame, received_us)).is_ok() };
    s.frames_received.fetch_add(1, Ordering::Relaxed);
    s.bytes_received.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
    let t = Instant::now();
    let sent = frames.send((frame, received_us)).is_ok();
    s.decode_queue_wait_us.fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
    sent
}

/// Video as QUIC datagrams with error correction (`nuvira_transport::fec`, prototype):
/// frames go to decoding as they are rebuilt. After a lost frame the next ones cannot
/// be decoded: they are dropped until a keyframe, asked for at most once a second.
pub(super) async fn receive_datagram_video(
    conn: &nuvira_transport::Connection,
    frames: &std::sync::mpsc::SyncSender<(VideoFrame, u64)>,
    request_keyframe: &mpsc::UnboundedSender<()>,
    loss: &VideoLoss,
    audio: Option<(&nuvira_audio::player::Player, &crate::recorder::AudioTap)>,
    stats: Option<&crate::session_stats::ViewerStats>,
) -> Result<()> {
    use prost::Message;
    let mut rx = nuvira_transport::fec::Receiver::default();
    let mut last = 0u64;
    let mut waiting_key = false;
    let mut asked: Option<Instant> = None;
    info!("receiving the video as datagrams");
    loop {
        let packet = match conn.read_datagram().await {
            Ok(p) => {
                tracing::trace!(bytes = p.len(), "video datagram");
                p
            }
            Err(quinn::ConnectionError::ApplicationClosed(_) | quinn::ConnectionError::LocallyClosed) => {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        if nuvira_audio::packet::is_audio(&packet) {
            if let Some((player, tap)) = audio
                && let Ok((seq, data)) = nuvira_audio::packet::decode(&packet)
            {
                player.push(seq, data);
                tap.push(seq, data);
            }
            continue;
        }
        let pushed = rx.push(&packet);
        let (shards, lost) = rx.take_loss();
        loss.shards.fetch_add(shards, Ordering::Relaxed);
        loss.shards_lost.fetch_add(lost, Ordering::Relaxed);
        if let Some(s) = stats {
            s.shards.fetch_add(shards, Ordering::Relaxed);
            s.shards_lost.fetch_add(lost, Ordering::Relaxed);
            s.frames_rebuilt.fetch_add(rx.take_rebuilt(), Ordering::Relaxed);
        }
        let (seq, bytes) = match pushed {
            Ok(Some(done)) => done,
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!("video datagram dropped: {e:#}");
                continue;
            }
        };
        let frame = VideoFrame::decode(bytes.as_slice()).context("video frame")?;
        if seq != last + 1 {
            tracing::debug!(lost = seq - last - 1, "video frames lost");
            loss.frames_lost.fetch_add(seq - last - 1, Ordering::Relaxed);
            if let Some(s) = stats {
                s.frames_lost.fetch_add(seq - last - 1, Ordering::Relaxed);
            }
            waiting_key = true;
        }
        last = seq;
        if waiting_key {
            if frame.keyframe {
                waiting_key = false;
                asked = None;
            } else {
                if asked.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
                    asked = Some(Instant::now());
                    let _ = request_keyframe.send(());
                }
                continue;
            }
        }
        if !to_decoder(frames, frame, stats) {
            return Ok(());
        }
    }
}

pub(super) fn decode_loop(
    frames: std::sync::mpsc::Receiver<(VideoFrame, u64)>,
    proxy: EventLoopProxy<NetEvent>,
    slot: Arc<FrameSlot>,
    request_keyframe: mpsc::UnboundedSender<()>,
    recording: &crate::recorder::Switch,
    audio: Option<Arc<crate::recorder::AudioTap>>,
    stats: Option<Arc<crate::session_stats::ViewerStats>>,
) {
    let mut decoder = match nuvira_codec::best_decoder() {
        Ok(d) => d,
        Err(e) => {
            let _ = proxy.send_event(NetEvent::Disconnected {
                reason: format!("decoder not available: {e:#}"),
                retry: false,
            });
            return;
        }
    };
    info!("decoder: {}", decoder.name());
    if let Some(s) = &stats {
        s.decoder(decoder.name());
    }
    let mut waiting_key = false;
    // The recording being written, with its number (see recorder::Switch).
    let mut recorder: Option<(u64, crate::recorder::Recorder)> = None;
    let close = |recorder: Option<(u64, crate::recorder::Recorder)>, error: Option<String>| {
        if let Some((_, r)) = recorder {
            let (files, error) = match r.finish() {
                Ok(files) => (files, error),
                Err(e) => (Vec::new(), Some(error.unwrap_or_else(|| format!("{e:#}")))),
            };
            let _ = proxy.send_event(NetEvent::RecordingSaved { files, error });
        }
    };
    loop {
        // Also without frames (still screen), so that a stop closes the file at once.
        let next = frames.recv_timeout(Duration::from_millis(200));
        let wanted = recording.wanted();
        if recorder.as_ref().map(|r| r.0) != wanted.as_ref().map(|w| w.0) {
            close(recorder.take(), None);
            if let Some((n, dir, stem)) = wanted {
                let r = crate::recorder::Recorder::new(dir, stem, recording.upload(), audio.is_some());
                recorder = Some((n, r));
                // It starts at a keyframe.
                let _ = request_keyframe.send(());
            }
            if let Some(tap) = &audio {
                tap.set_on(recorder.is_some());
            }
        }
        let frame = match next {
            Ok(f) => Some(f),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let Some((_, r)) = recorder.as_mut() {
            let mut result = frame.as_ref().map_or(Ok(()), |(f, received_us)| r.push(f, *received_us));
            if let Some(tap) = &audio {
                for (seq, packet, received_us) in tap.take() {
                    if result.is_ok() {
                        result = r.push_audio(seq, &packet, received_us);
                    }
                }
            }
            if let Err(e) = result {
                warn!("recording stopped: {e:#}");
                recording.stop();
                close(recorder.take(), Some(format!("{e:#}")));
                if let Some(tap) = &audio {
                    tap.set_on(false);
                }
            }
        }
        let Some((frame, received_us)) = frame else { continue };
        // After an error frames are dropped until the next keyframe.
        if waiting_key && !frame.keyframe {
            continue;
        }
        let t = Instant::now();
        tracing::trace!(seq = frame.seq, bytes = frame.data.len(), "frame received");
        match decoder.decode(&frame.data) {
            Ok(Some(yuv)) => {
                waiting_key = false;
                let decoded = DecodedFrame {
                    yuv: yuv.clone(),
                    capture_us: frame.capture_us,
                    encode_us: frame.encode_us,
                    // Older agents: estimated from capture and encoding times.
                    encoded_us: match frame.encoded_us {
                        0 => frame.capture_us + frame.encode_us as u64,
                        t => t,
                    },
                    received_us,
                    decode_us: t.elapsed().as_micros() as u32,
                    bytes: frame.data.len(),
                };
                if let Some(s) = &stats {
                    s.frames_decoded.fetch_add(1, Ordering::Relaxed);
                    s.decode.record_us(u64::from(decoded.decode_us));
                    s.size(yuv.width, yuv.height);
                }
                if slot.put(Box::new(decoded)) && proxy.send_event(NetEvent::FrameReady).is_err() {
                    break;
                }
            }
            Ok(None) => {}
            Err(e) => {
                warn!("frame {} not decodable: {e:#}; requesting a keyframe", frame.seq);
                // A failing hardware decoder is not trusted again: OpenH264 from the keyframe on.
                if decoder.name() != "openh264" {
                    match nuvira_codec::software_decoder() {
                        Ok(d) => {
                            warn!("hardware decoder dropped: {}", d.name());
                            decoder = d;
                        }
                        Err(e) => warn!("OpenH264 not available: {e:#}"),
                    }
                }
                waiting_key = true;
                let _ = request_keyframe.send(());
            }
        }
    }
    // End of the session: the recording is closed as it is.
    close(recorder.take(), None);
}

/// A pointer shape sent as PNG (`CursorShape.png`): RGBA pixels of the size
/// announced, or `None`.
pub(super) fn cursor_png(data: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().ok()?;
    let info = reader.info();
    if (info.width, info.height) != (width, height) || width > 256 || height > 256 {
        return None;
    }
    let mut buf = vec![0; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut buf).ok()?;
    if frame.color_type != png::ColorType::Rgba || frame.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(frame.buffer_size());
    Some(buf)
}
