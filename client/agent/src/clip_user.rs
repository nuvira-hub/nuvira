//! Windows, agent running as SYSTEM (the service's): what Explorer and many
//! programs copy is on the clipboard only as their OLE object, rendered on
//! request, and a SYSTEM process sees nothing of it — neither the copied files
//! nor, from programs such as Word, the text. This program is then started for
//! a moment as the signed-in person (`--clipboard-files`), reads the clipboard
//! as they would and prints it on one line, as JSON.

use std::io::Read;
use std::os::windows::io::FromRawHandle;
use std::path::PathBuf;
use std::time::Duration;

use tracing::debug;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
    STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::core::{HSTRING, PWSTR};

/// The line the helper prints, before its JSON.
const MARK: &str = "NUVIRA-CLIPBOARD ";

/// What the person's clipboard has, as read by the helper.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Seen {
    files: Vec<PathBuf>,
    text: Option<String>,
}

/// `--clipboard-files`: prints the clipboard's files and text (as the person
/// who started it) and exits.
pub fn print() {
    let (files, text) = nuvira_clipboard::peek();
    let seen = Seen { files, text };
    println!("{MARK}{}", serde_json::to_string(&seen).unwrap_or_default());
}

/// The clipboard of the person signed in, read by this program started as
/// them; `None` when it has neither files nor text, or it cannot be read.
pub fn read_as_user() -> Option<nuvira_clipboard::Content> {
    let token = crate::files::session_user_token().ok()??;
    let seen = run_helper(token);
    // SAFETY: the token from WTSQueryUserToken, closed once.
    unsafe {
        let _ = CloseHandle(token);
    }
    let seen = seen?;
    if !seen.files.is_empty() {
        return Some(nuvira_clipboard::Content::Files(seen.files));
    }
    seen.text.filter(|t| !t.is_empty()).map(nuvira_clipboard::Content::Text)
}

fn run_helper(token: HANDLE) -> Option<Seen> {
    let exe = std::env::current_exe().ok()?;
    // The command-line agent takes the option directly; the app as "agent ...".
    let args = match exe.file_stem().and_then(|s| s.to_str()) {
        Some("nuvira-agent") => "--clipboard-files",
        _ => "agent --clipboard-files",
    };
    let mut command: Vec<u16> = format!("\"{}\" {args}", exe.display()).encode_utf16().chain(Some(0)).collect();
    let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain(Some(0)).collect();
    // SAFETY: a pipe for the helper's output (its end inherited, ours not), the
    // process started with the person's token and environment, waited for at
    // most a few seconds and stopped otherwise; every handle closed.
    unsafe {
        let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
        let inherit =
            SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, bInheritHandle: true.into(), ..Default::default() };
        CreatePipe(&mut read, &mut write, Some(&inherit), 0).ok()?;
        let _ = SetHandleInformation(read, HANDLE_FLAG_INHERIT.0, Default::default());
        let mut env = std::ptr::null_mut();
        let _ = CreateEnvironmentBlock(&mut env, Some(token), false);
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            dwFlags: STARTF_USESTDHANDLES,
            hStdOutput: write,
            hStdError: write,
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();
        let started = CreateProcessAsUserW(
            Some(token),
            &HSTRING::from(exe.as_os_str()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            (!env.is_null()).then_some(env as *const _),
            None,
            &startup,
            &mut info,
        );
        let _ = CloseHandle(write);
        if !env.is_null() {
            let _ = DestroyEnvironmentBlock(env);
        }
        if let Err(e) = started {
            let _ = CloseHandle(read);
            debug!("clipboard helper not started: {e}");
            return None;
        }
        // Read while it runs: a long list would fill the pipe and hold it.
        let mut pipe = std::fs::File::from_raw_handle(read.0);
        let reader = std::thread::spawn(move || {
            let mut out = String::new();
            let _ = pipe.read_to_string(&mut out);
            out
        });
        if WaitForSingleObject(info.hProcess, Duration::from_secs(5).as_millis() as u32) != WAIT_OBJECT_0 {
            let _ = TerminateProcess(info.hProcess, 1);
        }
        let _ = CloseHandle(info.hProcess);
        let _ = CloseHandle(info.hThread);
        let out = reader.join().ok()?;
        let line = out.lines().find_map(|l| l.strip_prefix(MARK))?;
        serde_json::from_str(line).ok()
    }
}
