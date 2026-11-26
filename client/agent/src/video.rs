//! The video of a session: capture and encoding of a screen, restarted on another
//! one when the viewer changes it, the messages that describe the screens, and the
//! screens and wallpaper put back at the end.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use anyhow::{Context, Result};
use nuvira_codec::{EncoderPlan, EncoderSettings};
use nuvira_proto::v1::{
    Codec, ControlMessage, DisplayInfo, Displays, Quality, VideoFormat, VideoFrame, control_message::Msg,
};
use tokio::sync::mpsc;
use tracing::warn;

use crate::pipeline::{Controls, Pipeline};
use crate::{AgentConfig, MIN_BITRATE, control, session_stats};

/// Capture and encoding of a screen, restartable on another one during the session.
pub(crate) struct VideoSource {
    pub(crate) config: AgentConfig,
    /// Copy of the sender: keeps the video channel open while the screen changes.
    pub(crate) frames: mpsc::Sender<VideoFrame>,
    pub(crate) keyframe: Arc<AtomicBool>,
    pub(crate) bitrate: Arc<AtomicU32>,
    pub(crate) fps: Arc<AtomicU32>,
    pub(crate) view: Arc<AtomicU64>,
    pub(crate) stats: Option<Arc<session_stats::AgentStats>>,
}

impl VideoSource {
    pub(crate) async fn start(&self, display: Option<usize>) -> Result<(nuvira_capture::Display, Pipeline)> {
        let mut cap_config = self.config.clone();
        cap_config.display = display;
        // The encoder starts from the current bitrate of the adaptive control.
        let settings = EncoderSettings {
            bitrate_bps: self.bitrate.load(Ordering::Relaxed),
            max_fps: self.fps.load(Ordering::Relaxed),
            ..Default::default()
        };
        let plan = self.config.plan;
        // A new stream must start with a keyframe.
        self.keyframe.store(true, Ordering::Relaxed);
        let (frames, keyframe, bitrate, fps, view, stats) = (
            self.frames.clone(),
            self.keyframe.clone(),
            self.bitrate.clone(),
            self.fps.clone(),
            self.view.clone(),
            self.stats.clone(),
        );
        tokio::task::spawn_blocking(move || {
            Pipeline::start(
                move || cap_config.open_capturer(),
                move |gpu| nuvira_codec::create_encoder(plan, settings, gpu),
                plan == EncoderPlan::HardwareGpu,
                frames,
                Controls { max_fps: fps, keyframe, bitrate, view, stats },
            )
        })
        .await?
        .context("starting capture and encoding")
    }
}

pub(crate) fn video_format(display: &nuvira_capture::Display) -> ControlMessage {
    control(Msg::VideoFormat(VideoFormat {
        codec: Codec::H264 as i32,
        width: display.width,
        height: display.height,
    }))
}

/// List of screens for the viewer (none with the tests' synthetic image).
pub(crate) fn displays_message(config: &AgentConfig, current: &nuvira_capture::Display) -> Option<ControlMessage> {
    if config.synthetic.is_some() {
        return None;
    }
    let list = nuvira_capture::displays().ok()?;
    let all_available = list.len() > 1;
    let displays = list
        .into_iter()
        .map(|d| DisplayInfo {
            index: d.index as u32,
            name: d.name,
            width: d.width,
            height: d.height,
            primary: d.primary,
        })
        .collect();
    Some(control(Msg::Displays(Displays { displays, current: current.index as u32, all_available })))
}

pub(crate) fn screen_rect(display: &nuvira_capture::Display) -> nuvira_input::ScreenRect {
    nuvira_input::ScreenRect {
        left: display.left,
        top: display.top,
        width: display.width,
        height: display.height,
    }
}

/// Bitrate cap (bit/s) and fps for the quality chosen by the viewer.
pub(crate) fn quality_limits(config: &AgentConfig, quality: Quality) -> (u32, u32) {
    let (bps, fps) = (config.bitrate_kbps * 1000, config.fps);
    match quality {
        Quality::Sharp => (bps * 2, fps.min(30)),
        Quality::Fast => ((bps / 2).max(MIN_BITRATE), fps),
        Quality::Balanced | Quality::Unspecified => (bps, fps),
    }
}

/// Sessions, told apart by the screens they change.
pub(crate) static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// Puts back the screens a session changed, however it ends.
pub(crate) struct RestoreScreens(pub(crate) u64);

impl Drop for RestoreScreens {
    fn drop(&mut self) {
        nuvira_capture::resolution::restore(self.0, None);
    }
}

pub(crate) fn recover_screens(dir: &std::path::Path) {
    nuvira_capture::resolution::recover(&dir.join("resolution.txt"));
    nuvira_capture::wallpaper::recover(&dir.join(WALLPAPER_STATE));
}

/// What the wallpaper was, while a session keeps it hidden (`SetWallpaper`).
pub(crate) const WALLPAPER_STATE: &str = "wallpaper.txt";

/// Puts the wallpaper back at the end of the session, however it ends.
pub(crate) struct RestoreWallpaper(pub(crate) Option<PathBuf>);

impl Drop for RestoreWallpaper {
    fn drop(&mut self) {
        if let Some(state) = &self.0 {
            nuvira_capture::wallpaper::restore(state);
        }
    }
}

/// Stops the capture and opens screen `index` (`fallback` if it cannot be opened):
/// after a change of screen or of its resolution.
pub(crate) async fn reopen(
    source: &VideoSource,
    pipeline: &std::sync::Mutex<Option<Pipeline>>,
    index: usize,
    fallback: usize,
) -> Result<nuvira_capture::Display> {
    // First the current capture is stopped, then the new one is opened.
    let old = pipeline.lock().expect("pipeline").take();
    let _ = tokio::task::spawn_blocking(move || drop(old)).await;
    let (display, started) = match source.start(Some(index)).await {
        Ok(started) => started,
        Err(e) if index != fallback => {
            warn!("screen {index} not available: {e:#}; staying on the previous one");
            source.start(Some(fallback)).await?
        }
        Err(e) => return Err(e),
    };
    *pipeline.lock().expect("pipeline") = Some(started);
    Ok(display)
}
