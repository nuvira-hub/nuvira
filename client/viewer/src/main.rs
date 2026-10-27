//! nexodesk-viewer: the remote session window (see the `nexodesk_viewer` library).

fn main() -> anyhow::Result<()> {
    nexodesk_viewer::run(std::env::args_os())
}
