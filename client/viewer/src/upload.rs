//! Sends a session recording to the server (NuviraHub Pro, feature "pro.recording").
//!
//! The session window starts `nuvira viewer upload ...` as a process of its own for
//! every recording it closes, so that the upload goes on after the window is
//! closed. It registers on the server with the viewer's key (the server checks that
//! this device was the viewer of the session) and sends the file on a further
//! stream of the signalling connection (`Upload` in rendezvous.proto), inside the
//! same QUIC encryption as everything else towards the server. An interrupted
//! upload goes on from the bytes the server already has; failures are tried again
//! for some hours. The file stays on this computer.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use nuvira_proto::v1::{Upload, UploadReady, UploadResult};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::{info, warn};

/// Waits between attempts: about 6 hours in all.
const RETRIES: [u64; 12] = [10, 30, 60, 120, 300, 600, 900, 1800, 3600, 3600, 3600, 3600];

#[derive(Parser, Debug)]
#[command(name = "nuvira viewer upload", about = "Sends a session recording to the NuviraHub server")]
struct Args {
    /// nuvira-server the session went through (host[:port]).
    #[arg(long)]
    server: String,
    /// Fingerprint of its certificate, as seen by the session.
    #[arg(long)]
    server_key: Option<String>,
    #[arg(long, default_value = "auto")]
    transport: String,
    #[arg(long)]
    server_ws: Option<String>,
    /// The viewer's key (the server knows the device by it).
    #[arg(long)]
    identity: PathBuf,
    /// Session the recording belongs to.
    #[arg(long)]
    session: String,
    file: PathBuf,
}

/// Where the upload goes, known once the session has its route.
#[derive(Debug, Clone)]
pub struct Target {
    pub server: String,
    pub server_key: String,
    pub transport: nuvira_transport::signal::TransportMode,
    pub server_ws: Option<String>,
    pub identity: PathBuf,
    pub session: String,
}

/// Starts the upload of `file` in a process of its own, which outlives the window.
/// Its output goes to `upload.log` in the data folder.
pub fn spawn(target: &Target, file: &Path) {
    let result = (|| -> Result<()> {
        let exe = std::env::current_exe()?;
        let mut command = std::process::Command::new(exe);
        // Inside the app the viewer is `nuvira viewer ...`; on its own `nuvira-viewer ...`.
        if std::env::args().nth(1).as_deref() == Some("viewer") {
            command.arg("viewer");
        }
        use nuvira_transport::signal::TransportMode;
        let transport = match target.transport {
            TransportMode::Auto => "auto",
            TransportMode::Udp => "udp",
            TransportMode::WebSocket => "websocket",
        };
        command.args(["upload", "--server", &target.server, "--server-key", &target.server_key]);
        command.args(["--transport", transport, "--session", &target.session]);
        if let Some(ws) = &target.server_ws {
            command.args(["--server-ws", ws]);
        }
        command.arg("--identity").arg(&target.identity).arg(file);
        let log_path = nuvira_transport::identity::data_dir().join("upload.log");
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
        command.stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console window, and not ended with the session window.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            command.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
        }
        let child = command.spawn()?;
        info!(pid = child.id(), "upload of {} to the server started", file.display());
        Ok(())
    })();
    if let Err(e) = result {
        warn!("upload of {} not started: {e:#}", file.display());
    }
}

pub fn run(args: impl IntoIterator<Item = OsString>) -> Result<()> {
    let _ = tracing_subscriber::fmt().with_ansi(false).with_env_filter("info").try_init();
    let args = Args::parse_from(args);
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        for (attempt, wait) in std::iter::once(0).chain(RETRIES).enumerate() {
            tokio::time::sleep(Duration::from_secs(wait)).await;
            match send(&args).await {
                Ok(()) => return Ok(()),
                Err(e) if e.downcast_ref::<Refused>().is_some() => {
                    warn!("{}: not sent, {e:#}", args.file.display());
                    return Err(e);
                }
                Err(e) => warn!(attempt, "{}: upload failed, trying again: {e:#}", args.file.display()),
            }
        }
        bail!("{}: not sent, too many attempts", args.file.display())
    })
}

/// The server refuses the file: trying again would not change it.
#[derive(Debug)]
struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "refused by the server: {}", nuvira_i18n::message(&self.0))
    }
}

impl std::error::Error for Refused {}

/// Errors of the server that do not depend on the moment.
fn permanent(error: &str) -> bool {
    matches!(
        error,
        "@server.pro_required"
            | "@server.recording_session"
            | "@server.recording_name"
            | "@server.recording_too_large"
    )
}

async fn send(args: &Args) -> Result<()> {
    use nuvira_transport::{identity::Identity, signal};
    let mut file =
        tokio::fs::File::open(&args.file).await.with_context(|| args.file.display().to_string())?;
    let size = file.metadata().await?.len();
    let name = args.file.file_name().and_then(|n| n.to_str()).context("file name")?.to_string();

    let identity = Identity::load_or_create(&args.identity)?;
    let server = nuvira_transport::resolve_addr(&args.server, signal::DEFAULT_PORT)?;
    let expected = match &args.server_key {
        Some(key) => Some(nuvira_transport::parse_fingerprint(key)?),
        None => signal::known_server::get(&args.server),
    };
    let transport = match args.transport.as_str() {
        "udp" => signal::TransportMode::Udp,
        "websocket" => signal::TransportMode::WebSocket,
        _ => signal::TransportMode::Auto,
    };
    let ws_url =
        args.server_ws.clone().unwrap_or_else(|| nuvira_transport::ws::default_url(&args.server, server));
    let device_name = nuvira_transport::device_name();
    let device = signal::DeviceInfo { name: &device_name, reachable: false };
    let sig = signal::connect_auto(server, &ws_url, transport, expected, &identity, &device).await?;
    if !sig.features.iter().any(|f| f == "pro.recording") {
        return Err(Refused("@server.pro_required".into()).into());
    }

    let (mut tx, mut rx) = sig.conn.open_bi().await?;
    let header =
        Upload { kind: "recording".into(), session_id: args.session.clone(), name: name.clone(), size };
    tx.write_all(&nuvira_proto::framing::encode(&header)).await?;
    let ready =
        nuvira_transport::recv_one::<UploadReady>(&mut rx).await?.context("no answer from the server")?;
    if !ready.error.is_empty() {
        if permanent(&ready.error) {
            return Err(Refused(ready.error).into());
        }
        bail!("{}", nuvira_i18n::message(&ready.error));
    }
    let offset = ready.offset.min(size);
    info!(name, size, offset, session = args.session, "sending to the server");
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut sent = offset;
    let started = std::time::Instant::now();
    while sent < size {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            bail!("the file got shorter while being sent");
        }
        tx.write_all(&buf[..n]).await?;
        sent += n as u64;
    }
    tx.finish()?;
    let result =
        nuvira_transport::recv_one::<UploadResult>(&mut rx).await?.context("no result from the server")?;
    if !result.ok {
        if permanent(&result.error) {
            return Err(Refused(result.error).into());
        }
        bail!("{}", nuvira_i18n::message(&result.error));
    }
    let seconds = started.elapsed().as_secs_f64().max(0.001);
    info!(
        name,
        bytes = size - offset,
        mbit_s = format!("{:.1}", (size - offset) as f64 * 8.0 / seconds / 1e6),
        "recording sent to the server"
    );
    sig.conn.close(0u32.into(), b"");
    Ok(())
}
