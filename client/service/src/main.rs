//! nuvira-service: unattended access on Windows (see the `nuvira_service` library).

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    nuvira_service::run(std::env::args_os())
}

/// On Linux and macOS the service is part of the `nuvira` program (`nuvira service ...`):
/// its units and self-invocations name that program, so this standalone binary could
/// not install or start anything there (P9). It is not shipped on those systems.
#[cfg(not(windows))]
fn main() {
    eprintln!("nuvira-service is for Windows only: use `nuvira service ...` instead");
    std::process::exit(2);
}
