//! Installing, removing and checking the service (from an administrator prompt).

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::ERROR_SERVICE_DOES_NOT_EXIST;
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ENUM_SERVICE_TYPE, SERVICE_ERROR,
    SERVICE_NO_CHANGE, SERVICE_START_TYPE, ControlService, CreateServiceW, DeleteService, OpenSCManagerW,
    OpenServiceW, QueryServiceStatus, SC_ACTION, SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_ALL_ACCESS,
    SC_MANAGER_CONNECT, SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL,
    SERVICE_FAILURE_ACTIONSW, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE,
    SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS, StartServiceW,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use crate::config::{self, ServiceConfig};

const FIREWALL_RULE: &str = "NexoDesk agent";

/// Service manager handle, closed automatically.
struct Sc(SC_HANDLE);

impl Drop for Sc {
    fn drop(&mut self) {
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}

fn manager(access: u32) -> Result<Sc> {
    unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access) }
        .map(Sc)
        .context("access to the service manager denied: an administrator command prompt is needed")
}

/// `None` if the service is not installed.
fn open_service(scm: &Sc, access: u32) -> Result<Option<Sc>> {
    match unsafe { OpenServiceW(scm.0, &HSTRING::from(config::SERVICE_NAME), access) } {
        Ok(h) => Ok(Some(Sc(h))),
        Err(e) if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => Ok(None),
        Err(e) => Err(e).context("OpenService"),
    }
}

fn state(service: &Sc) -> Result<SERVICE_STATUS_CURRENT_STATE> {
    let mut status = SERVICE_STATUS::default();
    unsafe { QueryServiceStatus(service.0, &mut status) }.context("QueryServiceStatus")?;
    Ok(status.dwCurrentState)
}

fn wait_state(service: &Sc, wanted: SERVICE_STATUS_CURRENT_STATE, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if state(service)? == wanted {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(false)
}

fn stop(service: &Sc) -> Result<()> {
    if state(service)? == SERVICE_STOPPED {
        return Ok(());
    }
    let mut status = SERVICE_STATUS::default();
    // It may fail if it is already stopping: only the final state counts.
    let _ = unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) };
    if !wait_state(service, SERVICE_STOPPED, Duration::from_secs(20))? {
        bail!("the service did not stop within 20 s");
    }
    Ok(())
}

pub struct InstallOptions {
    pub server: String,
    pub server_key: Option<String>,
    pub password: Option<String>,
    pub transport: String,
    pub server_ws: Option<String>,
}

pub fn install(opts: InstallOptions) -> Result<()> {
    let scm = manager(SC_MANAGER_ALL_ACCESS)?;
    let existing = open_service(&scm, SERVICE_ALL_ACCESS)?;
    if let Some(service) = &existing {
        println!("service already installed: stopping it to update it");
        stop(service)?;
    }

    // The program in Program Files: the service (SYSTEM) must not run files a user can replace.
    let src = source_program()?;
    let program_dir = config::program_dir();
    std::fs::create_dir_all(&program_dir).with_context(|| format!("creating {}", program_dir.display()))?;
    let program = program_dir.join(config::PROGRAM_EXE);
    if !same_file(&src, &program) {
        std::fs::copy(&src, &program).with_context(|| format!("copying {} to {}", src.display(), program.display()))?;
    }
    for old in config::OLD_EXES {
        let _ = std::fs::remove_file(program_dir.join(old));
    }

    // Data readable only by SYSTEM and administrators: it holds the key and the password.
    let data_dir = config::data_dir();
    std::fs::create_dir_all(&data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    // The rules are set on the folder and the files inherit them: with /T applied to the
    // files too, (OI)(CI) is not valid for a file and on reinstall the files lost all permissions.
    run(Command::new("icacls").arg(&data_dir).args([
        "/inheritance:r",
        "/grant:r",
        "*S-1-5-18:(OI)(CI)F",
        "*S-1-5-32-544:(OI)(CI)F",
        "/Q",
    ]))
    .context("protecting the data folder")?;
    if std::fs::read_dir(&data_dir)?.next().is_some() {
        run(Command::new("icacls").arg(data_dir.join("*")).args(["/reset", "/T", "/Q"]))
            .context("permissions of the files in the data folder")?;
    }

    // Reinstalling without --password keeps the one already set.
    let previous = ServiceConfig::load().ok();
    let password = opts
        .password
        .or_else(|| previous.map(|c| c.password))
        .unwrap_or_else(config::random_password);
    let service_config = ServiceConfig {
        server: opts.server,
        server_key: opts.server_key,
        password: password.clone(),
        transport: opts.transport,
        server_ws: opts.server_ws,
    };
    service_config.save()?;

    let service = match existing {
        Some(service) => {
            // Earlier installations ran nexodesk-service.exe.
            set_command(&service, &program)?;
            service
        }
        None => create(&scm, &program)?,
    };

    // Inbound rule for the direct (UDP) path towards the agent.
    let agent = &program;
    let _ = run(Command::new("netsh").raw_arg(format!(
        "advfirewall firewall delete rule name=\"{FIREWALL_RULE}\""
    )));
    run(Command::new("netsh").raw_arg(format!(
        "advfirewall firewall add rule name=\"{FIREWALL_RULE}\" dir=in action=allow program=\"{}\" enable=yes profile=any",
        agent.display()
    )))
    .context("firewall rule")?;

    allow_software_sas()?;

    let _ = std::fs::remove_file(config::online_path());
    unsafe { StartServiceW(service.0, None) }.context("starting the service")?;
    if !wait_state(&service, SERVICE_RUNNING, Duration::from_secs(20))? {
        bail!("the service did not start: see {}", config::service_log().display());
    }
    println!("service installed and started");
    match wait_online(Duration::from_secs(30)) {
        Some(id) => println!("ID {id}, password {password}"),
        None => println!(
            "the agent has not registered on {} yet: check {}",
            service_config.server,
            config::agent_log().display()
        ),
    }
    Ok(())
}

/// "Allow software to generate SAS" policy (SoftwareSASGeneration): lets services
/// send Ctrl+Alt+Del (`SendSAS`). Values: 0 none, 1 services, 2 Ease of Access
/// applications, 3 both. Services are added without removing the rest.
fn allow_software_sas() -> Result<()> {
    const KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
    let output = Command::new("reg").args(["query", KEY, "/v", "SoftwareSASGeneration"]).output()?;
    let current = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .last()
        .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        .filter(|_| output.status.success())
        .unwrap_or(0);
    if current & 1 == 0 {
        run(Command::new("reg").args([
            "add",
            KEY,
            "/v",
            "SoftwareSASGeneration",
            "/t",
            "REG_DWORD",
            "/d",
            &(current | 1).to_string(),
            "/f",
        ]))
        .context("SoftwareSASGeneration policy")?;
    }
    Ok(())
}

/// The program to install: this one when it is the app (`nexodesk.exe service install`,
/// whatever the file is called), otherwise the app next to nexodesk-service.exe.
fn source_program() -> Result<std::path::PathBuf> {
    let here = std::env::current_exe()?;
    let name = here.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_ascii_lowercase();
    if !name.starts_with("nexodesk-service") {
        return Ok(here);
    }
    let app = here.with_file_name(config::PROGRAM_EXE);
    if !app.exists() {
        bail!("{} not found: it must be in the same folder as nexodesk-service.exe", app.display());
    }
    Ok(app)
}

fn service_command(program: &Path) -> String {
    format!("\"{}\" service run", program.display())
}

fn set_command(service: &Sc, program: &Path) -> Result<()> {
    unsafe {
        ChangeServiceConfigW(
            service.0,
            ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
            SERVICE_START_TYPE(SERVICE_NO_CHANGE),
            SERVICE_ERROR(SERVICE_NO_CHANGE),
            &HSTRING::from(service_command(program)),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )
    }
    .context("ChangeServiceConfig")
}

fn create(scm: &Sc, exe: &Path) -> Result<Sc> {
    let command = service_command(exe);
    let service = unsafe {
        CreateServiceW(
            scm.0,
            &HSTRING::from(config::SERVICE_NAME),
            &HSTRING::from(config::SERVICE_DISPLAY_NAME),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            &HSTRING::from(command),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            // No account: LocalSystem.
            PCWSTR::null(),
            PCWSTR::null(),
        )
    }
    .map(Sc)
    .context("CreateService")?;

    let mut description: Vec<u16> = config::SERVICE_DESCRIPTION.encode_utf16().chain(Some(0)).collect();
    let info = SERVICE_DESCRIPTIONW { lpDescription: PWSTR(description.as_mut_ptr()) };
    let _ = unsafe { ChangeServiceConfig2W(service.0, SERVICE_CONFIG_DESCRIPTION, Some((&raw const info).cast())) };

    // If the service crashes, Windows restarts it after 5 s (the counter resets every day).
    let mut actions = [SC_ACTION { Type: SC_ACTION_RESTART, Delay: 5_000 }; 3];
    let failure = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 24 * 3600,
        lpRebootMsg: PWSTR::null(),
        lpCommand: PWSTR::null(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    let _ = unsafe { ChangeServiceConfig2W(service.0, SERVICE_CONFIG_FAILURE_ACTIONS, Some((&raw const failure).cast())) };
    Ok(service)
}

pub fn uninstall() -> Result<()> {
    let scm = manager(SC_MANAGER_ALL_ACCESS)?;
    let Some(service) = open_service(&scm, SERVICE_ALL_ACCESS)? else {
        println!("the service is not installed");
        return Ok(());
    };
    stop(&service)?;
    unsafe { DeleteService(service.0) }.context("DeleteService")?;
    let _ = run(Command::new("netsh").raw_arg(format!(
        "advfirewall firewall delete rule name=\"{FIREWALL_RULE}\""
    )));
    println!("service removed");
    println!(
        "the programs remain in {} and the data in {} (with the device ID, reused if you reinstall)",
        config::program_dir().display(),
        config::data_dir().display()
    );
    Ok(())
}

pub fn status() -> Result<()> {
    let scm = manager(SC_MANAGER_CONNECT)?;
    let Some(service) = open_service(&scm, SERVICE_QUERY_STATUS)? else {
        println!("service: not installed");
        return Ok(());
    };
    let state = state(&service)?;
    let text = match state {
        SERVICE_RUNNING => "running",
        SERVICE_STOPPED => "stopped",
        _ => "changing state",
    };
    println!("service: {text}");
    match std::fs::read_to_string(config::online_path()) {
        Ok(info) => match info.split_whitespace().next() {
            Some(id) => println!("ID {}", format_id(id)),
            None => println!("ID: not registered yet"),
        },
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            println!("ID: visible only from an administrator command prompt")
        }
        Err(_) => println!("ID: not registered yet"),
    }
    println!("logs: {} and {}", config::service_log().display(), config::agent_log().display());
    Ok(())
}

/// Waits for the agent to write the ID file after registering.
fn wait_online(timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(info) = std::fs::read_to_string(config::online_path())
            && let Some(id) = info.split_whitespace().next()
        {
            return Some(format_id(id));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    None
}

fn format_id(id: &str) -> String {
    if id.len() == 9 { format!("{} {} {}", &id[..3], &id[3..6], &id[6..]) } else { id.to_string() }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn run(command: &mut Command) -> Result<()> {
    let output = command.output().context("starting the command")?;
    if !output.status.success() {
        bail!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
