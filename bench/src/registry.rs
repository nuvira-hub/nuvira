//! Cases, the registry and the run context (plan section 11).
//!
//! Every work package implements [`Case`] for its benchmarks and adds them in its
//! folder's `register(&mut Registry)`, which `lib.rs` calls. A case returns one
//! [`CaseResult`] (the `results[]` element of the JSON schema, `report.rs`).
//!
//! Cases that need process-wide environment variables (`NUVIRA_FORCE_RELAY`,
//! `NUVIRA_VIDEO_FEC`, `NUVIRA_DECODER`, ...) return them from [`Case::env`]: the
//! runner then re-spawns `nuvira-bench` for that case alone with those variables, so
//! they never leak into other cases (plan 4.3).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use crate::report::{CaseResult, Status};

/// Benchmark level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Micro,
    Macro,
    Res,
    /// Long sessions (B-RES-01; the Go relay soak B-RES-05 in the scripts): run
    /// once at the baseline and at the final check, not after every batch.
    Long,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Micro => "micro",
            Level::Macro => "macro",
            Level::Res => "res",
            Level::Long => "long",
        }
    }

    pub fn parse(s: &str) -> Option<Level> {
        match s {
            "micro" => Some(Level::Micro),
            "macro" => Some(Level::Macro),
            "res" => Some(Level::Res),
            "long" => Some(Level::Long),
            _ => None,
        }
    }
}

/// Tier: `quick` after every Phase 6 batch, `full` for the baseline and final check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Quick,
    Full,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Quick => "quick",
            Tier::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<Tier> {
        match s {
            "quick" => Some(Tier::Quick),
            "full" => Some(Tier::Full),
            _ => None,
        }
    }
}

/// Cases run in both tiers.
pub const BOTH_TIERS: &[Tier] = &[Tier::Quick, Tier::Full];
/// Cases run only in the full tier.
pub const FULL_ONLY: &[Tier] = &[Tier::Full];

/// What a case gets from the runner.
#[derive(Clone, Debug)]
pub struct RunCtx {
    /// Measured reps (micro) or sessions per run (macro).
    pub reps: u32,
    /// Warm-up reps, discarded.
    pub warmup: u32,
    pub tier: Tier,
    /// Fresh temporary directory of this run (removed afterwards); also the
    /// `NUVIRA_DATA_DIR` of in-process agents.
    pub tmp: PathBuf,
    /// Shared inputs produced once by the A binary (decode streams, files), plan 9.2 step 5.
    pub inputs: PathBuf,
    /// Seed for anything random the case does (loss patterns, jitter).
    pub rng_seed: u64,
}

impl RunCtx {
    /// Context for unit tests: 3 reps, 1 warm-up, quick tier, a temp directory.
    pub fn for_tests() -> RunCtx {
        let tmp = std::env::temp_dir().join(format!("nuvira-bench-test-{}", std::process::id()));
        RunCtx { reps: 3, warmup: 1, tier: Tier::Quick, inputs: tmp.join("inputs"), tmp, rng_seed: 1 }
    }

    /// Frames of a fixture sequence in this tier (plan 5.1: 120 quick, 300 full).
    pub fn frames(&self) -> u32 {
        crate::fixtures::frames_for(self.tier)
    }
}

/// One benchmark case (one ID x one parameter combination).
pub trait Case: Send {
    /// Plan ID, e.g. "B-MICRO-04".
    fn id(&self) -> &'static str;
    /// Parameters, e.g. "openh264/S5/1920x1080/4000k". (id, name) is unique.
    fn name(&self) -> String;
    fn level(&self) -> Level;
    fn tiers(&self) -> &'static [Tier];
    /// Config IDs of `hw-matrix.md` this case exercises on this machine.
    fn config_ids(&self) -> &'static [&'static str];
    /// Err = not runnable here, with the reason (missing hardware, OS, permission):
    /// recorded as `not-runnable`, never counted as passed.
    fn runnable(&self) -> Result<(), String>;
    fn run(&mut self, ctx: &mut RunCtx) -> anyhow::Result<CaseResult>;

    /// Process-wide environment variables the case needs; non-empty = the case runs
    /// in a child process of its own.
    fn env(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Runs in a child process even with no environment (e.g. it changes timer
    /// resolution or leaves threads behind).
    fn isolated(&self) -> bool {
        !self.env().is_empty()
    }

    /// Measured reps of this case in `tier` when the run gives no `--reps`:
    /// `default` (10 micro, 1 macro/res) unless the case caps it to fit the tier's
    /// time budget (never below 5, the statistical minimum of the review rules).
    fn reps_for(&self, tier: Tier, default: u32) -> u32 {
        let _ = tier;
        default
    }

    /// `id/name`, the key used by filters and by `--only`.
    fn key(&self) -> String {
        format!("{}/{}", self.id(), self.name())
    }
}

/// Which cases to select.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// Empty = every level.
    pub levels: Vec<Level>,
    pub tier: Option<Tier>,
    /// Regex on `id/name`.
    pub regex: Option<regex::Regex>,
    /// Exact `id/name` (child processes).
    pub only: Option<String>,
}

impl Filter {
    pub fn matches(&self, c: &dyn Case) -> bool {
        if !self.levels.is_empty() && !self.levels.contains(&c.level()) {
            return false;
        }
        if let Some(t) = self.tier
            && !c.tiers().contains(&t)
        {
            return false;
        }
        let key = c.key();
        if let Some(re) = &self.regex
            && !re.is_match(&key)
        {
            return false;
        }
        if let Some(only) = &self.only
            && *only != key
        {
            return false;
        }
        true
    }
}

/// All cases of the binary.
#[derive(Default)]
pub struct Registry {
    cases: Vec<Box<dyn Case>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, case: Box<dyn Case>) {
        self.cases.push(case);
    }

    pub fn add_case<C: Case + 'static>(&mut self, case: C) {
        self.add(Box::new(case));
    }

    pub fn len(&self) -> usize {
        self.cases.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cases.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn Case> {
        self.cases.iter().map(|c| c.as_ref())
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Box<dyn Case>> {
        self.cases.iter_mut()
    }

    /// Indexes of the cases matching `filter`, in registration order.
    pub fn select(&self, filter: &Filter) -> Vec<usize> {
        (0..self.cases.len()).filter(|&i| filter.matches(self.cases[i].as_ref())).collect()
    }

    pub fn get_mut(&mut self, index: usize) -> &mut dyn Case {
        self.cases[index].as_mut()
    }

    /// Duplicate `id/name` keys (a programming error: results would be ambiguous).
    pub fn duplicates(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.cases.iter().map(|c| c.key()).collect();
        keys.sort();
        let mut d: Vec<String> = keys.windows(2).filter(|w| w[0] == w[1]).map(|w| w[0].clone()).collect();
        d.dedup();
        d
    }
}

/// A ready-made case from closures, for simple cases and tests.
pub struct FnCase<F: FnMut(&mut RunCtx) -> anyhow::Result<CaseResult> + Send> {
    pub id: &'static str,
    pub name: String,
    pub level: Level,
    pub tiers: &'static [Tier],
    pub config_ids: &'static [&'static str],
    pub runnable: Result<(), String>,
    pub run: F,
}

impl<F: FnMut(&mut RunCtx) -> anyhow::Result<CaseResult> + Send> Case for FnCase<F> {
    fn id(&self) -> &'static str {
        self.id
    }
    fn name(&self) -> String {
        self.name.clone()
    }
    fn level(&self) -> Level {
        self.level
    }
    fn tiers(&self) -> &'static [Tier] {
        self.tiers
    }
    fn config_ids(&self) -> &'static [&'static str] {
        self.config_ids
    }
    fn runnable(&self) -> Result<(), String> {
        self.runnable.clone()
    }
    fn run(&mut self, ctx: &mut RunCtx) -> anyhow::Result<CaseResult> {
        (self.run)(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(id: &'static str, name: &str, level: Level, tiers: &'static [Tier]) -> Box<dyn Case> {
        Box::new(FnCase {
            id,
            name: name.into(),
            level,
            tiers,
            config_ids: &[],
            runnable: Ok(()),
            run: |_ctx: &mut RunCtx| anyhow::bail!("not run"),
        })
    }

    #[test]
    fn filters_by_level_tier_regex_and_key() {
        let mut r = Registry::new();
        r.add(case("B-MICRO-01", "S1/1920x1080", Level::Micro, BOTH_TIERS));
        r.add(case("B-MICRO-01", "S1/3840x2160", Level::Micro, FULL_ONLY));
        r.add(case("B-MACRO-03", "sw/1080p", Level::Macro, BOTH_TIERS));
        assert_eq!(r.select(&Filter::default()), [0, 1, 2]);
        let quick_micro =
            Filter { levels: vec![Level::Micro], tier: Some(Tier::Quick), ..Default::default() };
        assert_eq!(r.select(&quick_micro), [0]);
        let re = Filter { regex: Some(regex::Regex::new("2160").unwrap()), ..Default::default() };
        assert_eq!(r.select(&re), [1]);
        let only = Filter { only: Some("B-MACRO-03/sw/1080p".into()), ..Default::default() };
        assert_eq!(r.select(&only), [2]);
        assert!(r.duplicates().is_empty());
        r.add(case("B-MACRO-03", "sw/1080p", Level::Macro, BOTH_TIERS));
        assert_eq!(r.duplicates(), ["B-MACRO-03/sw/1080p"]);
    }
}
