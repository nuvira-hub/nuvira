//! Installazione, rimozione e stato del servizio (da un prompt amministratore).

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::ERROR_SERVICE_DOES_NOT_EXIST;
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW,
    OpenServiceW, QueryServiceStatus, SC_ACTION, SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_ALL_ACCESS,
    SC_MANAGER_CONNECT, SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL,
    SERVICE_FAILURE_ACTIONSW, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE,
    SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS, StartServiceW,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use crate::config::{self, ServiceConfig};

const FIREWALL_RULE: &str = "NexoDesk agent";

/// Handle del gestore servizi, chiuso automaticamente.
struct Sc(SC_HANDLE);

impl Drop for Sc {
    fn drop(&mut self) {
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}

fn manager(access: u32) -> Result<Sc> {
    unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access) }
        .map(Sc)
        .context("accesso al gestore servizi negato: serve un prompt dei comandi da amministratore")
}

/// `None` se il servizio non è installato.
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
    // Può fallire se si sta già fermando: conta solo lo stato finale.
    let _ = unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) };
    if !wait_state(service, SERVICE_STOPPED, Duration::from_secs(20))? {
        bail!("il servizio non si è fermato entro 20 s");
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
        println!("servizio già installato: lo fermo per aggiornarlo");
        stop(service)?;
    }

    // Programmi in Program Files: il servizio (SYSTEM) non deve eseguire file che un utente può sostituire.
    let here = std::env::current_exe()?;
    let source_dir = here.parent().context("cartella del programma")?;
    let program_dir = config::program_dir();
    std::fs::create_dir_all(&program_dir).with_context(|| format!("creazione di {}", program_dir.display()))?;
    for exe in [config::SERVICE_EXE, config::AGENT_EXE] {
        let (src, dst) = (source_dir.join(exe), program_dir.join(exe));
        if !src.exists() {
            bail!("{} non trovato: deve stare nella stessa cartella di nexodesk-service.exe", src.display());
        }
        if !same_file(&src, &dst) {
            std::fs::copy(&src, &dst).with_context(|| format!("copia di {} in {}", exe, program_dir.display()))?;
        }
    }

    // Dati leggibili solo da SYSTEM e amministratori: contengono chiave e password.
    let data_dir = config::data_dir();
    std::fs::create_dir_all(&data_dir).with_context(|| format!("creazione di {}", data_dir.display()))?;
    run(Command::new("icacls").arg(&data_dir).args([
        "/inheritance:r",
        "/grant:r",
        "*S-1-5-18:(OI)(CI)F",
        "*S-1-5-32-544:(OI)(CI)F",
        "/T",
        "/Q",
    ]))
    .context("protezione della cartella dati")?;

    // Reinstallando senza --password si tiene quella già impostata.
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
        Some(service) => service,
        None => create(&scm, &program_dir.join(config::SERVICE_EXE))?,
    };

    // Regola in ingresso per il percorso diretto (UDP) verso l'agent.
    let agent = program_dir.join(config::AGENT_EXE);
    let _ = run(Command::new("netsh").raw_arg(format!(
        "advfirewall firewall delete rule name=\"{FIREWALL_RULE}\""
    )));
    run(Command::new("netsh").raw_arg(format!(
        "advfirewall firewall add rule name=\"{FIREWALL_RULE}\" dir=in action=allow program=\"{}\" enable=yes profile=any",
        agent.display()
    )))
    .context("regola del firewall")?;

    let _ = std::fs::remove_file(config::online_path());
    unsafe { StartServiceW(service.0, None) }.context("avvio del servizio")?;
    if !wait_state(&service, SERVICE_RUNNING, Duration::from_secs(20))? {
        bail!("il servizio non è partito: vedi {}", config::service_log().display());
    }
    println!("servizio installato e avviato");
    match wait_online(Duration::from_secs(30)) {
        Some(id) => println!("ID {id}, password {password}"),
        None => println!(
            "l'agent non si è ancora registrato su {}: controlla {}",
            service_config.server,
            config::agent_log().display()
        ),
    }
    Ok(())
}

fn create(scm: &Sc, exe: &Path) -> Result<Sc> {
    let command = format!("\"{}\" run", exe.display());
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
            // Senza account: LocalSystem.
            PCWSTR::null(),
            PCWSTR::null(),
        )
    }
    .map(Sc)
    .context("CreateService")?;

    let mut description: Vec<u16> = config::SERVICE_DESCRIPTION.encode_utf16().chain(Some(0)).collect();
    let info = SERVICE_DESCRIPTIONW { lpDescription: PWSTR(description.as_mut_ptr()) };
    let _ = unsafe { ChangeServiceConfig2W(service.0, SERVICE_CONFIG_DESCRIPTION, Some((&raw const info).cast())) };

    // Se il servizio cade, Windows lo riavvia dopo 5 s (il contatore si azzera ogni giorno).
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
        println!("il servizio non è installato");
        return Ok(());
    };
    stop(&service)?;
    unsafe { DeleteService(service.0) }.context("DeleteService")?;
    let _ = run(Command::new("netsh").raw_arg(format!(
        "advfirewall firewall delete rule name=\"{FIREWALL_RULE}\""
    )));
    println!("servizio rimosso");
    println!(
        "restano i programmi in {} e i dati in {} (con l'ID del dispositivo, riusato se lo reinstalli)",
        config::program_dir().display(),
        config::data_dir().display()
    );
    Ok(())
}

pub fn status() -> Result<()> {
    let scm = manager(SC_MANAGER_CONNECT)?;
    let Some(service) = open_service(&scm, SERVICE_QUERY_STATUS)? else {
        println!("servizio: non installato");
        return Ok(());
    };
    let state = state(&service)?;
    let text = match state {
        SERVICE_RUNNING => "in esecuzione",
        SERVICE_STOPPED => "fermo",
        _ => "in transizione",
    };
    println!("servizio: {text}");
    match std::fs::read_to_string(config::online_path()) {
        Ok(info) => match info.split_whitespace().next() {
            Some(id) => println!("ID {}", format_id(id)),
            None => println!("ID: non ancora registrato"),
        },
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            println!("ID: visibile solo da un prompt dei comandi da amministratore")
        }
        Err(_) => println!("ID: non ancora registrato"),
    }
    println!("log: {} e {}", config::service_log().display(), config::agent_log().display());
    Ok(())
}

/// Attende che l'agent scriva il file con l'ID dopo la registrazione.
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
    let output = command.output().context("avvio del comando")?;
    if !output.status.success() {
        bail!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
