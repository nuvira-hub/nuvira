//! Counts what the clipboard poll sees while another program copies (tests of
//! copies lost to a busy clipboard): prints every content seen, for SECONDS.
fn main() {
    tracing_subscriber::fmt().with_env_filter("nuvira_clipboard=debug").with_writer(std::io::stderr).init();
    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(40);
    let mut clipboard = nuvira_clipboard::Clipboard::new().expect("clipboard");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut seen = 0;
    while std::time::Instant::now() < end {
        if let Some(c) = clipboard.poll() {
            seen += 1;
            match c {
                nuvira_clipboard::Content::Files(f) => println!("files {}", f.len()),
                nuvira_clipboard::Content::Text(t) => println!("text {}", t.len()),
                _ => println!("other"),
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
    println!("seen {seen}");
}
