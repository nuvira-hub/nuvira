//! nuvira-agent: the controlled device from the command line (see `nuvira_agent::cli`).

fn main() -> anyhow::Result<()> {
    nuvira_agent::cli::run(std::env::args_os())
}
