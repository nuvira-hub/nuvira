//! Deterministic fixtures (plan section 5) and their manifest.
//!
//! Fixtures are generated, not stored: integer-only generators with fixed seeds,
//! versioned by [`GENERATOR_VERSION`]. `bench/fixtures/manifest.json` (embedded in
//! the binary at build time, so a copied exe needs nothing else) holds the SHA-256
//! of every scene sequence, protocol sample, audio buffer and file;
//! `nuvira-bench fixtures verify` regenerates and compares (exit 2 on mismatch).
//!
//! Interfaces for the cases:
//! - [`Scene`]`::{Static, Terminal, Scroll, Video, Drag, Resize}`,
//!   [`Scene::frames`]`(w, h, n)` (iterator of owned [`Frame`]s) or
//!   [`scenes::SceneGen`] (`render(i, &mut Frame)` reusing one buffer);
//! - [`files::make`]`(size, dir) -> (PathBuf, [u8; 32])`;
//! - [`proto_samples::all`]`() -> Vec<(&'static str, Vec<u8>)>` (and typed
//!   [`proto_samples::samples`]);
//! - [`audio::pcm_stereo`], [`synthetic`] (reference of the agent's synthetic source).

pub mod audio;
pub mod files;
pub mod font;
pub mod proto_samples;
pub mod scenes;
pub mod synthetic;

use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::registry::Tier;

/// Bumped whenever a generator's output changes on purpose; then the manifest is
/// regenerated and committed together with the change.
pub const GENERATOR_VERSION: u32 = 1;

/// Manifest committed in `bench/fixtures/manifest.json`, embedded at build time.
pub const EMBEDDED_MANIFEST: &str = include_str!("../../fixtures/manifest.json");

/// Fixture resolutions.
pub const RESOLUTIONS: [(u32, u32); 4] = [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)];

/// Sequence lengths per tier (5 s / 2 s at 60 fps).
pub const FRAMES_FULL: u32 = 300;
pub const FRAMES_QUICK: u32 = 120;

pub fn frames_for(tier: Tier) -> u32 {
    match tier {
        Tier::Quick => FRAMES_QUICK,
        Tier::Full => FRAMES_FULL,
    }
}

/// Video scenes S1-S6.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scene {
    Static,
    Terminal,
    Scroll,
    Video,
    Drag,
    Resize,
}

impl Scene {
    pub const ALL: [Scene; 6] =
        [Scene::Static, Scene::Terminal, Scene::Scroll, Scene::Video, Scene::Drag, Scene::Resize];

    /// Plan ID: "S1".."S6".
    pub fn id(self) -> &'static str {
        match self {
            Scene::Static => "S1",
            Scene::Terminal => "S2",
            Scene::Scroll => "S3",
            Scene::Video => "S4",
            Scene::Drag => "S5",
            Scene::Resize => "S6",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Scene::Static => "static",
            Scene::Terminal => "terminal",
            Scene::Scroll => "scroll",
            Scene::Video => "video",
            Scene::Drag => "drag",
            Scene::Resize => "resize",
        }
    }

    /// From "S4" or "video".
    pub fn parse(s: &str) -> Option<Scene> {
        Scene::ALL.into_iter().find(|c| c.id().eq_ignore_ascii_case(s) || c.name() == s)
    }

    /// The generator (precomputes the scene's canvases; can take some 100 ms at 4K).
    pub fn generator(self, width: u32, height: u32, frames: u32) -> scenes::SceneGen {
        scenes::SceneGen::new(self, width, height, frames)
    }

    /// `n` frames at `width x height` (S6: that is the base size, see
    /// [`scenes::resize_sizes`]).
    pub fn frames(self, width: u32, height: u32, n: u32) -> Frames {
        Frames { generator: self.generator(width, height, n), next: 0, n }
    }
}

/// One BGRA frame, stride = width * 4.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub index: u32,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Frame {
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    /// As the production encoders and converters take it.
    pub fn image(&self) -> nuvira_codec::BgraImage<'_> {
        nuvira_codec::BgraImage {
            width: self.width,
            height: self.height,
            stride: self.stride(),
            data: &self.data,
        }
    }
}

/// Iterator of [`Scene::frames`].
pub struct Frames {
    pub generator: scenes::SceneGen,
    next: u32,
    n: u32,
}

impl Iterator for Frames {
    type Item = Frame;
    fn next(&mut self) -> Option<Frame> {
        if self.next >= self.n {
            return None;
        }
        let f = self.generator.frame(self.next);
        self.next += 1;
        Some(f)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = (self.n - self.next) as usize;
        (left, Some(left))
    }
}

/// Mean milliseconds to produce one frame (generator self-test, plan 5.1: must stay
/// below 25% of the frame interval or the macro case uses a pre-generated ring).
pub fn production_ms(scene: Scene, width: u32, height: u32, samples: u32) -> f64 {
    let mut g = scene.generator(width, height, samples.max(1));
    let mut f = Frame::default();
    g.render(0, &mut f);
    let t = Instant::now();
    for i in 1..=samples.max(1) {
        g.render(i, &mut f);
    }
    t.elapsed().as_secs_f64() * 1e3 / samples.max(1) as f64
}

/// True when the generator is fast enough for real-time pacing at `fps`.
pub fn fast_enough(scene: Scene, width: u32, height: u32, fps: u32) -> bool {
    production_ms(scene, width, height, 10) < 0.25 * 1000.0 / fps.max(1) as f64
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// SHA-256 of `frames` (several lengths) of a scene, in one pass: the shorter
/// sequences are prefixes of the longer one.
pub fn scene_digests(scene: Scene, width: u32, height: u32, lengths: &[u32]) -> Vec<(u32, u64, [u8; 32])> {
    let max = lengths.iter().copied().max().unwrap_or(0);
    let mut g = scene.generator(width, height, max);
    let mut h = Sha256::new();
    let mut f = Frame::default();
    let mut bytes = 0u64;
    let mut out = Vec::new();
    // S6 depends on the length (segments are a quarter of it): each length separately.
    if scene == Scene::Resize {
        for &n in lengths {
            let mut g = scene.generator(width, height, n);
            let mut h = Sha256::new();
            let mut bytes = 0u64;
            for i in 0..n {
                g.render(i, &mut f);
                h.update(&f.data);
                bytes += f.data.len() as u64;
            }
            out.push((n, bytes, h.finalize().into()));
        }
        return out;
    }
    for i in 0..max {
        g.render(i, &mut f);
        h.update(&f.data);
        bytes += f.data.len() as u64;
        if lengths.contains(&(i + 1)) {
            out.push((i + 1, bytes, h.clone().finalize().into()));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Manifest

pub const MANIFEST_SCHEMA: &str = "nuvira-bench-fixtures/1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneEntry {
    pub scene: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobEntry {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: String,
    pub generator_version: u32,
    pub scenes: Vec<SceneEntry>,
    pub proto: Vec<BlobEntry>,
    pub audio: Vec<BlobEntry>,
    pub files: Vec<BlobEntry>,
}

/// What to (re)generate.
#[derive(Clone, Debug)]
pub struct GenOptions {
    pub scenes: Vec<Scene>,
    pub resolutions: Vec<(u32, u32)>,
    pub lengths: Vec<u32>,
    /// Random files up to this size (the sparse file is included when >= 1 MiB).
    pub max_file: u64,
    pub threads: usize,
}

fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, 8)
}

impl GenOptions {
    /// Everything (the committed manifest).
    pub fn full() -> GenOptions {
        GenOptions {
            scenes: Scene::ALL.to_vec(),
            resolutions: RESOLUTIONS.to_vec(),
            lengths: vec![FRAMES_QUICK, FRAMES_FULL],
            max_file: files::GIB,
            threads: default_threads(),
        }
    }

    /// Fast check before each run: 1080p x 120 frames, files up to 100 MiB.
    pub fn quick() -> GenOptions {
        GenOptions {
            resolutions: vec![(1920, 1080)],
            lengths: vec![FRAMES_QUICK],
            max_file: 100 * files::MIB,
            ..GenOptions::full()
        }
    }
}

impl Default for GenOptions {
    fn default() -> Self {
        GenOptions::full()
    }
}

impl Manifest {
    /// Regenerates every fixture and returns its manifest.
    pub fn generate(opts: GenOptions) -> Manifest {
        let res = &opts.resolutions;
        let lengths = &opts.lengths;
        let mut jobs: Vec<(Scene, u32, u32)> = Vec::new();
        for &scene in &opts.scenes {
            for &(w, h) in res {
                jobs.push((scene, w, h));
            }
        }
        // Largest first, so the threads finish together.
        jobs.sort_by_key(|&(_, w, h)| std::cmp::Reverse(w * h));
        let next = std::sync::atomic::AtomicUsize::new(0);
        let results = std::sync::Mutex::new(Vec::new());
        let file_sizes: Vec<u64> = files::SIZES.iter().copied().filter(|&s| s <= opts.max_file).collect();
        let sparse = opts.max_file >= files::SPARSE_SIZE;
        let file_results = std::sync::Mutex::new(Vec::new());
        let file_next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..opts.threads.max(1) {
                s.spawn(|| {
                    loop {
                        let i = file_next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i < file_sizes.len() + usize::from(sparse) {
                            let (name, bytes, d) = if i < file_sizes.len() {
                                let size = file_sizes[i];
                                (files::file_name(size, false), size, files::digest(size, false))
                            } else {
                                let size = files::SPARSE_SIZE;
                                (files::file_name(size, true), size, files::digest(size, true))
                            };
                            file_results
                                .lock()
                                .unwrap()
                                .push((i, BlobEntry { name, bytes, sha256: hex(&d) }));
                            continue;
                        }
                        let j = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(&(scene, w, h)) = jobs.get(j) else { break };
                        let d = scene_digests(scene, w, h, lengths);
                        results.lock().unwrap().push((scene, w, h, d));
                    }
                });
            }
        });
        let mut scenes = Vec::new();
        let results = results.into_inner().unwrap();
        for &scene in &opts.scenes {
            for &(w, h) in res {
                let (_, _, _, d) =
                    results.iter().find(|r| r.0 == scene && r.1 == w && r.2 == h).expect("job done");
                for &(frames, bytes, sha) in d {
                    scenes.push(SceneEntry {
                        scene: scene.id().into(),
                        name: scene.name().into(),
                        width: w,
                        height: h,
                        frames,
                        bytes,
                        sha256: hex(&sha),
                    });
                }
            }
        }
        let mut fr = file_results.into_inner().unwrap();
        fr.sort_by_key(|r| r.0);
        let proto = proto_samples::all()
            .into_iter()
            .map(|(n, b)| BlobEntry { name: n.into(), bytes: b.len() as u64, sha256: hex(&sha256(&b)) })
            .collect();
        let pcm = audio::pcm_stereo(10);
        let pcm_bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        let audio = vec![BlobEntry {
            name: "pcm_48k_stereo_f32le_10s".into(),
            bytes: pcm_bytes.len() as u64,
            sha256: hex(&sha256(&pcm_bytes)),
        }];
        Manifest {
            schema: MANIFEST_SCHEMA.into(),
            generator_version: GENERATOR_VERSION,
            scenes,
            proto,
            audio,
            files: fr.into_iter().map(|r| r.1).collect(),
        }
    }

    pub fn parse(text: &str) -> Result<Manifest> {
        serde_json::from_str(text).context("invalid fixture manifest")
    }

    /// The committed manifest embedded in this binary.
    pub fn embedded() -> Result<Manifest> {
        Self::parse(EMBEDDED_MANIFEST)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("serialisable") + "\n"
    }

    /// SHA-256 of the canonical (compact) JSON, independent of line endings.
    pub fn sha256(&self) -> String {
        hex(&sha256(serde_json::to_string(self).expect("serialisable").as_bytes()))
    }

    /// Differences of `generated` from `self` (the reference). Entries `generated`
    /// does not contain (a partial check) are skipped; entries it has that `self`
    /// lacks are reported.
    pub fn verify(&self, generated: &Manifest) -> Vec<String> {
        let mut bad = Vec::new();
        if self.generator_version != generated.generator_version {
            bad.push(format!(
                "generator version: manifest {}, binary {}",
                self.generator_version, generated.generator_version
            ));
        }
        for g in &generated.scenes {
            match self.scenes.iter().find(|e| {
                e.scene == g.scene && e.width == g.width && e.height == g.height && e.frames == g.frames
            }) {
                Some(e) if e == g => {}
                Some(e) => bad.push(format!(
                    "scene {} {}x{} {} frames: manifest {}, generated {}",
                    g.scene, g.width, g.height, g.frames, e.sha256, g.sha256
                )),
                None => bad.push(format!(
                    "scene {} {}x{} {} frames: not in the manifest",
                    g.scene, g.width, g.height, g.frames
                )),
            }
        }
        for (kind, mine, theirs) in [
            ("proto", &self.proto, &generated.proto),
            ("audio", &self.audio, &generated.audio),
            ("file", &self.files, &generated.files),
        ] {
            for g in theirs {
                match mine.iter().find(|e| e.name == g.name) {
                    Some(e) if e == g => {}
                    Some(e) => {
                        bad.push(format!("{kind} {}: manifest {}, generated {}", g.name, e.sha256, g.sha256))
                    }
                    None => bad.push(format!("{kind} {}: not in the manifest", g.name)),
                }
            }
        }
        bad
    }

    /// Expected SHA-256 of a scene sequence, if the manifest has it.
    pub fn scene_sha256(&self, scene: Scene, width: u32, height: u32, frames: u32) -> Option<&str> {
        self.scenes
            .iter()
            .find(|e| e.scene == scene.id() && e.width == width && e.height == height && e.frames == frames)
            .map(|e| e.sha256.as_str())
    }

    /// Expected SHA-256 of a file (`files::file_name`) or protocol sample.
    pub fn blob_sha256(&self, name: &str) -> Option<&str> {
        self.files
            .iter()
            .chain(&self.proto)
            .chain(&self.audio)
            .find(|e| e.name == name)
            .map(|e| e.sha256.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenes_are_deterministic_and_sized() {
        for scene in Scene::ALL {
            let a: Vec<Frame> = scene.frames(320, 180, 8).collect();
            let b: Vec<Frame> = scene.frames(320, 180, 8).collect();
            assert_eq!(a, b, "{scene:?}");
            assert_eq!(a.len(), 8);
            for f in &a {
                assert_eq!(f.data.len(), f.width as usize * f.height as usize * 4);
                assert!(f.data.as_chunks::<4>().0.iter().all(|p| p[3] == 255), "{scene:?} alpha");
            }
        }
    }

    #[test]
    fn scenes_behave_as_specified() {
        // S1 identical every frame; the moving scenes change.
        let s1: Vec<Frame> = Scene::Static.frames(320, 180, 3).collect();
        assert_eq!(s1[0].data, s1[2].data);
        for scene in [Scene::Terminal, Scene::Scroll, Scene::Video, Scene::Drag] {
            let f: Vec<Frame> = scene.frames(1280, 720, 9).collect();
            assert_ne!(f[0].data, f[8].data, "{scene:?}");
        }
        // S3 scrolls by 4 px per frame: frame 1 row y = frame 0 row y + 4.
        let s3: Vec<Frame> = Scene::Scroll.frames(320, 180, 2).collect();
        let row = 320 * 4;
        assert_eq!(&s3[1].data[..row * 100], &s3[0].data[row * 4..row * 104]);
        // S6 at a 4K base: the big segment capped as the agent does (VID-3).
        assert_eq!(scenes::resize_sizes(3840, 2160), [(3840, 2160), (4096, 2304), (2560, 1440), (3840, 2160)]);
        // S6 sizes: base, 4/3, 2/3, base.
        let sizes: Vec<(u32, u32)> =
            Scene::Resize.frames(1920, 1080, 8).map(|f| (f.width, f.height)).collect();
        assert_eq!(
            sizes,
            [
                (1920, 1080),
                (1920, 1080),
                (2560, 1440),
                (2560, 1440),
                (1280, 720),
                (1280, 720),
                (1920, 1080),
                (1920, 1080)
            ]
        );
    }

    #[test]
    fn digests_are_prefix_consistent() {
        let d = scene_digests(Scene::Drag, 320, 180, &[3, 5]);
        let mut h = Sha256::new();
        for f in Scene::Drag.frames(320, 180, 3) {
            h.update(&f.data);
        }
        assert_eq!(d[0].2, <[u8; 32]>::from(h.finalize()));
        assert_eq!(d[1].1, 5 * 320 * 180 * 4);
    }

    #[test]
    fn manifest_partial_generation_is_reproducible() {
        let opts = GenOptions {
            scenes: Scene::ALL.to_vec(),
            resolutions: vec![(320, 180)],
            lengths: vec![4, 6],
            max_file: files::MIB,
            threads: 4,
        };
        let a = Manifest::generate(opts.clone());
        let b = Manifest::generate(opts);
        assert_eq!(a.scenes.len(), 12);
        assert_eq!(a.files.len(), 3);
        assert_eq!(a, b);
        assert!(a.verify(&b).is_empty());
        assert_eq!(a.sha256(), b.sha256());
        let mut c = b.clone();
        c.proto[0].sha256 = "00".into();
        assert_eq!(a.verify(&c).len(), 1);
    }

    #[test]
    fn embedded_manifest_matches_this_build() {
        // The committed manifest must agree with the generators (fast subset).
        let embedded = Manifest::embedded().expect("embedded manifest");
        if embedded.scenes.is_empty() {
            return; // bootstrap: manifest not generated yet
        }
        // Debug builds are slow: there only S1-S6 at 720p x 120 for two scenes.
        let opts = if cfg!(debug_assertions) {
            GenOptions {
                scenes: vec![Scene::Static, Scene::Drag],
                resolutions: vec![(1280, 720)],
                max_file: files::MIB,
                ..GenOptions::quick()
            }
        } else {
            GenOptions::quick()
        };
        let generated = Manifest::generate(opts);
        let bad = embedded.verify(&generated);
        assert!(bad.is_empty(), "{bad:#?}");
    }
}
