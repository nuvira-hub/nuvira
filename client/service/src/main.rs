//! nuvira-service: unattended access on Windows (see the `nuvira_service` library).

fn main() -> anyhow::Result<()> {
    nuvira_service::run(std::env::args_os())
}
