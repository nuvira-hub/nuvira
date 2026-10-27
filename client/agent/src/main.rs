//! nexodesk-agent: the controlled device from the command line (see `nexodesk_agent::cli`).

fn main() -> anyhow::Result<()> {
    nexodesk_agent::cli::run(std::env::args_os())
}
