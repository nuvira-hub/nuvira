//! Build script of the bench crate.
//!
//! 1. Module presence: the work packages of the plan land at different times, so
//!    `lib.rs` declares each package folder (and `mod.rs` files declare their
//!    sub-files) behind a cfg set here when the file exists:
//!    `bench_has_<dir>` for `src/<dir>/mod.rs`, `bench_has_<dir>_<stem>` for
//!    `src/<dir>/<stem>.rs`. Once every package has landed all of them are set.
//!    `NUVIRA_BENCH_SKIP=dir,dir` leaves folders out of the build.
//! 2. Build facts recorded in every result: rustc version, commit of the tree the
//!    bench is built in (`nuvira`), and the commit that last changed `bench/`.

use std::path::Path;
use std::process::Command;

/// Folders and files assigned by the plan (section 3); they are always declared to
/// `check-cfg`, present or not. Other files found are declared too.
const PLANNED: &[(&str, &[&str])] = &[
    (
        "micro",
        &[
            "convert", "codec_sw", "codec_hw", "gpu_win", "decode", "proto", "fec", "crypto", "audio",
            "capture",
        ],
    ),
    ("net", &["netem", "headless", "server_mode"]),
    ("macro_", &["composed", "session", "connect", "reconnect", "files", "input"]),
    ("res", &["long_session", "churn"]),
    ("compare", &["stats", "table", "go_import"]),
];

fn main() {
    let src = Path::new("src");
    println!("cargo::rerun-if-changed=src");
    // NUVIRA_BENCH_SKIP=compare,res: leave those folders out (a package half-way
    // through landing must not block building the others).
    println!("cargo::rerun-if-env-changed=NUVIRA_BENCH_SKIP");
    let skip: Vec<String> = std::env::var("NUVIRA_BENCH_SKIP")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let mut names: Vec<String> = Vec::new();
    let mut present: Vec<String> = Vec::new();
    for (dir, files) in PLANNED {
        names.push(format!("bench_has_{dir}"));
        for f in *files {
            names.push(format!("bench_has_{dir}_{f}"));
        }
        if skip.iter().any(|s| s == dir) {
            continue;
        }
        let d = src.join(dir);
        if d.join("mod.rs").is_file() {
            present.push(format!("bench_has_{dir}"));
        }
        if let Ok(entries) = std::fs::read_dir(&d) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "rs")
                    && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                    && stem != "mod"
                {
                    let name = format!("bench_has_{dir}_{stem}");
                    if !names.contains(&name) {
                        names.push(name.clone());
                    }
                    present.push(name);
                }
            }
        }
    }
    println!("cargo::rustc-check-cfg=cfg({})", names.join(", "));
    for p in present {
        println!("cargo::rustc-cfg={p}");
    }

    // rustc version (the same compiler builds this crate and the production code).
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = run(Command::new(rustc).arg("-V")).unwrap_or_default();
    println!("cargo::rustc-env=NUVIRA_BENCH_RUSTC={version}");

    // Git facts of the tree around bench/ (absent outside a git checkout).
    let root = Path::new("..");
    let git = |args: &[&str]| run(Command::new("git").arg("-C").arg(root).args(args));
    // bench-compare builds every side from one staging folder (the same path for A
    // and B, so path-derived hashes and the code layout match) that is not a git
    // checkout: it passes the side's facts as NUVIRA_BENCH_TREE_*.
    let over = |k: &str| {
        println!("cargo::rerun-if-env-changed={k}");
        std::env::var(k).ok()
    };
    let commit = over("NUVIRA_BENCH_TREE_COMMIT").or_else(|| git(&["rev-parse", "HEAD"])).unwrap_or_default();
    let describe = over("NUVIRA_BENCH_TREE_DESCRIBE")
        .or_else(|| git(&["describe", "--tags", "--always"]))
        .unwrap_or_default();
    let tags = over("NUVIRA_BENCH_TREE_TAGS")
        .or_else(|| git(&["tag", "--points-at", "HEAD"]))
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    // Dirty = tracked production files changed (bench/ itself is excluded: it is
    // copied into the baseline worktree on purpose).
    let dirty = match over("NUVIRA_BENCH_TREE_DIRTY") {
        Some(v) => v == "true",
        None => git(&["status", "--porcelain", "--untracked-files=no", "--", ".", ":(exclude)bench"])
            .map(|s| !s.is_empty())
            .unwrap_or(false),
    };
    let bench_commit = std::env::var("NUVIRA_BENCH_COMMIT")
        .ok()
        .or_else(|| git(&["log", "-1", "--format=%H", "--", "bench"]))
        .unwrap_or_default();
    println!("cargo::rustc-env=NUVIRA_BENCH_GIT_COMMIT={commit}");
    println!("cargo::rustc-env=NUVIRA_BENCH_GIT_DESCRIBE={describe}");
    println!("cargo::rustc-env=NUVIRA_BENCH_GIT_TAGS={tags}");
    println!("cargo::rustc-env=NUVIRA_BENCH_GIT_DIRTY={dirty}");
    println!("cargo::rustc-env=NUVIRA_BENCH_BENCH_COMMIT={bench_commit}");
    println!("cargo::rerun-if-env-changed=NUVIRA_BENCH_COMMIT");
    for p in ["HEAD", "index"] {
        if let Some(path) = git(&["rev-parse", "--git-path", p]) {
            let path = if Path::new(&path).is_absolute() { path } else { format!("../{path}") };
            println!("cargo::rerun-if-changed={path}");
        }
    }
}

fn run(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
