//! Offers fake files on the clipboard through the virtual-files code (Windows,
//! Linux FUSE),
//! with no network (tests of the paste on a real PC). The data of each file is
//! its index repeated; it runs for the given seconds.
fn main() {
    tracing_subscriber_init();
    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(120);
    let flat = std::env::args().nth(2).as_deref() == Some("flat");
    let files = if flat {
        vec![file("piccolo.txt", 23, false), file("uno.bin", 1_048_576, false)]
    } else {
        vec![
            file("cartella", 0, true),
            file("cartella/uno.bin", 1_048_576, false),
            file("piccolo.txt", 23, false),
            file("vuoto.bin", 0, false),
            file("grande.bin", 50_000_000, false),
        ]
    };
    let sizes: Vec<u64> = files.iter().map(|f| f.size).collect();
    let remote = nuvira_clipboard::RemoteFiles::new(files, move |index, offset| {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let size = sizes[index as usize];
        eprintln!("fetch index={index} offset={offset}");
        std::thread::spawn(move || {
            let mut pos = offset;
            while pos < size {
                let n = (size - pos).min(256 * 1024) as usize;
                if tx.blocking_send(Ok(vec![index as u8 + b'0'; n])).is_err() {
                    return;
                }
                pos += n as u64;
            }
        });
        rx
    });
    let mut clipboard = nuvira_clipboard::Clipboard::new().expect("clipboard");
    clipboard.set(nuvira_clipboard::Content::RemoteFiles(remote)).expect("set");
    eprintln!("files offered");
    std::thread::sleep(std::time::Duration::from_secs(seconds));
}

fn file(path: &str, size: u64, dir: bool) -> nuvira_proto::v1::ClipboardFile {
    nuvira_proto::v1::ClipboardFile { path: path.into(), size, dir, modified: 1_700_000_000 }
}

fn tracing_subscriber_init() {
    tracing_subscriber::fmt().with_env_filter("debug").with_writer(std::io::stderr).init();
}
