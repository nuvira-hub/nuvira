//! `nuvira-bench import-go`: turns the text output of `go test -bench
//! -benchmem` into the bench's JSON schema (plan section 8.1).
//!
//! The ID of each `Benchmark*` comes from the `// BENCH-ID: B-GO-03` comment
//! above it in the bench sources (`bench/go/<package>/bench_*_test.go`);
//! `// BENCH-METRIC: <unit> <lower|higher> <gate>` lines there set direction and
//! gate of a custom metric (`b.ReportMetric`). Built-in units: `ns/op`, `B/op`,
//! `allocs/op` are `micro` gates (lower is better), `MB/s` is `info` (derived
//! from ns/op). Any other unit without a declaration is `info`.
//!
//! Case name = the benchmark name without `Benchmark` and without the
//! `-GOMAXPROCS` suffix (`RelayForward/pairs=8`). Repeated lines (`-count N`)
//! become the values of one metric. A package whose output reports a failure
//! or a panic produces a `failed` result per benchmark function of that package.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::measure::{Better, Gate, Metric};

/// What the sources say about one benchmark function.
#[derive(Clone, Debug, Default)]
pub struct BenchDecl {
    pub id: String,
    pub package: String,
    /// unit as printed by Go -> (better, gate)
    pub metrics: BTreeMap<String, (Better, Gate)>,
}

/// Reads every `*_test.go` under `src` (one folder per package) and returns
/// function name (without `Benchmark`) -> declaration.
pub fn scan_sources(src: &Path) -> Result<BTreeMap<String, BenchDecl>> {
    let mut out = BTreeMap::new();
    let mut files = Vec::new();
    collect_go_files(src, &mut files)?;
    for file in files {
        let package =
            file.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()).unwrap_or("").to_string();
        let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        let mut pending: Option<BenchDecl> = None;
        for line in text.lines() {
            let t = line.trim();
            if let Some(id) = t.strip_prefix("// BENCH-ID:") {
                pending = Some(BenchDecl {
                    id: id.trim().to_string(),
                    package: package.clone(),
                    ..Default::default()
                });
            } else if let Some(spec) = t.strip_prefix("// BENCH-METRIC:") {
                let parts: Vec<&str> = spec.split_whitespace().collect();
                if let (Some(d), [unit, better, gate]) = (pending.as_mut(), parts.as_slice()) {
                    let better = parse_better(better).with_context(|| format!("{}: {t}", file.display()))?;
                    let gate = parse_gate(gate).with_context(|| format!("{}: {t}", file.display()))?;
                    d.metrics.insert(unit.to_string(), (better, gate));
                }
            } else if let Some(rest) = t.strip_prefix("func Benchmark") {
                let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                let decl = pending.take().unwrap_or_else(|| BenchDecl {
                    id: String::new(),
                    package: package.clone(),
                    ..Default::default()
                });
                out.insert(name, decl);
            } else if !t.starts_with("//") && !t.is_empty() {
                // A comment block not followed by a benchmark does not carry over.
                pending = None;
            }
        }
    }
    Ok(out)
}

fn collect_go_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let p = e?.path();
        if p.is_dir() {
            collect_go_files(&p, out)?;
        } else if p.file_name().and_then(|s| s.to_str()).is_some_and(|s| s.ends_with("_test.go")) {
            out.push(p);
        }
    }
    out.sort();
    Ok(())
}

pub fn parse_better(s: &str) -> Result<Better> {
    Ok(match s {
        "lower" => Better::Lower,
        "higher" => Better::Higher,
        _ => bail!("better must be lower or higher, not {s:?}"),
    })
}

pub fn parse_gate(s: &str) -> Result<Gate> {
    serde_json::from_value(Value::String(s.to_string())).map_err(|_| anyhow::anyhow!("unknown gate {s:?}"))
}

/// Metric name used in the JSON for a Go unit: `ns/op` -> `ns_op`, `pkts/s` -> `pkts_s`.
pub fn metric_name(unit: &str) -> String {
    let mut s: String =
        unit.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
    while s.contains("__") {
        s = s.replace("__", "_");
    }
    s.trim_matches('_').to_string()
}

fn builtin(unit: &str) -> (Better, Gate) {
    match unit {
        "ns/op" | "B/op" | "allocs/op" => (Better::Lower, Gate::Micro),
        "MB/s" => (Better::Higher, Gate::Info),
        _ => (Better::Lower, Gate::Info),
    }
}

/// Whether `case` failed: named by a FAIL line (itself or a parent benchmark), or
/// the package failed without naming any case.
fn case_failed(run: &GoRun, case: &str) -> bool {
    run.failed
        && (run.failed_cases.is_empty()
            || run.failed_cases.iter().any(|f| case == f || case.starts_with(&format!("{f}/"))))
}

/// One parsed raw output file.
#[derive(Debug, Default)]
pub struct GoRun {
    pub goos: String,
    pub goarch: String,
    pub cpu: String,
    pub pkg: String,
    /// case -> unit -> values
    pub cases: BTreeMap<String, BTreeMap<String, Vec<f64>>>,
    /// case -> iterations of each line
    pub iterations: BTreeMap<String, Vec<f64>>,
    pub failed: bool,
    /// Cases named by a `--- FAIL: Benchmark...` line (empty: the failure names
    /// none, e.g. a panic, and every case of the package counts as failed).
    pub failed_cases: Vec<String>,
    pub tail: Vec<String>,
}

pub fn strip_procs(name: &str) -> &str {
    if let Some(i) = name.rfind('-')
        && i + 1 < name.len()
        && name[i + 1..].bytes().all(|b| b.is_ascii_digit())
    {
        return &name[..i];
    }
    name
}

pub fn parse_output(text: &str) -> GoRun {
    let mut run = GoRun::default();
    for line in text.lines() {
        let l = line.trim_end();
        if let Some(v) = l.strip_prefix("goos: ") {
            run.goos = v.trim().into();
        } else if let Some(v) = l.strip_prefix("goarch: ") {
            run.goarch = v.trim().into();
        } else if let Some(v) = l.strip_prefix("cpu: ") {
            run.cpu = v.trim().into();
        } else if let Some(v) = l.strip_prefix("pkg: ") {
            run.pkg = v.trim().into();
        } else if l.starts_with("--- FAIL") || l.starts_with("FAIL") || l.starts_with("panic:") {
            run.failed = true;
        }
        // "--- FAIL: BenchmarkX/sub-12", also after a result column on the same line.
        if let Some(i) = l.find("--- FAIL: Benchmark") {
            run.failed = true;
            let name = l[i + "--- FAIL: Benchmark".len()..].split_whitespace().next().unwrap_or("");
            let case = strip_procs(name).to_string();
            if !case.is_empty() && !run.failed_cases.contains(&case) {
                run.failed_cases.push(case);
            }
        }
        if let Some(rest) = l.strip_prefix("Benchmark") {
            let mut fields = rest.split_whitespace();
            let (Some(name), Some(iters)) = (fields.next(), fields.next()) else { continue };
            let Ok(iters) = iters.parse::<f64>() else { continue };
            let case = strip_procs(name).to_string();
            let rest: Vec<&str> = fields.collect();
            let metrics = run.cases.entry(case.clone()).or_default();
            for pair in rest.chunks(2) {
                if let [v, unit] = pair
                    && let Ok(v) = v.parse::<f64>()
                {
                    metrics.entry(unit.to_string()).or_default().push(v);
                }
            }
            run.iterations.entry(case).or_default().push(iters);
        }
    }
    let lines: Vec<&str> = text.lines().collect();
    run.tail = lines[lines.len().saturating_sub(30)..].iter().map(|s| s.to_string()).collect();
    run
}

/// Builds the nuvira-bench/1 document from several raw files.
pub fn import(
    raws: &[PathBuf],
    decls: &BTreeMap<String, BenchDecl>,
    side: &str,
    round: u32,
    extra_run: &BTreeMap<String, String>,
) -> Result<Value> {
    let mut results = Vec::new();
    let mut machine = json!({});
    let mut packages = Vec::new();
    for raw in raws {
        let text = std::fs::read_to_string(raw).with_context(|| format!("reading {}", raw.display()))?;
        let run = parse_output(&text);
        if !run.goos.is_empty() {
            machine = json!({"os": run.goos, "arch": run.goarch, "cpu": run.cpu});
        }
        let pkg_short = run.pkg.rsplit('/').next().unwrap_or("").to_string();
        packages.push(run.pkg.clone());
        for (case, units) in &run.cases {
            let func = case.split('/').next().unwrap_or(case);
            let decl = decls.get(func);
            let id = decl.map(|d| d.id.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| "B-GO-?".into());
            let mut metrics = serde_json::Map::new();
            for (unit, values) in units {
                let (better, gate) =
                    decl.and_then(|d| d.metrics.get(unit).copied()).unwrap_or_else(|| builtin(unit));
                let m = Metric::from_values(unit, better, gate, values.clone());
                metrics.insert(metric_name(unit), serde_json::to_value(&m)?);
            }
            let mut notes = Vec::<String>::new();
            if id == "B-GO-?" {
                notes.push(format!("no // BENCH-ID: comment found for Benchmark{func}"));
            }
            results.push(json!({
                "id": id,
                "case": case,
                "level": "go",
                "config_ids": [],
                "status": if case_failed(&run, case) { "failed" } else { "ok" },
                "params": {"package": run.pkg, "count": units.values().map(|v| v.len()).max().unwrap_or(0),
                           "iterations": run.iterations.get(case)},
                "metrics": metrics,
                "notes": notes,
            }));
        }
        if run.failed {
            // Every benchmark function of the package that produced no line is failed too.
            let seen: Vec<&str> = run.cases.keys().map(|c| c.split('/').next().unwrap_or(c)).collect();
            for (func, d) in decls {
                if d.package == pkg_short && !seen.contains(&func.as_str()) && !d.id.is_empty() {
                    results.push(json!({
                        "id": d.id, "case": func, "level": "go", "status": "failed",
                        "metrics": {}, "notes": run.tail,
                    }));
                }
            }
            if pkg_short.is_empty() {
                results.push(json!({
                    "id": "B-GO-?", "case": raw.file_name().and_then(|s| s.to_str()).unwrap_or("go"),
                    "level": "go", "status": "failed", "metrics": {}, "notes": run.tail,
                }));
            }
        }
    }
    let mut run = json!({
        "id": format!("go-{side}-r{round}"),
        "side": side,
        "round": round,
        "suite": "go",
        "machine": machine,
        "packages": packages,
    });
    // `--set nuvira_commit=.. --set nuvira_describe=.. --set nuvira_dirty=true --set go=..`
    // fill the same fields as the Rust runner writes (repos, toolchain).
    let get = |k: &str| extra_run.get(k).cloned().unwrap_or_default();
    run["repos"] = json!({"nuvira": {"commit": get("nuvira_commit"), "describe": get("nuvira_describe"),
                                     "dirty": get("nuvira_dirty") == "true"}});
    run["toolchain"] = json!({"go": get("go"), "rustc": get("rustc"), "profile": "go test"});
    if let Some(alias) = extra_run.get("machine_alias") {
        run["machine"]["alias"] = Value::String(alias.clone());
    }
    for (k, v) in extra_run {
        if !k.starts_with("nuvira_") && !matches!(k.as_str(), "go" | "rustc" | "machine_alias") {
            run[k] = Value::String(v.clone());
        }
    }
    Ok(json!({"schema": "nuvira-bench/1", "run": run, "results": results}))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "goos: windows
goarch: amd64
pkg: github.com/nuvira-hub/nuvira/server/internal/relay
cpu: Intel(R) Core(TM) i7-10710U CPU @ 1.10GHz
BenchmarkRelayBind/first-12     	  177730	     13299 ns/op	     264 B/op	       5 allocs/op
BenchmarkRelayBind/first-12     	  177731	     13000 ns/op	     264 B/op	       5 allocs/op
BenchmarkRelayForward/pairs=8-12  3345	     80247 ns/op	  14.95 MB/s	         0 loss-%	     15658 pkts/s	    2144 B/op	       7 allocs/op
BenchmarkSolo 	 10	 5 ns/op
PASS
ok  	github.com/nuvira-hub/nuvira/server/internal/relay	31.620s
";

    #[test]
    fn parses_lines_and_units() {
        let r = parse_output(SAMPLE);
        assert!(!r.failed);
        assert_eq!(r.pkg, "github.com/nuvira-hub/nuvira/server/internal/relay");
        assert_eq!(r.cases["RelayBind/first"]["ns/op"], vec![13299.0, 13000.0]);
        assert_eq!(r.cases["RelayForward/pairs=8"]["pkts/s"], vec![15658.0]);
        assert_eq!(r.cases["Solo"]["ns/op"], vec![5.0]);
        assert_eq!(metric_name("loss-%"), "loss");
        assert_eq!(metric_name("pkts/s"), "pkts_s");
        assert_eq!(metric_name("allocs/op"), "allocs_op");
        assert_eq!(
            strip_procs("RelayDestination/sessions=100/parallel-12"),
            "RelayDestination/sessions=100/parallel"
        );
        assert_eq!(strip_procs("Store/by-id"), "Store/by-id");
    }

    #[test]
    fn failure_is_detected() {
        let r = parse_output("pkg: x/relay\n--- FAIL: BenchmarkX\nFAIL\n");
        assert!(r.failed);
        assert_eq!(r.failed_cases, vec!["X".to_string()]);
        // One sub-benchmark failed (its result column replaced by the FAIL line):
        // only that case is failed, the others of the package keep their results.
        let r2 = parse_output(concat!(
            "pkg: x/relay\n",
            "BenchmarkA/n-12 \t 10 \t 5 ns/op\n",
            "BenchmarkB/n3-12 \t--- FAIL: BenchmarkB/n3\n",
            "--- FAIL: BenchmarkB/n3-12\n",
            "FAIL\n"
        ));
        assert!(case_failed(&r2, "B/n3"));
        assert!(!case_failed(&r2, "A/n"));
    }
}
