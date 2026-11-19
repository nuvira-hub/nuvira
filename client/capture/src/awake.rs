//! Keeps the display on while a session captures it (Windows).
//!
//! With the monitor off for power saving, DWM presents nothing on some machines
//! (PC-LABORATORIO: not a single DXGI frame, review VID-10) and the viewer sees no
//! picture. A power request ([`DisplayAwake`]) stops the power plan from turning
//! the display off while it is held, the same as macOS's IOPMAssertion
//! (`macos.rs`). It does not turn a display that is already off back on (the
//! documented effect is "resetting the display idle timer"): the agent wakes it with
//! an injected one-pixel move (`nuvira_input::wake_display`) when no picture comes.
//!
//! [`display_off`] reads the console display state, for the agent and the tests.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use tracing::{debug, info};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Power::{
    DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, POWERBROADCAST_SETTING, PowerClearRequest, PowerCreateRequest,
    PowerRequestDisplayRequired, PowerRequestSystemRequired, PowerSetRequest,
    PowerSettingRegisterNotification,
};
use windows::Win32::System::SystemServices::{GUID_CONSOLE_DISPLAY_STATE, POWER_REQUEST_CONTEXT_VERSION};
use windows::Win32::System::Threading::{POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT};
use windows::Win32::UI::WindowsAndMessaging::{
    DEVICE_NOTIFY_CALLBACK, GetSystemMetrics, PBT_POWERSETTINGCHANGE, SM_REMOTESESSION,
};
use windows::core::PWSTR;

/// What `powercfg /requests` shows for the agent.
const REASON: &str = "NuviraHub remote session";

/// A power request that keeps the display on and the system awake, released
/// when dropped (and by Windows when the process ends, so a crash cannot leave it).
/// The documentation asks for both: "DisplayRequired" alone does not stop sleep.
/// Neither stops a sleep the person asks for (lid, power button) nor the screen
/// saver.
pub struct DisplayAwake {
    request: HANDLE,
}

// A kernel handle, usable from any thread.
unsafe impl Send for DisplayAwake {}

impl DisplayAwake {
    /// `None` (logged) if Windows refuses the request: the session goes on as before.
    pub fn new() -> Option<Self> {
        let mut reason: Vec<u16> = REASON.encode_utf16().chain([0]).collect();
        let mut context = REASON_CONTEXT {
            Version: POWER_REQUEST_CONTEXT_VERSION,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            ..Default::default()
        };
        context.Reason.SimpleReasonString = PWSTR(reason.as_mut_ptr());
        // SAFETY: the context and its string live through the call (Windows copies it).
        let request = match unsafe { PowerCreateRequest(&context) } {
            Ok(request) => request,
            Err(e) => {
                info!("display power request not created: {e}");
                return None;
            }
        };
        let awake = Self { request };
        for (kind, name) in [(PowerRequestDisplayRequired, "display"), (PowerRequestSystemRequired, "system")]
        {
            // SAFETY: a request handle of this process.
            if let Err(e) = unsafe { PowerSetRequest(awake.request, kind) } {
                info!("{name} power request refused: {e}");
            }
        }
        debug!("display kept on while the capture is open");
        Some(awake)
    }
}

impl Drop for DisplayAwake {
    fn drop(&mut self) {
        // SAFETY: the handle created in `new`, closed once.
        unsafe {
            let _ = PowerClearRequest(self.request, PowerRequestDisplayRequired);
            let _ = PowerClearRequest(self.request, PowerRequestSystemRequired);
            let _ = CloseHandle(self.request);
        }
        debug!("display power request released");
    }
}

/// The console display state as Windows last announced it: 0 off, 1 on, 2 dimmed;
/// [`UNKNOWN`] until the first notification.
static STATE: AtomicU8 = AtomicU8::new(UNKNOWN);
const UNKNOWN: u8 = u8::MAX;

/// Called by Windows with the console display state: once at registration, then at
/// every change.
unsafe extern "system" fn on_state(
    _context: *const core::ffi::c_void,
    kind: u32,
    setting: *const core::ffi::c_void,
) -> u32 {
    if kind == PBT_POWERSETTINGCHANGE && !setting.is_null() {
        // SAFETY: for PBT_POWERSETTINGCHANGE `setting` is a POWERBROADCAST_SETTING
        // with DataLength bytes of data (a DWORD for this setting).
        let setting = unsafe { &*setting.cast::<POWERBROADCAST_SETTING>() };
        if setting.PowerSetting == GUID_CONSOLE_DISPLAY_STATE && setting.DataLength >= 4 {
            let value = unsafe { std::ptr::read_unaligned(setting.Data.as_ptr().cast::<u32>()) };
            let state = value.min(2) as u8;
            if STATE.swap(state, Ordering::Relaxed) != state {
                debug!(state, "console display state");
            }
        }
    }
    0
}

/// The console display is off (power saving): Windows' own notification
/// (`GUID_CONSOLE_DISPLAY_STATE`). `false` when unknown.
pub fn display_off() -> bool {
    display_state() == Some(0)
}

/// The console display state, 0 off, 1 on, 2 dimmed (`None` until Windows has told
/// it, shortly after the first call, which registers for it for the life of the
/// process).
pub fn display_state() -> Option<u8> {
    static REGISTERED: OnceLock<bool> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        let params = Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
            Callback: Some(on_state),
            Context: std::ptr::null_mut(),
        }));
        let mut handle = std::ptr::null_mut();
        // SAFETY: the parameters are leaked (they must outlive the registration,
        // which lasts as long as the process).
        let ret = unsafe {
            PowerSettingRegisterNotification(
                &GUID_CONSOLE_DISPLAY_STATE,
                DEVICE_NOTIFY_CALLBACK,
                HANDLE(std::ptr::from_mut(params).cast()),
                &mut handle,
            )
        };
        if ret.is_err() {
            info!("console display state not followed: {ret:?}");
        }
        ret.is_ok()
    });
    let state = STATE.load(Ordering::Relaxed);
    (state != UNKNOWN).then_some(state)
}

/// This process runs in a Remote Desktop session: there is no monitor of its own to
/// wake (the power request is harmless there).
pub fn remote_session() -> bool {
    // SAFETY: a plain query.
    unsafe { GetSystemMetrics(SM_REMOTESESSION) != 0 }
}
