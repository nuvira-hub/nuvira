//! Phase 2 characterisation: the real system clipboard (Windows, X11, Wayland
//! through arboard's data-control, macOS) read and written by
//! `nuvira_clipboard::Clipboard` and its sync thread.
//!
//! Overwrites the clipboard of the session it runs in (the text found there is
//! put back at the end): only with `NUVIRA_TEST_CLIPBOARD=1` on a test machine.
//! Without it: `NOT RUN (needs ...)`, not a pass. Never on the NUC.
//!
//! Pinned (v0.28.0):
//! - what is on the clipboard when `Clipboard::new` runs is not reported;
//! - a local copy of text, an image or files is reported once by `poll`;
//! - `set` writes text and images (PNG in, pixels on the clipboard) and is not
//!   reported back; `set(Files)` does nothing;
//! - the same text copied again is not reported again;
//! - empty text and text over `MAX_TEXT_BYTES` are not reported;
//! - `spawn_sync`: local copies reach `outgoing`, `Incoming` is written and its
//!   `then` gets `true`, and what it wrote is not sent back.

use std::io::Write;
use std::time::Duration;

use nuvira_clipboard::{Clipboard, Content, Incoming, MAX_TEXT_BYTES};

fn gate() -> bool {
    if std::env::var("NUVIRA_TEST_CLIPBOARD").as_deref() == Ok("1") {
        return true;
    }
    let _ = writeln!(
        std::io::stderr(),
        "NOT RUN (needs a desktop session whose clipboard the test may overwrite): set NUVIRA_TEST_CLIPBOARD=1 to run it"
    );
    false
}

fn report(text: &str) {
    let _ = writeln!(std::io::stderr(), "RESULT {text}");
}

static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A copy by "another program" (another arboard handle of this process; on X11
/// and Wayland it keeps serving the selection while it lives).
struct Local(arboard::Clipboard);

impl Local {
    fn new() -> Self {
        Self(arboard::Clipboard::new().expect("system clipboard"))
    }
    fn text(&mut self, t: &str) {
        self.0.set_text(t.to_string()).expect("set text");
        settle();
    }
    fn get_text(&mut self) -> Option<String> {
        self.0.get_text().ok()
    }
}

fn settle() {
    std::thread::sleep(Duration::from_millis(300));
}

/// Polls a few times: on X11 the owner change comes as an event.
fn poll(c: &mut Clipboard) -> Option<Content> {
    for _ in 0..5 {
        if let Some(content) = c.poll() {
            return Some(content);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// The text found on the clipboard goes back at the end.
struct PutBack(Option<String>);

impl Drop for PutBack {
    fn drop(&mut self) {
        if let Some(text) = self.0.take()
            && let Ok(mut c) = arboard::Clipboard::new()
        {
            let _ = c.set_text(text);
            settle();
        }
    }
}

fn png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.write_header().unwrap().write_image_data(rgba).unwrap();
    out
}

#[test]
fn text_images_and_files_round_trip() {
    if !gate() {
        return;
    }
    let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut local = Local::new();
    let _back = PutBack(local.get_text());

    local.text("already there before the session");
    let mut clip = Clipboard::new().expect("clipboard");
    assert_eq!(poll(&mut clip), None, "content present at start is not reported");

    // A local copy of text: once.
    local.text("copied here: ü € 😀");
    assert_eq!(poll(&mut clip), Some(Content::Text("copied here: ü € 😀".into())));
    assert_eq!(poll(&mut clip), None, "reported once");

    // The other side's text written here: on the clipboard, not reported back.
    clip.set(Content::Text("from the viewer".into())).expect("set text");
    settle();
    assert_eq!(local.get_text().as_deref(), Some("from the viewer"));
    assert_eq!(poll(&mut clip), None, "own write not reported");

    // The same text copied again locally: not reported again.
    local.text("from the viewer");
    let again = poll(&mut clip);
    report(&format!("same text copied again: {again:?}"));
    assert_eq!(again, None, "the same text again is not new");

    // Empty and too long texts are not shared; a normal one after them is.
    local.text("");
    let empty = poll(&mut clip);
    report(&format!("empty text: {empty:?}"));
    assert_eq!(empty, None);
    local.text(&"x".repeat(MAX_TEXT_BYTES + 1));
    assert_eq!(poll(&mut clip), None, "text over MAX_TEXT_BYTES not shared");
    local.text("short again");
    assert_eq!(poll(&mut clip), Some(Content::Text("short again".into())));

    // Images: a local copy comes as PNG with the same pixels.
    let (w, h) = (3usize, 2usize);
    let rgba: Vec<u8> = (0..w * h).flat_map(|i| [(i * 40) as u8, 200 - (i * 30) as u8, 77, 255]).collect();
    local
        .0
        .set_image(arboard::ImageData { width: w, height: h, bytes: rgba.clone().into() })
        .expect("set image");
    settle();
    match poll(&mut clip) {
        Some(Content::Image(png)) => {
            let (pw, ph, pixels) = nuvira_clipboard::decode_png(&png).expect("PNG");
            report(&format!("image copied here: {pw}x{ph}, {} PNG bytes", png.len()));
            assert_eq!((pw, ph), (w, h));
            assert_eq!(pixels, rgba, "same pixels");
        }
        other => panic!("local image not reported as PNG: {other:?}"),
    }
    // The other side's image written here.
    let (w2, h2) = (4u32, 4u32);
    let rgba2: Vec<u8> = (0..w2 * h2).flat_map(|i| [10, (i * 15) as u8, 250, 255]).collect();
    clip.set(Content::Image(png(w2, h2, &rgba2))).expect("set image");
    settle();
    let img = local.0.get_image().expect("image on the clipboard");
    assert_eq!((img.width, img.height), (w2 as usize, h2 as usize));
    assert_eq!(img.bytes.as_ref(), &rgba2[..], "pixels written as they were");
    assert_eq!(poll(&mut clip), None, "own image not reported back");

    // Files copied here (as a file manager does): reported as paths.
    let dir = std::env::temp_dir().join(format!("nuvira-clip-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let a = dir.join("a.txt");
    let b = dir.join("b b.txt");
    std::fs::write(&a, b"a").unwrap();
    std::fs::write(&b, b"b").unwrap();
    let files = local.0.set().file_list(&[&a, &b]);
    report(&format!("file list set: {files:?}"));
    settle();
    let got = poll(&mut clip);
    report(&format!("files copied here: {got:?}"));
    match (files, got) {
        (Ok(()), Some(Content::Files(list))) => {
            let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
            let list: Vec<_> = list.iter().map(|p| canon(p)).collect();
            assert_eq!(list, vec![canon(&a), canon(&b)]);
        }
        (Err(e), _) => panic!("this clipboard cannot hold files: {e}"),
        (Ok(()), other) => panic!("files not reported: {other:?}"),
    }
    // Files from the other side are offered as RemoteFiles; plain Files are ignored.
    clip.set(Content::Files(vec![a.clone()])).expect("Files is a no-op");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sync_thread_sends_local_copies_and_writes_incoming() {
    if !gate() {
        return;
    }
    let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut local = Local::new();
    let _back = PutBack(local.get_text());
    local.text("before the thread");
    let (out_tx, out_rx) = std::sync::mpsc::channel::<Content>();
    let incoming = nuvira_clipboard::spawn_sync(move |c| out_tx.send(c).is_ok()).expect("sync thread");
    std::thread::sleep(Duration::from_millis(600));
    assert!(out_rx.try_recv().is_err(), "nothing sent for what was there");

    local.text("copied while syncing");
    let sent = out_rx.recv_timeout(Duration::from_secs(3));
    assert_eq!(
        sent.ok(),
        Some(Content::Text("copied while syncing".into())),
        "polled within 400 ms + margin"
    );

    let (done_tx, done_rx) = std::sync::mpsc::channel();
    incoming
        .send(Incoming {
            content: Content::Text("pasted from the other side".into()),
            then: Some(Box::new(move |ok| {
                let _ = done_tx.send(ok);
            })),
        })
        .unwrap();
    assert_eq!(done_rx.recv_timeout(Duration::from_secs(3)), Ok(true), "then(true) once written");
    settle();
    assert_eq!(local.get_text().as_deref(), Some("pasted from the other side"));
    std::thread::sleep(Duration::from_millis(1000));
    assert!(out_rx.try_recv().is_err(), "what it wrote is not sent back");
    drop(incoming);
}
