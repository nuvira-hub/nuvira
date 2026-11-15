//! Windows: the other side's files on this clipboard as "virtual files"
//! (FileGroupDescriptorW + FileContents), the way Explorer pastes from a
//! remote session. Explorer's own copy, with its progress window, reads each file
//! through an `IStream` that asks the other side for the data only then.
//!
//! OLE needs a single-threaded apartment with a message loop: the clipboard
//! object lives on its own thread, and Explorer's calls (also the blocking
//! `Read`s) come to it there.

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};

use anyhow::{Context, Result};
use tracing::{debug, warn};
use windows::Win32::Foundation::{
    E_FAIL, E_INVALIDARG, E_NOTIMPL, FILETIME, GlobalFree, HGLOBAL, S_FALSE, S_OK, STG_E_ACCESSDENIED,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::System::Com::{
    CoTaskMemAlloc, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl,
    IEnumFORMATETC, IEnumSTATDATA, ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG,
    STATFLAG_NONAME, STATSTG, STGTY_STREAM, STGC, STGMEDIUM, STGMEDIUM_0, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
    TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{
    DROPEFFECT_COPY, OleInitialize, OleSetClipboard, OleUninitialize,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{
    FD_ATTRIBUTES, FD_FILESIZE, FD_PROGRESSUI, FD_UNICODE, FD_WRITESTIME, FILEDESCRIPTORW, IDataObjectAsyncCapability,
    IDataObjectAsyncCapability_Impl, SHCreateStdEnumFmtEtc,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, WM_APP, WM_USER,
};
use windows::core::{BOOL, HRESULT, Ref, implement, w};

use crate::share::{IDLE, RemoteFiles, recv_timeout};

const DV_E_FORMATETC: HRESULT = HRESULT(0x8004_0064_u32 as i32);
const OLE_E_ADVISENOTSUPPORTED: HRESULT = HRESULT(0x8004_0003_u32 as i32);

enum Command {
    Set(RemoteFiles),
}

/// The thread that owns the virtual files on the clipboard. Dropping it takes
/// them off the clipboard (they would point to a session that is over).
pub struct VirtualClipboard {
    commands: Sender<Command>,
    thread: u32,
}

impl VirtualClipboard {
    pub fn spawn() -> Result<Self> {
        let (commands, rx) = channel();
        let (ready_tx, ready_rx) = channel();
        std::thread::Builder::new().name("clipboard-files".into()).spawn(move || run(rx, ready_tx))?;
        let thread = ready_rx.recv().context("clipboard files thread ended")??;
        Ok(Self { commands, thread })
    }

    /// Offers `files` on the clipboard, in place of what was there.
    pub fn set(&self, files: RemoteFiles) {
        if self.commands.send(Command::Set(files)).is_ok() {
            // SAFETY: wakes the thread's message loop; it then reads the command.
            unsafe {
                let _ = PostThreadMessageW(self.thread, WM_APP, Default::default(), Default::default());
            }
        }
    }
}

impl Drop for VirtualClipboard {
    fn drop(&mut self) {
        let (commands, _) = channel();
        drop(std::mem::replace(&mut self.commands, commands));
        // SAFETY: as in `set`: the loop sees the channel closed and ends.
        unsafe {
            let _ = PostThreadMessageW(self.thread, WM_APP, Default::default(), Default::default());
        }
    }
}

fn run(commands: Receiver<Command>, ready: Sender<Result<u32>>) {
    // SAFETY: once on this thread, undone at its end.
    if let Err(e) = unsafe { OleInitialize(None) } {
        let _ = ready.send(Err(e).context("OleInitialize"));
        return;
    }
    let mut msg = MSG::default();
    // The thread's message queue exists before anyone posts to it (a message
    // posted earlier is lost, and the first files would never be offered).
    unsafe {
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
    }
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));
    let mut current: Option<IDataObject> = None;
    // Commands sent before the queue was there are read at once.
    let mut pending = true;
    'outer: loop {
        if !pending {
            // SAFETY: the usual message loop; OLE's clipboard window and the calls
            // of other programs into the objects come through it.
            let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
            if got.0 <= 0 {
                break;
            }
        }
        if std::mem::take(&mut pending) || msg.hwnd.is_invalid() && msg.message == WM_APP {
            loop {
                match commands.try_recv() {
                    Ok(Command::Set(files)) => {
                        let object: IDataObject = DataObject::new(files).into();
                        // SAFETY: the object implements IDataObject; OLE keeps a reference.
                        match unsafe { OleSetClipboard(&object) } {
                            Ok(()) => {
                                debug!("other side's files on the clipboard");
                                current = Some(object);
                            }
                            Err(e) => warn!("files not put on the clipboard: {e}"),
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break 'outer,
                }
            }
            continue;
        }
        unsafe {
            DispatchMessageW(&msg);
        }
    }
    // Session over: the files are no longer readable. Still ours when OLE's
    // clipboard window (of this thread) owns the clipboard.
    let ours = unsafe {
        let owner = windows::Win32::System::DataExchange::GetClipboardOwner();
        let mut pid = 0;
        owner.is_ok_and(|w| {
            windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(w, Some(&mut pid))
                == GetCurrentThreadId()
        })
    };
    if current.is_some() && ours {
        let _ = unsafe { OleSetClipboard(None) };
        debug!("other side's files taken off the clipboard");
    }
    unsafe { OleUninitialize() };
}

struct Formats {
    descriptor: u16,
    contents: u16,
    effect: u16,
}

fn formats() -> Formats {
    // SAFETY: registering names returns the same number for the same name.
    unsafe {
        Formats {
            descriptor: RegisterClipboardFormatW(w!("FileGroupDescriptorW")) as u16,
            contents: RegisterClipboardFormatW(w!("FileContents")) as u16,
            effect: RegisterClipboardFormatW(w!("Preferred DropEffect")) as u16,
        }
    }
}

/// Explorer copies asynchronously (on its own thread, with its progress
/// window) only from data objects that say they can (`IDataObjectAsyncCapability`);
/// it also writes its own formats into the object (`SetData`: drop description,
/// performed effect) and gives up the paste when that fails.
#[implement(IDataObject, IDataObjectAsyncCapability)]
struct DataObject {
    files: RemoteFiles,
    formats: Formats,
    /// What Explorer wrote, by format.
    stored: Mutex<std::collections::HashMap<u16, Vec<u8>>>,
    async_mode: std::sync::atomic::AtomicBool,
    in_operation: std::sync::atomic::AtomicBool,
}

impl DataObject {
    fn new(files: RemoteFiles) -> Self {
        Self {
            files,
            formats: formats(),
            stored: Mutex::new(std::collections::HashMap::new()),
            async_mode: std::sync::atomic::AtomicBool::new(true),
            in_operation: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn stored(&self, format: &FORMATETC) -> Option<Vec<u8>> {
        if format.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
            return None;
        }
        self.stored.lock().expect("stored").get(&format.cfFormat).cloned()
    }

    fn descriptor(&self) -> windows::core::Result<HGLOBAL> {
        let count = self.files.files.len();
        let size = 4 + count * std::mem::size_of::<FILEDESCRIPTORW>();
        let mut descriptors = Vec::with_capacity(count);
        for file in &self.files.files {
            let mut d = FILEDESCRIPTORW {
                dwFlags: (FD_ATTRIBUTES.0 | FD_FILESIZE.0 | FD_WRITESTIME.0 | FD_PROGRESSUI.0 | FD_UNICODE.0) as u32,
                dwFileAttributes: if file.dir { FILE_ATTRIBUTE_DIRECTORY.0 } else { FILE_ATTRIBUTE_NORMAL.0 },
                nFileSizeHigh: (file.size >> 32) as u32,
                nFileSizeLow: file.size as u32,
                ftLastWriteTime: filetime(file.modified),
                ..Default::default()
            };
            let name: Vec<u16> = crate::list::safe_relative(&file.path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".into())
                .encode_utf16()
                .collect();
            let mut file_name = [0u16; 260];
            let n = name.len().min(file_name.len() - 1);
            file_name[..n].copy_from_slice(&name[..n]);
            d.cFileName = file_name;
            descriptors.push(d);
        }
        global(size, |p| {
            // SAFETY: `p` holds `size` bytes: the count, then the descriptors.
            unsafe {
                *(p as *mut u32) = count as u32;
                std::ptr::copy_nonoverlapping(
                    descriptors.as_ptr(),
                    p.add(4) as *mut FILEDESCRIPTORW,
                    count,
                );
            }
        })
    }

    fn known(&self, format: &FORMATETC) -> bool {
        let cf = format.cfFormat;
        (cf == self.formats.descriptor || cf == self.formats.effect) && format.tymed & TYMED_HGLOBAL.0 as u32 != 0
            || cf == self.formats.contents && format.tymed & TYMED_ISTREAM.0 as u32 != 0
    }
}

impl IDataObject_Impl for DataObject_Impl {
    fn GetData(&self, format: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let format = unsafe { format.as_ref() }.ok_or(E_INVALIDARG)?;
        tracing::trace!(cf = format.cfFormat, tymed = format.tymed, lindex = format.lindex, "GetData");
        if let Some(bytes) = self.stored(format) {
            let h = global(bytes.len().max(1), |p| unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len())
            })?;
            return Ok(hglobal_medium(h));
        }
        if !self.known(format) {
            return Err(DV_E_FORMATETC.into());
        }
        let cf = format.cfFormat;
        if cf == self.formats.descriptor {
            let h = self.descriptor()?;
            return Ok(hglobal_medium(h));
        }
        if cf == self.formats.effect {
            let h = global(4, |p| unsafe { *(p as *mut u32) = DROPEFFECT_COPY.0 })?;
            return Ok(hglobal_medium(h));
        }
        let index = format.lindex;
        let file = usize::try_from(index).ok().and_then(|i| self.files.files.get(i)).ok_or(DV_E_FORMATETC)?;
        if file.dir {
            return Err(DV_E_FORMATETC.into());
        }
        debug!(index, path = file.path, size = file.size, "Explorer reads a pasted file");
        let stream: IStream = Stream {
            files: self.files.clone(),
            index: index as u32,
            size: file.size,
            name: file.path.rsplit('/').next().unwrap_or_default().to_string(),
            state: Mutex::new(State::default()),
        }
        .into();
        Ok(STGMEDIUM {
            tymed: TYMED_ISTREAM.0 as u32,
            u: STGMEDIUM_0 { pstm: std::mem::ManuallyDrop::new(Some(stream)) },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        })
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        match unsafe { format.as_ref() } {
            Some(format) if self.known(format) || self.stored(format).is_some() => S_OK,
            _ => DV_E_FORMATETC,
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if let Some(out) = unsafe { out.as_mut() } {
            out.ptd = std::ptr::null_mut();
        }
        E_NOTIMPL
    }

    fn SetData(&self, format: *const FORMATETC, medium: *const STGMEDIUM, release: BOOL) -> windows::core::Result<()> {
        let format = unsafe { format.as_ref() }.ok_or(E_INVALIDARG)?;
        let medium_ref = unsafe { medium.as_ref() }.ok_or(E_INVALIDARG)?;
        if medium_ref.tymed != TYMED_HGLOBAL.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        // SAFETY: an HGLOBAL medium: its memory read under lock; with `release`
        // the medium is ours and is released here.
        let bytes = unsafe {
            let h = medium_ref.u.hGlobal;
            let size = windows::Win32::System::Memory::GlobalSize(h);
            let p = GlobalLock(h) as *const u8;
            let bytes = if p.is_null() { Vec::new() } else { std::slice::from_raw_parts(p, size).to_vec() };
            let _ = GlobalUnlock(h);
            if release.as_bool() {
                windows::Win32::System::Ole::ReleaseStgMedium(medium as *mut STGMEDIUM);
            }
            bytes
        };
        tracing::trace!(cf = format.cfFormat, bytes = bytes.len(), "SetData");
        self.stored.lock().expect("stored").insert(format.cfFormat, bytes);
        Ok(())
    }

    fn EnumFormatEtc(&self, direction: u32) -> windows::core::Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let etc = |cf: u16, tymed: i32| FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: tymed as u32,
        };
        let list = [
            etc(self.formats.descriptor, TYMED_HGLOBAL.0),
            etc(self.formats.contents, TYMED_ISTREAM.0),
            etc(self.formats.effect, TYMED_HGLOBAL.0),
        ];
        unsafe { SHCreateStdEnumFmtEtc(&list) }
    }

    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<IAdviseSink>) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

impl IDataObjectAsyncCapability_Impl for DataObject_Impl {
    fn SetAsyncMode(&self, on: BOOL) -> windows::core::Result<()> {
        self.async_mode.store(on.as_bool(), std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn GetAsyncMode(&self) -> windows::core::Result<BOOL> {
        Ok(self.async_mode.load(std::sync::atomic::Ordering::Relaxed).into())
    }

    fn StartOperation(&self, _: Ref<windows::Win32::System::Com::IBindCtx>) -> windows::core::Result<()> {
        debug!("Explorer starts copying the pasted files");
        self.in_operation.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn InOperation(&self) -> windows::core::Result<BOOL> {
        Ok(self.in_operation.load(std::sync::atomic::Ordering::Relaxed).into())
    }

    fn EndOperation(&self, result: HRESULT, _: Ref<windows::Win32::System::Com::IBindCtx>, effects: u32) -> windows::core::Result<()> {
        debug!(?result, effects, "Explorer finished copying the pasted files");
        self.in_operation.store(false, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

#[derive(Default)]
struct State {
    pos: u64,
    rx: Option<tokio::sync::mpsc::Receiver<crate::share::Chunk>>,
    buf: Vec<u8>,
    used: usize,
}

/// One pasted file, read from the other side as Explorer asks for it.
#[implement(IStream)]
struct Stream {
    files: RemoteFiles,
    index: u32,
    size: u64,
    name: String,
    state: Mutex<State>,
}

impl ISequentialStream_Impl for Stream_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, read: *mut u32) -> HRESULT {
        let mut state = self.state.lock().expect("stream");
        let out = pv as *mut u8;
        let mut n = 0usize;
        while n < cb as usize && state.pos < self.size {
            if state.used < state.buf.len() {
                let take = (cb as usize - n).min(state.buf.len() - state.used);
                // SAFETY: `pv` has room for `cb` bytes; `n + take <= cb`.
                unsafe { std::ptr::copy_nonoverlapping(state.buf.as_ptr().add(state.used), out.add(n), take) };
                state.used += take;
                state.pos += take as u64;
                n += take;
                continue;
            }
            if state.rx.is_none() {
                state.rx = Some(self.files.fetch(self.index, state.pos));
            }
            let rx = state.rx.as_mut().expect("rx");
            match recv_timeout(rx, IDLE) {
                Ok(Some(Ok(data))) => {
                    state.buf = data;
                    state.used = 0;
                }
                Ok(Some(Err(e))) => {
                    warn!(name = self.name, "pasted file not read: {e}");
                    state.rx = None;
                    return E_FAIL;
                }
                Ok(None) => {
                    warn!(name = self.name, pos = state.pos, size = self.size, "pasted file interrupted");
                    state.rx = None;
                    return E_FAIL;
                }
                Err(e) => {
                    warn!(name = self.name, "pasted file: {e:#}");
                    state.rx = None;
                    return E_FAIL;
                }
            }
        }
        if let Some(read) = unsafe { read.as_mut() } {
            *read = n as u32;
        }
        if n == cb as usize { S_OK } else { S_FALSE }
    }

    fn Write(&self, _: *const core::ffi::c_void, _: u32, _: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for Stream_Impl {
    fn Seek(&self, offset: i64, origin: STREAM_SEEK, new: *mut u64) -> windows::core::Result<()> {
        let mut state = self.state.lock().expect("stream");
        let base = match origin {
            STREAM_SEEK_SET => 0i128,
            STREAM_SEEK_CUR => state.pos as i128,
            STREAM_SEEK_END => self.size as i128,
            _ => return Err(E_INVALIDARG.into()),
        };
        let target = base + offset as i128;
        if target < 0 {
            return Err(E_INVALIDARG.into());
        }
        let target = target as u64;
        if target != state.pos {
            // Elsewhere in the file: what is in flight no longer fits.
            state.rx = None;
            state.buf.clear();
            state.used = 0;
            state.pos = target;
        }
        if let Some(new) = unsafe { new.as_mut() } {
            *new = target;
        }
        Ok(())
    }

    fn SetSize(&self, _: u64) -> windows::core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(&self, _: Ref<IStream>, _: u64, _: *mut u64, _: *mut u64) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _: &STGC) -> windows::core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Stat(&self, stat: *mut STATSTG, flag: &STATFLAG) -> windows::core::Result<()> {
        let stat = unsafe { stat.as_mut() }.ok_or(E_INVALIDARG)?;
        *stat = STATSTG { r#type: STGTY_STREAM.0 as u32, cbSize: self.size, ..Default::default() };
        if *flag != STATFLAG_NONAME {
            let name: Vec<u16> = self.name.encode_utf16().chain(Some(0)).collect();
            // SAFETY: the caller frees it with CoTaskMemFree.
            unsafe {
                let p = CoTaskMemAlloc(name.len() * 2) as *mut u16;
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(name.as_ptr(), p, name.len());
                    stat.pwcsName = windows::core::PWSTR(p);
                }
            }
        }
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

/// Moveable global memory of `size` bytes, filled by `fill`.
fn global(size: usize, fill: impl FnOnce(*mut u8)) -> windows::core::Result<HGLOBAL> {
    // SAFETY: allocated, locked, filled, unlocked; the receiver of the medium frees it.
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, size)?;
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(h));
            return Err(E_FAIL.into());
        }
        fill(p);
        let _ = GlobalUnlock(h);
        Ok(h)
    }
}

fn hglobal_medium(h: HGLOBAL) -> STGMEDIUM {
    STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: h },
        pUnkForRelease: std::mem::ManuallyDrop::new(None),
    }
}

/// Seconds since 1970 as a FILETIME (100 ns since 1601).
fn filetime(secs: i64) -> FILETIME {
    if secs <= 0 {
        return FILETIME::default();
    }
    let t = (secs as u64 + 11_644_473_600) * 10_000_000;
    FILETIME { dwLowDateTime: t as u32, dwHighDateTime: (t >> 32) as u32 }
}

/// As SYSTEM (the service's agent), Explorer — the signed-in person's — reads
/// pasted files through COM calls into this process, which Windows refuses by
/// default between accounts: Explorer then creates the folders and none of the
/// files. Allows local calls from the interactive user, SYSTEM and the
/// administrators, for this process. To be called at its start, before anything
/// uses COM; as a normal user it does nothing.
pub fn allow_session_user_calls() {
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::PSECURITY_DESCRIPTOR;
    use windows::Win32::System::Com::{
        COINIT_MULTITHREADED, CoInitializeEx, CoInitializeSecurity, EOAC_NONE, RPC_C_AUTHN_LEVEL_DEFAULT,
        RPC_C_IMP_LEVEL_IDENTIFY,
    };
    if !running_as_system() {
        return;
    }
    // SAFETY: COM joined once on this thread for the process; the descriptor
    // stays allocated for the whole process, as COM keeps using it.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // COM_RIGHTS_EXECUTE | COM_RIGHTS_EXECUTE_LOCAL for SYSTEM, administrators, interactive users.
        if let Err(e) = ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("O:SYG:SYD:(A;;0x3;;;SY)(A;;0x3;;;BA)(A;;0x3;;;IU)"),
            SDDL_REVISION_1,
            &mut sd,
            None,
        ) {
            warn!("COM access for the signed-in user not set: {e}");
            return;
        }
        // COM takes only the absolute form: its parts in their own buffers,
        // kept for the life of the process.
        let mut sizes = [0u32; 5];
        let [sd_len, dacl_len, sacl_len, owner_len, group_len] = &mut sizes;
        let _ = windows::Win32::Security::MakeAbsoluteSD(
            sd, None, sd_len, None, dacl_len, None, sacl_len, None, owner_len, None, group_len,
        );
        let buffer = |n: u32| Box::leak(vec![0u64; (n as usize).div_ceil(8).max(1)].into_boxed_slice()).as_mut_ptr();
        let (abs, dacl, sacl, owner, group) =
            (buffer(*sd_len), buffer(*dacl_len), buffer(*sacl_len), buffer(*owner_len), buffer(*group_len));
        if let Err(e) = windows::Win32::Security::MakeAbsoluteSD(
            sd,
            Some(PSECURITY_DESCRIPTOR(abs.cast())),
            sd_len,
            Some(dacl.cast()),
            dacl_len,
            Some(sacl.cast()),
            sacl_len,
            Some(windows::Win32::Security::PSID(owner.cast())),
            owner_len,
            Some(windows::Win32::Security::PSID(group.cast())),
            group_len,
        ) {
            warn!("COM access for the signed-in user not set: {e}");
            return;
        }
        match CoInitializeSecurity(
            Some(PSECURITY_DESCRIPTOR(abs.cast())),
            -1,
            None,
            None,
            RPC_C_AUTHN_LEVEL_DEFAULT,
            RPC_C_IMP_LEVEL_IDENTIFY,
            None,
            EOAC_NONE,
            None,
        ) {
            Ok(()) => debug!("COM calls from the signed-in user allowed (pasted files)"),
            Err(e) => warn!("COM access for the signed-in user not set: {e}"),
        }
    }
}

/// The process runs as LocalSystem.
pub fn running_as_system() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{GetTokenInformation, IsWellKnownSid, TOKEN_QUERY, TOKEN_USER, TokenUser, WinLocalSystemSid};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // SAFETY: the process's own token, queried and closed.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut buf = [0u8; 256];
        let mut len = 0;
        let ok = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), buf.len() as u32, &mut len).is_ok();
        let _ = CloseHandle(token);
        ok && IsWellKnownSid((*(buf.as_ptr() as *const TOKEN_USER)).User.Sid, WinLocalSystemSid).as_bool()
    }
}
