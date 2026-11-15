//! Windows: reads virtual files from the clipboard as a paste would (the
//! formats offered, the descriptor, the first file's contents), to check the
//! clipboard object from another process.
#[cfg(windows)]
fn main() {
    use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL, TYMED_ISTREAM};
    use windows::Win32::System::DataExchange::{GetClipboardFormatNameW, RegisterClipboardFormatW};
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::{OleGetClipboard, OleInitialize};
    use windows::core::w;
    unsafe {
        OleInitialize(None).expect("ole");
        let object = OleGetClipboard().expect("clipboard");
        let formats = object.EnumFormatEtc(1).expect("enum");
        let mut etc = [FORMATETC::default()];
        let mut fetched = 0;
        while formats.Next(&mut etc, Some(&mut fetched)).is_ok() && fetched == 1 {
            let mut buf = [0u16; 128];
            let n = GetClipboardFormatNameW(etc[0].cfFormat as u32, &mut buf);
            println!(
                "format {} {} tymed={} lindex={}",
                etc[0].cfFormat,
                String::from_utf16_lossy(&buf[..n.max(0) as usize]),
                etc[0].tymed,
                etc[0].lindex
            );
        }
        let descriptor = RegisterClipboardFormatW(w!("FileGroupDescriptorW")) as u16;
        let contents = RegisterClipboardFormatW(w!("FileContents")) as u16;
        let fmt = |cf, tymed: i32, lindex| FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex,
            tymed: tymed as u32,
        };
        let medium = object.GetData(&fmt(descriptor, TYMED_HGLOBAL.0, -1)).expect("descriptor");
        let p = GlobalLock(medium.u.hGlobal) as *const u32;
        println!("descriptor: {} files", *p);
        let _ = GlobalUnlock(medium.u.hGlobal);
        let t = std::time::Instant::now();
        let medium = object.GetData(&fmt(contents, TYMED_ISTREAM.0, 0)).expect("contents");
        let stream = medium.u.pstm.as_ref().expect("stream").clone();
        let mut total = 0u64;
        let mut buf = vec![0u8; 65536];
        loop {
            let mut read = 0u32;
            let hr = stream.Read(buf.as_mut_ptr().cast(), buf.len() as u32, Some(&mut read));
            total += read as u64;
            if hr.is_err() || read == 0 {
                println!("read end: {hr:?}");
                break;
            }
        }
        println!("first file: {total} bytes in {:?}, first byte {:?}", t.elapsed(), buf.first());
    }
}

#[cfg(not(windows))]
fn main() {}
