#!/usr/bin/env bash
# Runs bench-compare.sh natively on a Linux or macOS machine over SSH and brings the
# results back (plan section 9.2 and 10, "MANUAL" rows): everything is automated, the
# machine only needs SSH, git, a Rust toolchain and Go (and the client's build
# dependencies: on Linux those of .github/workflows/client-linux.yml).
#
#   bench/scripts/bench-ssh.sh --host USER@HOST --alias NAME [options] [-- bench-compare options]
#
# Options:
#   --host USER@HOST     the machine (e.g. alessandro@192.168.178.197)
#   --key FILE           SSH key (default $NUVIRA_SSH_KEY or ~/.ssh/nuvira_linux)
#   --alias NAME         machine alias of docs/review/hw-matrix.md (e.g. VM-MINT-X11, MAC-MINI-M1)
#   --config-ids LIST    hw-matrix config IDs (e.g. LINUX-X11,LINUX-VAAPI)
#   --remote-root DIR    folder on the machine (default ~/nuvira-bench)
#   --results DIR        local destination (default <nuvira-pro>/docs/review/manual-results)
#   --base REF           side A (default baseline-pre-review)
#   --new REF            side B commit (default HEAD of this repo; uncommitted changes
#                        outside bench/ are sent as a patch and applied there)
#   --poll S             seconds between checks of the remote run (default 30)
#   --perf               Linux: for the run, CPU governor "performance" and turbo/boost off
#                        (intel_pstate/no_turbo=1 or cpufreq/boost=0), restored afterwards (also
#                        when the run fails or this script is interrupted); recorded in the results
#                        (run.machine.power: plan, turbo, setup). Needs the remote sudo password in
#                        NUVIRA_SUDO_PW (given to `sudo -S` on stdin, never written anywhere).
#   The remote PATH gets ~/.cargo/bin, <remote-root>/sdk/go/bin, /usr/local/go/bin.
#   NUVIRA_BENCH_MACRO_SECS / _LONG_SECS / _EVENTS / _NOISE_CORES set here are passed on.
#   So are NUVIRA_STATS / NUVIRA_STATS_INTERVAL_MS (the statistics log, docs/48, to measure
#   its overhead: only builds that have it read them). And QLOGDIR (a folder on the
#   machine: the Go signalling benchmarks then write a qlog per QUIC connection, SRV-16).
#
# What it does: sends a git bundle (base tag + B commit), the current bench/ folder and the
# patch; checks out B in <remote-root>/repo; starts bench-compare.sh there detached from
# the SSH session (nohup), with NUVIRA_BENCH_WORK=<remote-root>/work; waits; copies the run
# folder (without the generated input files) to <results>/<alias>/<UTC time>/.
# A missing toolchain is recorded there as not-runnable (exit 2), never skipped silently.
#
# Examples:
#   bench/scripts/bench-ssh.sh --host alessandro@192.168.178.197 --alias VM-MINT-X11 \
#       --config-ids LINUX-X11 -- --tier quick --suite micro,macro
#   bench/scripts/bench-ssh.sh --host m1@MAC --key ~/.ssh/nuvira_mac --alias MAC-MINI-M1 \
#       --config-ids MAC-ARM,MAC-VIDEOTOOLBOX -- --module codec

set -u
set -o pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BENCH_SRC="$(cd "$SCRIPT_DIR/.." && pwd)"
SRC_REPO="$(cd "$BENCH_SRC/.." && pwd)"

HOST=""; KEY="${NUVIRA_SSH_KEY:-$HOME/.ssh/nuvira_linux}"; ALIAS=""; CONFIG_IDS=""
REMOTE_ROOT="nuvira-bench"; RESULTS=""; BASE="baseline-pre-review"; NEW="HEAD"; POLL=30; PERF=0

die() { echo "bench-ssh: $*" >&2; exit 2; }
say() { echo "== $*" >&2; }

while [ $# -gt 0 ]; do
    case "$1" in
        --host) HOST="$2"; shift ;;
        --key) KEY="$2"; shift ;;
        --alias) ALIAS="$2"; shift ;;
        --config-ids) CONFIG_IDS="$2"; shift ;;
        --remote-root) REMOTE_ROOT="$2"; shift ;;
        --results) RESULTS="$2"; shift ;;
        --base) BASE="$2"; shift ;;
        --new) NEW="$2"; shift ;;
        --poll) POLL="$2"; shift ;;
        --perf) PERF=1 ;;
        --) shift; break ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option $1 (bench-compare options go after --)" ;;
    esac
    shift
done
[ -n "$HOST" ] && [ -n "$ALIAS" ] || die "--host and --alias are required"
if [ -z "$RESULTS" ]; then
    RESULTS="$(cd "$SRC_REPO/.." && pwd)/nuvira-pro/docs/review/manual-results"
fi
STAMP="$(date -u +%Y-%m-%dT%H-%M-%SZ)"
DEST="$RESULTS/$ALIAS/$STAMP"
mkdir -p "$DEST" || die "cannot create $DEST"

SSH="ssh -i $KEY -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=30 -o LogLevel=ERROR $HOST"
SCP="scp -q -i $KEY -o BatchMode=yes -o LogLevel=ERROR"
TMPL="$(mktemp -d 2>/dev/null || mktemp -d -t benchssh)"
trap 'rm -rf "$TMPL"' EXIT

not_runnable() {
    # Records why the machine could not run the benchmarks.
    cat >"$DEST/not-runnable.json" <<EOF
{
  "schema": "nuvira-bench/1",
  "run": {"id": "ssh-$ALIAS-$STAMP", "side": "A", "round": 1, "suite": "all",
          "machine": {"alias": "$ALIAS", "config_ids": "$(echo "$CONFIG_IDS")"}},
  "results": [{"id": "B-ALL", "case": "$ALIAS", "level": "macro", "status": "not-runnable",
               "metrics": {}, "notes": ["$1"]}]
}
EOF
    die "$1 (recorded in $DEST/not-runnable.json)"
}

say "checking $HOST"
$SSH true 2>/dev/null || not_runnable "SSH to $HOST failed"
missing="$($SSH "for t in git cargo rustc go tar; do PATH=\"\$HOME/.cargo/bin:\$HOME/$REMOTE_ROOT/sdk/go/bin:/usr/local/go/bin:/opt/homebrew/bin:\$PATH\" command -v \$t >/dev/null 2>&1 || printf '%s ' \$t; done")"
[ -z "$missing" ] || not_runnable "missing on $HOST: $missing"

NEW_COMMIT="$(git -C "$SRC_REPO" rev-parse "$NEW^{commit}")" || die "unknown ref $NEW"
git -C "$SRC_REPO" rev-parse --verify -q "$BASE^{commit}" >/dev/null || die "unknown base $BASE"

say "bundle $BASE + $NEW_COMMIT, bench/ and patch"
refs="$BASE HEAD"
[ "$NEW" = HEAD ] || refs="$refs $NEW"
# shellcheck disable=SC2086
git -C "$SRC_REPO" bundle create "$TMPL/repo.bundle" $refs >/dev/null 2>&1 || die "git bundle failed"
(cd "$BENCH_SRC" && tar --exclude=./target -cf - .) >"$TMPL/bench.tar" || die "tar of bench/ failed"
: >"$TMPL/b.patch"
if [ "$NEW" = HEAD ]; then
    git -C "$SRC_REPO" diff --binary HEAD -- . ':(exclude)bench' >"$TMPL/b.patch"
fi
$SSH "mkdir -p $REMOTE_ROOT/incoming $REMOTE_ROOT/work" || die "mkdir on $HOST failed"
for f in repo.bundle bench.tar b.patch; do
    $SCP "$TMPL/$f" "$HOST:$REMOTE_ROOT/incoming/$f" || die "copy of $f failed"
done

# --perf: governor and turbo for the run (Linux), restored on exit.
POWER_SETUP=""
sudo_sh() {
    # sudo_sh 'COMMANDS': runs COMMANDS as root on the host; the password goes to sudo on stdin.
    printf '%s\n' "$NUVIRA_SUDO_PW" | $SSH "sudo -S -p '' sh -c '$1'"
}
restore_power() {
    [ "$PERF" = 1 ] && [ -n "$POWER_SETUP" ] || return 0
    say "restoring governor and turbo on $HOST"
    sudo_sh 'f=/var/tmp/nuvira-bench-power.saved; [ -f $f ] || exit 0;
        g=$(sed -n 1p $f); t=$(sed -n 2p $f); b=$(sed -n 3p $f);
        for c in /sys/devices/system/cpu/cpu[0-9]*/cpufreq/scaling_governor; do echo $g >$c; done;
        [ -n "$t" ] && echo $t >/sys/devices/system/cpu/intel_pstate/no_turbo;
        [ -n "$b" ] && echo $b >/sys/devices/system/cpu/cpufreq/boost; rm -f $f' \
        || echo "bench-ssh: WARNING: could not restore governor/turbo on $HOST (saved in /var/tmp/nuvira-bench-power.saved)" >&2
    POWER_SETUP=""
}
if [ "$PERF" = 1 ]; then
    [ -n "${NUVIRA_SUDO_PW:-}" ] || die "--perf needs NUVIRA_SUDO_PW (the remote sudo password; never written to files)"
    trap 'restore_power; rm -rf "$TMPL"' EXIT
    trap 'exit 130' INT TERM
    say "governor performance, turbo off on $HOST"
    sudo_sh 'set -e; f=/var/tmp/nuvira-bench-power.saved;
        g=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor);
        t=$(cat /sys/devices/system/cpu/intel_pstate/no_turbo 2>/dev/null || true);
        b=$(cat /sys/devices/system/cpu/cpufreq/boost 2>/dev/null || true);
        [ -f $f ] || printf "%s\n%s\n%s\n" "$g" "$t" "$b" >$f;
        for c in /sys/devices/system/cpu/cpu[0-9]*/cpufreq/scaling_governor; do echo performance >$c; done;
        if [ -f /sys/devices/system/cpu/intel_pstate/no_turbo ]; then echo 1 >/sys/devices/system/cpu/intel_pstate/no_turbo;
        elif [ -f /sys/devices/system/cpu/cpufreq/boost ]; then echo 0 >/sys/devices/system/cpu/cpufreq/boost; fi' \
        || die "could not set governor/turbo on $HOST (sudo)"
    POWER_SETUP="governor=performance turbo=off (restored afterwards)"
    state="$($SSH 'cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor; cat /sys/devices/system/cpu/intel_pstate/no_turbo 2>/dev/null || cat /sys/devices/system/cpu/cpufreq/boost 2>/dev/null')"
    say "now: $(echo $state)"
    echo "$POWER_SETUP; read back: $(echo $state)" >"$DEST/power.txt"
fi

# Remote driver: checkout, bench/, patch, then bench-compare.sh detached from SSH.
cat >"$TMPL/drive.sh" <<EOF
#!/usr/bin/env bash
set -u
export PATH="\$HOME/.cargo/bin:\$HOME/$REMOTE_ROOT/sdk/go/bin:/usr/local/go/bin:/opt/homebrew/bin:\$PATH"
export NUVIRA_BENCH_POWER_SETUP="$POWER_SETUP"
$(for v in NUVIRA_BENCH_MACRO_SECS NUVIRA_BENCH_LONG_SECS NUVIRA_BENCH_EVENTS NUVIRA_BENCH_NOISE_CORES NUVIRA_STATS NUVIRA_STATS_INTERVAL_MS QLOGDIR; do
    eval "x=\${$v:-}"; [ -n "$x" ] && printf 'export %s=%q
' "$v" "$x"; done)
cd "\$HOME/$REMOTE_ROOT" || cd "$REMOTE_ROOT" || exit 2
ROOT="\$(pwd)"
if [ ! -d repo/.git ]; then git clone -q incoming/repo.bundle repo || exit 2; fi
cd repo
git fetch -q ../incoming/repo.bundle "+refs/tags/*:refs/tags/*" "+refs/heads/*:refs/remotes/bundle/*" \
    "+HEAD:refs/remotes/bundle/HEAD" || exit 2
git checkout -q -f --detach $NEW_COMMIT || exit 2
git clean -fdq
rm -rf bench && mkdir bench && (cd bench && tar -xf ../../incoming/bench.tar) || exit 2
if [ -s ../incoming/b.patch ]; then git apply --whitespace=nowarn ../incoming/b.patch || exit 2; fi
OUT="\$ROOT/runs/$STAMP"
mkdir -p "\$OUT"
echo running >"\$OUT/state"
NUVIRA_BENCH_WORK="\$ROOT/work" bench/scripts/bench-compare.sh --base $BASE --machine "$ALIAS" \
    ${CONFIG_IDS:+--config-ids "$CONFIG_IDS"} --out "\$OUT" $(printf '%q ' "$@") >"\$OUT/bench-compare.log" 2>&1
echo \$? >"\$OUT/exit-code"
echo done >"\$OUT/state"
EOF
$SCP "$TMPL/drive.sh" "$HOST:$REMOTE_ROOT/incoming/drive.sh" || die "copy of drive.sh failed"
say "starting bench-compare.sh on $HOST (results $REMOTE_ROOT/runs/$STAMP)"
$SSH "chmod +x $REMOTE_ROOT/incoming/drive.sh && nohup $REMOTE_ROOT/incoming/drive.sh >$REMOTE_ROOT/incoming/drive-$STAMP.log 2>&1 </dev/null &" \
    || die "start failed"

# Wait for the end (the run survives SSH disconnections).
while :; do
    sleep "$POLL"
    state="$($SSH "cat $REMOTE_ROOT/runs/$STAMP/state 2>/dev/null || (pgrep -f drive.sh >/dev/null && echo starting || echo gone)" 2>/dev/null)"
    case "$state" in
        done) break ;;
        gone) $SCP "$HOST:$REMOTE_ROOT/incoming/drive-$STAMP.log" "$DEST/drive.log" 2>/dev/null
              die "the remote driver stopped before bench-compare.sh (see $DEST/drive.log)" ;;
        *) printf '.' >&2 ;;
    esac
done
echo >&2
code="$($SSH "cat $REMOTE_ROOT/runs/$STAMP/exit-code" 2>/dev/null)"
say "remote exit code ${code:-?}; fetching results"
$SSH "cd $REMOTE_ROOT/runs/$STAMP && tar --exclude=./inputs -cf - ." | (cd "$DEST" && tar -xf -) || die "fetch failed"
say "results in $DEST"
exit "${code:-2}"
