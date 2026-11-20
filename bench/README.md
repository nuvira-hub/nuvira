# nuvira-bench

Benchmark suite of NuviraHub: micro (B-MICRO-01..18), macro (B-MACRO-01..10), resources
(B-RES-02..04), long sessions (`long`: B-RES-01, and the Go relay soak B-RES-05) and Go server
benchmarks (B-GO-01..08), with an A/B comparison script.

- Design: `nuvira-pro/docs/review/baseline/bench-plan.md` (IDs, metrics, gates, statistics).
- Operating procedure (which command on which machine): `nuvira-pro/docs/review/baseline/bench-run.md`.

This crate has its own `[workspace]` and is not a member of the root workspace. It uses only
public APIs of the client crates, so the same `bench/` folder builds against the tag
`baseline-pre-review` and against later versions.

## Build

```bash
cd bench
CARGO_TARGET_DIR=/d/nuvira-review/bench-int cargo build --release   # never a target dir on C:
cargo test                                                          # unit tests (statistics, quality, compare, fixtures)
```

Format only these files: `rustfmt --edition 2024 <file>` (the repo's `rustfmt.toml` applies);
never `cargo fmt` at the repo root.

## Run (one side)

```bash
nuvira-bench list --tier quick                     # cases of a tier
nuvira-bench fixtures verify --quick               # generators = committed manifest
nuvira-bench run micro,macro,res --tier quick --out OUT --side A --round 1
nuvira-bench run micro --filter '^B-MICRO-04/'     # one ID
```

Results: `OUT/<side>/r<round>/<suite>.json` (schema `nuvira-bench/1`). Cases that need
hardware, another OS, the real screen or the mouse report `not-runnable` with the reason.
Screen and input cases run only with `NUVIRA_BENCH_SCREEN=1`, `NUVIRA_BENCH_REAL_CAPTURE=1`,
`NUVIRA_BENCH_INPUT=1` (set by `bench-remote.sh` on PC-LABORATORIO; on the NUC only with the
user's OK). Short-run knobs for checks (not for baselines): `NUVIRA_BENCH_MACRO_SECS=warm,measured`,
`NUVIRA_BENCH_EVENTS=n`, `NUVIRA_BENCH_LONG_SECS=s`.

## Compare (A/B)

```bash
export NUVIRA_BENCH_WORK=/d/nuvira-review
bench/scripts/bench-compare.sh --calibrate --tier quick   # once per machine: noise-<machine>.json
bench/scripts/bench-compare.sh --tier quick               # A = baseline-pre-review, B = this tree
bench/scripts/bench-compare.sh --base-only --tier full --rounds 2   # baseline table + noise (A, A')
bench/scripts/bench-compare.sh --base-only --tier full --suite long --rounds 1
```

`bench-compare.sh` builds both sides with each side's `Cargo.lock`, verifies the fixtures,
makes the shared inputs with the A binary (files, B-MICRO-08/09 decode streams), runs the rounds
in ABBA order, and writes `compare.md` / `compare.json`. It refuses to gate (exit 2) when the
machine has no noise file; `--no-noise` compares on the thresholds alone. Exit codes: 0 no
regression, 1 regression or exact mismatch, 2 invalid run. Other machines: `bench-remote.sh`
(PC-LABORATORIO), `bench-ssh.sh` (Linux VMs, Mac), `lan-ab.sh` (B-MACRO-09); see `bench-run.md`.

## Which machine gates

- **MSI-UBUNTU** (desktop i7-4770S, Linux): the gating reference for everything that is not
  Windows-specific. Run with `bench-ssh.sh --host msi-ubuntu --alias MSI-UBUNTU --perf -- ...`:
  `--perf` sets the CPU governor to `performance` and turbo off for the run and restores both
  afterwards (sudo password in `NUVIRA_SUDO_PW`, passed to `sudo -S` on stdin, never stored).
- **PC-LABORATORIO** (Windows): the gating machine for the Windows-only cases (Media Foundation,
  DXGI/GPU, Windows CPU time). `bench-compare.sh --remote mrbla@PC-LABORATORIO --perf ...`
  switches the power plan to High performance for the rounds and restores it.
- **PC-STUDIO-NUC** (the developer's PC): info-only. `thresholds.json` lists it under
  `"machines"` with `"gate": false`: `compare` reports every verdict (marked report-only) and
  exits 0 unless the run is invalid (a case failed or is missing). `--info-only` does the same
  for any machine. A machine is added to the list as
  `"ALIAS": { "gate": false, "reason": "..." }` (one line: the script greps for it).

The run metadata records the power state: `run.machine.power.plan` (governor or power plan),
`.turbo` (Linux) and `.setup` (what the script changed for the run).

## API changes

Cargo features `api-<name>` for Phase 6 batches that change an API of plan section 4.1
(plan 8.4). Build side B with them: `bench-compare.sh --features <list>`.

| Feature | Lot | Switches |
|---|---|---|
| `api-fec-encoder` | 18 (net-fec, NET-9) | B-MICRO-12 and the composed datagram sender of B-MACRO keep a `fec::Encoder` (codecs kept from frame to frame, as the agent does since lot 18) instead of calling `fec::encode` per frame. Same datagrams. |
