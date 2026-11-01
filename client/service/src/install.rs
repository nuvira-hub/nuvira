//! Installing, removing and checking the service (from an administrator prompt).

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

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

const FIREWALL_RULE: &str = "NuviraHub agent";

/// Scheduled task that starts the service if an automatic update dies after stopping
/// it (see [`update`]): the PC must never stay unreachable because of an update.
const RESCUE_TASK: &str = "NuviraHub update rescue";

/// Line of the installation output, with the time (the automatic update writes it to
/// update.log: if it stops halfway, the last line says where).
fn step(text: &str) {
    println!("{} {text}", utc_iso(SystemTime::now()));
}

/// `2026-11-01T00:30:00Z`.
fn utc_iso(time: SystemTime) -> String {
    let secs = time.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rest) = ((secs / 86_400) as i64, secs % 86_400);
    // Civil date from the days since 1970 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// Registers [`RESCUE_TASK`]: from `delay` on, every 5 minutes for an hour, the system
/// puts the previous program back if the new one is missing (see [`replace_program`])
/// and starts the service (harmless if it is already running). Independent of this process.
fn schedule_rescue(delay: Duration) -> Result<()> {
    let program = config::program_dir().join(config::PROGRAM_EXE);
    // The program moved aside if it is still there, otherwise the service's backup: an
    // antivirus that stopped the update deleted both files the update had touched
    // (Kaspersky, 2026-11-01). In parentheses: otherwise the start too is part of the if.
    let command = format!(
        r#"/c (if not exist "{0}" (if exist "{1}" (move /y "{1}" "{0}") else (copy /y "{2}" "{0}"))) &amp; sc.exe start {3}"#,
        program.display(),
        aside_path(&program).display(),
        config::backup_program().display(),
        config::SERVICE_NAME
    );
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Starts the NuviraHub service if an update stopped halfway.</Description></RegistrationInfo>
  <Triggers><TimeTrigger><StartBoundary>{}</StartBoundary><Repetition><Interval>PT5M</Interval><Duration>PT1H</Duration></Repetition><Enabled>true</Enabled></TimeTrigger></Triggers>
  <Principals><Principal id="System"><UserId>S-1-5-18</UserId><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><StartWhenAvailable>true</StartWhenAvailable><ExecutionTimeLimit>PT5M</ExecutionTimeLimit><Enabled>true</Enabled></Settings>
  <Actions Context="System"><Exec><Command>cmd.exe</Command><Arguments>{}</Arguments></Exec></Actions>
</Task>"#,
        utc_iso(SystemTime::now() + delay),
        command
    );
    // schtasks wants the file in UTF-16.
    let path = config::data_dir().join("rescue-task.xml");
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(&path, bytes)?;
    let result =
        run(Command::new("schtasks").args(["/create", "/tn", RESCUE_TASK, "/xml"]).arg(&path).arg("/f"));
    let _ = std::fs::remove_file(&path);
    result
}

/// Removes [`RESCUE_TASK`], if there is one (also at every start of the service).
pub fn remove_rescue() {
    let _ = Command::new("schtasks").args(["/delete", "/tn", RESCUE_TASK, "/f"]).output();
}

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
        step("service already installed: stopping it to update it");
        stop(service)?;
        step("service stopped");
    }

    // The program in Program Files: the service (SYSTEM) must not run files a user can replace.
    let src = source_program()?;
    let program_dir = config::program_dir();
    std::fs::create_dir_all(&program_dir).with_context(|| format!("creating {}", program_dir.display()))?;
    let program = program_dir.join(config::PROGRAM_EXE);
    if !same_file(&src, &program) {
        replace_program(&src, &program)?;
        step(&format!("program copied to {}", program.display()));
    }
    for old in config::OLD_EXES {
        let _ = std::fs::remove_file(program_dir.join(old));
    }
    // Customised installer: its configuration stays next to the program, for
    // the app of every user and across the updates (which bring the standard program).
    if let Some(doc) = crate::branding::read_trailer(&src) {
        std::fs::write(program_dir.join("branding.json"), doc).context("saving the customisation")?;
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
        (Some(password), _) => (nuvira_secret::hash(&password), None),
        (None, Some(previous)) => (previous.password_hash, None),
        (None, None) => {
            let password = config::random_password();
            (nuvira_secret::hash(&password), Some(password))
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
    step("configuration saved");

    let service = match existing {
        Some(service) => {
            // Earlier installations ran nuvira-service.exe.
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
    step("service configured, starting it");

    let _ = std::fs::remove_file(config::online_path());
    start_service(&service)?;
    step("service installed and started");
    // After the start: if an antivirus stops this process here, the PC is reachable anyway.
    register_app(&program)?;
    let id = wait_online(Duration::from_secs(30));
    match &id {
        Some(id) => match &generated_password {
            Some(password) => println!("ID {id}, permanent password {password}"),
            None => println!("ID {id}"),
        },
        None => println!(
            "the agent has not registered on {} yet: check {}",
            nuvira_transport::signal::server_or_public(&service_config.server),
            config::agent_log().display()
        ),
    }
    Ok(Installed { id, generated_password })
}

/// Key of the entry in Windows' installed apps.
const UNINSTALL_KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\nuvira";

fn start_menu_shortcut() -> std::path::PathBuf {
    std::env::var_os("ProgramData")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| r"C:\ProgramData".into())
        .join(r"Microsoft\Windows\Start Menu\Programs\NuviraHub.lnk")
}

/// Start menu shortcut and entry in the installed apps, so that NuviraHub can be
/// found and removed like any other program.
fn register_app(program: &Path) -> Result<()> {
    let program_s = program.display().to_string();
    let dir = program.parent().map(|d| d.display().to_string()).unwrap_or_default();
    let uninstall = format!("\"{program_s}\" service uninstall --remove-program");
    let values: [(&str, &str, &str); 9] = [
        ("DisplayName", "REG_SZ", "NuviraHub"),
        ("DisplayVersion", "REG_SZ", env!("CARGO_PKG_VERSION")),
        ("Publisher", "REG_SZ", "Alessandro Blason"),
        ("DisplayIcon", "REG_SZ", &program_s),
        ("InstallLocation", "REG_SZ", &dir),
        ("UninstallString", "REG_SZ", &uninstall),
        ("URLInfoAbout", "REG_SZ", "https://github.com/nuvira-hub/nuvira"),
        ("NoModify", "REG_DWORD", "1"),
        ("NoRepair", "REG_DWORD", "1"),
    ];
    for (name, kind, data) in values {
        run(Command::new("reg").args(["add", UNINSTALL_KEY, "/v", name, "/t", kind, "/d", data, "/f"]))
            .with_context(|| format!("installed apps entry ({name})"))?;
    }
    // Made once (the program path never changes), in this process: a shortcut made by
    // PowerShell started from here was taken for malware by Kaspersky (PDM:Trojan.Win32.Generic,
    // 2026-11-01), which stopped the update and removed the shortcut.
    let shortcut = start_menu_shortcut();
    if !shortcut.exists() {
        create_shortcut(&shortcut, program).context("Start menu shortcut")?;
    }
    Ok(())
}

fn create_shortcut(shortcut: &Path, target: &Path) -> Result<()> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::Interface;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&HSTRING::from(target.as_os_str()))?;
        if let Some(dir) = target.parent() {
            link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
        }
        link.cast::<IPersistFile>()?.Save(&HSTRING::from(shortcut.as_os_str()), true)?;
    }
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

/// Copies the new program in place of the installed one, which is first moved aside
/// (deleted at the next update). A running program (the app opened from the Start
/// menu, the agent of the service just stopped) cannot be overwritten but can be
/// renamed; and the previous program stays on disk: if this process is stopped
/// halfway, [`RESCUE_TASK`] puts it back. Overwritten, it was lost: an antivirus that
/// stopped the update deleted the new program and the PC stayed unreachable (2026-11-01).
fn replace_program(src: &Path, program: &Path) -> Result<()> {
    let aside = aside_path(program);
    let _ = std::fs::remove_file(&aside);
    if program.exists() {
        std::fs::rename(program, &aside).with_context(|| format!("moving aside {}", program.display()))?;
    }
    if let Err(e) = std::fs::copy(src, program) {
        let _ = std::fs::rename(&aside, program);
        return Err(e).with_context(|| format!("copying {} to {}", src.display(), program.display()));
    }
    Ok(())
}

fn aside_path(program: &Path) -> std::path::PathBuf {
    program.with_extension("old.exe")
}

/// Brings [`config::backup_program`] up to date with the running program (called by
/// the service once it has run well for a while). Written aside and renamed, so a
/// copy cut halfway never replaces a good backup.
pub fn refresh_backup() -> Result<()> {
    let program = std::env::current_exe()?;
    let backup = config::backup_program();
    let current = std::fs::read(&program).with_context(|| format!("reading {}", program.display()))?;
    if std::fs::read(&backup).is_ok_and(|saved| saved == current) {
        return Ok(());
    }
    if let Some(dir) = backup.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = backup.with_extension("partial");
    std::fs::write(&partial, &current).with_context(|| format!("writing {}", partial.display()))?;
    std::fs::rename(&partial, &backup).with_context(|| format!("renaming to {}", backup.display()))
}

/// The program to install: this one when it is the app (`nuvira.exe service install`,
/// whatever the file is called), otherwise the app next to nuvira-service.exe.
fn source_program() -> Result<std::path::PathBuf> {
    let here = std::env::current_exe()?;
    let name = here.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_ascii_lowercase();
    if !name.starts_with("nuvira-service") {
        return Ok(here);
    }
    let app = here.with_file_name(config::PROGRAM_EXE);
    if !app.exists() {
        bail!("{} not found: it must be in the same folder as nuvira-service.exe", app.display());
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

/// Starts the service, trying again a few times: right after the program was replaced
/// Windows once refused it with "access denied" (2026-10-30, the file probably still
/// held by a scan), and the PC stayed unreachable.
fn start_service(service: &Sc) -> Result<()> {
    let mut last = None;
    for attempt in 1..=5 {
        if attempt > 1 {
            std::thread::sleep(Duration::from_secs(5));
        }
        if let Err(e) = unsafe { StartServiceW(service.0, None) } {
            step(&format!("starting the service, attempt {attempt}: {e}"));
            last = Some(anyhow::Error::from(e));
            continue;
        }
        if wait_state(service, SERVICE_RUNNING, Duration::from_secs(20))? {
            return Ok(());
        }
        step(&format!("the service did not start, attempt {attempt}"));
        last = Some(anyhow::anyhow!("the service did not start: see {}", config::service_log().display()));
    }
    Err(last.expect("at least one attempt")).context("starting the service")
}

/// Automatic update: installs this program with the settings already in place.
/// Before the service is stopped a system task is registered that starts it again if
/// this process dies halfway; it is removed once the service runs.
pub fn update() -> Result<()> {
    let current = ServiceConfig::load()?;
    step(&format!("updating to {}", env!("CARGO_PKG_VERSION")));
    // Short (past the 20 s the stop may take): when an antivirus stops this process the
    // task is what starts the service.
    if let Err(e) = schedule_rescue(Duration::from_secs(60)) {
        step(&format!("rescue task not registered: {e:#}"));
    }
    let result = install(InstallOptions {
        server: current.server,
        server_key: current.server_key,
        // Kept by install().
        password: None,
        transport: current.transport,
        server_ws: current.server_ws,
    });
    match &result {
        Ok(_) => remove_rescue(),
        // The task stays: it keeps trying to start the service.
        Err(e) => step(&format!("update failed: {e:#}")),
    }
    result.map(|_| ())
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
    if let Some(dir) = config::backup_program().parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
    let _ = std::fs::remove_file(start_menu_shortcut());
    remove_program_dir();
    println!(
        "NuviraHub removed; the data stays in {} (with the device ID, reused if you reinstall)",
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

/// State of the service as the app sees it (no administrator rights needed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    NotInstalled,
    Stopped,
    Running,
    /// Starting or stopping.
    Changing,
}

pub fn service_state() -> Result<ServiceState> {
    let scm = manager(SC_MANAGER_CONNECT)?;
    let Some(service) = open_service(&scm, SERVICE_QUERY_STATUS)? else {
        return Ok(ServiceState::NotInstalled);
    };
    Ok(match state(&service)? {
        SERVICE_RUNNING => ServiceState::Running,
        SERVICE_STOPPED => ServiceState::Stopped,
        _ => ServiceState::Changing,
    })
}

/// Version of the installed program (from its version resource), `None` if unreadable.
pub fn installed_version() -> Option<String> {
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
    };

    let name = HSTRING::from(crate::installed_program().as_os_str());
    let size = unsafe { GetFileVersionInfoSizeW(&name, None) };
    if size == 0 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    unsafe { GetFileVersionInfoW(&name, None, size, data.as_mut_ptr().cast()) }.ok()?;
    let mut fixed = std::ptr::null_mut();
    let mut len = 0u32;
    let found =
        unsafe { VerQueryValueW(data.as_ptr().cast(), windows::core::w!("\\"), &mut fixed, &mut len) };
    if !found.as_bool() || fixed.is_null() || (len as usize) < size_of::<VS_FIXEDFILEINFO>() {
        return None;
    }
    // Inside `data`, which is still alive.
    let fixed = unsafe { &*(fixed as *const VS_FIXEDFILEINFO) };
    Some(format!(
        "{}.{}.{}",
        fixed.dwProductVersionMS >> 16,
        fixed.dwProductVersionMS & 0xFFFF,
        fixed.dwProductVersionLS >> 16
    ))
}

/// The service is stopped and its program may be older than this one (an automatic
/// update that died halfway before 0.12.3 left PCs like that): installs this program
/// with the settings in place if it is newer, otherwise just starts the service.
/// Started by the app as administrator.
pub fn repair() -> Result<()> {
    let installed = installed_version();
    step(&format!("repair: installed program {}", installed.as_deref().unwrap_or("of unknown version")));
    if installed.as_deref().is_none_or(|v| nuvira_update::newer(env!("CARGO_PKG_VERSION"), v)) {
        return update();
    }
    let scm = manager(SC_MANAGER_ALL_ACCESS)?;
    let service = open_service(&scm, SERVICE_ALL_ACCESS)?.context("the service is not installed")?;
    if state(&service)? != SERVICE_RUNNING {
        start_service(&service)?;
    }
    step("service started");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_time() {
        let at = |s| SystemTime::UNIX_EPOCH + Duration::from_secs(s);
        assert_eq!(utc_iso(at(0)), "1970-01-01T00:00:00Z");
        assert_eq!(utc_iso(at(1_793_488_200)), "2026-10-31T23:10:00Z");
        assert_eq!(utc_iso(at(951_782_400)), "2000-02-29T00:00:00Z");
    }
}
