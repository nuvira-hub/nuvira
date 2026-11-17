//! JSON result files (plan section 8.1): one file per run and suite,
//! `<out>/<side>/<round>/<suite>.json`, schema `nuvira-bench/1`.
//!
//! WP5's `compare` and `import-go` read and write exactly these types
//! ([`RunFile::read`], [`RunFile::write`]).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub use crate::measure::{Better, Distribution, Gate, Metric};
use crate::registry::{Case, Level, Tier};

pub const SCHEMA: &str = "nuvira-bench/1";

/// Status of one case result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Status {
    #[serde(rename = "ok")]
    Ok,
    /// Noisy reps still noisy after the retries.
    #[serde(rename = "noisy")]
    Noisy,
    /// Error text in `notes`.
    #[serde(rename = "failed")]
    Failed,
    /// Missing hardware/OS/permission: never counted as passed.
    #[serde(rename = "not-runnable")]
    NotRunnable,
    /// The quality reference does not match the build (e.g. synthetic source changed).
    #[serde(rename = "invalid_reference")]
    InvalidReference,
}

/// One point of a rate ladder (BD-rate input).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LadderPoint {
    pub kbps_target: f64,
    pub kbps_actual: f64,
    pub psnr_yuv_db: f64,
    pub ssim_y: f64,
}

/// One element of `results[]`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaseResult {
    pub id: String,
    pub case: String,
    pub level: Level,
    pub config_ids: Vec<String>,
    pub status: Status,
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub metrics: BTreeMap<String, Metric>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub distributions: BTreeMap<String, Distribution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ladder: Vec<LadderPoint>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checksums: BTreeMap<String, String>,
    #[serde(default)]
    pub notes: Vec<String>,
    /// Anything else a case records at top level (e.g. `"source": "ring"`).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl CaseResult {
    /// An `ok` result with the case's identity filled in.
    pub fn new(case: &dyn Case) -> CaseResult {
        CaseResult {
            id: case.id().into(),
            case: case.name(),
            level: case.level(),
            config_ids: case.config_ids().iter().map(|s| s.to_string()).collect(),
            status: Status::Ok,
            params: Default::default(),
            metrics: Default::default(),
            distributions: Default::default(),
            ladder: Vec::new(),
            checksums: Default::default(),
            notes: Vec::new(),
            extra: Default::default(),
        }
    }

    pub fn not_runnable(case: &dyn Case, reason: impl Into<String>) -> CaseResult {
        let mut r = Self::new(case);
        r.status = Status::NotRunnable;
        r.notes.push(reason.into());
        r
    }

    pub fn failed(case: &dyn Case, error: &anyhow::Error) -> CaseResult {
        let mut r = Self::new(case);
        r.status = Status::Failed;
        r.notes.push(format!("{error:#}"));
        r
    }

    pub fn param(&mut self, key: &str, value: impl Into<serde_json::Value>) -> &mut Self {
        self.params.insert(key.into(), value.into());
        self
    }

    pub fn metric(&mut self, name: &str, metric: Metric) -> &mut Self {
        self.metrics.insert(name.into(), metric);
        self
    }

    pub fn distribution(&mut self, name: &str, d: Distribution) -> &mut Self {
        self.distributions.insert(name.into(), d);
        self
    }

    pub fn checksum(&mut self, name: &str, sha256_hex: impl Into<String>) -> &mut Self {
        self.checksums.insert(name.into(), sha256_hex.into());
        self
    }

    pub fn note(&mut self, note: impl Into<String>) -> &mut Self {
        self.notes.push(note.into());
        self
    }

    /// Records the micro-rep facts (`iterations`, `warmup_reps`, `reps`, noise
    /// retries) and marks the result `noisy` when retries were exhausted.
    pub fn reps(&mut self, reps: &crate::measure::Reps) -> &mut Self {
        self.param("iterations", reps.iterations);
        self.param("warmup_reps", reps.warmup_reps);
        self.param("reps", reps.reps.len());
        if reps.retries > 0 {
            self.param("noise_retries", reps.retries);
        }
        let other: Vec<f64> = reps.reps.iter().filter_map(|r| r.other_cores).collect();
        if !other.is_empty() {
            // Cores used by other processes during the kept reps (noise guard).
            self.param("other_cores_median", (crate::measure::median(&other) * 1000.0).round() / 1000.0);
        }
        if reps.noisy && self.status == Status::Ok {
            self.status = Status::Noisy;
        }
        self
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RepoInfo {
    pub commit: String,
    pub describe: String,
    pub dirty: bool,
    /// Every tag on the commit (`describe` picks one; the baseline tag may share
    /// the commit with a release tag).
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BenchInfo {
    pub commit: String,
    pub generator_version: u32,
    pub manifest_sha256: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Repos {
    pub nuvira: RepoInfo,
    pub bench: BenchInfo,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Toolchain {
    pub rustc: String,
    pub go: Option<String>,
    pub profile: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Gpu {
    pub name: String,
    pub driver: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Power {
    /// Windows power plan or Linux CPU governor.
    pub plan: Option<String>,
    pub ac: Option<bool>,
    /// CPU turbo/boost on (Linux: `intel_pstate/no_turbo` or `cpufreq/boost`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turbo: Option<bool>,
    /// What the run script set for the run (`NUVIRA_BENCH_POWER_SETUP`, e.g.
    /// "governor=performance no_turbo=1"; restored afterwards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MachineInfo {
    pub alias: String,
    pub config_ids: Vec<String>,
    pub hostname: Option<String>,
    pub os: String,
    pub os_version: Option<String>,
    pub kernel: Option<String>,
    pub arch: String,
    pub cpu: String,
    pub cores: Option<usize>,
    pub threads: usize,
    pub ram_gb: f64,
    pub gpu: Vec<Gpu>,
    pub power: Power,
    pub displays: Option<u32>,
    pub antivirus: Option<String>,
    pub load_average: Option<[f64; 3]>,
}

impl MachineInfo {
    /// Detects what it can of this machine. `alias` defaults to the host name.
    pub fn detect(alias: Option<String>, config_ids: Vec<String>) -> MachineInfo {
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_all();
        sys.refresh_memory();
        let hostname = sysinfo::System::host_name();
        let la = sysinfo::System::load_average();
        let mut m = MachineInfo {
            alias: alias.or_else(|| hostname.clone()).unwrap_or_else(|| "unknown".into()),
            config_ids,
            hostname,
            os: std::env::consts::OS.into(),
            os_version: sysinfo::System::long_os_version(),
            kernel: sysinfo::System::kernel_version(),
            arch: std::env::consts::ARCH.into(),
            cpu: sys.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default(),
            cores: sysinfo::System::physical_core_count(),
            threads: sys.cpus().len(),
            ram_gb: (sys.total_memory() as f64 / (1u64 << 30) as f64 * 10.0).round() / 10.0,
            load_average: (!cfg!(windows)).then_some([la.one, la.five, la.fifteen]),
            ..Default::default()
        };
        platform::detect(&mut m);
        m.power.setup = std::env::var("NUVIRA_BENCH_POWER_SETUP").ok().filter(|v| !v.is_empty());
        m
    }
}

/// Run header.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RunInfo {
    pub id: String,
    pub side: String,
    pub round: u32,
    pub tier: String,
    pub suite: String,
    pub repos: Repos,
    pub toolchain: Toolchain,
    pub machine: MachineInfo,
    pub env: BTreeMap<String, Option<String>>,
    /// Cargo `api-*` features of the bench build (plan 8.4).
    #[serde(default)]
    pub features: Vec<String>,
    pub args: Vec<String>,
    pub started: String,
    pub finished: String,
}

/// Environment variables always recorded (null when unset), plus every other
/// `NUVIRA_*` variable that is set.
pub const RECORDED_ENV: &[&str] = &[
    "NUVIRA_DECODER",
    "NUVIRA_FORCE_RELAY",
    "NUVIRA_VIDEO_FEC",
    "NUVIRA_CAPTURE",
    "NUVIRA_KEY_STORE",
    "RUST_LOG",
];

impl RunInfo {
    /// Header for `side`/`round` of `suite`; `started` = now.
    pub fn new(side: &str, round: u32, tier: Tier, suite: &str, machine: MachineInfo) -> RunInfo {
        let started = utc_now();
        let mut env: BTreeMap<String, Option<String>> =
            RECORDED_ENV.iter().map(|k| (k.to_string(), std::env::var(k).ok())).collect();
        for (k, v) in std::env::vars() {
            if k.starts_with("NUVIRA_") {
                env.insert(k, Some(v));
            }
        }
        let manifest_sha256 = crate::fixtures::Manifest::embedded().map(|m| m.sha256()).unwrap_or_default();
        RunInfo {
            id: format!("{}-{}-r{}", started.replace(':', "-"), side, round),
            side: side.into(),
            round,
            tier: tier.as_str().into(),
            suite: suite.into(),
            repos: Repos {
                nuvira: RepoInfo {
                    commit: env!("NUVIRA_BENCH_GIT_COMMIT").into(),
                    describe: env!("NUVIRA_BENCH_GIT_DESCRIBE").into(),
                    dirty: env!("NUVIRA_BENCH_GIT_DIRTY") == "true",
                    tags: env!("NUVIRA_BENCH_GIT_TAGS").split_whitespace().map(String::from).collect(),
                },
                bench: BenchInfo {
                    commit: env!("NUVIRA_BENCH_BENCH_COMMIT").into(),
                    generator_version: crate::fixtures::GENERATOR_VERSION,
                    manifest_sha256,
                },
            },
            toolchain: Toolchain {
                rustc: env!("NUVIRA_BENCH_RUSTC").into(),
                go: go_version(),
                profile: if cfg!(debug_assertions) { "debug" } else { "release" }.into(),
            },
            machine,
            env,
            features: Vec::new(),
            args: std::env::args().collect(),
            started,
            finished: String::new(),
        }
    }
}

/// `NUVIRA_BENCH_GO` (set by bench-compare) or `go version` if Go is on PATH.
fn go_version() -> Option<String> {
    if let Ok(v) = std::env::var("NUVIRA_BENCH_GO") {
        return Some(v);
    }
    let out = std::process::Command::new("go").arg("version").output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// One result file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunFile {
    pub schema: String,
    pub run: RunInfo,
    pub results: Vec<CaseResult>,
}

impl RunFile {
    pub fn new(run: RunInfo) -> RunFile {
        RunFile { schema: SCHEMA.into(), run, results: Vec::new() }
    }

    /// Sets `finished` and writes pretty JSON (directories created).
    pub fn write(&mut self, path: &Path) -> Result<()> {
        self.run.finished = utc_now();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(self)? + "\n";
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }

    pub fn read(path: &Path) -> Result<RunFile> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let f: RunFile =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        anyhow::ensure!(f.schema == SCHEMA, "{}: schema {} (expected {SCHEMA})", path.display(), f.schema);
        Ok(f)
    }
}

/// Current UTC time as `2026-11-16T15:04:05Z` (no date crate: civil-from-days).
pub fn utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_utc(secs)
}

/// Formats seconds since 1970 as ISO 8601 UTC.
pub fn format_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

#[cfg(windows)]
mod platform {
    use super::{Gpu, MachineInfo};

    pub fn detect(m: &mut MachineInfo) {
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CMONITORS};
        // SAFETY: plain query.
        let n = unsafe { GetSystemMetrics(SM_CMONITORS) };
        m.displays = (n > 0).then_some(n as u32);
        // One PowerShell call for what has no simple API: GPU + driver, antivirus
        // products registered with the Security Center (with their on/off state;
        // stale registrations of uninstalled products can appear) and AC state.
        let script = "$ErrorActionPreference='SilentlyContinue';            Get-CimInstance Win32_VideoController | ForEach-Object { 'GPU|' + $_.Name + '|' + $_.DriverVersion };            Get-CimInstance -Namespace root/SecurityCenter2 -ClassName AntiVirusProduct | ForEach-Object { 'AV|' + $_.displayName + '|' + $_.productState };            Add-Type -AssemblyName System.Windows.Forms;            'AC|' + [System.Windows.Forms.SystemInformation]::PowerStatus.PowerLineStatus";
        if let Ok(out) = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut av = Vec::new();
            for line in text.lines() {
                let parts: Vec<&str> = line.trim().split('|').collect();
                match parts.as_slice() {
                    ["GPU", name, driver] => m.gpu.push(Gpu {
                        name: name.trim().into(),
                        driver: (!driver.trim().is_empty()).then(|| driver.trim().into()),
                    }),
                    ["AV", name, state] => {
                        // productState: bits 12-15 = 1 when real-time protection is on.
                        let on = state.trim().parse::<u32>().map(|s| (s >> 12) & 0xf == 1).unwrap_or(false);
                        av.push(format!("{} ({})", name.trim(), if on { "on" } else { "off" }));
                    }
                    ["AC", state] => {
                        m.power.ac = match state.trim() {
                            "Online" => Some(true),
                            "Offline" => Some(false),
                            _ => None,
                        }
                    }
                    _ => {}
                }
            }
            if !av.is_empty() {
                m.antivirus = Some(av.join(", "));
            }
        }
        // Active power plan: the name in parentheses of `powercfg /getactivescheme`
        // (localised; Win32_PowerPlan needs administrator rights over SSH).
        if let Ok(out) = std::process::Command::new("powercfg").arg("/getactivescheme").output() {
            let text = String::from_utf8_lossy(&out.stdout);
            if let (Some(a), Some(b)) = (text.find('('), text.rfind(')'))
                && b > a
            {
                m.power.plan = Some(text[a + 1..b].trim().to_string());
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{Gpu, MachineInfo};

    pub fn detect(m: &mut MachineInfo) {
        let read = |p: &str| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string());
        m.power.plan = read("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor");
        m.power.turbo = read("/sys/devices/system/cpu/intel_pstate/no_turbo")
            .map(|v| v == "0")
            .or_else(|| read("/sys/devices/system/cpu/cpufreq/boost").map(|v| v == "1"));
        if let Ok(dir) = std::fs::read_dir("/sys/class/power_supply") {
            for e in dir.flatten() {
                let p = e.path();
                if read(&p.join("type").to_string_lossy()).as_deref() == Some("Mains") {
                    m.power.ac = read(&p.join("online").to_string_lossy()).map(|v| v == "1");
                }
            }
        }
        if let Ok(out) = std::process::Command::new("lspci").output() {
            for l in String::from_utf8_lossy(&out.stdout).lines() {
                if l.contains("VGA compatible")
                    || l.contains("3D controller")
                    || l.contains("Display controller")
                {
                    let name = l.split_once(": ").map(|x| x.1).unwrap_or(l);
                    m.gpu.push(Gpu { name: name.trim().into(), driver: None });
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{Gpu, MachineInfo};

    pub fn detect(m: &mut MachineInfo) {
        if let Ok(out) = std::process::Command::new("pmset").args(["-g", "ps"]).output() {
            let t = String::from_utf8_lossy(&out.stdout);
            m.power.ac = Some(t.contains("AC Power"));
        }
        if let Ok(out) = std::process::Command::new("system_profiler").arg("SPDisplaysDataType").output() {
            let t = String::from_utf8_lossy(&out.stdout);
            for l in t.lines() {
                if let Some(name) = l.trim().strip_prefix("Chipset Model:") {
                    m.gpu.push(Gpu { name: name.trim().into(), driver: None });
                }
            }
            let n = t.lines().filter(|l| l.contains("Resolution:")).count();
            m.displays = (n > 0).then_some(n as u32);
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod platform {
    pub fn detect(_m: &mut super::MachineInfo) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{FnCase, RunCtx};

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_763_305_445), "2025-11-16T15:04:05Z");
    }

    #[test]
    fn result_json_shape() {
        let case = FnCase {
            id: "B-MICRO-04",
            name: "openh264/S5/1920x1080/4000k".into(),
            level: Level::Micro,
            tiers: crate::registry::BOTH_TIERS,
            config_ids: &["WIN11"],
            runnable: Ok(()),
            run: |_c: &mut RunCtx| anyhow::bail!("x"),
        };
        let mut r = CaseResult::new(&case);
        r.param("frames", 120)
            .metric("encode_ms", Metric::from_values("ms", Better::Lower, Gate::Micro, vec![2.0, 3.0, 4.0]));
        r.extra.insert("source".into(), "ring".into());
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["id"], "B-MICRO-04");
        assert_eq!(j["case"], "openh264/S5/1920x1080/4000k");
        assert_eq!(j["level"], "micro");
        assert_eq!(j["status"], "ok");
        assert_eq!(j["source"], "ring");
        assert_eq!(j["metrics"]["encode_ms"]["median"], 3.0);
        assert!(j.get("ladder").is_none());
        let back: CaseResult = serde_json::from_value(j).unwrap();
        assert_eq!(back.extra["source"], "ring");
        let nr = CaseResult::not_runnable(&case, "no GPU");
        assert_eq!(serde_json::to_value(&nr).unwrap()["status"], "not-runnable");
        let mut inv = CaseResult::new(&case);
        inv.status = Status::InvalidReference;
        assert_eq!(serde_json::to_value(&inv).unwrap()["status"], "invalid_reference");
    }

    #[test]
    fn run_file_roundtrip() {
        let m = MachineInfo { alias: "TEST".into(), ..Default::default() };
        let mut f = RunFile::new(RunInfo::new("A", 1, Tier::Quick, "micro", m));
        let dir = std::env::temp_dir().join(format!("nuvira-bench-report-{}", std::process::id()));
        let p = dir.join("A/1/micro.json");
        f.write(&p).unwrap();
        let back = RunFile::read(&p).unwrap();
        assert_eq!(back.schema, SCHEMA);
        assert_eq!(back.run.side, "A");
        assert!(back.run.id.ends_with("-A-r1"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
