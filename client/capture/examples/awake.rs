//! Display power on Windows, for the display-awake tests (review lot 13, VID-10):
//!
//! - `watch SECONDS`: prints the console display state (0 off, 1 on, 2 dimmed)
//!   at every change; works over SSH too (the state is global);
//! - `hold SECONDS`: the same while holding the session's power request
//!   ([`nuvira_capture::awake::DisplayAwake`], seen in `powercfg /requests`);
//! - `off`: turns the monitor off as the power plan does (`SC_MONITORPOWER` 2);
//! - `on`: `SC_MONITORPOWER` -1, the documented "on" (unreliable since Windows 8);
//! - `wake`: the agent's one-pixel wake-up move ([`nuvira_input::wake_display`]).
//!
//! The last three need the signed-in desktop (a scheduled task with `/it`).

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};

    use nuvira_capture::awake;
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, PostMessageW, SC_MONITORPOWER, WM_SYSCOMMAND,
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    let seconds = |i: usize| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(10u64);
    let monitor = |value: isize| unsafe {
        let _ = PostMessageW(
            Some(HWND_BROADCAST),
            WM_SYSCOMMAND,
            WPARAM(SC_MONITORPOWER as usize),
            LPARAM(value),
        );
    };
    let watch = |secs: u64| {
        let start = Instant::now();
        let mut last = None;
        while start.elapsed() < Duration::from_secs(secs) {
            let state = awake::display_state();
            if state != last {
                println!("{:>8.1} s display state {state:?}", start.elapsed().as_secs_f64());
                last = state;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        println!("{:>8.1} s end, display state {last:?}", start.elapsed().as_secs_f64());
    };
    match args.first().map(String::as_str) {
        Some("watch") => watch(seconds(1)),
        Some("hold") => {
            let request = awake::DisplayAwake::new();
            println!("power request held: {}", request.is_some());
            watch(seconds(1));
        }
        Some("off") => monitor(2),
        Some("on") => monitor(-1),
        Some("wake") => nuvira_input::wake_display(),
        _ => anyhow::bail!("usage: awake watch|hold SECONDS | off | on | wake"),
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}
