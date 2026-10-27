//! nexodesk-service: unattended access on Windows (see the `nexodesk_service` library).

fn main() -> anyhow::Result<()> {
    nexodesk_service::run(std::env::args_os())
}
