//! Installing, removing and checking the service (from an administrator prompt).

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::ERROR_SERVICE_DOES_NOT_EXIST;
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService, CreateServiceW,
    DeleteService, ENUM_SERVICE_TYPE, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_ACTION,
    SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_ALL_ACCESS, SC_MANAGER_CONNECT, SERVICE_ALL_ACCESS,
    SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION, SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_STOP,
    SERVICE_DESCRIPTIONW, SERVICE_ERROR, SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW, SERVICE_NO_CHANGE,
    SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START_TYPE, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE,
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

/// Outcome of an installation.
pub struct Installed {
    /// Device ID, if the agent registered within the wait.
    pub id: Option<String>,
    /// The permanent password, if it was generated here (otherwise whoever installs
    /// knows it, and only its hash is stored).
    pub generated_password: Option<String>,
}

pub fn install(opts: InstallOptions) -> Result<Installed> {
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
        replace_program(&src, &program)?;
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
    // Shown only when generated here: whoever chose it knows it, and the output of an
    // automatic update goes to a log, where passwords never go. Only the hash is stored.
    let (password_hash, generated_password) = match (opts.password, previous) {
        (Some(password), _) => (nexodesk_secret::hash(&password), None),
        (None, Some(previous)) => (previous.password_hash, None),
        (None, None) => {
            let password = config::random_password();
            (nexodesk_secret::hash(&password), Some(password))
        }
    };
    let service_config = ServiceConfig {
        server: opts.server,
        server_key: opts.server_key,
        password_hash,
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
    let _ =
        run(Command::new("netsh")
            .raw_arg(format!("advfirewall firewall delete rule name=\"{FIREWALL_RULE}\"")));
    run(Command::new("netsh").raw_arg(format!(
        "advfirewall firewall add rule name=\"{FIREWALL_RULE}\" dir=in action=allow program=\"{}\" enable=yes profile=any",
        agent.display()
    )))
    .context("firewall rule")?;

    allow_software_sas()?;
    register_app(&program)?;

    let _ = std::fs::remove_file(config::online_path());
    unsafe { StartServiceW(service.0, None) }.context("starting the service")?;
    if !wait_state(&service, SERVICE_RUNNING, Duration::from_secs(20))? {
        bail!("the service did not start: see {}", config::service_log().display());
    }
    println!("service installed and started");
    let id = wait_online(Duration::from_secs(30));
    match &id {
        Some(id) => match &generated_password {
            Some(password) => println!("ID {id}, permanent password {password}"),
            None => println!("ID {id}"),
        },
        None => println!(
            "the agent has not registered on {} yet: check {}",
            service_config.server,
            config::agent_log().display()
        ),
    }
    Ok(Installed { id, generated_password })
}

/// Key of the entry in Windows' installed apps.
const UNINSTALL_KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\NexoDesk";

fn start_menu_shortcut() -> std::path::PathBuf {
    std::env::var_os("ProgramData")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| r"C:\ProgramData".into())
        .join(r"Microsoft\Windows\Start Menu\Programs\NexoDesk.lnk")
}

/// Start menu shortcut and entry in the installed apps, so that NexoDesk can be
/// found and removed like any other program.
fn register_app(program: &Path) -> Result<()> {
    let program_s = program.display().to_string();
    let dir = program.parent().map(|d| d.display().to_string()).unwrap_or_default();
    let uninstall = format!("\"{program_s}\" service uninstall --remove-program");
    let values: [(&str, &str, &str); 9] = [
        ("DisplayName", "REG_SZ", "NexoDesk"),
        ("DisplayVersion", "REG_SZ", env!("CARGO_PKG_VERSION")),
        ("Publisher", "REG_SZ", "Alessandro Blason"),
        ("DisplayIcon", "REG_SZ", &program_s),
        ("InstallLocation", "REG_SZ", &dir),
        ("UninstallString", "REG_SZ", &uninstall),
        ("URLInfoAbout", "REG_SZ", "https://github.com/alexbn71/NexoDesk"),
        ("NoModify", "REG_DWORD", "1"),
        ("NoRepair", "REG_DWORD", "1"),
    ];
    for (name, kind, data) in values {
        run(Command::new("reg").args(["add", UNINSTALL_KEY, "/v", name, "/t", kind, "/d", data, "/f"]))
            .with_context(|| format!("installed apps entry ({name})"))?;
    }
    // The shortcut is made by Windows' own scripting object: no extra dependency.
    let script = format!(
        "$s = (New-Object -ComObject WScript.Shell).CreateShortcut('{}'); $s.TargetPath = '{}'; $s.WorkingDirectory = '{}'; $s.Save()",
        start_menu_shortcut().display().to_string().replace('\'', "''"),
        program_s.replace('\'', "''"),
        dir.replace('\'', "''"),
    );
    run(Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]))
        .context("Start menu shortcut")?;
    Ok(())
}

/// Starts `exe` with `params` as administrator (UAC prompt) and waits for it.
/// Returns its exit code; `Ok(None)` if the person declined the prompt.
pub fn run_elevated(exe: &Path, params: &str) -> Result<Option<u32>> {
    use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED};
    use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let verb = HSTRING::from("runas");
    let file = HSTRING::from(exe.as_os_str());
    let parameters = HSTRING::from(params);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
        if e.code() == ERROR_CANCELLED.to_hresult() {
            return Ok(None);
        }
        return Err(e).context("starting as administrator");
    }
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
    }
    Ok(Some(code))
}

/// Without administrator rights (e.g. from the installed apps entry) the same command
/// is started again as administrator. Returns true if that happened.
fn elevate_if_needed() -> Result<bool> {
    if unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS) }.map(Sc).is_ok() {
        return Ok(false);
    }
    let params = std::env::args()
        .skip(1)
        .map(|a| if a.contains(' ') { format!("\"{a}\"") } else { a })
        .collect::<Vec<_>>()
        .join(" ");
    run_elevated(&std::env::current_exe()?, &params)?;
    Ok(true)
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

/// Copies the new program over the installed one. The installed one may still be
/// running (the app opened from the Start menu, or the agent of the service just
/// stopped, which takes a moment to exit): a running program cannot be overwritten
/// but can be renamed, so it is moved aside and deleted at the next update.
fn replace_program(src: &Path, program: &Path) -> Result<()> {
    let aside = program.with_extension("old.exe");
    let _ = std::fs::remove_file(&aside);
    if std::fs::copy(src, program).is_ok() {
        return Ok(());
    }
    std::fs::rename(program, &aside).with_context(|| format!("moving aside {}", program.display()))?;
    std::fs::copy(src, program)
        .with_context(|| format!("copying {} to {}", src.display(), program.display()))?;
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
    let _ = unsafe {
        ChangeServiceConfig2W(service.0, SERVICE_CONFIG_DESCRIPTION, Some((&raw const info).cast()))
    };

    // If the service crashes, Windows restarts it after 5 s (the counter resets every day).
    let mut actions = [SC_ACTION { Type: SC_ACTION_RESTART, Delay: 5_000 }; 3];
    let failure = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 24 * 3600,
        lpRebootMsg: PWSTR::null(),
        lpCommand: PWSTR::null(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    let _ = unsafe {
        ChangeServiceConfig2W(service.0, SERVICE_CONFIG_FAILURE_ACTIONS, Some((&raw const failure).cast()))
    };
    Ok(service)
}

/// Automatic update: installs this program with the settings already in place.
pub fn update() -> Result<()> {
    let current = ServiceConfig::load()?;
    println!("updating to {}", env!("CARGO_PKG_VERSION"));
    install(InstallOptions {
        server: current.server,
        server_key: current.server_key,
        // Kept by install().
        password: None,
        transport: current.transport,
        server_ws: current.server_ws,
    })
    .map(|_| ())
}

pub fn uninstall(remove_program: bool) -> Result<()> {
    if elevate_if_needed()? {
        return Ok(());
    }
    let scm = manager(SC_MANAGER_ALL_ACCESS)?;
    match open_service(&scm, SERVICE_ALL_ACCESS)? {
        Some(service) => {
            stop(&service)?;
            unsafe { DeleteService(service.0) }.context("DeleteService")?;
            println!("service removed");
        }
        None => println!("the service is not installed"),
    }
    let _ =
        run(Command::new("netsh")
            .raw_arg(format!("advfirewall firewall delete rule name=\"{FIREWALL_RULE}\"")));
    if !remove_program {
        println!(
            "the program remains in {} and the data in {} (with the device ID, reused if you reinstall)",
            config::program_dir().display(),
            config::data_dir().display()
        );
        return Ok(());
    }
    let _ = run(Command::new("reg").args(["delete", UNINSTALL_KEY, "/f"]));
    let _ = std::fs::remove_file(start_menu_shortcut());
    remove_program_dir();
    println!(
        "NexoDesk removed; the data stays in {} (with the device ID, reused if you reinstall)",
        config::data_dir().display()
    );
    Ok(())
}

/// Removes the program folder. This program is running from it, so a separate command
/// deletes it once this process has exited; that command leaves the caller's job if it
/// can (a remote prompt closes its whole job when it ends). No deletion at restart as a
/// fallback: a reinstallation before the restart would be deleted too.
fn remove_program_dir() {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let command =
        format!("/c ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", config::program_dir().display());
    let spawn = |flags| Command::new("cmd").raw_arg(&command).creation_flags(flags).spawn();
    if spawn(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB).is_err() {
        let _ = spawn(CREATE_NO_WINDOW);
    }
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
