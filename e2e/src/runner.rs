//! The runner of the `e2e` test binary (`harness = false`).
//!
//! `cargo test` runs the binary with no arguments (or with a filter after `--`):
//! the parent process lists the cases, starts each one as a child process of the
//! same binary (`--e2e-case NAME`) with its own temporary folder, data folder
//! (`NUVIRA_DATA_DIR`, file key store) and environment, waits for it with a
//! timeout, and prints a libtest-like summary. The child's output goes to
//! `<run dir>/<case>.log` (its tail is printed when it fails) and the results to
//! `<run dir>/results.json`.
//!
//! Groups (a case not run is always printed as `skipped` with the reason, and
//! counted in the summary, never silently passed):
//! - default: always;
//! - long: with `NUVIRA_E2E_LONG=1`;
//! - xvfb: Linux with `NUVIRA_E2E_XVFB=1`, under the virtual X server that
//!   `scripts/run-xvfb.sh` starts;
//! - real-desktop: the file cases on the machine's real screen capture, with
//!   `NUVIRA_E2E_REAL_DESKTOP=1` (Windows / macOS test machines: manual entry).
//!
//! Needs:
//! - `server`: the `nuvira-server` binary: `NUVIRA_BENCH_SERVER_EXE`, else built
//!   here with `go build` from `../server` into `<target>/<profile>/e2e-server/`
//!   (`GOCACHE` defaults to `<target>/go-cache`). Missing = the case fails.
//! - `wildcard`: the case binds 0.0.0.0 on a fixed port (`serve_direct`): on
//!   Windows only with `NUVIRA_E2E_WILDCARD=1`, because the firewall shows a
//!   prompt on the desktop for a new program listening there.
//!
//! Options: `NUVIRA_E2E_REPEAT=n` runs every selected case n times;
//! `NUVIRA_E2E_OUT=dir` sets the run directory; arguments are substring filters
//! (`--exact` makes them exact names); `--list` lists the cases.
//!
//! Inside a case, [`Ctx::note`] records facts in the results (measured times,
//! counts), and [`track_pid`] registers a process the case started so that the
//! parent kills it if the case dies before cleaning up.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Default,
    Long,
    Xvfb,
    /// The machine's real desktop (capture, files): only on a test machine whose
    /// screen may be used, with `NUVIRA_E2E_REAL_DESKTOP=1` (manual test entry).
    RealDesktop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    Server,
    Wildcard,
}

pub struct Case {
    pub name: &'static str,
    pub group: Group,
    pub needs: &'static [Need],
    /// Extra environment of the child process.
    pub env: &'static [(&'static str, &'static str)],
    pub timeout: Duration,
    pub run: fn(&mut Ctx) -> Result<()>,
}

impl Case {
    pub const fn new(name: &'static str, run: fn(&mut Ctx) -> Result<()>) -> Case {
        Case { name, group: Group::Default, needs: &[], env: &[], timeout: Duration::from_secs(180), run }
    }

    pub const fn group(mut self, g: Group) -> Case {
        self.group = g;
        self
    }

    pub const fn needs(mut self, n: &'static [Need]) -> Case {
        self.needs = n;
        self
    }

    pub const fn env(mut self, e: &'static [(&'static str, &'static str)]) -> Case {
        self.env = e;
        self
    }

    pub const fn timeout_s(mut self, s: u64) -> Case {
        self.timeout = Duration::from_secs(s);
        self
    }
}

/// What a running case has.
pub struct Ctx {
    pub name: String,
    /// A fresh folder of this case.
    pub tmp: PathBuf,
}

impl Ctx {
    /// A fact recorded in the results (`notes` of the case).
    pub fn note(&self, text: impl AsRef<str>) {
        println!("E2E-NOTE {}", text.as_ref().replace('\n', " "));
        let _ = std::io::stdout().flush();
    }
}

/// Registers a process started by the case: the parent kills it after the case
/// ends, in case the case did not (timeout, panic).
pub fn track_pid(pid: u32) {
    if let Some(dir) = std::env::var_os("NUVIRA_E2E_TMP") {
        let path = Path::new(&dir).join("pids.txt");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{pid}");
        }
    }
}

const CASE_ARG: &str = "--e2e-case";

/// Entry point of the test binary: `extra` handles other child roles
/// (`--e2e-...` first argument), returning `Some(exit code)` when it did.
pub fn main(cases: &[Case], extra: fn(&[String]) -> Option<i32>) -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = extra(&args) {
        return code;
    }
    if args.first().map(String::as_str) == Some(CASE_ARG) {
        let name = args.get(1).cloned().unwrap_or_default();
        return run_child(cases, &name);
    }
    match run_parent(cases, &args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("e2e runner: {e:#}");
            101
        }
    }
}

fn run_child(cases: &[Case], name: &str) -> i32 {
    let Some(case) = cases.iter().find(|c| c.name == name) else {
        eprintln!("no case {name}");
        return 2;
    };
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info,quinn=warn,quinn_proto=warn".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
    let tmp = PathBuf::from(std::env::var_os("NUVIRA_E2E_TMP").unwrap_or_default());
    let mut ctx = Ctx { name: name.to_string(), tmp };
    let t = Instant::now();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (case.run)(&mut ctx)));
    match r {
        Ok(Ok(())) => {
            println!("E2E-RESULT ok {:.1}", t.elapsed().as_secs_f64());
            0
        }
        Ok(Err(e)) => {
            println!("E2E-RESULT failed {:.1}: {e:#}", t.elapsed().as_secs_f64());
            1
        }
        Err(_) => {
            println!("E2E-RESULT panicked {:.1}", t.elapsed().as_secs_f64());
            1
        }
    }
}

/// Why a case does not run here, if it does not.
fn skip_reason(case: &Case) -> Option<String> {
    let on = |k: &str| std::env::var(k).is_ok_and(|v| v == "1");
    match case.group {
        Group::Long if !on("NUVIRA_E2E_LONG") => return Some("long case: NUVIRA_E2E_LONG=1 runs it".into()),
        Group::Xvfb if !cfg!(target_os = "linux") => return Some("Xvfb case: Linux only".into()),
        Group::Xvfb if !on("NUVIRA_E2E_XVFB") => {
            return Some("Xvfb case: scripts/run-xvfb.sh runs it (NUVIRA_E2E_XVFB=1 under Xvfb)".into());
        }
        Group::RealDesktop if !on("NUVIRA_E2E_REAL_DESKTOP") => {
            return Some(
                "real desktop case: NUVIRA_E2E_REAL_DESKTOP=1 on a test machine whose screen may be used"
                    .into(),
            );
        }
        _ => {}
    }
    if case.needs.contains(&Need::Wildcard) && cfg!(windows) && !on("NUVIRA_E2E_WILDCARD") {
        return Some(
            "binds 0.0.0.0 (serve_direct): the Windows firewall prompts on the desktop; NUVIRA_E2E_WILDCARD=1 runs it"
                .into(),
        );
    }
    None
}

#[derive(serde::Serialize)]
struct Outcome {
    name: String,
    run: u32,
    status: String,
    secs: f64,
    detail: String,
    notes: Vec<String>,
}

fn run_parent(cases: &[Case], args: &[String]) -> Result<i32> {
    let exact = args.iter().any(|a| a == "--exact");
    if args.iter().any(|a| a == "--list") {
        for c in cases {
            println!("{}: test ({:?})", c.name, c.group);
        }
        return Ok(0);
    }
    let filters: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let selected: Vec<&Case> = cases
        .iter()
        .filter(|c| {
            filters.is_empty()
                || filters
                    .iter()
                    .any(|f| if exact { c.name == f.as_str() } else { c.name.contains(f.as_str()) })
        })
        .collect();
    let repeat: u32 =
        std::env::var("NUVIRA_E2E_REPEAT").ok().and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
    let exe = std::env::current_exe()?;
    let profile_dir =
        exe.parent().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let out = match std::env::var_os("NUVIRA_E2E_OUT") {
        Some(d) => PathBuf::from(d),
        None => profile_dir.join("e2e-runs").join(format!("{stamp}-{}", std::process::id())),
    };
    std::fs::create_dir_all(&out)?;
    let needs_server = selected.iter().any(|c| c.needs.contains(&Need::Server) && skip_reason(c).is_none());
    let server = if needs_server { Some(server_exe(&profile_dir)) } else { None };
    println!("\nrunning {} e2e cases x{repeat} (output in {})", selected.len(), out.display());
    if let Some(Ok(s)) = &server {
        println!("nuvira-server: {}", s.display());
    }
    let mut results = Vec::new();
    let (mut passed, mut failed, mut skipped) = (0u32, 0u32, 0u32);
    let total = Instant::now();
    for r in 1..=repeat {
        for case in &selected {
            let label = if repeat > 1 { format!("{} #{r}", case.name) } else { case.name.to_string() };
            if let Some(why) = skip_reason(case) {
                println!("test {label} ... skipped ({why})");
                skipped += 1;
                results.push(Outcome {
                    name: case.name.into(),
                    run: r,
                    status: "skipped".into(),
                    secs: 0.0,
                    detail: why,
                    notes: Vec::new(),
                });
                continue;
            }
            let server = match (&server, case.needs.contains(&Need::Server)) {
                (Some(Err(e)), true) => {
                    println!("test {label} ... FAILED (no nuvira-server: {e})");
                    failed += 1;
                    results.push(Outcome {
                        name: case.name.into(),
                        run: r,
                        status: "failed".into(),
                        secs: 0.0,
                        detail: format!("no nuvira-server: {e}"),
                        notes: Vec::new(),
                    });
                    continue;
                }
                (Some(Ok(s)), _) => Some(s.clone()),
                _ => None,
            };
            print!("test {label} ... ");
            let _ = std::io::stdout().flush();
            let o = run_one(&exe, case, r, &out, server.as_deref())?;
            match o.status.as_str() {
                "ok" => {
                    passed += 1;
                    println!("ok ({:.1} s)", o.secs);
                }
                _ => {
                    failed += 1;
                    println!("FAILED ({:.1} s): {}", o.secs, o.detail);
                    let log = out.join(format!("{}-{r}.log", file_name(case.name)));
                    print_tail(&log, 40);
                }
            }
            results.push(o);
        }
    }
    std::fs::write(out.join("results.json"), serde_json::to_string_pretty(&results)?)?;
    if let Some(path) = std::env::var_os("NUVIRA_E2E_RESULT_JSON") {
        write_review_result(Path::new(&path), &results, (passed, failed, skipped), args)?;
    }
    println!(
        "\ntest result: {}. {passed} passed; {failed} failed; {skipped} skipped; finished in {:.1} s\n",
        if failed == 0 { "ok" } else { "FAILED" },
        total.elapsed().as_secs_f64()
    );
    Ok(if failed == 0 { 0 } else { 101 })
}

/// The run as a `nuvira-review-result/1` document (`docs/review/manual-tests.md`),
/// for the manual entries: `NUVIRA_E2E_RESULT_JSON=<file>`, with
/// `NUVIRA_E2E_RESULT_ID` (e.g. MT-0xx) and `NUVIRA_E2E_MACHINE`.
fn write_review_result(
    path: &Path,
    results: &[Outcome],
    counts: (u32, u32, u32),
    args: &[String],
) -> Result<()> {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    let (passed, failed, skipped) = counts;
    let tests: Vec<serde_json::Value> = results
        .iter()
        .map(|o| {
            let result = match o.status.as_str() {
                "ok" => "passed",
                "skipped" => "ignored",
                _ => "failed",
            };
            serde_json::json!({"binary": "e2e", "name": format!("{} #{}", o.name, o.run), "result": result,
                "seconds": o.secs, "detail": o.detail, "notes": o.notes})
        })
        .collect();
    let doc = serde_json::json!({
        "schema": "nuvira-review-result/1",
        "id": var("NUVIRA_E2E_RESULT_ID"),
        "kind": "e2e-suite",
        "machine": var("NUVIRA_E2E_MACHINE"),
        "date": format!("unix {}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs()),
        "os": {"name": std::env::consts::OS, "arch": std::env::consts::ARCH},
        "command": format!("e2e {}", args.join(" ")),
        "env": {
            "NUVIRA_E2E_LONG": var("NUVIRA_E2E_LONG"),
            "NUVIRA_E2E_REAL_DESKTOP": var("NUVIRA_E2E_REAL_DESKTOP"),
            "NUVIRA_E2E_WILDCARD": var("NUVIRA_E2E_WILDCARD"),
            "NUVIRA_E2E_REPEAT": var("NUVIRA_E2E_REPEAT"),
        },
        "exit_code": if failed == 0 { 0 } else { 101 },
        "summary": {"binaries": 1, "passed": passed, "failed": failed, "ignored": skipped, "binaries_without_result": 0},
        "tests": tests,
        "notes": "skipped cases are counted as ignored (not run)",
    });
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
    Ok(())
}

fn file_name(case: &str) -> String {
    case.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect()
}

fn run_one(exe: &Path, case: &Case, run: u32, out: &Path, server: Option<&Path>) -> Result<Outcome> {
    let base = file_name(case.name);
    let tmp = out.join(format!("{base}-{run}"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("data"))?;
    let log_path = out.join(format!("{base}-{run}.log"));
    let log = std::fs::File::create(&log_path)?;
    let mut cmd = Command::new(exe);
    cmd.args([CASE_ARG, case.name])
        .env("NUVIRA_E2E_TMP", &tmp)
        .env("NUVIRA_DATA_DIR", tmp.join("data"))
        .env("NUVIRA_KEY_STORE", "file")
        .env_remove("NUVIRA_FORCE_RELAY")
        .env_remove("NUVIRA_RELAY_IMPAIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log.try_clone()?);
    if let Some(s) = server {
        cmd.env("NUVIRA_BENCH_SERVER_EXE", s);
    }
    for (k, v) in case.env {
        cmd.env(k, v);
    }
    let t = Instant::now();
    let mut child = cmd.spawn().with_context(|| format!("starting {}", exe.display()))?;
    // The child's stdout: copied to the log, notes and the result line kept.
    let stdout = child.stdout.take().context("stdout")?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut log_out = log;
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = writeln!(log_out, "{line}");
            if line.starts_with("E2E-") {
                let _ = tx.send(line);
            }
        }
    });
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Some(s);
        }
        if t.elapsed() > case.timeout {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = reader.join();
    kill_tracked(&tmp);
    let secs = t.elapsed().as_secs_f64();
    let lines: Vec<String> = rx.try_iter().collect();
    let notes = lines.iter().filter_map(|l| l.strip_prefix("E2E-NOTE ")).map(String::from).collect();
    let result = lines.iter().rev().find_map(|l| l.strip_prefix("E2E-RESULT ")).unwrap_or("").to_string();
    let (status, detail) = match status {
        None => ("failed".to_string(), format!("timed out after {:?}", case.timeout)),
        Some(s) if s.success() && result.starts_with("ok") => ("ok".to_string(), String::new()),
        Some(s) => (
            "failed".to_string(),
            if result.is_empty() { format!("exit {s} without a result") } else { result.clone() },
        ),
    };
    Ok(Outcome { name: case.name.into(), run, status, secs, detail, notes })
}

/// Kills the processes the case registered and left running.
fn kill_tracked(tmp: &Path) {
    let Ok(text) = std::fs::read_to_string(tmp.join("pids.txt")) else { return };
    for pid in text.lines().filter_map(|l| l.trim().parse::<u32>().ok()) {
        #[cfg(windows)]
        let _ = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        #[cfg(unix)]
        // SAFETY: plain kill(2) on a pid this run started.
        unsafe {
            libc_kill(pid as i32);
        }
    }
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // A pid that already exited (and was reaped) may only be reused after a long
    // time on these short runs; ESRCH is ignored.
    unsafe {
        kill(pid, 9);
    }
}

fn print_tail(path: &Path, lines: usize) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let all: Vec<&str> = text.lines().collect();
    println!("---- last lines of {} ----", path.display());
    for l in &all[all.len().saturating_sub(lines)..] {
        println!("    {l}");
    }
    println!("----");
}

/// The `nuvira-server` binary: `NUVIRA_BENCH_SERVER_EXE`, else `go build` of
/// `../server` (this crate's tree) into `<profile dir>/e2e-server/`.
fn server_exe(profile_dir: &Path) -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("NUVIRA_BENCH_SERVER_EXE") {
        let p = PathBuf::from(p);
        return if p.is_file() {
            Ok(p)
        } else {
            Err(format!("NUVIRA_BENCH_SERVER_EXE={} does not exist", p.display()))
        };
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("server");
    if !src.join("go.mod").is_file() {
        return Err(format!("no Go sources in {} and no NUVIRA_BENCH_SERVER_EXE", src.display()));
    }
    let dest = profile_dir.join("e2e-server").join(format!("nuvira-server{}", std::env::consts::EXE_SUFFIX));
    let mut cmd = Command::new("go");
    cmd.current_dir(&src).args(["build", "-trimpath", "-o"]).arg(&dest).arg("./cmd/nuvira-server");
    if std::env::var_os("GOCACHE").is_none() {
        cmd.env("GOCACHE", profile_dir.join("..").join("go-cache"));
    }
    match cmd.status() {
        Ok(s) if s.success() => Ok(dest),
        Ok(s) => Err(format!("go build in {} exited with {s}", src.display())),
        Err(e) => Err(format!("go build: {e} (set NUVIRA_BENCH_SERVER_EXE or put go on PATH)")),
    }
}

/// Fails with `what` when `cond` is false (assertions that report instead of
/// panicking, so the case's result line carries the reason).
pub fn ensure(cond: bool, what: impl FnOnce() -> String) -> Result<()> {
    if !cond {
        bail!("{}", what());
    }
    Ok(())
}
