//! Linux only, under a virtual X server (Xvfb) that `scripts/run-xvfb.sh` starts
//! for the run: the agent with the **real** X11 capture, input (XTest) and
//! clipboard, all on that virtual display, so the parts the synthetic source turns
//! off can be exercised: the file manager (transfers small and large, cut and
//! resumed, byte-exact), the shared clipboard both ways and the mouse. Nothing
//! reaches the machine's real screen, input or clipboard: every case first checks
//! that `DISPLAY` is the Xvfb the script started (`NUVIRA_E2E_XVFB_PID`), and the
//! script removes `WAYLAND_DISPLAY` and the session bus from the environment.
//!
//! The agent is `serve_direct` (`once = false`, so a cut session can be followed
//! by a new one) on 0.0.0.0 of the Linux machine. Destination folders are always
//! explicit temporary folders (`transfer_id` != 0: never the user's Downloads).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nuvira_e2e::rig::{self, AgentEntry, HeadlessSession, HeadlessViewer, LoopbackAgent, ViewerOptions};
use nuvira_e2e::runner::{Case, Ctx, Group, Need, ensure};
use nuvira_proto::v1::control_message::Msg;
use nuvira_proto::v1::{ControlMessage, FileChunk, FsDownload, FsRequest, FsUploadOffset, fs_request};
use nuvira_transport::{FramedRecv, FramedSend};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub fn cases() -> Vec<Case> {
    vec![
        Case::new("xvfb/files/upload-download-exact", files_exact).group(Group::Xvfb).timeout_s(600),
        Case::new("xvfb/files/cut-and-resumed", files_resumed).group(Group::Xvfb).timeout_s(400),
        Case::new("xvfb/clipboard/both-ways", clipboard).group(Group::Xvfb),
        Case::new("xvfb/clipboard/alternating-kinds", clipboard_kinds).group(Group::Xvfb),
        Case::new("xvfb/input/mouse-moves-the-x-pointer", mouse).group(Group::Xvfb),
        // The same file cases with the machine's real capture (Windows, macOS):
        // `scripts/run-real-desktop.*`, only where the screen may be used.
        Case::new("real-desktop/files/upload-download-exact", files_exact)
            .group(Group::RealDesktop)
            .needs(&[Need::Wildcard])
            .timeout_s(900),
        Case::new("real-desktop/files/cut-and-resumed", files_resumed)
            .group(Group::RealDesktop)
            .needs(&[Need::Wildcard])
            .timeout_s(600),
        // Takes the machine's clipboard (nothing put back): a test machine only.
        Case::new("real-desktop/clipboard/alternating-kinds", clipboard_kinds)
            .group(Group::RealDesktop)
            .needs(&[Need::Wildcard]),
    ]
}

const CHUNK: usize = 256 * 1024;

/// Refuses to run anywhere but on the run's own Xvfb.
fn check_xvfb() -> Result<String> {
    let display = std::env::var("DISPLAY").context("no DISPLAY")?;
    ensure(std::env::var_os("WAYLAND_DISPLAY").is_none(), || "WAYLAND_DISPLAY is set".into())?;
    let pid = std::env::var("NUVIRA_E2E_XVFB_PID").context("no NUVIRA_E2E_XVFB_PID")?;
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
    ensure(comm.trim() == "Xvfb" && cmdline.split(' ').any(|a| a == display), || {
        format!("DISPLAY {display} is not the run's Xvfb (pid {pid}: {comm:?} {cmdline:?})")
    })?;
    Ok(display)
}

/// An agent with the real capture of the Xvfb display, and the way to sessions.
pub(crate) struct Rig {
    _art: tokio::runtime::Runtime,
    pub(crate) vrt: tokio::runtime::Runtime,
    agent: LoopbackAgent,
    next_id: u32,
}

impl Rig {
    pub(crate) fn start(ctx: &Ctx) -> Result<Rig> {
        if !ctx.name.starts_with("real-desktop/") {
            check_xvfb()?;
        }
        // Windows: DXGI gives no frame at all while the monitor sleeps (known,
        // pinned in client/capture/tests/screen.rs): the display is woken and kept
        // on for the case, otherwise the session never gets a picture.
        #[cfg(windows)]
        if ctx.name.starts_with("real-desktop/") {
            crate::desktop::keep_display_on(ctx);
        }
        let (art, vrt) = rig::runtimes()?;
        let config = nuvira_bench::net::server_mode::real_capture_config(3000, 30);
        let agent = LoopbackAgent::start(&art, config, AgentEntry::Direct, &ctx.tmp.join("agent"), false)?;
        Ok(Rig { _art: art, vrt, agent, next_id: 0 })
    }

    pub(crate) async fn session(&self) -> Result<HeadlessSession> {
        // The viewer's handshake waits for the video stream, which the agent opens
        // with its first frame: without a picture it would wait for ever.
        let s = tokio::time::timeout(
            Duration::from_secs(60),
            HeadlessViewer::connect_direct(
                self.agent.addr,
                Some(self.agent.fingerprint),
                ViewerOptions::default(),
            ),
        )
        .await
        .context("no video stream from the agent in 60 s (no picture captured)")??;
        rig::frames(&s, 5, Duration::from_secs(20)).await.context("no video from the Xvfb capture")?;
        ensure(s.agent.file_manager && s.agent.paste, || format!("agent Hello {:?}", s.agent))?;
        Ok(s)
    }

    fn id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }
}

pub(crate) async fn wait_msg<T>(
    s: &mut HeadlessSession,
    timeout: Duration,
    mut pred: impl FnMut(&Msg) -> Option<T>,
) -> Result<T> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(deadline, s.events().recv()).await {
            Err(_) => bail!("no answer from the agent in {timeout:?}"),
            Ok(None) => bail!("session closed: {:?}", s.error()),
            Ok(Some(ControlMessage { msg: Some(m) })) => {
                if let Some(v) = pred(&m) {
                    return Ok(v);
                }
            }
            Ok(Some(_)) => {}
        }
    }
}

/// Downloads `src` into `dest_dir` from `offset` (continuing `name.part`), stopping
/// after `stop_at` bytes of the file if given (the `.part` stays). Returns the
/// final path (or the `.part` when stopped).
async fn download(
    s: &mut HeadlessSession,
    id: u32,
    src: &Path,
    dest_dir: &Path,
    offset: u64,
    stop_at: Option<u64>,
) -> Result<PathBuf> {
    let name = src.file_name().context("name")?.to_string_lossy().into_owned();
    let part = dest_dir.join(format!("{name}.part"));
    let req = FsRequest {
        id,
        op: Some(fs_request::Op::Download(FsDownload { path: src.display().to_string(), offset })),
    };
    s.control().send_msg(Msg::FsRequest(req)).await?;
    let conn = s.conn.clone();
    let mut rx = loop {
        let stream =
            tokio::time::timeout(Duration::from_secs(20), conn.accept_uni()).await.context("no stream")??;
        let mut rx = FramedRecv::new(stream);
        match rx.recv::<FileChunk>().await? {
            Some(first) if first.transfer_id == id => {
                ensure(first.offset == offset, || {
                    format!("agent resumes from {} not {offset}", first.offset)
                })?;
                break rx;
            }
            _ => continue,
        }
    };
    let answer = wait_msg(s, Duration::from_secs(10), |m| match m {
        Msg::FsResponse(r) if r.id == id => Some(r.error.clone()),
        _ => None,
    })
    .await?;
    ensure(answer.is_empty(), || format!("download refused: {answer}"))?;
    let mut file = if offset == 0 {
        tokio::fs::File::create(&part).await?
    } else {
        tokio::fs::OpenOptions::new().append(true).open(&part).await?
    };
    let mut have = offset;
    while let Some(chunk) = rx.recv::<FileChunk>().await? {
        file.write_all(&chunk.data).await?;
        have += chunk.data.len() as u64;
        if stop_at.is_some_and(|l| have >= l) {
            file.flush().await?;
            return Ok(part);
        }
    }
    file.flush().await?;
    drop(file);
    let dest = dest_dir.join(&name);
    tokio::fs::rename(&part, &dest).await?;
    Ok(dest)
}

/// Uploads `src` into `dest_dir` on the agent, asking first where to resume;
/// stops after `stop_at` bytes of the file if given. (offset used, final path).
async fn upload(
    s: &mut HeadlessSession,
    id: u32,
    src: &Path,
    dest_dir: &Path,
    stop_at: Option<u64>,
) -> Result<(u64, Option<PathBuf>)> {
    let name = src.file_name().context("name")?.to_string_lossy().into_owned();
    let dir = dest_dir.display().to_string();
    let req = FsRequest {
        id,
        op: Some(fs_request::Op::UploadOffset(FsUploadOffset { dir: dir.clone(), name: name.clone() })),
    };
    s.control().send_msg(Msg::FsRequest(req)).await?;
    let (error, offset) = wait_msg(s, Duration::from_secs(10), |m| match m {
        Msg::FsResponse(r) if r.id == id => Some((r.error.clone(), r.offset)),
        _ => None,
    })
    .await?;
    ensure(error.is_empty(), || format!("upload offset refused: {error}"))?;
    let mut file = tokio::fs::File::open(src).await?;
    let total = file.metadata().await?.len();
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut tx = FramedSend::new(s.conn.open_uni().await?);
    tx.send(&FileChunk { name, size: total, dir, offset, transfer_id: id, ..Default::default() }).await?;
    let mut buf = vec![0u8; CHUNK];
    let mut sent = offset;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        tx.send(&FileChunk { data: buf[..n].to_vec(), ..Default::default() }).await?;
        sent += n as u64;
        if stop_at.is_some_and(|l| sent >= l) {
            return Ok((offset, None));
        }
    }
    tx.finish()?;
    let received = wait_msg(s, Duration::from_secs(600), |m| match m {
        Msg::FileReceived(f) if f.transfer_id == id => Some(f.clone()),
        _ => None,
    })
    .await?;
    ensure(received.ok, || format!("the agent did not keep the file: {}", received.message))?;
    Ok((offset, Some(PathBuf::from(received.path))))
}

/// 1 KiB, 1 MiB and 200 MB, each way, SHA-256 equal and no `.part` left.
fn files_exact(ctx: &mut Ctx) -> Result<()> {
    let mut rig = Rig::start(ctx)?;
    let src_dir = ctx.tmp.join("src");
    let sizes: [(u64, &str); 3] = [(1024, "1KiB.bin"), (1 << 20, "1MiB.bin"), (200_000_000, "200MB.bin")];
    let mut files = Vec::new();
    for (i, (size, name)) in sizes.iter().enumerate() {
        let p = src_dir.join(name);
        rig::make_file(&p, *size, 0x9e37_79b9 + i as u64)?;
        files.push((p.clone(), rig::sha256_file(&p)?, *size));
    }
    let (up_dir, down_dir) = (ctx.tmp.join("agent-side"), ctx.tmp.join("viewer-side"));
    std::fs::create_dir_all(&up_dir)?;
    std::fs::create_dir_all(&down_dir)?;
    let ids: Vec<u32> = (0..files.len() * 2).map(|_| rig.id()).collect();
    rig.vrt.block_on(async {
        let mut s = rig.session().await?;
        for (k, (src, sha, size)) in files.iter().enumerate() {
            let t = Instant::now();
            let (offset, dest) = upload(&mut s, ids[2 * k], src, &up_dir, None).await?;
            let dest = dest.context("no path")?;
            let up = t.elapsed();
            ensure(offset == 0, || format!("upload resumed from {offset}"))?;
            ensure(rig::sha256_file(&dest)? == *sha, || format!("upload of {} differs", src.display()))?;
            ensure(
                !up_dir.join(format!("{}.part", src.file_name().unwrap().to_string_lossy())).exists(),
                || "upload .part left".into(),
            )?;
            let t = Instant::now();
            let got = download(&mut s, ids[2 * k + 1], src, &down_dir, 0, None).await?;
            let down = t.elapsed();
            ensure(rig::sha256_file(&got)? == *sha, || format!("download of {} differs", src.display()))?;
            let mb = *size as f64 / 1e6;
            ctx.note(format!(
                "{size} bytes: upload {:.2} s ({:.1} MB/s), download {:.2} s ({:.1} MB/s), sha256 equal",
                up.as_secs_f64(),
                mb / up.as_secs_f64(),
                down.as_secs_f64(),
                mb / down.as_secs_f64()
            ));
        }
        s.close().await;
        anyhow::Ok(())
    })?;
    // Uploading the same name again gets another name, not an overwrite.
    rig.vrt.block_on(async {
        let mut s = rig.session().await?;
        let id = rig.next_id + 1;
        let (_, dest) = upload(&mut s, id, &files[0].0, &up_dir, None).await?;
        let dest = dest.context("no path")?;
        ctx.note(format!("same name again: {}", dest.display()));
        ensure(dest.file_name().is_some_and(|n| n.to_string_lossy() == "1KiB (1).bin"), || {
            format!("{dest:?}")
        })?;
        s.close().await;
        anyhow::Ok(())
    })?;
    Ok(())
}

/// A 64 MB upload and download cut at half (the session closed), then resumed in
/// a new session from where the other side has it: byte-exact.
fn files_resumed(ctx: &mut Ctx) -> Result<()> {
    let mut rig = Rig::start(ctx)?;
    let size = 64_000_000u64;
    let src = ctx.tmp.join("src").join("cut.bin");
    rig::make_file(&src, size, 77)?;
    let sha = rig::sha256_file(&src)?;
    let (up_dir, down_dir) = (ctx.tmp.join("agent-side"), ctx.tmp.join("viewer-side"));
    std::fs::create_dir_all(&up_dir)?;
    std::fs::create_dir_all(&down_dir)?;
    let (u1, u2, d1, d2) = (rig.id(), rig.id(), rig.id(), rig.id());
    // Upload cut, then resumed.
    let part = up_dir.join("cut.bin.part");
    rig.vrt.block_on(async {
        let mut s = rig.session().await?;
        upload(&mut s, u1, &src, &up_dir, Some(size / 2)).await?;
        s.close().await;
        anyhow::Ok(())
    })?;
    // The agent keeps what reached it.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut kept = 0;
    while Instant::now() < deadline {
        kept = std::fs::metadata(&part).map_or(0, |m| m.len());
        std::thread::sleep(Duration::from_millis(500));
        if kept == std::fs::metadata(&part).map_or(0, |m| m.len()) && kept > 0 {
            break;
        }
    }
    ctx.note(format!("upload cut: the agent kept {kept} of {size} bytes"));
    ensure(kept > 0 && kept <= size / 2 + CHUNK as u64, || format!("kept {kept}"))?;
    rig.vrt.block_on(async {
        let mut s = rig.session().await?;
        let (offset, dest) = upload(&mut s, u2, &src, &up_dir, None).await?;
        ctx.note(format!("upload resumed from {offset}"));
        ensure(offset == kept, || format!("resumed from {offset}, the .part had {kept}"))?;
        let dest = dest.context("no path")?;
        ensure(rig::sha256_file(&dest)? == sha, || "resumed upload differs".into())?;
        ensure(!part.exists(), || ".part left".into())?;
        // Download cut, then resumed.
        let p = download(&mut s, d1, &src, &down_dir, 0, Some(size / 2)).await?;
        s.close().await;
        let have = std::fs::metadata(&p)?.len();
        ctx.note(format!("download cut at {have}"));
        let mut s = rig.session().await?;
        let got = download(&mut s, d2, &src, &down_dir, have, None).await?;
        ensure(rig::sha256_file(&got)? == sha, || "resumed download differs".into())?;
        s.close().await;
        anyhow::Ok(())
    })?;
    Ok(())
}

fn xclip(display: &str, args: &[&str], input: Option<&str>) -> Result<String> {
    use std::io::Write;
    let mut cmd = Command::new("xclip");
    cmd.args(["-display", display, "-selection", "clipboard"]).args(args);
    cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
    let mut child = cmd.spawn().context("xclip (apt install xclip)")?;
    if let Some(text) = input {
        child.stdin.take().context("stdin")?.write_all(text.as_bytes())?;
    } else {
        drop(child.stdin.take());
    }
    // `xclip -i` stays in the background to serve the selection: not waited.
    if input.is_some() {
        nuvira_e2e::runner::track_pid(child.id());
        return Ok(String::new());
    }
    let out = child.wait_with_output()?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Viewer -> agent: `Clipboard` puts the text on the X clipboard; agent -> viewer:
/// text copied on the X display comes to the viewer as `Clipboard`.
fn clipboard(ctx: &mut Ctx) -> Result<()> {
    let display = check_xvfb()?;
    let rig = Rig::start(ctx)?;
    rig.vrt.block_on(async {
        let mut s = rig.session().await?;
        let text = format!("e2e from the viewer {}", std::process::id());
        let clip = nuvira_proto::v1::Clipboard { text: text.clone(), ..Default::default() };
        s.control().send_msg(Msg::Clipboard(clip)).await?;
        let t = Instant::now();
        let mut seen = String::new();
        while t.elapsed() < Duration::from_secs(10) {
            seen = xclip(&display, &["-o"], None).unwrap_or_default();
            if seen == text {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        ctx.note(format!("viewer -> X clipboard in {} ms: {seen:?}", t.elapsed().as_millis()));
        ensure(seen == text, || format!("X clipboard {seen:?}"))?;
        let local = format!("e2e from the X display {}", std::process::id());
        xclip(&display, &["-i"], Some(&local))?;
        let t = Instant::now();
        let got = wait_msg(&mut s, Duration::from_secs(15), |m| match m {
            Msg::Clipboard(c) if c.text == local => Some(c.text.clone()),
            _ => None,
        })
        .await?;
        ctx.note(format!("X clipboard -> viewer in {} ms: {got:?}", t.elapsed().as_millis()));
        s.close().await;
        anyhow::Ok(())
    })
}

/// A program on the agent's machine copying: `xclip` on the run's Xvfb (another
/// process), or on a real desktop another clipboard handle of this process (as
/// the clipboard probes do; on Windows and macOS a copy by any program changes
/// the same counter).
enum Copier {
    Xclip { display: String, dir: PathBuf },
    Arboard(Box<arboard::Clipboard>),
}

impl Copier {
    fn text(&mut self, text: &str) -> Result<()> {
        match self {
            Copier::Xclip { display, .. } => xclip(display, &["-i"], Some(text)).map(drop),
            Copier::Arboard(c) => c.set_text(text.to_string()).context("set text"),
        }
    }

    fn image(&mut self, seed: u8) -> Result<()> {
        let (w, h, rgba) = aba_image(seed);
        match self {
            Copier::Xclip { display, dir } => {
                let file = dir.join(format!("aba-{seed}.png"));
                std::fs::write(&file, encode_png(w, h, &rgba)?)?;
                let path = file.display().to_string();
                let child = Command::new("xclip")
                    .args(["-display", display, "-selection", "clipboard", "-t", "image/png", "-i", &path])
                    .stdin(std::process::Stdio::null())
                    .spawn()
                    .context("xclip (apt install xclip)")?;
                nuvira_e2e::runner::track_pid(child.id());
                Ok(())
            }
            Copier::Arboard(c) => c
                .set_image(arboard::ImageData { width: w as usize, height: h as usize, bytes: rgba.into() })
                .context("set image"),
        }
    }

    /// What the clipboard holds now, as the agent's side sees it: a text, or "image".
    fn holds(&mut self) -> String {
        match self {
            Copier::Xclip { display, .. } => {
                let targets = xclip(display, &["-o", "-t", "TARGETS"], None).unwrap_or_default();
                if targets.lines().any(|t| t.trim() == "image/png") {
                    "image".into()
                } else {
                    xclip(display, &["-o"], None).unwrap_or_default()
                }
            }
            Copier::Arboard(c) => match c.get_text() {
                Ok(t) => t,
                Err(_) if c.get_image().is_ok() => "image".into(),
                Err(_) => String::new(),
            },
        }
    }
}

/// Opaque, different for every seed.
fn aba_image(seed: u8) -> (u32, u32, Vec<u8>) {
    let (w, h) = (32u32, 24u32);
    let rgba = (0..w * h).flat_map(|i| [(i as u8).wrapping_mul(seed), (i as u8) ^ seed, seed, 255]).collect();
    (w, h, rgba)
}

fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.write_header()?.write_image_data(rgba)?;
    Ok(out)
}

/// Which `aba_image` seed a PNG from the agent shows ("image ?" if none).
fn image_seed(png: &[u8]) -> String {
    let decoded = (|| -> Result<(u32, u32, Vec<u8>)> {
        let mut dec = png::Decoder::new(std::io::Cursor::new(png));
        dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = dec.read_info()?;
        let mut buf = vec![0; reader.output_buffer_size().context("size")?];
        let frame = reader.next_frame(&mut buf)?;
        buf.truncate(frame.buffer_size());
        ensure(frame.color_type == png::ColorType::Rgba, || format!("{:?}", frame.color_type))?;
        Ok((frame.width, frame.height, buf))
    })();
    match decoded {
        Ok(img) => match (1..=255u8).find(|&s| aba_image(s) == img) {
            Some(s) => format!("image {s}"),
            None => format!("image ? {}x{}", img.0, img.1),
        },
        Err(e) => format!("image not decoded: {e:#}"),
    }
}

/// The next content the agent sent from its clipboard within `wait`: `text ...`
/// (control channel) or `image N` (its own stream), or "none".
async fn next_clip(s: &mut HeadlessSession, wait: Duration) -> Result<String> {
    let conn = s.conn.clone();
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        tokio::select! {
            m = s.events().recv() => match m {
                None => bail!("session closed: {:?}", s.error()),
                Some(ControlMessage { msg: Some(Msg::Clipboard(c)) }) => return Ok(format!("text {:?}", c.text)),
                Some(_) => {}
            },
            stream = conn.accept_uni() => {
                let mut rx = FramedRecv::new(stream?);
                if let Some(first) = rx.recv::<FileChunk>().await?
                    && first.clipboard_image
                {
                    let png = nuvira_transport::read_stream(&mut rx, &first, 32 << 20).await?;
                    return Ok(image_seed(&png));
                }
            }
            _ = tokio::time::sleep_until(deadline) => return Ok("none".into()),
        }
    }
}

/// What a step of `clipboard_kinds` does.
enum Step<'a> {
    Text(&'a str),
    Image(u8),
    ViewerText(&'a str),
    ViewerImage(u8),
}

/// Copies of alternating kinds on the agent's machine (image A, text B, image A
/// again; text A, image C, text A again) each reach the viewer, also when the
/// viewer's text or image was written there in between, which is not sent back;
/// the same text twice in a row is sent once. Before the Phase 7 fix the second
/// image A and the second text A never arrived.
fn clipboard_kinds(ctx: &mut Ctx) -> Result<()> {
    let mut copier = if ctx.name.starts_with("real-desktop/") {
        Copier::Arboard(Box::new(arboard::Clipboard::new().context("system clipboard")?))
    } else {
        Copier::Xclip { display: check_xvfb()?, dir: ctx.tmp.clone() }
    };
    let rig = Rig::start(ctx)?;
    let settle = Duration::from_millis(300);
    let (arrive, quiet) = (Duration::from_secs(8), Duration::from_secs(2));
    let pid = std::process::id();
    let (text_a, text_b, text_e) = (format!("aba A {pid}"), format!("aba B {pid}"), format!("aba E {pid}"));
    let steps = [
        ("image A", Step::Image(3), "image 3".to_string()),
        ("text B", Step::Text(&text_b), format!("text {text_b:?}")),
        ("image A again", Step::Image(3), "image 3".into()),
        ("text A", Step::Text(&text_a), format!("text {text_a:?}")),
        ("image C", Step::Image(7), "image 7".into()),
        ("text A again", Step::Text(&text_a), format!("text {text_a:?}")),
        ("the same text A twice", Step::Text(&text_a), "none".into()),
        ("image D", Step::Image(5), "image 5".into()),
        ("viewer's text written (no echo)", Step::ViewerText("aba from the viewer"), "none".into()),
        ("image D again", Step::Image(5), "image 5".into()),
        ("text E", Step::Text(&text_e), format!("text {text_e:?}")),
        ("viewer's image written (no echo)", Step::ViewerImage(9), "none".into()),
        ("text E again", Step::Text(&text_e), format!("text {text_e:?}")),
    ];
    let mut log = Vec::new();
    let mut failed = Vec::new();
    let result = rig.vrt.block_on(async {
        let mut s = rig.session().await?;
        // The agent's clipboard thread takes what is there now as already seen.
        tokio::time::sleep(Duration::from_millis(800)).await;
        for (name, step, want) in steps {
            let t = Instant::now();
            let wait = if want == "none" { quiet } else { arrive };
            match step {
                Step::Text(text) => copier.text(text)?,
                Step::Image(seed) => copier.image(seed)?,
                Step::ViewerText(text) => {
                    let clip = nuvira_proto::v1::Clipboard { text: text.to_string(), ..Default::default() };
                    s.control().send_msg(Msg::Clipboard(clip)).await?;
                    wait_holds(&mut copier, text).await?;
                }
                Step::ViewerImage(seed) => {
                    let (w, h, rgba) = aba_image(seed);
                    let header = FileChunk { clipboard_image: true, ..Default::default() };
                    nuvira_transport::send_stream(&s.conn, header, &encode_png(w, h, &rgba)?).await?;
                    wait_holds(&mut copier, "image").await?;
                }
            }
            tokio::time::sleep(settle).await;
            let got = next_clip(&mut s, wait).await?;
            log.push(format!("{name}: {got} ({} ms)", t.elapsed().as_millis()));
            if got != want {
                failed.push(format!("{name}: got {got}, expected {want}"));
            }
        }
        s.close().await;
        anyhow::Ok(())
    });
    for line in &log {
        ctx.note(line);
    }
    result?;
    ensure(failed.is_empty(), || failed.join("; "))
}

/// Waits until the agent's clipboard holds what the viewer sent (`Copier::holds`).
async fn wait_holds(copier: &mut Copier, want: &str) -> Result<()> {
    let t = Instant::now();
    let mut seen = String::new();
    while t.elapsed() < Duration::from_secs(10) {
        seen = copier.holds();
        if seen == want {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("the agent's clipboard holds {seen:?}, not {want:?}")
}

/// `MouseMove` events move the pointer of the X display (XTest), read back with
/// `xdotool getmouselocation`.
fn mouse(ctx: &mut Ctx) -> Result<()> {
    use nuvira_proto::v1::{InputEvent, MouseMove, input_event::Event};
    let display = check_xvfb()?;
    let rig = Rig::start(ctx)?;
    let pointer = || -> Result<(i32, i32)> {
        let out = Command::new("xdotool").args(["getmouselocation"]).env("DISPLAY", &display).output()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let num = |k: &str| {
            text.split_whitespace().find_map(|w| w.strip_prefix(k)).and_then(|v| v.parse::<i32>().ok())
        };
        Ok((num("x:").context("x")?, num("y:").context("y")?))
    };
    rig.vrt.block_on(async {
        let mut s = rig.session().await?;
        let (w, h) = (s.format.width as f32, s.format.height as f32);
        for (x, y) in [(0.25f32, 0.5f32), (0.75, 0.25), (0.5, 0.75)] {
            let ev = InputEvent { event: Some(Event::MouseMove(MouseMove { x, y })) };
            s.input().send(&ev).await?;
            let (want_x, want_y) = ((x * w) as i32, (y * h) as i32);
            let t = Instant::now();
            let mut at = (0, 0);
            while t.elapsed() < Duration::from_secs(5) {
                at = pointer()?;
                if (at.0 - want_x).abs() <= 2 && (at.1 - want_y).abs() <= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            ctx.note(format!(
                "({x}, {y}) -> pointer {at:?}, expected ({want_x}, {want_y}), {} ms",
                t.elapsed().as_millis()
            ));
            ensure((at.0 - want_x).abs() <= 2 && (at.1 - want_y).abs() <= 2, || {
                format!("pointer at {at:?}")
            })?;
        }
        s.close().await;
        anyhow::Ok(())
    })
}
