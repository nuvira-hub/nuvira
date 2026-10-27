//! Desktop di input di Windows.
//!
//! Mouse, tastiera e schermo appartengono a un desktop che cambia: `Default` per
//! l'utente, `Winlogon` per le richieste di amministratore (UAC), la schermata di
//! blocco e Ctrl+Alt+Canc. Cattura e iniezione dell'input agiscono sul desktop del
//! proprio thread, quindi i thread che le eseguono devono seguire quello attivo.
//!
//! Solo un processo SYSTEM (avviato da nexodesk-service) può aprire `Winlogon`:
//! da utente normale il passaggio fallisce e la cattura riprende al ritorno sul
//! desktop dell'utente.

use std::cell::Cell;

use tracing::debug;
use windows::Win32::Foundation::{GENERIC_ALL, HANDLE};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetThreadDesktop, GetUserObjectInformationW, HDESK,
    OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::GetCurrentThreadId;

thread_local! {
    /// Desktop aperto da noi e assegnato al thread: va chiuso quando si cambia.
    static OWNED: Cell<Option<HDESK>> = const { Cell::new(None) };
}

/// Sposta il thread corrente sul desktop che riceve l'input, se è cambiato.
/// Restituisce il nome del nuovo desktop quando c'è stato un passaggio.
pub fn follow_input_desktop() -> Option<String> {
    let input = match unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0)) } {
        Ok(desk) => desk,
        Err(e) => {
            // Normale per un processo non SYSTEM quando è attivo il desktop sicuro.
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
