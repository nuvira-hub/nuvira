//! nuvira-agent: the controlled device from the command line (see `nuvira_agent::cli`).

fn main() -> anyhow::Result<()> {
    // macOS: the black screens of privacy mode are this program started again.
    nuvira_input::privacy::overlay_if_asked();
    // macOS: the wallpaper's helper (see nuvira_capture::wallpaper).
    nuvira_capture::wallpaper::helper_if_asked();
    nuvira_agent::cli::run(std::env::args_os())
}
