//! Prints what the clipboard poll sees for a few seconds (tests on real PCs).
fn main() {
    let mut clipboard = nuvira_clipboard::Clipboard::new().expect("clipboard");
    let mut inner = arboard::Clipboard::new().expect("arboard");
    println!("start: files={:?}", inner.get().file_list());
    for _ in 0..40 {
        if let Some(c) = clipboard.poll() {
            match c {
                nuvira_clipboard::Content::Files(f) => println!("files {f:?}"),
                other => println!("{other:?}"),
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    println!("end: files={:?} text={:?}", inner.get().file_list(), inner.get_text().map(|t| t.len()));
}
