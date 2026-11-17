//! `nuvira-bench`: NuviraHub benchmark runner.
//!
//! ```text
//! nuvira-bench list    [--level micro,macro,res] [--tier quick|full] [--filter RE]
//! nuvira-bench run     [micro|macro|res|all ...] [--tier quick|full] [--filter RE]
//!                      [--out DIR] [--side A] [--round 1] [--reps N] [--warmup N]
//!                      [--machine ALIAS] [--config-ids A,B] [--inputs DIR] [--tmp DIR]
//!                      [--seed N] [--no-verify]
//! nuvira-bench fixtures gen    [--out FILE] [--quick]
//! nuvira-bench fixtures verify [--manifest FILE] [--quick]
//! nuvira-bench fixtures files  --dir DIR [--max BYTES]
//! nuvira-bench fixtures selftest [--fps 60]
//! nuvira-bench fixtures decode-streams --dir DIR [--tier quick|full]
//! nuvira-bench machine [--machine ALIAS] [--config-ids A,B]
//! nuvira-bench netem | agent-host | compare | import-go  ...  (other packages)
//! ```
//!
//! `run` writes `<out>/<side>/r<round>/<suite>.json` (schema `nuvira-bench/1`).
//! Exit codes: 0 ok; 1 usage or internal error; 2 fixture mismatch (or a package
//! not in this build).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use nuvira_bench::fixtures::{GenOptions, Manifest, Scene, files, hex};
use nuvira_bench::registry::{Case, Filter, Level, RunCtx, Tier};
use nuvira_bench::report::{CaseResult, MachineInfo, RunFile, RunInfo, Status};

/// Minimal flag parser: `--name value`, `--flag`, positionals.
struct Args {
    positional: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

/// Flags that take no value.
const SWITCHES: &[&str] = &["--quick", "--no-verify", "--json", "--strict"];

impl Args {
    fn parse(args: &[String]) -> Result<Args> {
        let mut positional = Vec::new();
        let mut flags = Vec::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if let Some((k, v)) = a.split_once('=').filter(|_| a.starts_with("--")) {
                flags.push((k.to_string(), Some(v.to_string())));
            } else if a.starts_with("--") {
                if SWITCHES.contains(&a.as_str()) {
                    flags.push((a.clone(), None));
                } else {
                    let v = it.next().with_context(|| format!("{a} needs a value"))?;
                    flags.push((a.clone(), Some(v.clone())));
                }
            } else {
                positional.push(a.clone());
            }
        }
        Ok(Args { positional, flags })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.flags.iter().rev().find(|(k, _)| k == name).and_then(|(_, v)| v.as_deref())
    }

    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == name)
    }

    fn num<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>> {
        self.get(name)
            .map(|v| v.parse::<T>().map_err(|_| anyhow::anyhow!("{name}: invalid number {v}")))
            .transpose()
    }

    fn list(&self, name: &str) -> Vec<String> {
        self.get(name)
            .map(|v| v.split(',').filter(|s| !s.is_empty()).map(String::from).collect())
            .unwrap_or_default()
    }
}

fn main() -> ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match real_main(&argv) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("nuvira-bench: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn real_main(argv: &[String]) -> Result<i32> {
    let Some(cmd) = argv.first() else {
        print_usage();
        return Ok(1);
    };
    let rest = &argv[1..];
    match cmd.as_str() {
        "list" => list(&Args::parse(rest)?),
        "run" => run(&Args::parse(rest)?),
        "fixtures" => fixtures(rest),
        "machine" => {
            let a = Args::parse(rest)?;
            let m = MachineInfo::detect(a.get("--machine").map(String::from), a.list("--config-ids"));
            println!("{}", serde_json::to_string_pretty(&m)?);
            Ok(0)
        }
        "netem" => nuvira_bench::dispatch::netem(rest),
        "agent-host" => nuvira_bench::dispatch::agent_host(rest),
        "compare" => nuvira_bench::dispatch::compare(rest),
        "import-go" => nuvira_bench::dispatch::import_go(rest),
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(0)
        }
        other => {
            eprintln!("unknown command {other}");
            print_usage();
            Ok(1)
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage: nuvira-bench <list|run|fixtures gen|fixtures verify|fixtures files|fixtures decode-streams|fixtures selftest|machine|netem|agent-host|compare|import-go> [options]\n\
         see the comment at the top of bench/src/main.rs"
    );
}

fn levels_of(names: &[String]) -> Result<Vec<Level>> {
    let mut out = Vec::new();
    for n in names {
        if n == "all" {
            return Ok(Vec::new());
        }
        out.push(
            Level::parse(n).with_context(|| format!("unknown suite {n} (micro, macro, res, long, all)"))?,
        );
    }
    Ok(out)
}

fn filter_of(a: &Args, levels: Vec<Level>) -> Result<Filter> {
    let tier =
        a.get("--tier").map(|t| Tier::parse(t).with_context(|| format!("unknown tier {t}"))).transpose()?;
    let regex = a.get("--filter").map(regex::Regex::new).transpose().context("--filter")?;
    Ok(Filter { levels, tier, regex, only: a.get("--only").map(String::from) })
}

fn list(a: &Args) -> Result<i32> {
    let reg = nuvira_bench::registry();
    let filter = filter_of(a, levels_of(&a.list("--level"))?)?;
    let missing: Vec<&str> = nuvira_bench::packages().into_iter().filter(|p| !p.1).map(|p| p.0).collect();
    if !missing.is_empty() {
        eprintln!("packages not in this build: {}", missing.join(", "));
    }
    for i in reg.select(&filter) {
        let c = reg.iter().nth(i).expect("selected");
        let tiers: Vec<&str> = c.tiers().iter().map(|t| t.as_str()).collect();
        let runnable = match c.runnable() {
            Ok(()) => "runnable".to_string(),
            Err(e) => format!("not-runnable: {e}"),
        };
        println!("{}\t{}\t{}\t{}\t{}", c.id(), c.name(), c.level().as_str(), tiers.join(","), runnable);
    }
    for d in reg.duplicates() {
        eprintln!("duplicate case key: {d}");
    }
    Ok(0)
}

/// Fresh directory under `base`.
fn fresh_dir(base: &Path, tag: &str) -> Result<PathBuf> {
    for n in 0..1000u32 {
        let p = base.join(format!("nuvira-bench-{}-{tag}-{n}", std::process::id()));
        if !p.exists() {
            std::fs::create_dir_all(&p).with_context(|| format!("creating {}", p.display()))?;
            return Ok(p);
        }
    }
    bail!("no free temporary directory under {}", base.display())
}

fn run(a: &Args) -> Result<i32> {
    let tier =
        a.get("--tier").map(Tier::parse).unwrap_or(Some(Tier::Quick)).context("--tier: quick or full")?;
    let side = a.get("--side").unwrap_or("A").to_string();
    let round: u32 = a.num("--round")?.unwrap_or(1);
    let suites: Vec<String> = if a.positional.is_empty() {
        vec!["all".into()]
    } else {
        a.positional.iter().flat_map(|p| p.split(',').map(String::from)).collect()
    };
    let levels = levels_of(&suites)?;
    // `all` = micro, macro, res; the `long` suite only when named.
    let levels = if levels.is_empty() { vec![Level::Micro, Level::Macro, Level::Res] } else { levels };
    let tmp_base = a.get("--tmp").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let inputs = a.get("--inputs").map(PathBuf::from).unwrap_or_else(|| tmp_base.join("nuvira-bench-inputs"));
    let reps_default = 10;
    let seed: u64 = a.num("--seed")?.unwrap_or(nuvira_bench::rng::DEFAULT_SEED);
    let child_out = a.get("--child-out").map(PathBuf::from);

    // A child process runs exactly one case and writes its result (no verify, no file).
    if let Some(out) = child_out {
        let mut reg = nuvira_bench::registry();
        let filter = filter_of(a, Vec::new())?;
        let sel = reg.select(&filter);
        let [index] = sel.as_slice() else { bail!("--only must select exactly one case, got {}", sel.len()) };
        let mut ctx = RunCtx {
            reps: a.num("--reps")?.unwrap_or(reps_default),
            warmup: a.num("--warmup")?.unwrap_or(1),
            tier,
            tmp: fresh_dir(&tmp_base, "case")?,
            inputs,
            rng_seed: seed,
        };
        let r = run_in_process(reg.get_mut(*index), &mut ctx);
        std::fs::remove_dir_all(&ctx.tmp).ok();
        std::fs::write(&out, serde_json::to_string_pretty(&r)?)?;
        return Ok(0);
    }

    if !a.has("--no-verify") {
        let bad = verify(None, true)?;
        if !bad.is_empty() {
            for b in &bad {
                eprintln!("fixture mismatch: {b}");
            }
            eprintln!("fixtures differ from the manifest: no run (exit 2)");
            return Ok(2);
        }
    }
    let machine = MachineInfo::detect(a.get("--machine").map(String::from), a.list("--config-ids"));
    let out_base = a.get("--out").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("bench-results"));
    let mut reg = nuvira_bench::registry();
    let dups = reg.duplicates();
    if !dups.is_empty() {
        bail!("duplicate case keys: {}", dups.join(", "));
    }
    let mut any_failed = false;
    for level in levels {
        let filter = Filter { levels: vec![level], ..filter_of(a, Vec::new())? };
        let filter = Filter { tier: Some(tier), ..filter };
        let selected = reg.select(&filter);
        let mut file = RunFile::new(RunInfo::new(&side, round, tier, level.as_str(), machine.clone()));
        let reps_given: Option<u32> = a.num("--reps")?;
        let reps_default = match level {
            Level::Micro => 10,
            Level::Macro | Level::Res | Level::Long => 1,
        };
        for index in selected {
            let case = reg.get_mut(index);
            eprintln!("[{}] {} ...", level.as_str(), case.key());
            let started = std::time::Instant::now();
            let mut ctx = RunCtx {
                reps: reps_given.unwrap_or_else(|| case.reps_for(tier, reps_default)),
                warmup: a.num("--warmup")?.unwrap_or(1),
                tier,
                tmp: fresh_dir(&tmp_base, "case")?,
                inputs: inputs.clone(),
                rng_seed: seed,
            };
            let mut r = if let Err(reason) = case.runnable() {
                CaseResult::not_runnable(case, reason)
            } else if case.isolated() {
                run_in_child(case, &ctx, a)
            } else {
                run_in_process(case, &mut ctx)
            };
            std::fs::remove_dir_all(&ctx.tmp).ok();
            // Wall time of the whole case (set-up, warm-up, reps, quality): for tier
            // budgets only, never compared.
            r.param("cpu_clock", nuvira_bench::measure::cpu_source());
            r.param("case_wall_s", (started.elapsed().as_secs_f64() * 10.0).round() / 10.0);
            any_failed |= r.status == Status::Failed;
            eprintln!("[{}] {} -> {:?}", level.as_str(), case.key(), r.status);
            file.results.push(r);
        }
        let path = out_base.join(&side).join(format!("r{round}")).join(format!("{}.json", level.as_str()));
        file.write(&path)?;
        eprintln!("wrote {} ({} results)", path.display(), file.results.len());
    }
    Ok(if any_failed && a.has("--strict") { 1 } else { 0 })
}

fn run_in_process(case: &mut dyn Case, ctx: &mut RunCtx) -> CaseResult {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| case.run(ctx)));
    match outcome {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => CaseResult::failed(case, &e),
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "panic".into());
            CaseResult::failed(case, &anyhow::anyhow!("panicked: {msg}"))
        }
    }
}

/// Re-runs this binary for one case with the case's environment variables.
fn run_in_child(case: &mut dyn Case, ctx: &RunCtx, a: &Args) -> CaseResult {
    let result = (|| -> Result<CaseResult> {
        let exe = std::env::current_exe()?;
        let out = ctx.tmp.join("child-result.json");
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("run")
            .args(["--only", &case.key()])
            .args(["--child-out", &out.to_string_lossy()])
            .args(["--tier", ctx.tier.as_str()])
            .args(["--reps", &ctx.reps.to_string()])
            .args(["--warmup", &ctx.warmup.to_string()])
            .args(["--seed", &ctx.rng_seed.to_string()])
            .args(["--inputs", &ctx.inputs.to_string_lossy()])
            .args(["--tmp", &ctx.tmp.to_string_lossy()]);
        if let Some(m) = a.get("--machine") {
            cmd.args(["--machine", m]);
        }
        for (k, v) in case.env() {
            cmd.env(k, v);
        }
        let status = cmd.status().context("starting the child process")?;
        if !status.success() {
            bail!("child process for {} exited with {status}", case.key());
        }
        let text = std::fs::read_to_string(&out).context("child result")?;
        Ok(serde_json::from_str(&text)?)
    })();
    result.unwrap_or_else(|e| CaseResult::failed(case, &e))
}

fn default_manifest_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/manifest.json"))
}

/// Regenerates (quick or full) and compares with the manifest file or the embedded one.
fn verify(manifest: Option<&Path>, quick: bool) -> Result<Vec<String>> {
    let reference = match manifest {
        Some(p) => {
            Manifest::parse(&std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?)?
        }
        None => Manifest::embedded()?,
    };
    if reference.scenes.is_empty() {
        return Ok(vec!["the manifest is empty (run `nuvira-bench fixtures gen`)".into()]);
    }
    let generated = Manifest::generate(if quick { GenOptions::quick() } else { GenOptions::full() });
    Ok(reference.verify(&generated))
}

fn fixtures(rest: &[String]) -> Result<i32> {
    let Some(sub) = rest.first() else { bail!("fixtures gen|verify|files|decode-streams|selftest") };
    let a = Args::parse(&rest[1..])?;
    match sub.as_str() {
        "gen" => {
            let out = a.get("--out").map(PathBuf::from).unwrap_or_else(default_manifest_path);
            let t = std::time::Instant::now();
            let m =
                Manifest::generate(if a.has("--quick") { GenOptions::quick() } else { GenOptions::full() });
            if let Some(dir) = out.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&out, m.to_json())?;
            eprintln!(
                "wrote {} ({} scenes, {} protocol samples, {} files) in {:.1} s, sha256 {}",
                out.display(),
                m.scenes.len(),
                m.proto.len(),
                m.files.len(),
                t.elapsed().as_secs_f64(),
                m.sha256()
            );
            Ok(0)
        }
        "verify" => {
            let t = std::time::Instant::now();
            let bad = verify(a.get("--manifest").map(Path::new), a.has("--quick"))?;
            for b in &bad {
                eprintln!("mismatch: {b}");
            }
            if bad.is_empty() {
                eprintln!("fixtures verified in {:.1} s", t.elapsed().as_secs_f64());
                Ok(0)
            } else {
                Ok(2)
            }
        }
        "files" => {
            let dir = PathBuf::from(a.get("--dir").context("--dir DIR")?);
            let max: u64 = a.num("--max")?.unwrap_or(files::GIB);
            let manifest = Manifest::embedded()?;
            let mut bad = 0;
            for size in files::SIZES.iter().copied().filter(|&s| s <= max) {
                let (p, h) = files::make(size, &dir)?;
                let ok = manifest.blob_sha256(&files::file_name(size, false)) == Some(hex(&h).as_str());
                bad += usize::from(!ok);
                println!("{}\t{}\t{}", p.display(), hex(&h), if ok { "ok" } else { "MISMATCH" });
            }
            let (p, h) = files::make_sparse(files::SPARSE_SIZE, &dir)?;
            println!("{}\t{}", p.display(), hex(&h));
            Ok(if bad == 0 { 0 } else { 2 })
        }
        "decode-streams" => {
            let dir = PathBuf::from(a.get("--dir").context("--dir DIR")?);
            let tier = a.get("--tier").unwrap_or("quick");
            let tier = Tier::parse(tier).with_context(|| format!("unknown tier {tier}"))?;
            nuvira_bench::dispatch::decode_streams(&dir, tier)
        }
        "selftest" => {
            let fps: u32 = a.num("--fps")?.unwrap_or(60);
            let budget = 0.25 * 1000.0 / fps as f64;
            println!("scene\tresolution\tms/frame\tbudget {budget:.2} ms");
            for scene in Scene::ALL {
                for (w, h) in nuvira_bench::fixtures::RESOLUTIONS {
                    let ms = nuvira_bench::fixtures::production_ms(scene, w, h, 20);
                    println!("{}\t{w}x{h}\t{ms:.2}\t{}", scene.id(), if ms < budget { "ok" } else { "ring" });
                }
            }
            Ok(0)
        }
        other => bail!("unknown fixtures command {other}"),
    }
}
