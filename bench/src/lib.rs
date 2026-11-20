//! NuviraHub benchmark suite (design: `nuvira-pro/docs/review/baseline/bench-plan.md`).
//!
//! Core (WP1): [`registry`] (cases, filters, tiers), [`measure`] (reps,
//! statistics, CPU time, pacing), [`report`] (JSON schema), [`alloc`] (counting
//! allocator), [`resources`] (RSS, threads, handles, sockets), [`rng`], [`fixtures`]
//! (scenes, protocol samples, files, manifest) and [`quality`] (PSNR, SSIM, BD-rate).
//!
//! The other work packages each own one folder; its `mod.rs` declares the folder's
//! files and, for case folders, provides `pub fn register(reg: &mut Registry)`:
//! - `micro/` (WP2): B-MICRO-01..18;
//! - `net/` (WP3: `mod.rs`, `netem.rs`, `headless.rs`; WP4: `server_mode.rs`) with
//!   `pub fn cli_netem(args: &[String]) -> anyhow::Result<i32>` and
//!   `pub fn cli_agent_host(args: &[String]) -> anyhow::Result<i32>`;
//! - `macro_/` (WP3: `mod.rs`, `composed.rs`, `session.rs`; WP4: `connect.rs`,
//!   `reconnect.rs`, `files.rs`, `input.rs`), `register`;
//! - `res/` (WP4), `register`;
//! - `compare/` (WP5) with `pub fn cli_compare(args: &[String]) -> anyhow::Result<i32>`
//!   and `pub fn cli_import_go(args: &[String]) -> anyhow::Result<i32>`.
//!
//! Until a package lands its folder is simply absent: `build.rs` sets
//! `bench_has_<dir>` when `src/<dir>/mod.rs` exists and `bench_has_<dir>_<file>` for
//! each `src/<dir>/<file>.rs`, so a `mod.rs` can declare a sibling file another
//! package writes with `#[cfg(bench_has_macro__connect)] pub mod connect;` (note the
//! double underscore for `macro_`) and every package builds in any landing order.

pub mod alloc;
pub mod fixtures;
pub mod measure;
pub mod quality;
pub mod registry;
pub mod report;
pub mod resources;
pub mod rng;

#[cfg(bench_has_micro)]
pub mod micro;

#[cfg(bench_has_net)]
pub mod net;

#[cfg(bench_has_macro_)]
pub mod macro_;

#[cfg(bench_has_res)]
pub mod res;

#[cfg(bench_has_compare)]
pub mod compare;

pub use registry::{Case, Level, Registry, RunCtx, Tier};

#[global_allocator]
static GLOBAL: alloc::CountingAlloc = alloc::CountingAlloc;

/// Every case of this build.
pub fn registry() -> Registry {
    #[allow(unused_mut)]
    let mut reg = Registry::new();
    #[cfg(bench_has_micro)]
    micro::register(&mut reg);
    #[cfg(bench_has_macro_)]
    macro_::register(&mut reg);
    #[cfg(bench_has_res)]
    res::register(&mut reg);
    reg
}

/// Which work-package folders this build contains (printed by `list`).
pub fn packages() -> Vec<(&'static str, bool)> {
    vec![
        ("micro", cfg!(bench_has_micro)),
        ("net", cfg!(bench_has_net)),
        ("macro_", cfg!(bench_has_macro_)),
        ("res", cfg!(bench_has_res)),
        ("compare", cfg!(bench_has_compare)),
    ]
}

/// Dispatch of the subcommands implemented by other packages; exit code 2 with a
/// message when the package is not in this build.
pub mod dispatch {
    #[allow(dead_code)] // unused once every package is in the build
    fn missing(cmd: &str, pkg: &str) -> anyhow::Result<i32> {
        eprintln!("nuvira-bench {cmd}: the `{pkg}` package is not part of this build");
        Ok(2)
    }

    pub fn netem(args: &[String]) -> anyhow::Result<i32> {
        #[cfg(bench_has_net)]
        return crate::net::cli_netem(args);
        #[cfg(not(bench_has_net))]
        {
            let _ = args;
            missing("netem", "net")
        }
    }

    pub fn agent_host(args: &[String]) -> anyhow::Result<i32> {
        #[cfg(bench_has_net)]
        return crate::net::cli_agent_host(args);
        #[cfg(not(bench_has_net))]
        {
            let _ = args;
            missing("agent-host", "net")
        }
    }

    pub fn compare(args: &[String]) -> anyhow::Result<i32> {
        #[cfg(bench_has_compare)]
        return crate::compare::cli_compare(args);
        #[cfg(not(bench_has_compare))]
        {
            let _ = args;
            missing("compare", "compare")
        }
    }

    /// `fixtures decode-streams --dir DIR [--tier quick|full]`: the B-MICRO-08/09
    /// input streams, made with this build's OpenH264 when missing.
    pub fn decode_streams(dir: &std::path::Path, tier: crate::Tier) -> anyhow::Result<i32> {
        #[cfg(bench_has_micro)]
        {
            for (p, sha, made) in crate::micro::decode::make_streams(dir, tier)? {
                println!("{}	{sha}	{}", p.display(), if made { "made" } else { "kept" });
            }
            Ok(0)
        }
        #[cfg(not(bench_has_micro))]
        {
            let _ = (dir, tier);
            missing("fixtures decode-streams", "micro")
        }
    }

    pub fn import_go(args: &[String]) -> anyhow::Result<i32> {
        #[cfg(bench_has_compare)]
        return crate::compare::cli_import_go(args);
        #[cfg(not(bench_has_compare))]
        {
            let _ = args;
            missing("import-go", "compare")
        }
    }
}
