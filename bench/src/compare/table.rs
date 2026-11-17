//! Markdown and JSON tables of `nuvira-bench compare`: before/after with
//! percentage differences, and the baseline table (side A only) used for
//! `docs/review/baseline/benchmarks.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde_json::{Value, json};

use super::{NOISE_MAX_AGE_DAYS, Noise, Outcome, Row, SideData, Thresholds, stats};
use crate::measure::{Gate, median};

/// A number with about 4 significant digits, no exponent for usual sizes.
pub fn num(v: f64) -> String {
    if !v.is_finite() {
        return "-".into();
    }
    let a = v.abs();
    if a == 0.0 {
        "0".into()
    } else if a >= 1e7 || a < 1e-4 {
        format!("{v:.3e}")
    } else if a >= 1000.0 {
        format!("{v:.0}")
    } else if a >= 100.0 {
        format!("{v:.1}")
    } else if a >= 1.0 {
        format!("{v:.2}")
    } else {
        format!("{v:.4}")
    }
}

fn opt(v: Option<f64>) -> String {
    v.map_or("-".into(), num)
}

fn pct(v: Option<f64>) -> String {
    v.map_or("-".into(), |p| if p.is_finite() { format!("{p:+.1}%") } else { "-".into() })
}

fn esc(s: &str) -> String {
    s.replace('|', "\\|")
}

fn gate_name(g: Gate) -> String {
    serde_json::to_value(g).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

fn run_line(side: &SideData) -> String {
    let get = |f: &str| side.runs.iter().find_map(|r| r.get(f).cloned()).unwrap_or(Value::Null);
    let repos = get("repos");
    let nuvira = repos.get("nuvira").cloned().unwrap_or(Value::Null);
    let describe = nuvira.get("describe").and_then(Value::as_str).unwrap_or("?");
    let commit = nuvira.get("commit").and_then(Value::as_str).unwrap_or("?");
    let dirty = if nuvira.get("dirty").and_then(Value::as_bool).unwrap_or(false) { " (dirty)" } else { "" };
    let rounds: BTreeSet<u32> = side.obs.values().flatten().map(|o| o.round).collect();
    format!(
        "`{}` {}{} — {} files, rounds {:?}",
        describe,
        &commit[..commit.len().min(12)],
        dirty,
        side.files,
        rounds
    )
}

fn machine_line(side: &SideData) -> String {
    let m = side.runs.iter().find_map(|r| r.get("machine").cloned()).unwrap_or(Value::Null);
    let s = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let tc = side.runs.iter().find_map(|r| r.get("toolchain").cloned()).unwrap_or(Value::Null);
    format!(
        "{} {} {} {}; rustc {}; go {}",
        s("alias"),
        s("os"),
        s("os_version"),
        s("cpu"),
        tc.get("rustc").and_then(Value::as_str).unwrap_or("?"),
        tc.get("go").and_then(Value::as_str).unwrap_or("?")
    )
}

pub fn summary_counts(o: &Outcome) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for r in &o.rows {
        *m.entry(r.verdict.clone()).or_insert(0) += 1;
    }
    m.insert("not-runnable".into(), o.not_runnable.len());
    m.insert("invalid_reference".into(), o.invalid_reference.len());
    m
}

fn row_line(r: &Row) -> String {
    let ci = r.ci95_ratio.map_or("-".into(), |[lo, hi]| format!("{lo:.3}–{hi:.3}"));
    let thr = match (r.threshold_pct, r.threshold_abs) {
        (Some(p), _) => format!("{p}%"),
        (_, Some(a)) => format!("{a} {}", r.unit),
        _ => "-".into(),
    };
    let delta = if r.threshold_abs.is_some() || r.delta_pct.is_none() {
        r.delta_abs.map_or("-".into(), |d| format!("{d:+.4} {}", r.unit))
    } else {
        pct(r.delta_pct)
    };
    let verdict = if r.report_only && r.verdict == "regression" {
        "regression (report-only)".to_string()
    } else {
        r.verdict.clone()
    };
    let better = if r.better == crate::measure::Better::Higher { "↑" } else { "↓" };
    format!(
        "| {} | {} | {} {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
        r.id,
        esc(&r.case),
        esc(&r.metric),
        better,
        esc(&r.unit),
        gate_name(r.gate),
        opt(r.base_median),
        opt(r.new_median),
        delta,
        r.noise_pct.map_or("-".into(), |n| format!("{n:.1}%")),
        thr,
        ci,
        esc(&if r.notes.is_empty() { verdict } else { format!("{verdict} ({})", r.notes.join("; ")) }),
    )
}

const HEADER: &str = "| ID | Case | Metric | Unit | Gate | A median | B median | Δ | Noise | Threshold | CI95 B/A | Verdict |\n|---|---|---|---|---|---|---|---|---|---|---|---|";

pub fn compare_markdown(
    base: &SideData,
    new: &SideData,
    o: &Outcome,
    t: &Thresholds,
    noise: &Noise,
    title: Option<&str>,
) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# {}\n", title.unwrap_or("Benchmark comparison (A = base, B = new)"));
    let _ = writeln!(s, "- A: {}", run_line(base));
    let _ = writeln!(s, "- B: {}", run_line(new));
    let _ = writeln!(s, "- Machine: {}", machine_line(base));
    let _ = writeln!(
        s,
        "- Thresholds (beyond noise): micro {}%, macro latency {}%, macro CPU {}%, macro throughput {}%, security ±{}%, quality PSNR -{} dB / SSIM -{} / BD-rate +{}% / bytes +{}%{}",
        t.relative(Gate::Micro).unwrap_or(0.0),
        t.relative(Gate::MacroLatency).unwrap_or(0.0),
        t.relative(Gate::MacroCpu).unwrap_or(0.0),
        t.relative(Gate::MacroThroughput).unwrap_or(0.0),
        t.relative(Gate::SecurityBothWays).unwrap_or(0.0),
        t.psnr_db,
        t.ssim,
        t.bd_rate_pct,
        t.bytes_pct,
        if t.ci { " (CI mode: x2, speed report-only)" } else { "" }
    );
    match (&noise.path, noise.age_days()) {
        (None, _) => {
            let _ = writeln!(
                s,
                "- Noise: **not calibrated** (run `bench-compare.sh --calibrate`): noise taken as 0"
            );
        }
        (Some(p), age) => {
            let stale = age.is_some_and(|d| d > NOISE_MAX_AGE_DAYS);
            let _ = writeln!(
                s,
                "- Noise: `{}`, {} day(s) old{}",
                p.display(),
                age.map_or("?".into(), |d| d.to_string()),
                if stale { " — **stale, recalibrate**" } else { "" }
            );
        }
    }
    let counts = summary_counts(o);
    let _ = writeln!(
        s,
        "- Result: **exit {}** — {}\n",
        o.exit_code,
        counts
            .iter()
            .filter(|(_, n)| **n > 0)
            .map(|(k, n)| format!("{k} {n}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    if !o.not_runnable.is_empty() {
        let _ = writeln!(s, "## Not runnable here (never counted as passed)\n");
        for (id, case, why) in &o.not_runnable {
            let _ = writeln!(s, "- {id} {case}: {why}");
        }
        s.push('\n');
    }
    if !o.invalid_reference.is_empty() {
        let _ = writeln!(s, "## Invalid reference\n");
        for (id, case) in &o.invalid_reference {
            let _ = writeln!(s, "- {id} {case}");
        }
        s.push('\n');
    }
    let attention: Vec<&Row> = o
        .rows
        .iter()
        .filter(|r| {
            matches!(r.verdict.as_str(), "regression" | "exact_mismatch" | "missing" | "failed" | "suspect")
        })
        .collect();
    if !attention.is_empty() {
        let _ = writeln!(s, "## Needs attention\n\n{HEADER}");
        for r in attention {
            let _ = writeln!(s, "{}", row_line(r));
        }
        s.push('\n');
    }
    if !o.checksum_changes.is_empty() {
        let _ = writeln!(s, "## Output checksums that changed (information)\n");
        for c in &o.checksum_changes {
            let _ = writeln!(s, "- {c}");
        }
        s.push('\n');
    }
    let levels: BTreeSet<&str> = o.rows.iter().map(|r| r.level.as_str()).collect();
    for level in levels {
        let _ = writeln!(s, "## {}\n\n{HEADER}", if level.is_empty() { "other" } else { level });
        for r in o.rows.iter().filter(|r| r.level == level) {
            let _ = writeln!(s, "{}", row_line(r));
        }
        s.push('\n');
    }
    s
}

/// Per (ID, case, metric) of side A: per-round medians and their statistics.
fn baseline_rows(side: &SideData) -> Vec<Value> {
    let mut rows = Vec::new();
    for ((id, case), obs) in &side.obs {
        let status = super::status_of(obs).to_string();
        let names: BTreeSet<&String> = obs.iter().flat_map(|o| o.metrics.keys()).collect();
        if names.is_empty() {
            rows.push(json!({"id": id, "case": case, "level": obs[0].level, "metric": "-", "status": status,
                             "notes": super::first_note(obs)}));
        }
        for name in names {
            let spec = obs.iter().find_map(|o| o.metrics.get(name)).expect("present");
            let per_round: Vec<f64> =
                obs.iter().filter_map(|o| o.metrics.get(name).map(|m| m.value)).collect();
            let all: Vec<f64> =
                obs.iter().filter_map(|o| o.metrics.get(name)).flat_map(|m| m.values.clone()).collect();
            let med = median(&per_round);
            let s = crate::measure::Stats::of(&all);
            rows.push(json!({
                "id": id, "case": case, "level": obs[0].level, "metric": name, "unit": spec.unit,
                "better": spec.better, "gate": spec.gate, "status": status,
                "rounds": per_round.len(), "median": med,
                "mad_pct": if med != 0.0 { 100.0 * stats::mad(&per_round) / med.abs() } else { 0.0 },
                "stddev": s.map(|s| s.stddev), "min": s.map(|s| s.min), "max": s.map(|s| s.max),
                "p95": s.map(|s| s.p95), "values": all.len(),
            }));
        }
    }
    rows
}

pub fn baseline_json(side: &SideData) -> Value {
    json!({
        "schema": "nuvira-bench-baseline/1",
        "dir": side.dir,
        "repos": side.runs.iter().find_map(|r| r.get("repos").cloned()),
        "machine": side.runs.iter().find_map(|r| r.get("machine").cloned()),
        "toolchain": side.runs.iter().find_map(|r| r.get("toolchain").cloned()),
        "rows": baseline_rows(side),
    })
}

pub fn baseline_markdown(side: &SideData, title: Option<&str>) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# {}\n", title.unwrap_or("Benchmark baseline"));
    let _ = writeln!(s, "- Code: {}", run_line(side));
    let _ = writeln!(s, "- Machine: {}\n", machine_line(side));
    let _ = writeln!(
        s,
        "Median of the per-round medians; MAD in % of the median across rounds; stddev/min/max/p95 over every value.\n"
    );
    let rows = baseline_rows(side);
    let levels: BTreeSet<String> =
        rows.iter().map(|r| r["level"].as_str().unwrap_or("").to_string()).collect();
    for level in levels {
        let _ = writeln!(
            s,
            "## {}\n\n| ID | Case | Metric | Unit | Gate | Status | Rounds | Median | MAD % | Stddev | Min | Max | p95 |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|",
            if level.is_empty() { "other" } else { &level }
        );
        for r in rows.iter().filter(|r| r["level"].as_str().unwrap_or("") == level) {
            let f = |k: &str| r.get(k).and_then(Value::as_f64).map_or("-".into(), num);
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                r["id"].as_str().unwrap_or(""),
                esc(r["case"].as_str().unwrap_or("")),
                esc(r["metric"].as_str().unwrap_or("")),
                esc(r.get("unit").and_then(Value::as_str).unwrap_or("")),
                r.get("gate").and_then(Value::as_str).unwrap_or(""),
                r["status"].as_str().unwrap_or(""),
                r.get("rounds").and_then(Value::as_u64).unwrap_or(0),
                f("median"),
                r.get("mad_pct").and_then(Value::as_f64).map_or("-".into(), |p| format!("{p:.1}")),
                f("stddev"),
                f("min"),
                f("max"),
                f("p95"),
            );
        }
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_compact() {
        assert_eq!(num(0.0), "0");
        assert_eq!(num(13299.4), "13299");
        assert_eq!(num(433.12), "433.1");
        assert_eq!(num(1.5012), "1.50");
        assert_eq!(num(0.00213), "0.0021");
        assert_eq!(num(f64::NAN), "-");
    }
}
