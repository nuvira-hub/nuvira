#!/usr/bin/env bash
# A/B benchmark comparison of NuviraHub (plan: nuvira-pro/docs/review/baseline/bench-plan.md, section 9).
#
#   bench/scripts/bench-compare.sh [options]
#
# Builds side A (the baseline tag) and side B (the version under test) at the
# same moment with the same toolchain, runs the benchmarks alternately on this
# machine (round k: A then B when k is odd, B then A when even), and writes a
# before/after table (compare.md) and its machine-readable form (compare.json).
#
# Options:
#   --base REF          side A (default baseline-pre-review)
#   --new DIR|REF       side B: a working tree (default: the tree this script is in,
#                       uncommitted changes included) or a git ref (own worktree)
#   --new-patch FILE    with --new REF: apply this patch (git diff) on B's worktree
#   --tier quick|full   (default quick)
#   --suite LIST        micro,macro,res,go,long (default: micro,macro,res,go; long = B-RES-01
#                       long session + B-RES-05 Go relay soak: once at the baseline and the final check)
#   --ids LIST          only these benchmark IDs, e.g. B-MICRO-01,B-GO-03
#   --module LIST       only the benchmarks of these modules (see --list-modules)
#   --filter REGEX      only cases whose "ID/case" matches (Rust: the registry key; Go: "ID/Function")
#   --rounds R          rounds (default 5)
#   --go-count N        go test -count per round (default 2 quick, 4 full)
#   --go-benchtime D    go test -benchtime (default 500ms quick, 1s full)
#   --idle S            seconds of idle between runs (default 10; thermal settle)
#   --out DIR           results (default $WORK/runs/<machine>/<UTC time>)
#   --work DIR          work folder: worktrees, target dirs, Go copies, binaries
#                       (default $NUVIRA_BENCH_WORK, else <parent of the repo>/nuvira-review)
#   --machine ALIAS     machine alias recorded in the results (default hostname)
#   --config-ids LIST   hw-matrix config IDs of this machine (e.g. WIN11,GPU-INTEL-QSV)
#   --noise FILE        noise calibration (default $WORK/noise-<machine>.json when present)
#   --calibrate         A against A' (two builds of the base): writes the noise file
#   --base-only         the baseline run: side A only, written as baseline.md / baseline.json
#                       (medians and deviations of A). With --rounds >= 2 a second build of A
#                       (A') runs alternately with it and the noise file is written as well
#                       (as --calibrate), so one run gives the baseline and the noise.
#   --perf              with --remote: Windows power plan High performance for the rounds (restored)
#   --info-only         report the verdicts, never gate (exit 0 unless the run is invalid); automatic
#                       for machines listed with "gate": false in bench/thresholds.json (the NUC)
#   --no-noise          compare without a noise calibration (thresholds only); without it a
#                       compare refuses to run (exit 2) when the machine has no noise file
#   --ci                CI mode: thresholds x2, speed gates report-only, software only
#   --features LIST     cargo features of side B (API escape hatch, plan 8.4)
#   --with-binaries     also build nuvira-agent / nuvira-viewer / animate per side (two-process
#                       and window cases, lan-ab.sh); large: own target dirs app-a / app-b
#   --remote USER@HOST  run the rounds on a remote Windows PC (bench-remote.sh)
#   --remote-dir DIR    folder on the remote PC (default C:\Users\<user>\nuvira-bench)
#   --skip-build        reuse the binaries already in $WORK/dist (same --work)
#   --keep-worktrees    do not remove the temporary worktrees at the end
#   --dry-run           print what would run, run nothing
#   --list-modules      print the module -> benchmark IDs table and exit
#
# Exit codes: 0 = no regression; 1 = at least one regression or exact mismatch;
# 2 = invalid run (build failed, fixtures differ, a case failed or is missing on one side).
#
# Bash 3.2 compatible (macOS): no associative arrays, no mapfile, no ${x,,},
# no GNU-only flags. All statistics are done by `nuvira-bench compare` (Rust).

set -u
set -o pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BENCH_SRC="$(cd "$SCRIPT_DIR/.." && pwd)"
SRC_REPO="$(cd "$BENCH_SRC/.." && pwd)"

# ---- options --------------------------------------------------------------------

BASE="baseline-pre-review"
NEW=""
NEW_PATCH=""
TIER="quick"
SUITES=""
IDS=""
MODULES=""
FILTER=""
ROUNDS=5
GO_COUNT=""
GO_BENCHTIME=""
IDLE=10
OUT=""
WORK="${NUVIRA_BENCH_WORK:-}"
MACHINE=""
CONFIG_IDS=""
NOISE=""
CALIBRATE=0
NO_NOISE=0
INFO_ONLY=0
PERF=0
BASE_ONLY=0
CI=0
FEATURES=""
WITH_BINARIES=0
REMOTE=""
REMOTE_DIR=""
SKIP_BUILD=0
KEEP_WT=0
DRY=0

die() { echo "bench-compare: $*" >&2; exit 2; }
say() { echo "== $*" >&2; }

. "$SCRIPT_DIR/lib.sh"

while [ $# -gt 0 ]; do
    case "$1" in
        --base) BASE="$2"; shift ;;
        --new) NEW="$2"; shift ;;
        --new-patch) NEW_PATCH="$2"; shift ;;
        --tier) TIER="$2"; shift ;;
        --suite|--suites) SUITES="$2"; shift ;;
        --ids) IDS="$2"; shift ;;
        --module|--modules) MODULES="$2"; shift ;;
        --filter) FILTER="$2"; shift ;;
        --rounds) ROUNDS="$2"; shift ;;
        --go-count) GO_COUNT="$2"; shift ;;
        --go-benchtime) GO_BENCHTIME="$2"; shift ;;
        --idle) IDLE="$2"; shift ;;
        --out) OUT="$2"; shift ;;
        --work) WORK="$2"; shift ;;
        --machine) MACHINE="$2"; shift ;;
        --config-ids) CONFIG_IDS="$2"; shift ;;
        --noise) NOISE="$2"; shift ;;
        --calibrate) CALIBRATE=1 ;;
        --no-noise) NO_NOISE=1 ;;
        --info-only) INFO_ONLY=1 ;;
        --perf) PERF=1 ;;
        --base-only) BASE_ONLY=1 ;;
        --ci) CI=1 ;;
        --features) FEATURES="$2"; shift ;;
        --with-binaries) WITH_BINARIES=1 ;;
        --remote) REMOTE="$2"; shift ;;
        --remote-dir) REMOTE_DIR="$2"; shift ;;
        --skip-build) SKIP_BUILD=1 ;;
        --keep-worktrees) KEEP_WT=1 ;;
        --dry-run) DRY=1 ;;
        --list-modules)
            for m in $BENCH_MODULES; do printf '%-10s %s\n' "$m" "$(module_ids "$m")"; done
            exit 0 ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option $1 (see --help)" ;;
    esac
    shift
done

case "$TIER" in quick|full) ;; *) die "--tier must be quick or full" ;; esac
case "$ROUNDS" in ''|*[!0-9]*) die "--rounds must be a number" ;; esac
[ "$ROUNDS" -ge 1 ] || die "--rounds must be >= 1"
if [ -z "$GO_COUNT" ]; then [ "$TIER" = quick ] && GO_COUNT=2 || GO_COUNT=4; fi
if [ -z "$GO_BENCHTIME" ]; then [ "$TIER" = quick ] && GO_BENCHTIME=500ms || GO_BENCHTIME=1s; fi

# ---- platform ---------------------------------------------------------------------

EXE=""
IS_WINDOWS=0
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) EXE=".exe"; IS_WINDOWS=1 ;;
esac
# Remote runs measure the remote PC: its alias names the results and the noise file.
if [ -z "$MACHINE" ] && [ -n "$REMOTE" ]; then MACHINE="${REMOTE_MACHINE:-${REMOTE#*@}}"; fi
[ -n "$MACHINE" ] || MACHINE="$(hostname 2>/dev/null | cut -d. -f1)"
[ -n "$MACHINE" ] || MACHINE="machine"

[ -n "$WORK" ] || WORK="$(cd "$SRC_REPO/.." && pwd)/nuvira-review"
case "$WORK" in /*) ;; *) WORK="$(pwd)/$WORK" ;; esac
if [ "$IS_WINDOWS" = 1 ]; then
    case "$WORK" in
        /c/*|/C/*|C:*|c:*)
            [ "${NUVIRA_BENCH_ALLOW_C:-}" = 1 ] || die "work folder $WORK is on C: (system disk): use --work or NUVIRA_BENCH_WORK on another disk (NUVIRA_BENCH_ALLOW_C=1 to force)" ;;
    esac
fi


run() {
    # Prints and runs a command (or only prints it with --dry-run).
    echo "+ $*" >&2
    [ "$DRY" = 1 ] && return 0
    "$@"
}

# ---- selection ------------------------------------------------------------------

SEL_IDS=""
if [ -n "$MODULES" ]; then
    for m in $(echo "$MODULES" | tr ',' ' '); do
        ids="$(module_ids "$m")" || die "unknown module $m (see --list-modules)"
        SEL_IDS="$SEL_IDS $ids"
    done
fi
[ -n "$IDS" ] && SEL_IDS="$SEL_IDS $(echo "$IDS" | tr ',' ' ')"
SEL_IDS="$(echo $SEL_IDS | tr ' ' '\n' | sort -u | tr '\n' ' ')"
SEL_IDS="${SEL_IDS% }"


if [ -z "$SUITES" ]; then
    if [ -n "$SEL_IDS" ]; then
        for id in $SEL_IDS; do
            case "$id" in
                B-MICRO-*) s=micro ;; B-MACRO-*) s=macro ;; B-RES-05) s=go-soak ;; B-RES-01) s=long ;; B-RES-*) s=res ;; B-GO-*) s=go ;;
                *) die "unknown benchmark ID $id" ;;
            esac
            has_word "$SUITES" "$s" || SUITES="$SUITES $s"
        done
    else
        SUITES="micro macro res go"   # the long suite (B-RES-01, B-RES-05) only when asked for
    fi
else
    SUITES="$(echo "$SUITES" | tr ',' ' ')"
    # The long suite: B-RES-01 (Rust) and the Go relay soak B-RES-05.
    if has_word "$SUITES" long && ! has_word "$SUITES" go-soak; then SUITES="$SUITES go-soak"; fi
fi
SUITES="$(echo $SUITES)"
RUST_SUITES=""
for s in $SUITES; do
    case "$s" in
        micro|macro|res|long) RUST_SUITES="${RUST_SUITES:+$RUST_SUITES,}$s" ;;
        go|go-soak) ;;
        *) die "unknown suite $s (micro, macro, res, long, go)" ;;
    esac
done
NEED_GO=0
has_word "$SUITES" go && NEED_GO=1
has_word "$SUITES" go-soak && NEED_GO=1
has_word "$SUITES" macro && NEED_GO=1   # nuvira-server for server-mode cases
has_word "$SUITES" res && NEED_GO=1
has_word "$SUITES" long && NEED_GO=1

# Regex for the Rust runner, matched on the registry key "ID/case".
RUST_FILTER="$FILTER"
if [ -n "$SEL_IDS" ]; then
    alt="$(echo "$SEL_IDS" | tr ' ' '|')"
    if [ -n "$FILTER" ]; then RUST_FILTER="^($alt)/.*($FILTER)"; else RUST_FILTER="^($alt)/"; fi
fi

# ---- folders ----------------------------------------------------------------------

STAMP="$(date -u +%Y-%m-%dT%H-%M-%SZ)"
[ -n "$OUT" ] || OUT="$WORK/runs/$MACHINE/$STAMP"
[ -n "$NOISE" ] || NOISE="$WORK/noise-$MACHINE.json"
# Info-only machines (thresholds.json "machines": {"ALIAS": { "gate": false ...}}) never gate.
if grep -q "\"$MACHINE\": *{ *\"gate\": *false" "$BENCH_SRC/thresholds.json" 2>/dev/null; then INFO_ONLY=1; fi
# Gating needs the machine's noise (plan 9.3: threshold + noise): run --calibrate first.
[ "$BASE_ONLY" = 1 ] && [ "$CALIBRATE" = 1 ] && die "--base-only and --calibrate exclude each other (run --calibrate first, then --base-only)"
[ "$BASE_ONLY" = 1 ] && [ -n "$REMOTE" ] && die "--base-only is local only for now (on the LAB use the A side of a --remote A/B)"
# Sides measured: both, or only A for the baseline run.
# --base-only with >= 2 rounds: A and A' (second build of A) alternate, for the noise.
BASE_A2=0; [ "$BASE_ONLY" = 1 ] && [ "$ROUNDS" -ge 2 ] && BASE_A2=1
SIDES="A B"; [ "$BASE_ONLY" = 1 ] && [ "$BASE_A2" = 0 ] && SIDES="A"
# (Not for info-only machines: nothing gates there; their noise file is used when present.)
if [ "$CALIBRATE" = 0 ] && [ "$BASE_ONLY" = 0 ] && [ "$INFO_ONLY" = 0 ] && [ "$NO_NOISE" = 0 ] && [ "$DRY" = 0 ] && [ ! -f "$NOISE" ]; then
    die "no noise calibration for machine $MACHINE ($NOISE): run 'bench-compare.sh --calibrate' first (same --machine/--work${REMOTE:+/--remote}), or pass --no-noise to gate on the thresholds alone"
fi
if [ "$CALIBRATE" = 0 ] && [ "$BASE_ONLY" = 0 ] && [ "$NO_NOISE" = 0 ] && [ -f "$NOISE" ]; then
    # The noise of another machine does not describe this one.
    noise_alias="$(grep -o '"alias": *"[^"]*"' "$NOISE" | head -n 1 | sed 's/.*"\([^"]*\)"$/\1/')"
    [ -z "$noise_alias" ] || [ "$noise_alias" = "$MACHINE" ] \
        || die "noise file $NOISE was calibrated on $noise_alias, not on $MACHINE: calibrate this machine (--calibrate) or pass --no-noise"
fi
DIST="$WORK/dist"
STAGE="$WORK/build-tree"   # one build path for every side (stage_tree)
TMPW="$WORK/tmp"
export GOCACHE="${GOCACHE_BENCH:-$WORK/go-cache}"
export GOTMPDIR="$TMPW"
export TMPDIR="$TMPW" TMP="$(native "$TMPW")" TEMP="$(native "$TMPW")"

if [ "$CALIBRATE" = 1 ] || [ "$BASE_ONLY" = 1 ]; then
    SIDE_A_TARGET="$WORK/bench-a"; SIDE_B_TARGET="$WORK/bench-a2"
else
    SIDE_A_TARGET="$WORK/bench-a"; SIDE_B_TARGET="$WORK/bench-b"
fi

say "work folder $WORK, results $OUT"
say "suites: $SUITES${SEL_IDS:+; IDs: $SEL_IDS}${FILTER:+; filter: $FILTER}; tier $TIER; $ROUNDS rounds"
run mkdir -p "$WORK" "$OUT" "$DIST" "$TMPW" || die "cannot create $WORK"

CREATED_WT=""
cleanup() {
    if [ "$KEEP_WT" = 0 ] && [ -n "$CREATED_WT" ]; then
        for wt in $CREATED_WT; do
            run git -C "$SRC_REPO" worktree remove --force "$wt" >/dev/null 2>&1 || true
        done
    fi
    # Restore a bench/Cargo.lock replaced in a tree that is not a temporary worktree.
    if [ -n "${RESTORE_LOCK:-}" ] && [ -f "$RESTORE_LOCK.bench-compare-save" ]; then
        mv -f "$RESTORE_LOCK.bench-compare-save" "$RESTORE_LOCK"
    fi
}
trap cleanup EXIT

# ---- 1. trees --------------------------------------------------------------------

copy_bench() {
    # copy_bench DEST_TREE: the bench code of this tree into DEST_TREE/bench (no target dir).
    [ "$(cd "$1" && pwd)" = "$SRC_REPO" ] && return 0
    run rm -rf "$1/bench" || return 1
    run mkdir -p "$1/bench" || return 1
    [ "$DRY" = 1 ] && return 0
    (cd "$BENCH_SRC" && tar --exclude=./target -cf - .) | (cd "$1/bench" && tar -xf -)
}

make_worktree() {
    # make_worktree DIR REF
    if [ -d "$1" ]; then
        run git -C "$SRC_REPO" worktree remove --force "$1" >/dev/null 2>&1 || run rm -rf "$1"
    fi
    run git -C "$SRC_REPO" worktree prune
    run git -C "$SRC_REPO" worktree add --detach "$1" "$2" >/dev/null || return 1
    CREATED_WT="$CREATED_WT $1"
}

git -C "$SRC_REPO" rev-parse --verify -q "$BASE^{commit}" >/dev/null || die "unknown base ref $BASE"
TREE_A="$WORK/wt-a"
if [ "$SKIP_BUILD" = 0 ]; then
    say "1. trees"
    make_worktree "$TREE_A" "$BASE" || die "worktree of $BASE failed"
    copy_bench "$TREE_A" || die "copying bench/ into side A failed"
fi
if [ "$CALIBRATE" = 1 ] || [ "$BASE_ONLY" = 1 ]; then
    TREE_B="$TREE_A"
elif [ -z "$NEW" ]; then
    TREE_B="$SRC_REPO"
elif [ -d "$NEW" ]; then
    TREE_B="$(cd "$NEW" && pwd)"
    [ "$SKIP_BUILD" = 0 ] && { copy_bench "$TREE_B" || die "copying bench/ into side B failed"; }
else
    git -C "$SRC_REPO" rev-parse --verify -q "$NEW^{commit}" >/dev/null || die "--new: not a folder nor a ref: $NEW"
    TREE_B="$WORK/wt-b"
    if [ "$SKIP_BUILD" = 0 ]; then
        make_worktree "$TREE_B" "$NEW" || die "worktree of $NEW failed"
        if [ -n "$NEW_PATCH" ]; then
            run git -C "$TREE_B" apply --whitespace=nowarn "$NEW_PATCH" || die "patch $NEW_PATCH does not apply"
        fi
        copy_bench "$TREE_B" || die "copying bench/ into side B failed"
    fi
fi
note=""; [ "$CALIBRATE" = 1 ] && note=" (calibration: B = second build of A)"
[ "$BASE_ONLY" = 1 ] && note=" (baseline run: side A only)"
[ "$BASE_A2" = 1 ] && note=" (baseline run: B = A', second build of A, for the noise)"
say "A = $BASE ($(git -C "$SRC_REPO" rev-parse --short "$BASE")), B = $TREE_B$note"
# Recorded in the Go results (the Rust runner records them itself).
COMMIT_A="$(git -C "$SRC_REPO" rev-parse "$BASE^{commit}")"; DESC_A="$BASE"; DIRTY_A=false
if [ "$CALIBRATE" = 1 ] || [ "$BASE_ONLY" = 1 ]; then
    COMMIT_B="$COMMIT_A"; DESC_B="$BASE (second build)"; DIRTY_B=false
else
    COMMIT_B="$(git -C "$TREE_B" rev-parse HEAD 2>/dev/null)"
    DESC_B="$(git -C "$TREE_B" describe --tags --always 2>/dev/null)"
    DIRTY_B=false
    [ -n "$(git -C "$TREE_B" status --porcelain --untracked-files=no -- . ':(exclude)bench' 2>/dev/null)" ] && DIRTY_B=true
fi
GO_VERSION="$(go env GOVERSION 2>/dev/null)"

# ---- 2-3. lock files and builds -----------------------------------------------------

stage_tree() {
    # stage_tree TREE: the tracked and untracked (not ignored) files of TREE, bench/ included,
    # into $STAGE, a folder with the same path for every side. Rust derives crate hashes,
    # symbol names and codegen-unit partitioning from the source path, so two builds of the
    # same code from two paths have different code layout (measured: the tag in wt-a and
    # the same code in this tree gave .text sections 480 bytes apart and up to 15% apart in
    # some micro cases); from one path they are byte-identical. tar keeps the files'
    # times, so each side's target dir stays incremental.
    run rm -rf "$STAGE" || return 1
    run mkdir -p "$STAGE" || return 1
    [ "$DRY" = 1 ] && return 0
    (cd "$1" && git ls-files -co --exclude-standard -z | tar --null -T - -cf -) | (cd "$STAGE" && tar -xf -)
}

tree_sum() {
    # tree_sum TREE: a checksum of the files stage_tree copies (content and names, not times).
    (cd "$1" && git ls-files -co --exclude-standard -z | xargs -0 cksum | cksum)
}

fresh_target() {
    # fresh_target TARGET SUM: TARGET (a shared CARGO_TARGET_DIR: bench-a, bench-b, bench-a2,
    # app-a, app-b) last built another tree. stage_tree keeps the files' times and cargo takes
    # a source older than its last build as unchanged, so a target dir last used by a newer
    # build of other code (the calibration's A' in app-b, an earlier lot) gave B stale crates
    # (lot 25 on LAB). Every staged file is then touched: the workspace crates are rebuilt,
    # third-party crates stay cached. The stamp is written by stamp_target after the build.
    [ "$DRY" = 1 ] && return 0
    if [ -f "$1/.nuvira-bench-tree" ] && [ "$(cat "$1/.nuvira-bench-tree")" = "$2" ]; then return 0; fi
    say "target $1 last built other sources: touching the staged tree (full rebuild of the workspace crates)"
    find "$STAGE" -type f -exec touch {} + || return 1
}

stamp_target() {
    # stamp_target TARGET SUM
    [ "$DRY" = 1 ] && return 0
    mkdir -p "$1" && printf '%s
' "$2" >"$1/.nuvira-bench-tree"
}

build_side() {
    # build_side SIDE TREE TARGET FEATURES
    local side tree target feats fargs app_target gostage pkg d b dirty sum
    side="$1"; tree="$2"; target="$3"; feats="$4"
    stage_tree "$tree" || return 1
    # The bench crate also builds with the side's features; bench/ comes from this tree.
    sum="$(tree_sum "$tree") $feats $(tree_sum "$BENCH_SRC")"
    # Rule 2.1: the side's own third-party versions (in the staged copy only).
    run cp -f "$STAGE/Cargo.lock" "$STAGE/bench/Cargo.lock" || return 1
    fargs=""
    [ -n "$feats" ] && fargs="--features $feats"
    # Facts of the side's real tree, recorded in the results (the stage is no git checkout).
    dirty=false
    [ -n "$(git -C "$tree" status --porcelain --untracked-files=no -- . ':(exclude)bench' 2>/dev/null)" ] && dirty=true
    export NUVIRA_BENCH_TREE_COMMIT="$(git -C "$tree" rev-parse HEAD 2>/dev/null)"
    export NUVIRA_BENCH_TREE_DESCRIBE="$(git -C "$tree" describe --tags --always 2>/dev/null)"
    export NUVIRA_BENCH_TREE_TAGS="$(git -C "$tree" tag --points-at HEAD 2>/dev/null | tr '\n' ' ')"
    export NUVIRA_BENCH_TREE_DIRTY="$dirty"
    export NUVIRA_BENCH_COMMIT="$(git -C "$SRC_REPO" log -1 --format=%H -- bench 2>/dev/null)"
    say "build $side: bench crate ($(cd "$tree" && rustc -V)) from $STAGE"
    fresh_target "$target" "$sum" || return 1
    ( cd "$STAGE/bench" && CARGO_TARGET_DIR="$(native "$target")" run cargo build --release $fargs ) || return 1
    stamp_target "$target" "$sum"
    run mkdir -p "$DIST/$side" || return 1
    run cp -f "$target/release/nuvira-bench$EXE" "$DIST/$side/nuvira-bench$EXE" || return 1
    if [ "$WITH_BINARIES" = 1 ]; then
        app_target="$WORK/app-$(echo "$side" | tr 'AB' 'ab')"
        say "build $side: nuvira-agent, nuvira-viewer, animate"
        fresh_target "$app_target" "$sum" || return 1
        ( cd "$STAGE" && CARGO_TARGET_DIR="$(native "$app_target")" run cargo build --release -p nuvira-agent -p nuvira-viewer ) || return 1
        ( cd "$STAGE" && CARGO_TARGET_DIR="$(native "$app_target")" run cargo build --release -p nuvira-viewer --example animate ) || return 1
        stamp_target "$app_target" "$sum"
        for b in nuvira-agent nuvira-viewer; do run cp -f "$app_target/release/$b$EXE" "$DIST/$side/$b$EXE" || return 1; done
        run cp -f "$app_target/release/examples/animate$EXE" "$DIST/$side/animate$EXE" || return 1
    fi
    unset NUVIRA_BENCH_TREE_COMMIT NUVIRA_BENCH_TREE_DESCRIBE NUVIRA_BENCH_TREE_TAGS NUVIRA_BENCH_TREE_DIRTY NUVIRA_BENCH_COMMIT
    if [ "$NEED_GO" = 1 ]; then
        say "build $side: Go server and test binaries"
        gostage="$WORK/go-$side"
        run rm -rf "$gostage" || return 1
        run mkdir -p "$gostage" || return 1
        [ "$DRY" = 0 ] && { (cd "$tree" && tar -cf - server) | (cd "$gostage" && tar -xf -) || return 1; }
        # The Go benchmarks live in bench/go/<package>/ and are copied into the package folders
        # of a copy of the side's server/ (the side's tree itself is not touched).
        for d in "$BENCH_SRC"/go/*/; do
            pkg="$(basename "$d")"
            run cp -f "$d"bench_*_test.go "$gostage/server/internal/$pkg/" || return 1
        done
        ( cd "$gostage/server" && run gofmt -l ./internal ) | grep . && { echo "gofmt: files not formatted" >&2; return 1; }
        ( cd "$gostage/server" && run go vet ./internal/relay ./internal/signal ./internal/store ) || return 1
        # -trimpath: no file system paths in the binaries (go-A vs go-B).
        for pkg in relay signal store; do
            ( cd "$gostage/server" && run go test -c -trimpath -o "$(native "$DIST/$side/$pkg.test$EXE")" "./internal/$pkg" ) || return 1
        done
        ( cd "$gostage/server" && run go build -trimpath -o "$(native "$DIST/$side/nuvira-server$EXE")" ./cmd/nuvira-server ) || return 1
    fi
}

if [ "$SKIP_BUILD" = 0 ]; then
    say "2-3. builds"
    build_side A "$TREE_A" "$SIDE_A_TARGET" "" || die "build of side A failed"
    if [ "$BASE_ONLY" = 1 ] && [ "$BASE_A2" = 0 ]; then
        :   # no side B
    elif [ "$CALIBRATE" = 1 ] || [ "$BASE_ONLY" = 1 ]; then
        build_side B "$TREE_A" "$SIDE_B_TARGET" "" || die "second build of side A failed"
    else
        build_side B "$TREE_B" "$SIDE_B_TARGET" "$FEATURES" || die "build of side B failed"
    fi
    va="$(cd "$TREE_A" && rustc -V)"; vb="$(cd "$TREE_B" && rustc -V)"
    [ "$va" = "$vb" ] || die "rustc differs between the sides: $va / $vb"
    if [ "$NEED_GO" = 1 ] && [ "$SIDES" = "A B" ]; then
        ga="$(cd "$WORK/go-A/server" 2>/dev/null && go env GOVERSION)"; gb="$(cd "$WORK/go-B/server" 2>/dev/null && go env GOVERSION)"
        [ "$ga" = "$gb" ] || die "go differs between the sides: $ga / $gb"
    fi
fi
JUDGE="$DIST/B/nuvira-bench$EXE"     # the compare tool always comes from the new build
[ "$BASE_ONLY" = 1 ] && JUDGE="$DIST/A/nuvira-bench$EXE"   # A's tree carries this tree's bench/
[ "$DRY" = 1 ] || [ -x "$JUDGE" ] || die "missing $JUDGE (build first, or drop --skip-build)"

# ---- 4. fixtures ----------------------------------------------------------------------

if [ -n "$RUST_SUITES" ]; then
    say "4. fixtures verify"
    for side in $SIDES; do
        run "$DIST/$side/nuvira-bench$EXE" fixtures verify --quick || die "fixtures of side $side differ from the manifest"
    done
fi

# ---- remote run: everything else happens on the other PC --------------------------------

if [ -n "$REMOTE" ]; then
    say "rounds on $REMOTE (bench-remote.sh)"
    for b in nuvira-agent nuvira-server; do
        [ "$DRY" = 1 ] || [ -f "$DIST/A/$b$EXE" ] \
            || echo "bench-compare: warning: no $b in $DIST/A: cases needing it are not-runnable on $REMOTE (build with --with-binaries / the macro or res suite)" >&2
    done
    set -- --remote "$REMOTE" --dist "$DIST" --out "$OUT" --rounds "$ROUNDS" --tier "$TIER" \
        --suites "$(echo "$SUITES" | tr ' ' ',')" --idle "$IDLE" --go-count "$GO_COUNT" --go-benchtime "$GO_BENCHTIME" \
        --machine "$MACHINE"
    [ -n "$REMOTE_DIR" ] && set -- "$@" --remote-dir "$REMOTE_DIR"
    [ -n "$CONFIG_IDS" ] && set -- "$@" --config-ids "$CONFIG_IDS"
    [ -n "$RUST_FILTER" ] && set -- "$@" --filter "$RUST_FILTER"
    [ -n "$SEL_IDS" ] && set -- "$@" --ids "$(echo "$SEL_IDS" | tr ' ' ',')"
    [ "$PERF" = 1 ] && set -- "$@" --perf
    run "$SCRIPT_DIR/bench-remote.sh" "$@" || die "remote run failed"
    SKIP_LOCAL_ROUNDS=1
else
    SKIP_LOCAL_ROUNDS=0
fi

# ---- 5. shared inputs ------------------------------------------------------------------

INPUTS="$OUT/inputs"
run mkdir -p "$INPUTS"
if has_word "$SUITES" macro || has_word "$SUITES" res; then
    if [ "$SKIP_LOCAL_ROUNDS" = 0 ]; then
        max=$((100 * 1024 * 1024)); [ "$TIER" = full ] && max=$((1024 * 1024 * 1024))
        say "5. shared input files (A's generator)"
        run "$DIST/A/nuvira-bench$EXE" fixtures files --dir "$(native "$INPUTS/files")" --max "$max" >"$OUT/inputs-files.txt" \
            || die "input files differ from the manifest"
    fi
fi

if has_word "$SUITES" micro && [ "$SKIP_LOCAL_ROUNDS" = 0 ]; then
    # B-MICRO-08/09 decode the same bytes on both sides: streams encoded once by A.
    say "5. decode streams for B-MICRO-08/09 (A's encoder)"
    run "$DIST/A/nuvira-bench$EXE" fixtures decode-streams --dir "$(native "$INPUTS")" --tier "$TIER" >"$OUT/inputs-decode.txt" || die "making the decode streams with side A failed"
fi

# ---- 6. rounds --------------------------------------------------------------------------

export NUVIRA_BENCH_SERVER_EXE_A="$(native "$DIST/A/nuvira-server$EXE")"
export NUVIRA_BENCH_SERVER_EXE_B="$(native "$DIST/B/nuvira-server$EXE")"
[ "$(uname -s)" = Darwin ] && export NUVIRA_KEY_STORE=file   # no Keychain dialogs

INVALID=0
run_side() {
    # run_side SIDE ROUND OUTDIR
    local side k outdir bin rdir raws pkg names raw secs c dsc dty
    side="$1"; k="$2"; outdir="$3"
    bin="$DIST/$side/nuvira-bench$EXE"
    # Binaries of this side for the cases that start other programs (plan 6.x, WP4).
    export NUVIRA_BENCH_SERVER_EXE="$(native "$DIST/$side/nuvira-server$EXE")"
    export NUVIRA_BENCH_AGENT_EXE="$(native "$DIST/$side/nuvira-agent$EXE")"
    export NUVIRA_BENCH_VIEWER_EXE="$(native "$DIST/$side/nuvira-viewer$EXE")"
    export NUVIRA_BENCH_ANIMATE_EXE="$(native "$DIST/$side/animate$EXE")"
    # B-MICRO-17 reads NUVIRA_BENCH_ANIMATE (used only with NUVIRA_BENCH_SCREEN=1, which this
    # script never sets: on the NUC the screen/input cases stay not-runnable without the user's OK).
    if [ -f "$DIST/$side/animate$EXE" ]; then export NUVIRA_BENCH_ANIMATE="$NUVIRA_BENCH_ANIMATE_EXE"; else unset NUVIRA_BENCH_ANIMATE; fi
    export NUVIRA_BENCH_SIDE="$side"
    if [ -n "$RUST_SUITES" ]; then
        say "round $k side $side: $RUST_SUITES"
        set -- run "$RUST_SUITES" --tier "$TIER" --side "$side" --round "$k" --out "$(native "$outdir")" \
            --inputs "$(native "$INPUTS")" --tmp "$(native "$TMPW")" --machine "$MACHINE" --no-verify
        [ -n "$CONFIG_IDS" ] && set -- "$@" --config-ids "$CONFIG_IDS"
        [ -n "$RUST_FILTER" ] && set -- "$@" --filter "$RUST_FILTER"
        run "$bin" "$@" || { echo "side $side round $k: nuvira-bench run failed" >&2; INVALID=1; }
    fi
    rdir="$outdir/$side/r$k"
    run mkdir -p "$rdir"
    if has_word "$SUITES" go; then
        raws=""
        for pkg in relay signal store; do
            names="$(go_selected | awk -v p="$pkg" '$1 == p { print $2 }' | tr '\n' '|')"
            names="${names%|}"
            [ -n "$names" ] || continue
            say "round $k side $side: go $pkg ($names)"
            raw="$rdir/go-$pkg.txt"
            if [ "$DRY" = 1 ]; then
                run "$DIST/$side/$pkg.test$EXE" -test.run '^$' -test.bench "^Benchmark($names)\$" -test.benchmem \
                    -test.count "$GO_COUNT" -test.benchtime "$GO_BENCHTIME" -test.timeout 60m
            else
                echo "+ $pkg.test -test.bench ^Benchmark($names)\$ -test.count $GO_COUNT -test.benchtime $GO_BENCHTIME" >&2
                ( cd "$TMPW" && "$DIST/$side/$pkg.test$EXE" -test.run '^$' -test.bench "^Benchmark($names)\$" -test.benchmem \
                    -test.count "$GO_COUNT" -test.benchtime "$GO_BENCHTIME" -test.timeout 60m ) >"$raw" 2>&1 \
                    || echo "side $side round $k: go $pkg benchmarks failed (see $raw)" >&2
            fi
            raws="$raws $(native "$raw")"
        done
        if [ -n "$raws" ]; then
            if [ "$side" = A ]; then c="$COMMIT_A"; dsc="$DESC_A"; dty="$DIRTY_A"; else c="$COMMIT_B"; dsc="$DESC_B"; dty="$DIRTY_B"; fi
            # shellcheck disable=SC2086
            run "$JUDGE" import-go --src "$(native "$BENCH_SRC/go")" --side "$side" --round "$k" \
                --set tier="$TIER" --set machine_alias="$MACHINE" --set go="$GO_VERSION" \
                --set nuvira_commit="$c" --set nuvira_describe="$dsc" --set nuvira_dirty="$dty" \
                --out "$(native "$rdir/go.json")" $raws || INVALID=1
        fi
    fi
    if has_word "$SUITES" go-soak && [ "$k" -le "${SOAK_ROUNDS:-1}" ]; then
        secs=300; [ "$TIER" = full ] && secs=1800
        say "round $k side $side: B-RES-05 relay soak ($secs s)"
        ( cd "$TMPW" && NUVIRA_BENCH_SOAK=1 NUVIRA_BENCH_SOAK_SECONDS="$secs" NUVIRA_BENCH_SIDE="$side" \
            NUVIRA_BENCH_SOAK_OUT="$(native "$rdir/go-soak.json")" \
            run "$DIST/$side/relay.test$EXE" -test.run '^TestBenchSoakRelay$' -test.v -test.timeout 90m ) \
            >"$rdir/go-soak.txt" 2>&1 || { echo "side $side: soak failed (see $rdir/go-soak.txt)" >&2; INVALID=1; }
    fi
}

check_literals() {
    # The Go benchmarks use wire-format literals: they must still match each side.
    # A failure is retried twice (a loaded machine or the antivirus can fail a
    # loopback test once); the output of the last try is kept in OUT.
    local side t pkg test
    for side in $SIDES; do
        for pkg in relay:TestBenchWireLiterals signal:TestBenchSignalLiterals; do
            test="${pkg#*:}"; pkg="${pkg%%:*}"
            t=1
            until ( cd "$TMPW" && run "$DIST/$side/$pkg.test$EXE" -test.run "^$test\$" -test.v ) \
                >"$OUT/literals-$side-$pkg.log" 2>&1; do
                [ "$t" -ge 3 ] && die "side $side: $test failed 3 times: wire literals of the Go benchmarks do not match (see $OUT/literals-$side-$pkg.log)"
                t=$((t + 1))
            done
        done
    done
}

rounds() {
    # rounds R OUTDIR
    local r outdir k order side
    r="$1"; outdir="$2"
    k=1
    while [ "$k" -le "$r" ]; do
        if [ $((k % 2)) -eq 1 ]; then order="A B"; else order="B A"; fi
        [ "$SIDES" = A ] && order="A"
        for side in $order; do
            run_side "$side" "$k" "$outdir"
            [ "$DRY" = 0 ] && [ "$IDLE" -gt 0 ] && sleep "$IDLE"
        done
        k=$((k + 1))
    done
}

if [ "$SKIP_LOCAL_ROUNDS" = 0 ]; then
    has_word "$SUITES" go && check_literals
    say "6. $ROUNDS rounds (ABBA)"
    rounds "$ROUNDS" "$OUT"
fi

# ---- 7. compare -------------------------------------------------------------------------

cargs="--thresholds $(native "$BENCH_SRC/thresholds.json")"
[ "$CI" = 1 ] && cargs="$cargs --ci"
[ "$INFO_ONLY" = 1 ] && cargs="$cargs --info-only"
if [ "$CALIBRATE" = 1 ]; then
    say "7. noise calibration -> $NOISE"
    # shellcheck disable=SC2086
    run "$JUDGE" compare --base "$(native "$OUT/A")" --new "$(native "$OUT/B")" $cargs \
        --calibrate-out "$(native "$NOISE")" --markdown "$(native "$OUT/calibration.md")" --json "$(native "$OUT/calibration.json")" \
        --title "Noise calibration: A vs A' ($MACHINE)"
    code=$?
    # The table compares A with A' without any noise yet (it is what the noise is made
    # of): its regressions are not findings. Exit 0 unless the run is invalid (2).
    [ "$code" = 1 ] && code=0
    run cp -f "$NOISE" "$OUT/noise.json"
    [ "$INVALID" = 1 ] && code=2
    say "calibration done (exit $code): $NOISE"
    exit "$code"
fi

if [ "$BASE_ONLY" = 1 ]; then
    if [ "$BASE_A2" = 1 ]; then
        say "7. noise (A vs A') -> $NOISE"
        # shellcheck disable=SC2086
        run "$JUDGE" compare --base "$(native "$OUT/A")" --new "$(native "$OUT/B")" $cargs \
            --calibrate-out "$(native "$NOISE")" --markdown "$(native "$OUT/calibration.md")" \
            --json "$(native "$OUT/calibration.json")" --title "Noise calibration: A vs A' ($MACHINE, $TIER)"
        # Exit 2 = a case failed or is missing on one side: the run is invalid.
        [ $? -ge 2 ] && INVALID=1
    fi
    say "7. baseline table (side A)"
    run "$JUDGE" compare --base "$(native "$OUT/A")" --markdown "$(native "$OUT/baseline.md")" \
        --json "$(native "$OUT/baseline.json")" --title "Baseline $BASE ($MACHINE, $TIER)"
    code=$?
    [ -f "$NOISE" ] && run cp -f "$NOISE" "$OUT/noise.json"
    [ "$INVALID" = 1 ] && [ "$code" -lt 2 ] && code=2
    say "results: $OUT/baseline.md (exit $code)"
    exit "$code"
fi
if [ "$NO_NOISE" = 0 ] && [ -f "$NOISE" ]; then
    cargs="$cargs --noise $(native "$NOISE")"; run cp -f "$NOISE" "$OUT/noise.json"
else
    say "no noise calibration used: verdicts on the thresholds alone"
fi
say "7. compare"
# shellcheck disable=SC2086
run "$JUDGE" compare --base "$(native "$OUT/A")" --new "$(native "$OUT/B")" $cargs \
    --markdown "$(native "$OUT/compare.md")" --json "$(native "$OUT/compare.json")" --suspects "$(native "$OUT/suspects.txt")" \
    --title "Benchmarks A ($BASE) vs B ($MACHINE, $TIER)"
code=$?

# Suspects: one rerun with twice the rounds, then the final table uses the rerun for those cases.
if [ "$DRY" = 0 ] && [ "$SKIP_LOCAL_ROUNDS" = 0 ] && [ -s "$OUT/suspects.txt" ] && [ "${NUVIRA_BENCH_NO_RERUN:-}" != 1 ]; then
    n="$(wc -l <"$OUT/suspects.txt" | tr -d ' ')"
    say "rerun of $n suspect case(s) with $((ROUNDS * 2)) rounds"
    ids="$(cut -f1 "$OUT/suspects.txt" | sort -u | tr '\n' ' ')"
    # Rust cases by exact key "ID/case" (regex metacharacters escaped); Go benchmarks by ID.
    keys="$(awk -F '\t' '{ k = $1 "/" $2; gsub(/[][(){}.*+?^$|\\]/, "\\\\&", k); print k }' "$OUT/suspects.txt" | tr '\n' '|')"
    keys="${keys%|}"
    SAVE_IDS="$SEL_IDS"; SAVE_FILTER="$RUST_FILTER"; SAVE_SUITES="$SUITES"; SAVE_RS="$RUST_SUITES"
    SEL_IDS="$ids"; FILTER=""; RUST_FILTER="^($keys)\$"
    SUITES=""; RUST_SUITES=""
    for id in $ids; do
        case "$id" in
            B-MICRO-*) s=micro ;; B-MACRO-*) s=macro ;; B-RES-05) s=go-soak ;; B-RES-01) s=long ;; B-RES-*) s=res ;; B-GO-*) s=go ;; *) s="" ;;
        esac
        [ -n "$s" ] && ! has_word "$SUITES" "$s" && SUITES="$SUITES $s"
        case "$s" in micro|macro|res|long) case ",$RUST_SUITES," in *",$s,"*) ;; *) RUST_SUITES="${RUST_SUITES:+$RUST_SUITES,}$s" ;; esac ;; esac
    done
    SOAK_ROUNDS=0
    rounds $((ROUNDS * 2)) "$OUT/rerun"
    SEL_IDS="$SAVE_IDS"; RUST_FILTER="$SAVE_FILTER"; SUITES="$SAVE_SUITES"; RUST_SUITES="$SAVE_RS"
    # shellcheck disable=SC2086
    run "$JUDGE" compare --base "$(native "$OUT/A")" --new "$(native "$OUT/B")" $cargs \
        --rerun-base "$(native "$OUT/rerun/A")" --rerun-new "$(native "$OUT/rerun/B")" \
        --markdown "$(native "$OUT/compare.md")" --json "$(native "$OUT/compare.json")" \
        --suspects "$(native "$OUT/suspects-after-rerun.txt")" \
        --title "Benchmarks A ($BASE) vs B ($MACHINE, $TIER; suspects rerun x2)"
    code=$?
fi

[ "$INVALID" = 1 ] && [ "$code" -lt 2 ] && code=2
{
    echo "bench-compare.sh $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "base=$BASE ($(git -C "$SRC_REPO" rev-parse "$BASE")) new=$TREE_B"
    echo "tier=$TIER suites=$SUITES ids=$SEL_IDS filter=$FILTER rounds=$ROUNDS go_count=$GO_COUNT go_benchtime=$GO_BENCHTIME"
    echo "machine=$MACHINE config_ids=$CONFIG_IDS ci=$CI features=$FEATURES remote=$REMOTE no_noise=$NO_NOISE info_only=$INFO_ONLY"
    echo "exit=$code"
} >"$OUT/command.txt" 2>/dev/null || true
say "results: $OUT/compare.md (exit $code)"
exit "$code"
