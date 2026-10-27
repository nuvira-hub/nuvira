//! Windows input desktop.
//!
//! Mouse, keyboard and screen belong to a desktop that changes: `Default` for the
//! user, `Winlogon` for administrator prompts (UAC), the lock screen and
//! Ctrl+Alt+Del. Capture and input injection act on the desktop of their own
//! thread, so the threads doing them must follow the active one.
//!
//! Only a SYSTEM process (started by nexodesk-service) can open `Winlogon`: as a
//! normal user the switch fails and capture resumes when the user's desktop
//! comes back.

use std::cell::Cell;

use tracing::debug;
use windows::Win32::Foundation::{GENERIC_ALL, HANDLE};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetThreadDesktop, GetUserObjectInformationW, HDESK,
    OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::GetCurrentThreadId;

thread_local! {
    /// Desktop opened by us and assigned to the thread: closed when switching.
    static OWNED: Cell<Option<HDESK>> = const { Cell::new(None) };
}

/// Moves the current thread to the desktop receiving input, if it changed.
/// Returns the name of the new desktop when a switch happened.
pub fn follow_input_desktop() -> Option<String> {
    let input = match unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0)) } {
        Ok(desk) => desk,
        Err(e) => {
            // Normal for a non-SYSTEM process while the secure desktop is active.
            debug!("OpenInputDesktop: {e}");
            return None;
        }
    };
    let current = unsafe { GetThreadDesktop(GetCurrentThreadId()) }.ok();
    let name = desktop_name(input);
    if current.is_some_and(|c| desktop_name(c) == name) {
        let _ = unsafe { CloseDesktop(input) };
        return None;
    }
    if let Err(e) = unsafe { SetThreadDesktop(input) } {
        debug!("SetThreadDesktop({name}): {e}");
        let _ = unsafe { CloseDesktop(input) };
        return None;
    }
    if let Some(old) = OWNED.replace(Some(input)) {
        let _ = unsafe { CloseDesktop(old) };
    }
    Some(name)
}

fn desktop_name(desk: HDESK) -> String {
    let mut buf = [0u16; 128];
    let mut needed = 0u32;
    let ok = unsafe {
        GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            size_of_val(&buf) as u32,
            Some(&mut needed),
        )
    };
    if ok.is_err() {
        return String::new();
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}
