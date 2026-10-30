//! nuvira-viewer: the remote session window (see the `nuvira_viewer` library).

fn main() -> anyhow::Result<()> {
    nuvira_viewer::run(std::env::args_os())
}
