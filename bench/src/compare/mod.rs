//! `nuvira-bench compare` and `nuvira-bench import-go` (plan sections 8 and 9).
//!
//! `compare` reads the per-run JSON files of side A (`--base DIR`) and, when
//! given, side B (`--new DIR`): every `*.json` with schema `nuvira-bench/1`
//! below the folder, the round taken from `run.round` (or from an `r<k>` folder
//! name). For each (ID, case, metric) the value of a round is the median of
//! that run's values; rounds are paired by number (ABBA order is chosen by
//! `bench-compare.sh`), the ratio new/base of each pair is taken, and the
//! verdict follows section 9.3:
//!
//! - `regression`: worse by more than threshold + noise and the bootstrap CI95
//!   of the median ratio entirely on the worse side of 1;
//! - `suspect`: worse by more than the threshold but not the above
//!   (`bench-compare.sh` reruns those once with twice the rounds);
//! - `improved`: the mirror image; `ok` otherwise; `info` never gates.
//! - `quality` compares medians by absolute (dB, SSIM, fractions) or relative
//!   (bytes, BD-rate) difference; `exact` requires identical values;
//!   `security_both_ways` flags a change in either direction; `resource` adds
//!   absolute limits (e.g. heap slope > 2 MB/h).
//!
//! Without `--new` it writes the baseline table (medians and deviations).
//! With `--calibrate-out FILE` (A against A') it writes the noise per metric.
//!
//! Exit code (returned by [`cli_compare`]): 0 = no regression, 1 = at least one
//! `regression` or `exact_mismatch` (in `--ci` mode only quality and exact
//! gates count), 2 = invalid run (a case `failed` or `missing` on one side,
//! unreadable input).

pub mod go_import;
pub mod stats;
pub mod table;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};

use crate::measure::{Better, Gate, median};

pub const SCHEMA: &str = "nuvira-bench/1";
pub const COMPARE_SCHEMA: &str = "nuvira-bench-compare/1";
pub const NOISE_SCHEMA: &str = "nuvira-bench-noise/1";

/// Noise calibrations older than this are reported as stale (plan: 30 days).
const NOISE_MAX_AGE_DAYS: u64 = 30;

// ---- Input model ------------------------------------------------------------------

/// One metric of one run (one round of one side).
#[derive(Clone, Debug)]
pub struct MetricObs {
    pub unit: String,
    pub better: Better,
    pub gate: Gate,
    pub values: Vec<f64>,
    /// Median of `values`: the value of this round.
    pub value: f64,
}

/// One result (ID + case) of one run.
#[derive(Clone, Debug)]
pub struct Obs {
    pub round: u32,
    pub level: String,
    pub status: String,
    pub metrics: BTreeMap<String, MetricObs>,
    pub checksums: BTreeMap<String, String>,
    pub ladder: Vec<Value>,
    pub notes: Vec<String>,
}

pub type Key = (String, String);

/// Everything read from one side's folder.
#[derive(Default, Debug)]
pub struct SideData {
    pub dir: PathBuf,
    pub runs: Vec<Value>,
    pub obs: BTreeMap<Key, Vec<Obs>>,
    pub files: usize,
}

fn gate_of(v: &Value) -> Gate {
    v.as_str().and_then(|s| go_import::parse_gate(s).ok()).unwrap_or(Gate::Info)
}

fn better_of(v: &Value) -> Better {
    match v.as_str() {
        Some("higher") => Better::Higher,
        _ => Better::Lower,
    }
}

fn values_of(v: &Value) -> Vec<f64> {
    match v {
        Value::Array(a) => a.iter().map(|x| x.as_f64().unwrap_or(f64::NAN)).collect(),
        Value::Number(n) => vec![n.as_f64().unwrap_or(f64::NAN)],
        _ => Vec::new(),
    }
}

fn metric_obs(m: &Value) -> Option<MetricObs> {
    let mut values = values_of(m.get("values").unwrap_or(&Value::Null));
    if values.is_empty()
        && let Some(med) = m.get("median").and_then(Value::as_f64)
    {
        values.push(med);
    }
    if values.is_empty() {
        return None;
    }
    Some(MetricObs {
        unit: m.get("unit").and_then(Value::as_str).unwrap_or("").to_string(),
        better: better_of(m.get("better").unwrap_or(&Value::Null)),
        gate: gate_of(m.get("gate").unwrap_or(&Value::Null)),
        value: median(&values),
        values,
    })
}

fn single(unit: &str, better: Better, gate: Gate, v: f64) -> MetricObs {
    MetricObs { unit: unit.into(), better, gate, values: vec![v], value: v }
}

/// Metrics of a raw viewer `--stats-file` report (`client/viewer/src/report.rs`)
/// embedded by `lan-ab.sh` as `viewer_report` (B-MACRO-09).
fn viewer_report_metrics(r: &Value, out: &mut BTreeMap<String, MetricObs>) {
    use Better::*;
    use Gate::*;
    let fields: &[(&str, &str, Better, Gate)] = &[
        ("first_frame_ms", "ms", Lower, MacroLatency),
        ("fps", "fps", Higher, MacroThroughput),
        ("mbit_s", "Mbit/s", Lower, Info),
        ("latency_ms_p50", "ms", Lower, MacroLatency),
        ("latency_ms_p95", "ms", Lower, MacroLatency),
        ("latency_ms_max", "ms", Lower, Info),
        ("rtt_ms_p50", "ms", Lower, Info),
        ("encode_ms_avg", "ms", Lower, MacroLatency),
        ("decode_ms_avg", "ms", Lower, MacroLatency),
        ("render_ms_avg", "ms", Lower, MacroLatency),
        ("dropped", "1", Lower, Info),
        ("freezes", "1", Lower, Info),
        ("freeze_ms", "ms", Lower, Info),
        ("frames", "1", Higher, Info),
    ];
    for (name, unit, better, gate) in fields {
        if let Some(v) = r.get(*name).and_then(Value::as_f64) {
            out.insert(format!("viewer_{name}"), single(unit, *better, *gate, v));
        }
    }
}

/// Values of the agent's 5-second log line (`client/agent/src/pipeline.rs`
/// `Stats::report`): "X fps sent (Y captured, Z skipped), encoding W ms/frame, K kbit/s".
pub fn parse_agent_line(line: &str) -> Option<[f64; 5]> {
    let num = |s: &str| s.trim().parse::<f64>().ok();
    let (sent, rest) = line.split_once(" fps sent (")?;
    let sent = num(sent.rsplit(' ').next()?)?;
    let (captured, rest) = rest.split_once(" captured, ")?;
    let (skipped, rest) = rest.split_once(" skipped), encoding ")?;
    let (encode, rest) = rest.split_once(" ms/frame, ")?;
    let (kbit, _) = rest.split_once(" kbit/s")?;
    Some([sent, num(captured)?, num(skipped)?, num(encode)?, num(kbit)?])
}

fn agent_log_metrics(lines: &[Value], out: &mut BTreeMap<String, MetricObs>) {
    let parsed: Vec<[f64; 5]> = lines.iter().filter_map(Value::as_str).filter_map(parse_agent_line).collect();
    // The first line includes the session start: left out when there are others.
    let use_from = usize::from(parsed.len() > 1);
    let col = |i: usize| parsed[use_from..].iter().map(|p| p[i]).collect::<Vec<_>>();
    if parsed.is_empty() {
        return;
    }
    use Better::*;
    use Gate::*;
    let specs: [(&str, &str, Better, Gate); 5] = [
        ("agent_fps_sent", "fps", Higher, MacroThroughput),
        ("agent_fps_captured", "fps", Higher, Info),
        ("agent_skipped", "1", Lower, Info),
        ("agent_encode_ms", "ms", Lower, MacroLatency),
        ("agent_kbit_s", "kbit/s", Lower, Info),
    ];
    for (i, (name, unit, better, gate)) in specs.iter().enumerate() {
        let v = median(&col(i));
        out.insert(name.to_string(), single(unit, *better, *gate, v));
    }
}

fn round_from_path(p: &Path) -> Option<u32> {
    for c in p.components().rev() {
        let s = c.as_os_str().to_str()?;
        if let Some(n) = s.strip_prefix('r').and_then(|n| n.parse::<u32>().ok()) {
            return Some(n);
        }
    }
    None
}

fn collect_json(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let p = e?.path();
        if p.is_dir() {
            collect_json(&p, out)?;
        } else if p.extension().is_some_and(|x| x == "json") {
            out.push(p);
        }
    }
    Ok(())
}

/// Reads every nuvira-bench/1 file below `dir`.
pub fn load_side(dir: &Path) -> Result<SideData> {
    let mut files = Vec::new();
    collect_json(dir, &mut files)?;
    files.sort();
    let mut side = SideData { dir: dir.to_path_buf(), ..Default::default() };
    for f in files {
        let text = std::fs::read_to_string(&f).with_context(|| format!("reading {}", f.display()))?;
        let Ok(doc) = serde_json::from_str::<Value>(&text) else {
            bail!("{} is not valid JSON", f.display());
        };
        if doc.get("schema").and_then(Value::as_str) != Some(SCHEMA) {
            continue; // compare.json, noise.json, raw files...
        }
        side.files += 1;
        let run = doc.get("run").cloned().unwrap_or(Value::Null);
        let round = run
            .get("round")
            .and_then(Value::as_u64)
            .map(|r| r as u32)
            .or_else(|| round_from_path(&f))
            .unwrap_or(1);
        side.runs.push(run);
        for r in doc.get("results").and_then(Value::as_array).into_iter().flatten() {
            let id = r.get("id").and_then(Value::as_str).unwrap_or("?").to_string();
            let case = r.get("case").and_then(Value::as_str).unwrap_or("").to_string();
            let mut metrics = BTreeMap::new();
            if let Some(m) = r.get("metrics").and_then(Value::as_object) {
                for (name, mv) in m {
                    if let Some(o) = metric_obs(mv) {
                        metrics.insert(name.clone(), o);
                    }
                }
            }
            if let Some(vr) = r.get("viewer_report") {
                viewer_report_metrics(vr, &mut metrics);
            }
            if let Some(lines) = r.get("agent_log").and_then(Value::as_array) {
                agent_log_metrics(lines, &mut metrics);
            }
            let checksums = r
                .get("checksums")
                .and_then(Value::as_object)
                .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect())
                .unwrap_or_default();
            let notes = r
                .get("notes")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default();
            let obs = Obs {
                round,
                level: r.get("level").and_then(Value::as_str).unwrap_or("").to_string(),
                status: r.get("status").and_then(Value::as_str).unwrap_or("ok").to_string(),
                metrics,
                checksums,
                ladder: r.get("ladder").and_then(Value::as_array).cloned().unwrap_or_default(),
                notes,
            };
            side.obs.entry((id, case)).or_default().push(obs);
        }
    }
    Ok(side)
}

// ---- Thresholds and noise ----------------------------------------------------------

/// Gates of plan section 9.3 (percent unless said otherwise).
#[derive(Clone, Debug, Serialize)]
pub struct Thresholds {
    pub micro: f64,
    pub macro_latency: f64,
    pub macro_cpu: f64,
    pub macro_throughput: f64,
    pub security_both_ways: f64,
    pub resource_relative: f64,
    /// Absolute limits of resource metrics by metric name (e.g. heap_slope_mb_h: 2).
    pub resource_limits: BTreeMap<String, f64>,
    /// Quality: PSNR drop in dB, SSIM drop, BD-rate increase %, bytes/frame increase %,
    /// fraction drop in percentage points.
    pub psnr_db: f64,
    pub ssim: f64,
    pub bd_rate_pct: f64,
    pub bytes_pct: f64,
    pub fraction_points: f64,
    /// `--ci`: relative thresholds multiplied by `ci_multiplier` and speed gates report-only.
    pub ci: bool,
    /// Machines whose results never gate (`"machines": {"ALIAS": {"gate": false}}` in
    /// thresholds.json): every verdict is reported, none fails the run.
    pub info_only_machines: Vec<String>,
    /// This comparison is info-only (`--info-only`, or a machine listed above).
    pub info_only: bool,
    /// The comparison includes the 2R rerun of the suspects: a regression on noisy
    /// reps is then final (otherwise a machine whose reps are always noisy could
    /// never fail).
    pub rerun_done: bool,
    pub ci_multiplier: f64,
    pub bootstrap_resamples: usize,
    pub bootstrap_seed: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        let limits = [
            ("heap_slope_mb_h", 2.0),
            ("rss_slope_mb_h", 2.0),
            ("goroutines_delta", 0.0),
            ("threads_delta", 0.0),
            ("handles_delta", 0.0),
            ("fds_delta", 0.0),
            ("sockets_delta", 0.0),
            ("handles_per_session", 0.0),
            ("rss_per_session_kib", 64.0),
        ];
        Thresholds {
            micro: 5.0,
            macro_latency: 3.0,
            macro_cpu: 3.0,
            macro_throughput: 3.0,
            security_both_ways: 5.0,
            resource_relative: 50.0,
            resource_limits: limits.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            psnr_db: 0.10,
            ssim: 0.002,
            bd_rate_pct: 3.0,
            bytes_pct: 3.0,
            fraction_points: 1.0,
            ci: false,
            info_only_machines: Vec::new(),
            info_only: false,
            rerun_done: false,
            ci_multiplier: 2.0,
            bootstrap_resamples: stats::BOOTSTRAP_RESAMPLES,
            bootstrap_seed: 1,
        }
    }
}

fn num_in(v: &Value, keys: &[&str]) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    keys.iter().find_map(|k| v.get(*k).and_then(Value::as_f64))
}

impl Thresholds {
    /// Reads `bench/thresholds.json` (schema `nuvira-bench-thresholds/1`:
    /// `{"gates": {"micro": {"pct": 5}, "quality": {...}, "resource": {...}},
    /// "ci": {"multiplier": 2}, "decision": {...}}`; a flat `{"micro": 5}` works
    /// too). Quality drops may be written negative (`"psnr_db": -0.10`): the size
    /// counts. Missing entries keep the plan's values.
    pub fn load(path: &Path) -> Result<Thresholds> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let doc: Value =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let g = doc.get("gates").unwrap_or(&doc);
        let mut t = Thresholds::default();
        let pct = ["pct", "threshold_pct", "relative_pct", "percent", "threshold"];
        let set = |name: &str, field: &mut f64| {
            if let Some(n) = g.get(name).and_then(|v| num_in(v, &pct)) {
                *field = n;
            }
        };
        set("micro", &mut t.micro);
        set("macro_latency", &mut t.macro_latency);
        set("macro_cpu", &mut t.macro_cpu);
        set("macro_throughput", &mut t.macro_throughput);
        set("security_both_ways", &mut t.security_both_ways);
        if let Some(q) = g.get("quality") {
            let qs = |keys: &[&str], field: &mut f64| {
                if let Some(n) = keys.iter().find_map(|k| q.get(*k).and_then(Value::as_f64)) {
                    *field = n;
                }
            };
            qs(&["psnr_db", "psnr_drop_db", "psnr"], &mut t.psnr_db);
            qs(&["ssim", "ssim_drop"], &mut t.ssim);
            qs(&["bd_rate_pct", "bd_rate"], &mut t.bd_rate_pct);
            qs(&["bytes_pct", "bytes_frame_pct", "bytes"], &mut t.bytes_pct);
            qs(&["fraction_points", "fec_recovered_points", "recovered_points"], &mut t.fraction_points);
            for v in
                [&mut t.psnr_db, &mut t.ssim, &mut t.bd_rate_pct, &mut t.bytes_pct, &mut t.fraction_points]
            {
                *v = v.abs();
            }
        }
        if let Some(r) = g.get("resource") {
            if let Some(n) = num_in(r, &["pct", "relative_pct", "rss_slope_vs_baseline_pct"]) {
                t.resource_relative = n;
            }
            let mut limit = |key: &str, metrics: &[&str]| {
                if let Some(n) = r.get(key).and_then(Value::as_f64) {
                    for m in metrics {
                        t.resource_limits.insert(m.to_string(), n);
                    }
                }
            };
            limit("rss_slope_mb_per_h", &["rss_slope_mb_h", "rss_slope_mb_per_h", "heap_slope_mb_h"]);
            limit("churn_handles_per_session", &["handles_per_session"]);
            limit("churn_rss_kib_per_session", &["rss_per_session_kib", "rss_kib_per_session"]);
            limit("file_peak_rss_independent_mib", &["peak_rss_growth_mib"]);
            if let Some(l) = r.get("limits").and_then(Value::as_object) {
                for (k, v) in l {
                    if let Some(n) = v.as_f64() {
                        t.resource_limits.insert(k.clone(), n);
                    }
                }
            }
        }
        if let Some(ms) = doc.get("machines").and_then(Value::as_object) {
            for (alias, cfg) in ms {
                if cfg.get("gate").and_then(Value::as_bool) == Some(false) {
                    t.info_only_machines.push(alias.clone());
                }
            }
        }
        if let Some(m) = doc.get("ci").and_then(|c| c.get("multiplier")).and_then(Value::as_f64) {
            t.ci_multiplier = m;
        }
        if let Some(d) = doc.get("decision") {
            if let Some(n) = d.get("bootstrap_resamples").and_then(Value::as_u64) {
                t.bootstrap_resamples = n.max(100) as usize;
            }
            if let Some(n) = d.get("bootstrap_seed").and_then(Value::as_u64) {
                t.bootstrap_seed = n;
            }
        }
        Ok(t)
    }

    /// Relative threshold of a gate (doubled in CI mode); None for gates that
    /// are not ratio based.
    pub fn relative(&self, gate: Gate) -> Option<f64> {
        let base = match gate {
            Gate::Micro => self.micro,
            Gate::MacroLatency => self.macro_latency,
            Gate::MacroCpu => self.macro_cpu,
            Gate::MacroThroughput => self.macro_throughput,
            Gate::SecurityBothWays => self.security_both_ways,
            Gate::Resource => self.resource_relative,
            _ => return None,
        };
        Some(if self.ci { base * self.ci_multiplier } else { base })
    }
}

/// Noise per (ID, case, metric) of this machine.
#[derive(Default, Debug)]
pub struct Noise {
    pub entries: BTreeMap<String, (f64, f64)>,
    pub created_unix: Option<u64>,
    pub path: Option<PathBuf>,
}

pub fn noise_key(id: &str, case: &str, metric: &str) -> String {
    format!("{id}|{case}|{metric}")
}

impl Noise {
    pub fn load(path: &Path) -> Result<Noise> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let doc: Value = serde_json::from_str(&text)?;
        let mut n = Noise { path: Some(path.into()), ..Default::default() };
        n.created_unix = doc.get("created_unix").and_then(Value::as_u64);
        if let Some(o) = doc.get("noise").and_then(Value::as_object) {
            for (k, v) in o {
                let pct = v.get("pct").and_then(Value::as_f64).or_else(|| v.as_f64()).unwrap_or(0.0);
                let abs = v.get("abs").and_then(Value::as_f64).unwrap_or(0.0);
                n.entries.insert(k.clone(), (pct, abs));
            }
        }
        Ok(n)
    }

    pub fn age_days(&self) -> Option<u64> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
        self.created_unix.map(|c| now.saturating_sub(c) / 86_400)
    }

    fn get(&self, id: &str, case: &str, metric: &str) -> Option<(f64, f64)> {
        self.entries.get(&noise_key(id, case, metric)).copied()
    }
}

// ---- Verdicts --------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct Row {
    pub id: String,
    pub case: String,
    pub level: String,
    pub metric: String,
    pub unit: String,
    pub gate: Gate,
    pub better: Better,
    pub base_median: Option<f64>,
    pub new_median: Option<f64>,
    /// (new / base - 1) x 100, sign as measured (not "worse").
    pub delta_pct: Option<f64>,
    /// For absolute comparisons: new - base.
    pub delta_abs: Option<f64>,
    pub noise_pct: Option<f64>,
    pub threshold_pct: Option<f64>,
    pub threshold_abs: Option<f64>,
    /// CI95 of the median ratio new/base (as measured, not oriented).
    pub ci95_ratio: Option<[f64; 2]>,
    pub rounds_paired: usize,
    pub verdict: String,
    /// In `--ci` mode a speed regression is reported but does not fail.
    pub report_only: bool,
    pub notes: Vec<String>,
}

fn round_values(obs: &[Obs], metric: &str) -> Vec<(u32, f64)> {
    obs.iter().filter_map(|o| o.metrics.get(metric).map(|m| (o.round, m.value))).collect()
}

fn all_values(obs: &[Obs], metric: &str) -> Vec<f64> {
    obs.iter().filter_map(|o| o.metrics.get(metric)).flat_map(|m| m.values.iter().copied()).collect()
}

fn fmt_noise_missing(noise: &Noise) -> bool {
    noise.entries.is_empty()
}

/// Kind of a quality comparison.
enum QualityKind {
    /// Absolute drop allowed (threshold in the metric's unit).
    Abs(f64),
    /// Relative increase allowed, percent.
    Rel(f64),
}

fn quality_kind(name: &str, unit: &str, t: &Thresholds) -> QualityKind {
    let n = name.to_ascii_lowercase();
    if unit == "dB" || n.contains("psnr") {
        QualityKind::Abs(t.psnr_db)
    } else if n.contains("ssim") {
        QualityKind::Abs(t.ssim)
    } else if n.contains("bd_rate") {
        QualityKind::Abs(t.bd_rate_pct)
    } else if unit == "%" || unit == "pts" {
        QualityKind::Abs(t.fraction_points)
    } else if n.contains("fraction") || n.contains("recovered") {
        QualityKind::Abs(t.fraction_points / 100.0)
    } else {
        QualityKind::Rel(t.bytes_pct)
    }
}

#[allow(clippy::too_many_arguments)]
fn judge_metric(
    id: &str,
    case: &str,
    level: &str,
    name: &str,
    spec: &MetricObs,
    base: &[Obs],
    new: &[Obs],
    t: &Thresholds,
    noise: &Noise,
) -> Row {
    let bv = round_values(base, name);
    let nv = round_values(new, name);
    let bmed = median(&bv.iter().map(|x| x.1).collect::<Vec<_>>());
    let nmed = median(&nv.iter().map(|x| x.1).collect::<Vec<_>>());
    let mut row = Row {
        id: id.into(),
        case: case.into(),
        level: level.into(),
        metric: name.into(),
        unit: spec.unit.clone(),
        gate: spec.gate,
        better: spec.better,
        base_median: bmed.is_finite().then_some(bmed),
        new_median: nmed.is_finite().then_some(nmed),
        delta_pct: (bmed.is_finite() && nmed.is_finite() && bmed != 0.0).then(|| (nmed / bmed - 1.0) * 100.0),
        delta_abs: (bmed.is_finite() && nmed.is_finite()).then_some(nmed - bmed),
        noise_pct: None,
        threshold_pct: None,
        threshold_abs: None,
        ci95_ratio: None,
        rounds_paired: 0,
        verdict: "ok".into(),
        report_only: false,
        notes: Vec::new(),
    };
    if bv.is_empty() || nv.is_empty() {
        // An informational metric on one side only does not invalidate the run.
        row.verdict = if spec.gate == Gate::Info { "info" } else { "missing" }.into();
        row.notes.push(format!("metric only on side {}", if bv.is_empty() { "B" } else { "A" }));
        return row;
    }
    let (noise_pct, noise_abs) = noise.get(id, case, name).unwrap_or((0.0, 0.0));
    row.noise_pct = Some(noise_pct);
    let higher = spec.better == Better::Higher;
    match spec.gate {
        Gate::Info => row.verdict = "info".into(),
        Gate::Exact => {
            let a = all_values(base, name);
            let b = all_values(new, name);
            // Deterministic values: the same set of distinct values on both sides
            // (round counts may differ).
            let distinct = |v: &[f64]| {
                let mut d: Vec<u64> = v.iter().map(|x| x.to_bits()).collect();
                d.sort_unstable();
                d.dedup();
                d
            };
            let same = distinct(&a) == distinct(&b);
            row.verdict = if same { "ok" } else { "exact_mismatch" }.into();
        }
        Gate::Quality => {
            // Deterministic: medians, no ratio statistics.
            let worse_abs = if higher { bmed - nmed } else { nmed - bmed };
            match quality_kind(name, &spec.unit, t) {
                QualityKind::Abs(thr) => {
                    row.threshold_abs = Some(thr);
                    let allowed = thr + noise_abs;
                    row.verdict = if worse_abs > allowed + 1e-12 {
                        "regression"
                    } else if -worse_abs > allowed + 1e-12 {
                        "improved"
                    } else {
                        "ok"
                    }
                    .into();
                }
                QualityKind::Rel(thr) => {
                    row.threshold_pct = Some(thr);
                    let worse_pct = if bmed != 0.0 { 100.0 * worse_abs / bmed.abs() } else { 0.0 };
                    let allowed = thr + noise_pct;
                    row.verdict = if worse_pct > allowed {
                        "regression"
                    } else if -worse_pct > allowed {
                        "improved"
                    } else {
                        "ok"
                    }
                    .into();
                    if bmed == 0.0 && nmed != 0.0 && worse_abs > 0.0 {
                        row.verdict = "regression".into();
                    }
                }
            }
        }
        gate => {
            let thr = t.relative(gate).unwrap_or(t.micro);
            row.threshold_pct = Some(thr);
            row.report_only = t.ci;
            let mut ratios = stats::paired_ratios(&bv, &nv);
            row.rounds_paired = ratios.len();
            if ratios.is_empty() {
                ratios = stats::unpaired_ratios(&bv, &nv);
                if !ratios.is_empty() {
                    row.notes.push("rounds not paired: ratios against the base median".into());
                }
            }
            if ratios.is_empty() {
                // Base 0 (allocs, deltas): absolute comparison.
                let worse = if higher { bmed - nmed } else { nmed - bmed };
                let all_worse =
                    nv.iter().all(|(_, n)| bv.iter().all(|(_, b)| if higher { n < b } else { n > b }));
                row.verdict = if worse > 0.0 && all_worse {
                    "regression"
                } else if worse > 0.0 {
                    "suspect"
                } else if worse < 0.0 {
                    "improved"
                } else {
                    "ok"
                }
                .into();
            } else {
                let (lo, hi) = stats::bootstrap_ci95_median(&ratios, t.bootstrap_resamples, t.bootstrap_seed);
                row.ci95_ratio = Some([lo, hi]);
                let r = median(&ratios);
                // Oriented: q > 1 is worse.
                let (q, q_lo, q_hi) = if higher { (1.0 / r, 1.0 / hi, 1.0 / lo) } else { (r, lo, hi) };
                let worse_pct = (q - 1.0) * 100.0;
                let allowed = thr + noise_pct;
                row.verdict = if gate == Gate::SecurityBothWays {
                    if worse_pct.abs() > allowed && (q_lo > 1.0 || q_hi < 1.0) {
                        row.notes.push(
                            if worse_pct > 0.0 { "slower" } else { "faster: protection weakened" }.into(),
                        );
                        "regression"
                    } else if worse_pct.abs() > thr {
                        "suspect"
                    } else {
                        "ok"
                    }
                } else if worse_pct > allowed && q_lo > 1.0 {
                    "regression"
                } else if worse_pct > thr {
                    "suspect"
                } else if -worse_pct > allowed && q_hi < 1.0 {
                    "improved"
                } else {
                    "ok"
                }
                .into();
                // The paired ratios must agree with the medians: with values at or near
                // 0 (e.g. fps of a link that delivers no frame) a single 0/x ratio can
                // say "infinitely worse" while both medians are equal.
                let medians_worse = if higher { nmed < bmed } else { nmed > bmed };
                let medians_better = if higher { nmed > bmed } else { nmed < bmed };
                if gate != Gate::SecurityBothWays
                    && ((row.verdict == "regression" && !medians_worse)
                        || (row.verdict == "improved" && !medians_better))
                {
                    row.notes.push("paired ratios and medians disagree".into());
                    row.verdict = if nmed == bmed { "ok" } else { "suspect" }.into();
                }
            }
            // Absolute limits (leak rules): over the limit and clearly above the
            // baseline (beyond its noise and the relative resource threshold), so a
            // count that is already above the limit at the baseline (e.g. 24 vs 25
            // handles after an in-process session) is not a regression by itself.
            let margin = noise_abs.max(bmed.abs() * thr / 100.0);
            if gate == Gate::Resource
                && let Some(limit) = t.resource_limits.get(name)
                && nmed > *limit
                && (bmed <= *limit || nmed - bmed > margin)
            {
                row.notes.push(format!("above the absolute limit {limit}"));
                row.verdict = "regression".into();
            }
            if gate == Gate::Resource && row.verdict == "regression" {
                // Leaks are not timing noise: they count in CI mode too.
                row.report_only = false;
            }
        }
    }
    row
}

/// BD-rate (percent, B vs A) on the ladders of the first round of each side,
/// for quality `q` (`psnr_yuv_db` or `ssim_y`).
fn ladder_bd_rate(base: &[Obs], new: &[Obs], q: &str) -> Option<f64> {
    let pts = |o: &Obs| -> Vec<(f64, f64)> {
        o.ladder
            .iter()
            .filter_map(|p| Some((p.get("kbps_actual")?.as_f64()?, p.get(q)?.as_f64()?)))
            .filter(|(r, v)| *r > 0.0 && v.is_finite())
            .collect()
    };
    let a = base.iter().min_by_key(|o| o.round).map(pts)?;
    let b = new.iter().min_by_key(|o| o.round).map(pts)?;
    if a.len() < 2 || b.len() < 2 {
        return None;
    }
    let v = crate::quality::bd_rate(&a, &b);
    v.is_finite().then_some(v)
}

/// Ladder point of one result: (kbps_actual, quality `q`) of its single ladder
/// entry, median over the rounds (B-MICRO-04/05 store one point per case).
fn single_point(obs: &[Obs], q: &str) -> Option<(f64, f64)> {
    let mut rates = Vec::new();
    let mut quals = Vec::new();
    for o in obs {
        let [p] = o.ladder.as_slice() else { return None };
        let (Some(r), Some(v)) =
            (p.get("kbps_actual").and_then(Value::as_f64), p.get(q).and_then(Value::as_f64))
        else {
            continue;
        };
        if r > 0.0 && v.is_finite() {
            rates.push(r);
            quals.push(v);
        }
    }
    (!rates.is_empty()).then(|| (median(&rates), median(&quals)))
}

/// `"openh264/S5/1920x1080/4000k"` -> `"openh264/S5/1920x1080"`: the case name
/// without its bitrate, which groups the points of one ladder.
pub fn ladder_group(case: &str) -> Option<&str> {
    let (head, last) = case.rsplit_once('/')?;
    let digits = last.strip_suffix('k')?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(head)
}

/// A BD-rate row (quality gate, absolute threshold in percent).
#[allow(clippy::too_many_arguments)]
fn bd_row(
    id: &str,
    case: &str,
    level: &str,
    q: &str,
    bd: f64,
    points: usize,
    t: &Thresholds,
    noise: &Noise,
    note: &str,
) -> Row {
    let allowed = t.bd_rate_pct + noise.get(id, case, &format!("bd_rate_{q}")).map_or(0.0, |x| x.1);
    Row {
        id: id.into(),
        case: case.into(),
        level: level.into(),
        metric: format!("bd_rate_{q}"),
        unit: "%".into(),
        gate: Gate::Quality,
        better: Better::Lower,
        base_median: Some(0.0),
        new_median: Some(bd),
        delta_pct: None,
        delta_abs: Some(bd),
        noise_pct: None,
        threshold_pct: None,
        threshold_abs: Some(t.bd_rate_pct),
        ci95_ratio: None,
        rounds_paired: points,
        verdict: if bd > allowed {
            "regression"
        } else if bd < -allowed {
            "improved"
        } else {
            "ok"
        }
        .into(),
        report_only: false,
        notes: vec![note.into()],
    }
}

/// BD-rate across cases: results whose case names differ only in the trailing
/// bitrate (`.../<kbps>k`, one ladder point each, as B-MICRO-04/05) form one
/// ladder per (ID, case without the bitrate). Rows are named `<group>/ladder`.
fn grouped_bd_rows(
    base: &SideData,
    new: &SideData,
    t: &Thresholds,
    noise: &Noise,
    filter: Option<&regex::Regex>,
) -> Vec<Row> {
    // (id, group) -> [(level, base obs, new obs)]
    type Members<'a> = Vec<(&'a str, &'a [Obs], &'a [Obs])>;
    let mut groups: BTreeMap<(String, String), Members> = BTreeMap::new();
    for (key, b) in &base.obs {
        let (id, case) = key;
        let Some(n) = new.obs.get(key) else { continue };
        let Some(group) = ladder_group(case) else { continue };
        if let Some(f) = filter
            && !f.is_match(&format!("{id}/{case}"))
        {
            continue;
        }
        if status_of(b) != "ok" && status_of(b) != "noisy" || status_of(n) != "ok" && status_of(n) != "noisy"
        {
            continue;
        }
        let level = b.first().map_or("", |o| o.level.as_str());
        groups.entry((id.clone(), group.to_string())).or_default().push((level, b, n));
    }
    let mut rows = Vec::new();
    for ((id, group), members) in groups {
        if members.len() < 2 {
            continue;
        }
        let level = members[0].0;
        for q in ["psnr_yuv_db", "ssim_y"] {
            let mut a = Vec::new();
            let mut b = Vec::new();
            for (_, bo, no) in &members {
                // A point counts only when both sides have it (same ladder on both).
                if let (Some(pa), Some(pb)) = (single_point(bo, q), single_point(no, q)) {
                    a.push(pa);
                    b.push(pb);
                }
            }
            if a.len() < 2 {
                continue;
            }
            a.sort_by(|x, y| x.0.total_cmp(&y.0));
            b.sort_by(|x, y| x.0.total_cmp(&y.0));
            let bd = crate::quality::bd_rate(&a, &b);
            if bd.is_finite() {
                let case = format!("{group}/ladder");
                let note =
                    format!("BD-rate of B against A over {} ladder points (cases {group}/*k)", a.len());
                rows.push(bd_row(&id, &case, level, q, bd, a.len(), t, noise, &note));
            }
        }
    }
    rows
}

/// Comparison outcome.
#[derive(Debug, Serialize)]
pub struct Outcome {
    pub rows: Vec<Row>,
    /// (ID, case, reason)
    pub not_runnable: Vec<(String, String, String)>,
    pub invalid_reference: Vec<(String, String)>,
    pub problems: Vec<String>,
    pub checksum_changes: Vec<String>,
    pub exit_code: i32,
}

fn status_of(obs: &[Obs]) -> &str {
    for s in ["failed", "not-runnable", "invalid_reference", "noisy"] {
        if obs.iter().any(|o| o.status == s) {
            return s;
        }
    }
    "ok"
}

fn first_note(obs: &[Obs]) -> String {
    obs.iter().flat_map(|o| o.notes.iter()).next().cloned().unwrap_or_default()
}

pub fn compare(
    base: &SideData,
    new: &SideData,
    t: &Thresholds,
    noise: &Noise,
    filter: Option<&regex::Regex>,
) -> Outcome {
    let mut out = Outcome {
        rows: Vec::new(),
        not_runnable: Vec::new(),
        invalid_reference: Vec::new(),
        problems: Vec::new(),
        checksum_changes: Vec::new(),
        exit_code: 0,
    };
    let keys: BTreeSet<&Key> = base.obs.keys().chain(new.obs.keys()).collect();
    let empty: Vec<Obs> = Vec::new();
    for key in keys {
        let (id, case) = key;
        if let Some(f) = filter
            && !f.is_match(&format!("{id}/{case}"))
        {
            continue;
        }
        let b = base.obs.get(key).unwrap_or(&empty);
        let n = new.obs.get(key).unwrap_or(&empty);
        let level = b.first().or(n.first()).map(|o| o.level.clone()).unwrap_or_default();
        let (sb, sn) = (status_of(b), status_of(n));
        let push_problem = |out: &mut Outcome, verdict: &str, note: String| {
            out.rows.push(Row {
                id: id.clone(),
                case: case.clone(),
                level: level.clone(),
                metric: "-".into(),
                unit: String::new(),
                gate: Gate::Info,
                better: Better::Lower,
                base_median: None,
                new_median: None,
                delta_pct: None,
                delta_abs: None,
                noise_pct: None,
                threshold_pct: None,
                threshold_abs: None,
                ci95_ratio: None,
                rounds_paired: 0,
                verdict: verdict.into(),
                report_only: false,
                notes: vec![note.clone()],
            });
            out.problems.push(format!("{verdict}: {id} {case}: {note}"));
        };
        if b.is_empty() || n.is_empty() {
            let other = if b.is_empty() { n } else { b };
            if status_of(other) == "not-runnable" {
                out.not_runnable.push((id.clone(), case.clone(), first_note(other)));
            } else {
                push_problem(
                    &mut out,
                    "missing",
                    format!("no result on side {}", if b.is_empty() { "A" } else { "B" }),
                );
            }
            continue;
        }
        if sb == "failed" || sn == "failed" {
            let side = if sb == "failed" { b } else { n };
            push_problem(
                &mut out,
                "failed",
                format!("side {}: {}", if sb == "failed" { "A" } else { "B" }, first_note(side)),
            );
            continue;
        }
        match (sb == "not-runnable", sn == "not-runnable") {
            (true, true) => {
                out.not_runnable.push((id.clone(), case.clone(), first_note(b)));
                continue;
            }
            (true, false) | (false, true) => {
                push_problem(
                    &mut out,
                    "missing",
                    format!("not-runnable on side {} only", if sb == "not-runnable" { "A" } else { "B" }),
                );
                continue;
            }
            _ => {}
        }
        if sb == "invalid_reference" || sn == "invalid_reference" {
            out.invalid_reference.push((id.clone(), case.clone()));
            continue;
        }
        let names: BTreeSet<&String> = b.iter().chain(n.iter()).flat_map(|o| o.metrics.keys()).collect();
        for name in names {
            let spec = b.iter().chain(n.iter()).find_map(|o| o.metrics.get(name)).expect("metric present");
            let mut row = judge_metric(id, case, &level, name, spec, b, n, t, noise);
            if sb == "noisy" || sn == "noisy" {
                row.notes.push("noisy reps".into());
                // Other processes used the CPU beyond the guard's limit during this
                // case even after the retries: a timing regression measured then is
                // re-measured (suspect -> 2R rerun), not reported as found.
                // Deterministic gates (exact, quality of software paths) are kept.
                if row.verdict == "regression"
                    && !t.rerun_done
                    && !matches!(row.gate, Gate::Exact | Gate::Quality | Gate::Resource)
                {
                    row.verdict = "suspect".into();
                    row.notes.push("regression under background load: rerun".into());
                }
            }
            out.rows.push(row);
        }
        for q in ["psnr_yuv_db", "ssim_y"] {
            if let Some(bd) = ladder_bd_rate(b, n, q) {
                let note = "BD-rate of B against A on the bitrate ladder";
                out.rows.push(bd_row(id, case, &level, q, bd, 0, t, noise, note));
            }
        }
        // Checksums: reported (information), exactness is a metric with gate "exact".
        let cb = &b[0].checksums;
        let cn = &n[0].checksums;
        for (k, v) in cb {
            if let Some(w) = cn.get(k)
                && v != w
            {
                out.checksum_changes.push(format!("{id} {case}: {k} changed"));
            }
        }
    }
    out.rows.extend(grouped_bd_rows(base, new, t, noise, filter));
    if t.info_only {
        for r in &mut out.rows {
            r.report_only = true;
        }
    }
    let mut code = 0;
    for r in &out.rows {
        match r.verdict.as_str() {
            "missing" | "failed" => code = 2,
            _ if t.info_only => {}
            "exact_mismatch" => code = code.max(1),
            "regression" if !r.report_only => code = code.max(1),
            _ => {}
        }
    }
    if base.files == 0 || new.files == 0 {
        out.problems.push("no nuvira-bench/1 file on one side".into());
        code = 2;
    }
    out.exit_code = code;
    out
}

/// Replaces the results of the keys present in `over` (the suspect rerun).
pub fn apply_override(side: &mut SideData, over: SideData) {
    for (k, v) in over.obs {
        side.obs.insert(k, v);
    }
}

// ---- Calibration -----------------------------------------------------------------

pub fn calibrate(a: &SideData, a2: &SideData) -> Value {
    let mut noise = serde_json::Map::new();
    for (key, b) in &a.obs {
        let Some(n) = a2.obs.get(key) else { continue };
        let names: BTreeSet<&String> = b.iter().chain(n.iter()).flat_map(|o| o.metrics.keys()).collect();
        for name in names {
            let bv = round_values(b, name);
            let nv = round_values(n, name);
            if bv.is_empty() || nv.is_empty() {
                continue;
            }
            let ratios = stats::paired_ratios(&bv, &nv);
            let mut pooled: Vec<f64> = bv.iter().map(|x| x.1).collect();
            pooled.extend(nv.iter().map(|x| x.1));
            let pct = stats::noise_pct(&ratios, &pooled);
            let abs = stats::noise_abs(
                &bv.iter().map(|x| x.1).collect::<Vec<_>>(),
                &nv.iter().map(|x| x.1).collect::<Vec<_>>(),
            );
            noise.insert(
                noise_key(&key.0, &key.1, name),
                json!({"pct": round6(pct), "abs": round6(abs), "rounds": ratios.len()}),
            );
        }
    }
    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let machine = a.runs.iter().find_map(|r| r.get("machine").cloned()).unwrap_or(Value::Null);
    json!({"schema": NOISE_SCHEMA, "created_unix": now, "machine": machine, "noise": noise})
}

fn round6(x: f64) -> f64 {
    if x.is_finite() { (x * 1e6).round() / 1e6 } else { 0.0 }
}

// ---- CLI -------------------------------------------------------------------------

const COMPARE_USAGE: &str =
    "usage: nuvira-bench compare --base DIR [--new DIR] [--thresholds FILE] [--noise FILE]
    [--markdown FILE] [--json FILE] [--ci] [--filter REGEX] [--calibrate-out FILE]
    [--rerun-base DIR --rerun-new DIR] [--suspects FILE] [--title TEXT] [--info-only]
  Without --new: baseline table of side A. Exit: 0 ok, 1 regression, 2 invalid run.";

const IMPORT_USAGE: &str =
    "usage: nuvira-bench import-go --src DIR --side A|B --round N --out FILE [--set key=value]... RAW...
  RAW = text output of `go test -bench . -benchmem` (one file per package).";

struct Args {
    items: Vec<String>,
    i: usize,
}

impl Args {
    fn value(&mut self, flag: &str) -> Result<String> {
        self.i += 1;
        self.items.get(self.i).cloned().with_context(|| format!("{flag} needs a value"))
    }
}

/// `nuvira-bench compare ARGS` (ARGS without the subcommand); returns the exit code.
pub fn cli_compare(args: &[String]) -> Result<i32> {
    let mut a = Args { items: args.to_vec(), i: 0 };
    let (mut base, mut new, mut thresholds, mut noise_path, mut md, mut js) =
        (None, None, None, None, None, None);
    let (mut ci, mut filter, mut calib, mut rb, mut rn, mut suspects, mut title) =
        (false, None, None, None, None, None, None);
    let mut info_only = false;
    while a.i < a.items.len() {
        let f = a.items[a.i].clone();
        match f.as_str() {
            "--base" => base = Some(PathBuf::from(a.value(&f)?)),
            "--new" => new = Some(PathBuf::from(a.value(&f)?)),
            "--thresholds" => thresholds = Some(PathBuf::from(a.value(&f)?)),
            "--noise" => noise_path = Some(PathBuf::from(a.value(&f)?)),
            "--markdown" => md = Some(PathBuf::from(a.value(&f)?)),
            "--json" => js = Some(PathBuf::from(a.value(&f)?)),
            "--ci" => ci = true,
            "--info-only" => info_only = true,
            "--filter" => filter = Some(a.value(&f)?),
            "--calibrate-out" => calib = Some(PathBuf::from(a.value(&f)?)),
            "--rerun-base" => rb = Some(PathBuf::from(a.value(&f)?)),
            "--rerun-new" => rn = Some(PathBuf::from(a.value(&f)?)),
            "--suspects" => suspects = Some(PathBuf::from(a.value(&f)?)),
            "--title" => title = Some(a.value(&f)?),
            "-h" | "--help" => {
                println!("{COMPARE_USAGE}");
                return Ok(0);
            }
            other => {
                eprintln!("unknown option {other}\n{COMPARE_USAGE}");
                return Ok(2);
            }
        }
        a.i += 1;
    }
    let Some(base) = base else {
        eprintln!("{COMPARE_USAGE}");
        return Ok(2);
    };
    let filter = match filter.as_deref().map(regex::Regex::new).transpose() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("--filter: {e}");
            return Ok(2);
        }
    };
    let mut t = match &thresholds {
        Some(p) if p.is_file() => Thresholds::load(p)?,
        Some(p) => {
            eprintln!("warning: {} not found, using the plan's thresholds", p.display());
            Thresholds::default()
        }
        None => Thresholds::default(),
    };
    t.ci = ci;
    t.info_only = info_only;
    let mut base_data = match load_side(&base) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e:#}");
            return Ok(2);
        }
    };
    let Some(new) = new else {
        let md_text = table::baseline_markdown(&base_data, title.as_deref());
        write_or_print(md.as_deref(), &md_text)?;
        if let Some(j) = &js {
            write_file(j, &serde_json::to_string_pretty(&table::baseline_json(&base_data))?)?;
        }
        return Ok(if base_data.files == 0 { 2 } else { 0 });
    };
    let mut new_data = match load_side(&new) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e:#}");
            return Ok(2);
        }
    };
    // A machine listed with "gate": false in thresholds.json only informs.
    let alias =
        |d: &SideData| d.runs.iter().find_map(|r| r.get("machine")?.get("alias")?.as_str().map(String::from));
    for a in [alias(&base_data), alias(&new_data)].into_iter().flatten() {
        if t.info_only_machines.contains(&a) && !t.info_only {
            eprintln!(
                "machine {a} is info-only (thresholds.json \"machines\"): verdicts are reported, none gates"
            );
            t.info_only = true;
        }
    }
    if let Some(p) = &calib {
        let doc = calibrate(&base_data, &new_data);
        write_file(p, &serde_json::to_string_pretty(&doc)?)?;
        eprintln!(
            "noise of {} metrics written to {}",
            doc["noise"].as_object().map_or(0, |o| o.len()),
            p.display()
        );
    }
    if let (Some(rb), Some(rn)) = (&rb, &rn) {
        t.rerun_done = true;
        apply_override(&mut base_data, load_side(rb)?);
        apply_override(&mut new_data, load_side(rn)?);
    }
    let noise = match &noise_path {
        Some(p) if p.is_file() => Noise::load(p)?,
        _ => Noise::default(),
    };
    let outcome = compare(&base_data, &new_data, &t, &noise, filter.as_ref());
    let md_text = table::compare_markdown(&base_data, &new_data, &outcome, &t, &noise, title.as_deref());
    write_or_print(md.as_deref(), &md_text)?;
    if let Some(j) = &js {
        let doc = json!({
            "schema": COMPARE_SCHEMA,
            "base": {"dir": base_data.dir, "runs": base_data.runs.len(), "repos": first_field(&base_data, "repos"),
                     "machine": first_field(&base_data, "machine"), "toolchain": first_field(&base_data, "toolchain")},
            "new": {"dir": new_data.dir, "runs": new_data.runs.len(), "repos": first_field(&new_data, "repos"),
                    "machine": first_field(&new_data, "machine"), "toolchain": first_field(&new_data, "toolchain")},
            "thresholds": t,
            "noise": {"file": noise.path, "age_days": noise.age_days(), "calibrated": !fmt_noise_missing(&noise)},
            "rows": outcome.rows,
            "not_runnable": outcome.not_runnable,
            "invalid_reference": outcome.invalid_reference,
            "checksum_changes": outcome.checksum_changes,
            "problems": outcome.problems,
            "summary": table::summary_counts(&outcome),
            "exit_code": outcome.exit_code,
        });
        write_file(j, &serde_json::to_string_pretty(&doc)?)?;
    }
    if let Some(s) = &suspects {
        let mut seen = BTreeSet::new();
        let mut text = String::new();
        for r in outcome.rows.iter().filter(|r| r.verdict == "suspect") {
            if seen.insert((r.id.clone(), r.case.clone())) {
                text.push_str(&format!("{}\t{}\t{}\n", r.id, r.case, r.level));
            }
        }
        write_file(s, &text)?;
    }
    Ok(outcome.exit_code)
}

fn first_field(side: &SideData, field: &str) -> Value {
    side.runs.iter().find_map(|r| r.get(field).cloned()).unwrap_or(Value::Null)
}

fn write_file(p: &Path, text: &str) -> Result<()> {
    if let Some(dir) = p.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(p, text).with_context(|| format!("writing {}", p.display()))
}

fn write_or_print(p: Option<&Path>, text: &str) -> Result<()> {
    match p {
        Some(p) => write_file(p, text),
        None => {
            print!("{text}");
            Ok(())
        }
    }
}

/// `nuvira-bench import-go ARGS`; returns the exit code (2 on bad input).
pub fn cli_import_go(args: &[String]) -> Result<i32> {
    let mut a = Args { items: args.to_vec(), i: 0 };
    let (mut src, mut side, mut round, mut out) = (None, None, None, None);
    let mut raws = Vec::new();
    let mut extra = BTreeMap::new();
    while a.i < a.items.len() {
        let f = a.items[a.i].clone();
        match f.as_str() {
            "--src" => src = Some(PathBuf::from(a.value(&f)?)),
            "--side" => side = Some(a.value(&f)?),
            "--round" => round = Some(a.value(&f)?.parse::<u32>().context("--round")?),
            "--out" => out = Some(PathBuf::from(a.value(&f)?)),
            "--raw" => raws.push(PathBuf::from(a.value(&f)?)),
            "--set" => {
                let kv = a.value(&f)?;
                let (k, v) = kv.split_once('=').context("--set needs key=value")?;
                extra.insert(k.to_string(), v.to_string());
            }
            "-h" | "--help" => {
                println!("{IMPORT_USAGE}");
                return Ok(0);
            }
            s if s.starts_with("--") => {
                eprintln!("unknown option {s}\n{IMPORT_USAGE}");
                return Ok(2);
            }
            _ => raws.push(PathBuf::from(&f)),
        }
        a.i += 1;
    }
    let (Some(src), Some(side), Some(out)) = (src, side, out) else {
        eprintln!("{IMPORT_USAGE}");
        return Ok(2);
    };
    if raws.is_empty() {
        eprintln!("no raw go output given\n{IMPORT_USAGE}");
        return Ok(2);
    }
    let decls = go_import::scan_sources(&src)?;
    let doc = go_import::import(&raws, &decls, &side, round.unwrap_or(1), &extra)?;
    let failed = doc["results"].as_array().is_some_and(|r| r.iter().any(|x| x["status"] == "failed"));
    write_file(&out, &serde_json::to_string_pretty(&doc)?)?;
    Ok(if failed { 2 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(round: u32, metric: &str, gate: Gate, better: Better, v: f64) -> Obs {
        let mut metrics = BTreeMap::new();
        metrics.insert(
            metric.to_string(),
            MetricObs { unit: "ns".into(), better, gate, values: vec![v], value: v },
        );
        Obs {
            round,
            level: "micro".into(),
            status: "ok".into(),
            metrics,
            checksums: BTreeMap::new(),
            ladder: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn side(values: &[f64], gate: Gate, better: Better) -> SideData {
        let mut s = SideData { files: 1, ..Default::default() };
        let v: Vec<Obs> =
            values.iter().enumerate().map(|(i, v)| obs(i as u32 + 1, "t", gate, better, *v)).collect();
        s.obs.insert(("B-MICRO-01".into(), "c".into()), v);
        s
    }

    fn verdict(a: &[f64], b: &[f64], gate: Gate, better: Better) -> (String, i32) {
        let o = compare(
            &side(a, gate, better),
            &side(b, gate, better),
            &Thresholds::default(),
            &Noise::default(),
            None,
        );
        (o.rows[0].verdict.clone(), o.exit_code)
    }

    #[test]
    fn verdicts_follow_the_rules() {
        let a = [100.0, 101.0, 99.0, 100.0, 100.5];
        assert_eq!(verdict(&a, &[100.5, 100.0, 99.5, 101.0, 100.0], Gate::Micro, Better::Lower).0, "ok");
        assert_eq!(
            verdict(&a, &[110.0, 111.0, 109.0, 110.0, 110.5], Gate::Micro, Better::Lower),
            ("regression".into(), 1)
        );
        assert_eq!(verdict(&a, &[90.0, 91.0, 89.0, 90.0, 90.5], Gate::Micro, Better::Lower).0, "improved");
        // Higher is better: a drop is a regression.
        assert_eq!(
            verdict(&a, &[90.0, 91.0, 89.0, 90.0, 90.5], Gate::MacroThroughput, Better::Higher).0,
            "regression"
        );
        // 4% on a 3% gate with scattered ratios: above the threshold, CI crosses 1.
        assert_eq!(
            verdict(&a, &[104.0, 95.0, 110.0, 104.5, 99.0], Gate::MacroLatency, Better::Lower).0,
            "suspect"
        );
        // Security: faster is flagged.
        assert_eq!(
            verdict(&a, &[80.0, 81.0, 79.0, 80.0, 80.5], Gate::SecurityBothWays, Better::Lower).0,
            "regression"
        );
        assert_eq!(verdict(&a, &[200.0; 5], Gate::Info, Better::Lower), ("info".into(), 0));
        assert_eq!(verdict(&[1.0], &[2.0], Gate::Exact, Better::Lower), ("exact_mismatch".into(), 1));
        assert_eq!(verdict(&[0.0, 0.0], &[1.0, 1.0], Gate::Micro, Better::Lower).0, "regression");
    }

    #[test]
    fn ci_mode_reports_speed_only() {
        let t = Thresholds { ci: true, ..Default::default() };
        let a = side(&[100.0; 5], Gate::Micro, Better::Lower);
        let b = side(&[120.0; 5], Gate::Micro, Better::Lower);
        let o = compare(&a, &b, &t, &Noise::default(), None);
        assert_eq!(o.rows[0].verdict, "regression");
        assert!(o.rows[0].report_only);
        assert_eq!(o.exit_code, 0);
    }

    #[test]
    fn quality_uses_absolute_drops() {
        let mut a = side(&[40.0], Gate::Quality, Better::Higher);
        let mut b = side(&[39.95], Gate::Quality, Better::Higher);
        for s in [&mut a, &mut b] {
            for o in s.obs.values_mut().flatten() {
                let m = o.metrics.remove("t").unwrap();
                o.metrics.insert("psnr_yuv_db".into(), MetricObs { unit: "dB".into(), ..m });
            }
        }
        let o = compare(&a, &b, &Thresholds::default(), &Noise::default(), None);
        assert_eq!(o.rows[0].verdict, "ok");
        for o in b.obs.values_mut().flatten() {
            o.metrics.get_mut("psnr_yuv_db").unwrap().value = 39.8;
        }
        let o = compare(&a, &b, &Thresholds::default(), &Noise::default(), None);
        assert_eq!(o.rows[0].verdict, "regression");
    }

    #[test]
    fn missing_and_not_runnable() {
        let a = side(&[1.0], Gate::Micro, Better::Lower);
        let b = SideData { files: 1, ..Default::default() };
        let o = compare(&a, &b, &Thresholds::default(), &Noise::default(), None);
        assert_eq!(o.exit_code, 2);
        let mut nr = side(&[1.0], Gate::Micro, Better::Lower);
        for o in nr.obs.values_mut().flatten() {
            o.status = "not-runnable".into();
        }
        let o = compare(&nr, &b, &Thresholds::default(), &Noise::default(), None);
        assert_eq!(o.exit_code, 0);
        assert_eq!(o.not_runnable.len(), 1);
    }

    #[test]
    fn bd_rate_groups_one_point_cases() {
        assert_eq!(ladder_group("openh264/S5/1920x1080/4000k"), Some("openh264/S5/1920x1080"));
        assert_eq!(ladder_group("clone/1920x1080"), None);
        assert_eq!(ladder_group("x/k"), None);
        // One ladder point per case (as B-MICRO-04/05); B needs 10% more bits for the
        // same quality at every point: BD-rate +10%, a regression.
        let ladder = |scale: f64| {
            let mut s = SideData { files: 1, ..Default::default() };
            for (kbps, psnr, ssim) in [(1000.0, 34.0, 0.90), (2000.0, 37.0, 0.94), (4000.0, 40.0, 0.97)] {
                let mut o = obs(1, "encode_ms", Gate::Info, Better::Lower, 1.0);
                o.ladder = vec![
                    json!({"kbps_target": kbps, "kbps_actual": kbps * scale, "psnr_yuv_db": psnr, "ssim_y": ssim}),
                ];
                s.obs.insert(("B-MICRO-04".into(), format!("openh264/S5/1920x1080/{kbps}k")), vec![o]);
            }
            s
        };
        let o = compare(&ladder(1.0), &ladder(1.1), &Thresholds::default(), &Noise::default(), None);
        let bd: Vec<&Row> = o.rows.iter().filter(|r| r.metric.starts_with("bd_rate_")).collect();
        assert_eq!(bd.len(), 2, "{bd:?}");
        for r in bd {
            assert_eq!(r.case, "openh264/S5/1920x1080/ladder");
            assert!((r.new_median.unwrap() - 10.0).abs() < 0.01, "{r:?}");
            assert_eq!(r.verdict, "regression");
        }
        assert_eq!(o.exit_code, 1);
        let same = compare(&ladder(1.0), &ladder(1.0), &Thresholds::default(), &Noise::default(), None);
        assert!(same.rows.iter().filter(|r| r.metric.starts_with("bd_rate_")).all(|r| r.verdict == "ok"));
        assert_eq!(same.exit_code, 0);
    }

    #[test]
    fn resource_limit_needs_growth_over_the_baseline() {
        let run = |a: &[f64], b: &[f64]| {
            let mut t = Thresholds::default();
            t.resource_limits.insert("t".into(), 0.0);
            let o = compare(
                &side(a, Gate::Resource, Better::Lower),
                &side(b, Gate::Resource, Better::Lower),
                &t,
                &Noise::default(),
                None,
            );
            o.rows[0].verdict.clone()
        };
        // Already above the limit at the baseline, same within noise: not a regression.
        assert_ne!(run(&[25.0, 23.0], &[25.0, 25.0]), "regression");
        // Back to the start at the baseline, a leak in the new version.
        assert_eq!(run(&[0.0, 0.0], &[3.0, 3.0]), "regression");
        // Clearly more than the baseline.
        assert_eq!(run(&[24.0, 24.0], &[60.0, 60.0]), "regression");
    }

    #[test]
    fn noisy_regressions_are_rerun_first() {
        let a = side(&[100.0; 5], Gate::Micro, Better::Lower);
        let mut b = side(&[130.0; 5], Gate::Micro, Better::Lower);
        assert_eq!(
            compare(&a, &b, &Thresholds::default(), &Noise::default(), None).rows[0].verdict,
            "regression"
        );
        for o in b.obs.values_mut().flatten() {
            o.status = "noisy".into();
        }
        let o = compare(&a, &b, &Thresholds::default(), &Noise::default(), None);
        assert_eq!(o.rows[0].verdict, "suspect");
        assert_eq!(o.exit_code, 0);
        // After the rerun the regression stands, noisy or not.
        let t = Thresholds { rerun_done: true, ..Default::default() };
        assert_eq!(compare(&a, &b, &t, &Noise::default(), None).rows[0].verdict, "regression");
    }

    #[test]
    fn info_only_machines_never_gate() {
        let a = side(&[100.0; 5], Gate::Micro, Better::Lower);
        let b = side(&[130.0; 5], Gate::Micro, Better::Lower);
        let t = Thresholds { info_only: true, ..Default::default() };
        let o = compare(&a, &b, &t, &Noise::default(), None);
        assert_eq!(o.rows[0].verdict, "regression");
        assert!(o.rows[0].report_only);
        assert_eq!(o.exit_code, 0);
        let doc: Value =
            serde_json::from_str(r#"{"machines": {"PC-X": {"gate": false}, "LAB": {"gate": true}}}"#)
                .unwrap();
        let p = std::env::temp_dir().join(format!("nb-thr-{}.json", std::process::id()));
        std::fs::write(&p, doc.to_string()).unwrap();
        let t = Thresholds::load(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(t.info_only_machines, vec!["PC-X".to_string()]);
    }

    #[test]
    fn equal_medians_are_not_a_regression() {
        // fps of a starved link: A [0.8, 0, 0], B [0, 0, 0] (both medians 0).
        let a = side(&[0.8, 0.0, 0.0], Gate::MacroThroughput, Better::Higher);
        let b = side(&[0.0, 0.0, 0.0], Gate::MacroThroughput, Better::Higher);
        let o = compare(&a, &b, &Thresholds::default(), &Noise::default(), None);
        assert_eq!(o.rows[0].verdict, "ok", "{:?}", o.rows[0]);
        assert_eq!(o.exit_code, 0);
    }

    #[test]
    fn agent_log_line() {
        let l = "2026-11-16T10:00:00Z INFO nuvira_agent::pipeline: 59.8 fps sent (60.0 captured, 1 skipped), encoding 4.2 ms/frame, 3120 kbit/s";
        assert_eq!(parse_agent_line(l), Some([59.8, 60.0, 1.0, 4.2, 3120.0]));
    }
}
