#!/usr/bin/env bash
# test-loom-daemon-update.sh — Tests for loom-daemon-update.sh (Issue #3968).
#
# Focus: (1) staleness detection (--check, built-commit vs. source-HEAD
# comparison), (2) flag preservation across a rebuild+restart (the persisted
# .loom/.daemon.flags file must be replayed verbatim, never widened), and
# (3) the "was not running -> do not start" / --no-restart / --dry-run
# guardrails that keep the FLAGS-OFF/opt-in contract intact.
#
# Style matches test-loom-daemon-start.sh — plain bash, hand-rolled
# assertions, no bats.
#
# Strategy: build a throwaway repo root (temp dir) with a real `.git`, a
# stub `loom-daemon/Cargo.toml`, real copies of loom-daemon-start.sh /
# loom-daemon-stop.sh (so the restart path exercises the actual scripts, not
# a mock), a FAKE `cargo` on PATH that "builds" by copying a prepared fake
# daemon binary into target/release/ instead of actually compiling Rust, and
# FAKE daemon binaries whose `--version` output embeds a controllable commit
# and whose normal-run mode records the autonomy env vars it inherited to a
# marker file before looping (staying "alive" for PID-liveness checks).
#
# Usage:
#   ./defaults/scripts/tests/test-loom-daemon-update.sh

set -uo pipefail

# Production-binary checksum guard (#4381 incident): 2026-07-29 ~06:03Z, the
# REAL machine-level `~/.local/bin/loom-daemon` was overwritten with a fake,
# infinitely-looping test-fixture stub for ~9 hours (an operator `status` poll
# hung; a crash at any point would have had launchd relaunch the stub). Root
# cause: three tests below (37/39/43, the ff-sync/staleness-detection cases)
# deliberately invoke loom-daemon-update.sh with NEITHER LOOM_DAEMON_BIN NOR a
# stubbed scripts/install/provision-daemon.sh, so the update script's
# provision_machine_daemon() call fell through to its real, un-sandboxed
# default destination (`${LOOM_DAEMON_BIN_DIR:-$HOME/.local/bin}`) — the
# operator's own real $HOME. The LOOM_DAEMON_BIN_DIR export below (threaded to
# every sub-invocation) is the actual fix; THIS checksum snapshot is the
# regression backstop that fails the suite outright if any current or future
# test call site regresses into writing the real production binary, even if
# the sandboxing above is bypassed or a new call site is added without it.
# Recorded before ANYTHING else runs, using the checksum tool most likely
# present (sha256sum on Linux, shasum on macOS); skipped (empty) only when
# neither is on PATH or no binary exists yet at that path (nothing to compare
# against — a fresh-machine first run).
_PROD_DAEMON_BIN="$HOME/.local/bin/loom-daemon"
_prod_daemon_checksum() {
    [[ -e "$_PROD_DAEMON_BIN" ]] || { echo "<absent>"; return 0; }
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$_PROD_DAEMON_BIN" 2>/dev/null | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$_PROD_DAEMON_BIN" 2>/dev/null | awk '{print $1}'
    else
        echo "<no-checksum-tool>"
    fi
}
_PROD_DAEMON_CHECKSUM_BEFORE="$(_prod_daemon_checksum)"

# Force the legacy nohup path everywhere in this suite (#3972): the restart
# flow below exercises the REAL loom-daemon-start.sh / loom-daemon-stop.sh
# (not a mock), and this test must NEVER touch the real machine's
# ~/Library/LaunchAgents/ -- which may hold an actual production
# com.rjwalters.loom-daemon LaunchAgent under the exact same default label.
export LOOM_DAEMON_LAUNCHD=0

# Force the legacy nohup path on LINUX too (#4799 CI hang). The launchd knob
# above only sandboxes Darwin; on a Linux runner `MINIMAL_PATH` below still
# reaches the REAL /usr/bin/systemctl and `is_linux_systemd` (lib/systemd-user.sh)
# returns true whenever the runner has a reachable `systemctl --user` manager.
# That made the suite non-hermetic AND non-terminating in CI:
#
#   1. scenario 5's real loom-daemon-start.sh took the systemd branch and ran
#      `systemctl --user enable --now loom-daemon.service` -- the DEFAULT unit
#      name, i.e. the operator's/runner's real user unit, plus a real
#      `loom-daemon-watchdog.timer` (provision_watchdog_job_systemd derives its
#      name from the daemon unit, NOT from the sandboxed LOOM_WATCHDOG_LABEL).
#   2. every LATER scenario then saw `systemd_unit_loaded` == true, so
#      loom-daemon-update.sh resolved DAEMON_MANAGER=systemd and took the
#      supervised-restart branch: `"$PROVISION_TARGET" restart`.
#   3. PROVISION_TARGET is the fake-daemon fixture, which has no `restart`
#      handler -- so it fell straight into its `while true; do sleep 1; done`
#      daemon body IN THE FOREGROUND and never returned. Scenario 5b wedged
#      there for the full 1200s per-suite CI budget (exit 124).
#
# A macOS dev run never exercises any of this (no systemctl), which is exactly
# why the suite passed locally in ~4m and hung in CI. The systemd-specific
# scenarios (26-34) opt back in explicitly with LOOM_DAEMON_SYSTEMD=1 alongside
# their LOOM_SYSTEMD_FORCE=1 seam and their own stub `systemctl` on PATH.
export LOOM_DAEMON_SYSTEMD=0
# Belt-and-braces on top of the knob, mirroring LOOM_LAUNCHD_LABEL below: even
# if a future regression re-opens a systemd path, the unit it would resolve is a
# per-run scratch name, never the real `loom-daemon.service`. The scenarios that
# drive systemd deliberately pin their own LOOM_SYSTEMD_UNIT per invocation.
export LOOM_SYSTEMD_UNIT="loom-daemon-update-test-$$.service"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLI_DIR="$(cd "$SCRIPT_DIR/../cli" && pwd)"
UPDATE_SCRIPT="$CLI_DIR/loom-daemon-update.sh"

# Shared launchd sandbox (#4078). Belt-and-braces on top of LOOM_DAEMON_LAUNCHD=0:
#   - a scratch LOOM_LAUNCHD_LABEL so any launchd lookup that DID fire could not
#     resolve the operator's real com.rjwalters.loom-daemon job, and
#   - stub launchctl/pgrep installed onto the test PATH (below), so the real
#     tools are unreachable even if a future regression re-opens a launchd/pgrep
#     path in the stop script the restart flow drives.
# This closes the exact hole that booted out the operator's live daemon: the
# update suite exercises the REAL start/stop scripts, and launchd is
# machine-global.
# shellcheck source=lib/launchd-sandbox.sh
source "$SCRIPT_DIR/lib/launchd-sandbox.sh"
export LOOM_LAUNCHD_LABEL="$(launchd_sandbox_new_label)"

# Background-PID bookkeeping (#4773): every fake-daemon/decoy process this
# suite backgrounds is tracked here so the EXIT/INT/TERM trap below can reap
# it even if the parent test process is interrupted before its own inline
# cleanup runs.
# shellcheck source=lib/bg-proc-trap.sh
source "$SCRIPT_DIR/lib/bg-proc-trap.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

assert_eq() {
    local expected="$1" actual="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$expected" == "$actual" ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "${GREEN}✓${NC} $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "${RED}✗${NC} $msg"
        echo -e "  expected: [$expected]"
        echo -e "  actual:   [$actual]"
    fi
}

assert_true() {
    local cond="$1" msg="$2"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$cond" == "true" ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "${GREEN}✓${NC} $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "${RED}✗${NC} $msg"
    fi
}

# Repo root of the actual Loom checkout this test suite lives in, so fixtures
# can pull in the real scripts/install/provision-daemon.sh (which defines the
# #4016 sign_daemon_binary helper loom-daemon-update.sh sources at
# $REPO_ROOT/scripts/install/provision-daemon.sh).
LOOM_REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

# ---------- fixture builder ----------
# Sets up a fresh throwaway repo root at $1 with a real git HEAD, a stub
# loom-daemon crate, real start/stop scripts, a real copy of
# provision-daemon.sh (so the #4016 signing step is exercised, not silently
# skipped as "not found/sourceable"), and a minimal, machine-agnostic PATH
# (excludes ~/.local/bin and similar, so a real loom-daemon possibly
# installed on the dev machine can never leak into a test).
new_fixture() {
    local root="$1"
    mkdir -p "$root/.loom/logs" "$root/.loom/scripts/cli" "$root/.loom/scripts/lib" "$root/loom-daemon" "$root/scripts/install"
    cp "$CLI_DIR/loom-daemon-start.sh" "$root/.loom/scripts/cli/loom-daemon-start.sh"
    cp "$CLI_DIR/loom-daemon-stop.sh" "$root/.loom/scripts/cli/loom-daemon-stop.sh"
    chmod +x "$root/.loom/scripts/cli/"*.sh
    # The fixture start/stop scripts source ../lib/launchd-domain.sh for the
    # shared gui/<uid> ↦ user/<uid> resolver (#4130), so it must exist alongside
    # them in the throwaway tree — else a launchd-mode restart path would find no
    # resolve_launchd_domain. Mirrors the real defaults/scripts/lib layout.
    cp "$CLI_DIR/../lib/launchd-domain.sh" "$root/.loom/scripts/lib/launchd-domain.sh"
    # Same for lib/systemd-user.sh (#4268): the fixture start script's systemd
    # --user path (invoked via perform_systemd_relaunch's call to $START_SCRIPT,
    # #4260 sub-issue C) sources it relative to ITS OWN location, so it must exist
    # alongside the fixture copy too, not just in the real repo tree.
    cp "$CLI_DIR/../lib/systemd-user.sh" "$root/.loom/scripts/lib/systemd-user.sh"
    # Same for lib/bounded-run.sh (#4799): the fixture start script's
    # print_calibrate_hint() sources it relative to ITS OWN location to bound
    # its `calibrate` command substitution, so it must exist alongside the
    # fixture copy too.
    cp "$CLI_DIR/../lib/bounded-run.sh" "$root/.loom/scripts/lib/bounded-run.sh"
    # Same for lib/locate-daemon-bin.sh (#4875): the fixture start script
    # sources it relative to ITS OWN location to resolve the daemon binary
    # under a minimal PATH, so every fixture flow that execs the copied
    # loom-daemon-start.sh (restart, --relaunch, the full update run) needs it
    # in the throwaway tree. Without it those flows abort with
    # "locate-daemon-bin.sh not found at <fixture>/.loom/scripts/lib" before
    # reaching the behaviour under test.
    cp "$CLI_DIR/../lib/locate-daemon-bin.sh" "$root/.loom/scripts/lib/locate-daemon-bin.sh"
    cp "$LOOM_REPO_ROOT/scripts/install/provision-daemon.sh" "$root/scripts/install/provision-daemon.sh"
    cat > "$root/loom-daemon/Cargo.toml" <<'EOF'
[package]
name = "loom-daemon"
version = "0.0.0"
EOF
    ( cd "$root" && git init -q && git -c user.email=test@test -c user.name=test commit -q --allow-empty -m init )
}

# new_fixture_with_origin <root> <bare_dir> (#4330) — builds on new_fixture(),
# adding a local BARE repo as `origin` so the ff-first sync path (which
# resolves the default branch via refs/remotes/origin/HEAD, then fetches and
# compares against origin/<branch>) has a real remote to talk to — entirely
# offline (a plain filesystem path, no network). Forces the branch name to
# `main` (deterministic regardless of the test host's init.defaultBranch) and
# sets refs/remotes/origin/HEAD via `git remote set-head origin -a` so
# loom_default_branch() resolves it the same way a real clone would.
new_fixture_with_origin() {
    local root="$1" bare="$2"
    new_fixture "$root"
    ( cd "$root" && git branch -q -M main )
    git init -q --bare "$bare"
    ( cd "$root" && git remote add origin "$bare" && git push -q origin HEAD:refs/heads/main )
    git -C "$bare" symbolic-ref HEAD refs/heads/main
    ( cd "$root" && git remote set-head origin -a >/dev/null 2>&1 )
}

# push_extra_commits_to_origin <bare_dir> <n> — advances the bare `origin`
# repo `n` commits ahead of whatever a fixture repo's `main` currently is, by
# cloning it into a throwaway dir, committing there, and pushing back. Echoes
# the new origin tip's short commit on stdout. The fixture repo passed to
# new_fixture_with_origin is left untouched (still behind by exactly <n>).
push_extra_commits_to_origin() {
    local bare="$1" n="$2" tmpclone
    tmpclone="$(mktemp -d)"
    git clone -q "$bare" "$tmpclone"
    local i
    for i in $(seq 1 "$n"); do
        ( cd "$tmpclone" && git -c user.email=test@test -c user.name=test commit -q --allow-empty -m "extra ${i}" )
    done
    ( cd "$tmpclone" && git push -q origin HEAD:refs/heads/main )
    local tip
    tip="$(cd "$tmpclone" && git rev-parse --short HEAD)"
    rm -rf "$tmpclone"
    echo "$tip"
}

# Writes a fake daemon binary at $1 that reports commit $2 on --version and,
# on a normal run, appends its inherited LOOM_WORK_FINDER / LOOM_MAIN_HEALTH_GATE
# to marker file $3 before looping forever (so it stays alive for kill -0).
#
# `calibrate` is handled explicitly (#4799) and exits immediately with no
# output: this fixture has no real calibrate implementation, and every
# successful loom-daemon-start.sh run (nohup/launchd/systemd, all three
# reached by this suite's restart scenarios) calls `$DAEMON_BIN calibrate
# --workspace ... --json` via print_calibrate_hint(). Before this fix, that
# call fell through to the `while true` loop below and hung forever inside
# print_calibrate_hint()'s blocking `$(...)` -- the exact hang
# ci-excluded.txt documented. print_calibrate_hint() is bounded independently
# now (lib/bounded-run.sh), but this fixture also short-circuits so the suite
# stays fast rather than eating that timeout on every restart.
#
# GENERALIZED (#4799 CI hang): `calibrate` was only one instance of a whole
# CLASS of wedge. ANY *subcommand* the lifecycle scripts dispatch that this
# fixture does not recognize used to fall through to the `while true` daemon
# body IN THE FOREGROUND and block its caller forever -- which is precisely how
# the CI run hung: with a real `systemctl --user` reachable, the update script
# resolved DAEMON_MANAGER=systemd and ran `"$PROVISION_TARGET" restart`, and
# this fixture (no `restart` handler) looped instead of answering. So the
# catch-all below exits non-zero for any unrecognized NON-FLAG first argument
# (a real daemon rejects an unknown subcommand; it does not daemonize). A
# leading `-`/`--` still falls through to the daemon body, because the
# supervisors DO launch the daemon proper with flags.
write_fake_daemon() {
    local path="$1" commit="$2" marker="$3"
    cat > "$path" <<EOF
#!/usr/bin/env bash
if [[ "\${1:-}" == "--version" ]]; then
    echo "loom-daemon 0.15.0 (commit ${commit}, built 2026-07-26T00:00:00Z)"
    exit 0
fi
if [[ "\${1:-}" == "calibrate" ]]; then
    exit 1
fi
if [[ -n "\${1:-}" && "\${1:-}" != -* ]]; then
    echo "fake loom-daemon: unsupported subcommand: \$*" >&2
    exit 1
fi
echo "FAKE_DAEMON WF=[\${LOOM_WORK_FINDER:-}] HG=[\${LOOM_MAIN_HEALTH_GATE:-}]" > "${marker}"
while true; do sleep 1; done
EOF
    chmod +x "$path"
}

# Writes a fake daemon binary at $1 that reports commit $2 on --version, and on a
# `restart` subcommand (the #4077 supervised primitive, #4042) appends a line to
# marker file $3 and exits with code $4 — so a launchd-managed update test can
# assert the update drove `restart` (NOT stop+start) and control whether the
# request "succeeds" (0, supervised) or is "refused" (non-zero, pre-#4077 binary).
# On a normal run it loops forever (kept alive for any liveness check).
write_fake_daemon_restart() {
    local path="$1" commit="$2" restart_marker="$3" restart_rc="$4"
    cat > "$path" <<EOF
#!/usr/bin/env bash
if [[ "\${1:-}" == "--version" ]]; then
    echo "loom-daemon 0.15.0 (commit ${commit}, built 2026-07-26T00:00:00Z)"
    exit 0
fi
if [[ "\${1:-}" == "restart" ]]; then
    echo "restart" >> "${restart_marker}"
    exit ${restart_rc}
fi
# See write_fake_daemon's comment (#4799): no real calibrate implementation,
# exit fast instead of falling into the daemon-body loop below.
if [[ "\${1:-}" == "calibrate" ]]; then
    exit 1
fi
# Same catch-all as write_fake_daemon (#4799): an unrecognized NON-FLAG
# subcommand must never fall into the foreground daemon loop and wedge its
# caller. Flags still reach the daemon body.
if [[ -n "\${1:-}" && "\${1:-}" != -* ]]; then
    echo "fake loom-daemon: unsupported subcommand: \$*" >&2
    exit 1
fi
while true; do sleep 1; done
EOF
    chmod +x "$path"
}

# Writes a fake `launchctl` at $1 that logs invocations to $2 and, on
# `launchctl print`, reports a LOADED job with a live-looking pid (exit 0) —
# simulating a launchd-managed loom-daemon for the #4042 ownership-detection
# tests. Paired with a fake `uname`->Darwin so the update script's Darwin-gated
# launchd path fires deterministically on any host.
#
# Optional $3/$4 (#4232): when a <post_restart_marker> file path is given,
# `print` reports pid 4242 until that file EXISTS, at which point it reports
# <post_restart_pid> instead — simulating "launchd relaunched the job onto a
# new (real, live) pid the instant the restart request was accepted". Tests
# that don't pass $3/$4 keep the old static-4242-forever behavior (they never
# exercise the #4232 pid-verification poll).
write_fake_launchd_loaded_bin() {
    local bin_dir="$1" log="$2" post_restart_marker="${3:-}" post_restart_pid="${4:-}"
    mkdir -p "$bin_dir"
    : > "$log"
    cat > "$bin_dir/launchctl" <<EOF
#!/usr/bin/env bash
echo "\$*" >> "${log}"
case "\${1:-}" in
  print)
    if [[ -n "${post_restart_marker}" && -e "${post_restart_marker}" ]]; then
      echo "	pid = ${post_restart_pid}"
    else
      echo "	pid = 4242"
    fi
    exit 0 ;;
  *)     exit 0 ;;
esac
EOF
    chmod +x "$bin_dir/launchctl"
    cat > "$bin_dir/uname" <<'EOF'
#!/usr/bin/env bash
echo "Darwin"
EOF
    chmod +x "$bin_dir/uname"
}

# Writes a fake `launchctl` at $1 for the #4232 restart-VERIFICATION tests: the
# reported pid lives in a state file ($3) rather than being hardcoded, so a
# test can hold it "stuck" on the pre-restart pid to simulate launchd failing
# to relaunch on its own. `print` reports whatever pid (if any) is currently in
# the state file plus a "last exit status" line (diagnostic breadcrumb,
# #4232). `kickstart` is recorded VERBATIM to $2 (so a test can assert it was
# never invoked with `-k`) and, only when $4 (<kickstart_new_pid>) is given,
# overwrites the state file with it — simulating a kickstart-triggered
# relaunch; when $4 is omitted, `kickstart` is a no-op (a kickstart that also
# fails to bring the job up).
write_fake_launchd_pid_bin() {
    local bin_dir="$1" log="$2" state_file="$3" kickstart_new_pid="${4:-}"
    mkdir -p "$bin_dir"
    : > "$log"
    cat > "$bin_dir/launchctl" <<EOF
#!/usr/bin/env bash
echo "\$*" >> "${log}"
case "\${1:-}" in
  print)
    pid="\$(cat "${state_file}" 2>/dev/null)"
    if [[ -n "\$pid" ]]; then
      echo "	state = running"
      echo "	pid = \$pid"
    else
      echo "	state = not running"
    fi
    echo "	last exit status = 0"
    exit 0 ;;
  kickstart)
    if [[ -n "${kickstart_new_pid}" ]]; then
      echo "${kickstart_new_pid}" > "${state_file}"
    fi
    exit 0 ;;
  *) exit 0 ;;
esac
EOF
    chmod +x "$bin_dir/launchctl"
    cat > "$bin_dir/uname" <<'EOF'
#!/usr/bin/env bash
echo "Darwin"
EOF
    chmod +x "$bin_dir/uname"
}

# Writes a stale, PRE-#4077 LaunchAgent plist at $1 (the exact state that causes
# the refused-restart exit-6 fallback): KeepAlive=false, NO LOOM_DAEMON_SUPERVISOR
# key, and four autonomy env keys plus a SENTINEL PATH. The re-render on the
# --relaunch path (#4118) must (a) install KeepAlive:{SuccessfulExit:true} +
# LOOM_DAEMON_SUPERVISOR=launchd, (b) carry all four autonomy keys through
# unchanged, and (c) NOT round-trip the sentinel PATH (start.sh rebuilds PATH).
# Args: <plist_path> <label> <bin> <homedir>.
write_fixture_plist_pre4077() {
    local plist="$1" label="$2" bin="$3" homedir="$4"
    mkdir -p "$(dirname "$plist")"
    cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${bin}</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/opt/sentinel-oldpath-4118/bin:/usr/bin:/bin</string>
        <key>HOME</key>
        <string>${homedir}</string>
        <key>LOOM_WORK_FINDER</key>
        <string>1</string>
        <key>LOOM_MAIN_HEALTH_GATE</key>
        <string>1</string>
        <key>LOOM_WORK_FINDER_MAX_CONCURRENT</key>
        <string>10</string>
        <key>LOOM_PER_TOKEN_CONCURRENCY</key>
        <string>5</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>${homedir}/daemon.log</string>
    <key>StandardErrorPath</key>
    <string>${homedir}/daemon.log</string>
</dict>
</plist>
EOF
}

# Writes a stub `systemctl` at $1/systemctl that logs invocations to $2 and
# reports an ACTIVE unit with MainPID $3 — the systemd analog of
# write_fake_launchd_loaded_bin, used with the LOOM_SYSTEMD_FORCE=1 test seam
# (lib/systemd-user.sh) since this suite runs on Darwin runners. Structurally
# unable to touch a real systemd --user manager.
write_fake_systemd_active_bin() {
    local bin_dir="$1" log="$2" mainpid="$3"
    mkdir -p "$bin_dir"
    : > "$log"
    cat > "$bin_dir/systemctl" <<EOF
#!/usr/bin/env bash
echo "\$*" >> "${log}"
if [[ "\${1:-}" == "--user" ]]; then shift; fi
case "\${1:-}" in
  is-active)  exit 0 ;;
  is-enabled) exit 0 ;;
  show)       echo "${mainpid}" ;;
  *)          exit 0 ;;
esac
EOF
    chmod +x "$bin_dir/systemctl"
}

# Writes a stub `systemctl` at $1/systemctl for the #4950 restart-VERIFICATION
# tests: `MainPID`/`ActiveState`/`Result` all live in a single state file ($3,
# colon-delimited "pid:activestate:result") rather than being hardcoded, so a
# test can drive the exact transitions the update script's #4950 poll +
# self-heal logic reacts to. Structurally unable to touch a real systemd
# --user manager — this suite runs on Darwin runners (LOOM_SYSTEMD_FORCE=1).
#
# Args: <bin_dir> <log> <state_file> <pre_state> [post_restart_marker]
#       [post_state] [recovery_state]
#
#   <pre_state>            the initial "pid:activestate:result" written to
#                           <state_file> on first invocation if it does not
#                           already exist (e.g. "4242:active:success").
#   <post_restart_marker>  (optional) when this file EXISTS, the state auto-
#                           advances from <pre_state> to <post_state> exactly
#                           once — simulating "the instant the restart request
#                           was accepted, systemd's own transition took
#                           effect" (paired with write_fake_daemon_restart,
#                           which creates the marker as a side effect of its
#                           `restart` subcommand).
#   <post_state>            the state to advance to once the marker exists
#                           (e.g. "<new-live-pid>:active:success" to simulate
#                           an immediate spontaneous relaunch, or
#                           "0:failed:timeout" to simulate the #4950 incident
#                           shape — a stop-timeout escalation that
#                           Restart=on-success never fires for).
#   <recovery_state>        (optional) the state `start` (as invoked by the
#                           update script's `systemctl --user reset-failed
#                           <unit> && systemctl --user start <unit>` self-heal)
#                           advances to. Omit to simulate a self-heal attempt
#                           that ALSO fails to bring the unit up (the state
#                           stays whatever `reset-failed` left it at:
#                           "<pid>:inactive:success").
write_fake_systemd_pid_bin() {
    local bin_dir="$1" log="$2" state_file="$3" pre_state="$4"
    local post_restart_marker="${5:-}" post_state="${6:-}" recovery_state="${7:-}"
    mkdir -p "$bin_dir"
    : > "$log"
    echo "${pre_state}" > "${state_file}"
    cat > "$bin_dir/systemctl" <<EOF
#!/usr/bin/env bash
echo "\$*" >> "${log}"
if [[ "\${1:-}" == "--user" ]]; then shift; fi

state_file="${state_file}"
cur="\$(cat "\$state_file" 2>/dev/null)"
if [[ -n "${post_restart_marker}" && -e "${post_restart_marker}" && "\$cur" == "${pre_state}" ]]; then
    echo "${post_state}" > "\$state_file"
    cur="${post_state}"
fi
IFS=: read -r cur_pid cur_active cur_result <<< "\$cur"

case "\${1:-}" in
  is-active)
    [[ "\$cur_active" == "active" ]] && exit 0 || exit 1 ;;
  is-enabled)
    exit 0 ;;
  show)
    prop=""
    for a in "\$@"; do
      case "\$a" in
        MainPID) prop=MainPID ;;
        ActiveState) prop=ActiveState ;;
        Result) prop=Result ;;
      esac
    done
    case "\$prop" in
      MainPID)     echo "\$cur_pid" ;;
      ActiveState) echo "\$cur_active" ;;
      Result)      echo "\$cur_result" ;;
      *)           echo "" ;;
    esac
    ;;
  status)
    echo "● loom-daemon.service - Loom autonomous daemon (loom-daemon)"
    echo "   Loaded: loaded"
    echo "   Active: \$cur_active (Result: \$cur_result) since Mon 2026-08-02 03:17:36 UTC"
    echo "Main PID: \$cur_pid"
    ;;
  reset-failed)
    echo "\$cur_pid:inactive:success" > "\$state_file"
    ;;
  start)
    if [[ -n "${recovery_state}" ]]; then
      echo "${recovery_state}" > "\$state_file"
    fi
    ;;
  *) exit 0 ;;
esac
EOF
    chmod +x "$bin_dir/systemctl"
}

# Writes a stale, PRE-#4267 systemd --user unit at $1 (the exact state that
# causes the refused-restart exit-6 fallback): NO Restart=on-success, NO
# LOOM_DAEMON_SUPERVISOR key, four autonomy Environment= lines, and a SENTINEL
# PATH. The re-render on the --relaunch path must (a) install Restart=on-success
# + LOOM_DAEMON_SUPERVISOR=systemd, (b) carry all four autonomy keys through
# unchanged, and (c) NOT round-trip the sentinel PATH (start.sh rebuilds PATH).
# Args: <unit_path> <bin>.
write_fixture_unit_pre4267() {
    local unit_path="$1" bin="$2"
    mkdir -p "$(dirname "$unit_path")"
    cat > "$unit_path" <<EOF
[Unit]
Description=Loom autonomous daemon (loom-daemon)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=/tmp
ExecStart=${bin}
Environment=PATH=/opt/sentinel-oldpath-4267/bin:/usr/bin:/bin
Environment=HOME=/tmp
Environment=LOOM_WORK_FINDER=1
Environment=LOOM_MAIN_HEALTH_GATE=1
Environment=LOOM_WORK_FINDER_MAX_CONCURRENT=10
Environment=LOOM_PER_TOKEN_CONCURRENCY=5
StandardOutput=append:/tmp/daemon.log
StandardError=append:/tmp/daemon.log

[Install]
WantedBy=default.target
EOF
}

# Writes a fake `cargo` that, on `cargo build --release` (cwd = loom-daemon/),
# copies $NEW_FAKE_BIN_SRC into target/release/loom-daemon instead of
# compiling. Tests export NEW_FAKE_BIN_SRC before invoking loom-daemon-update.sh.
write_fake_cargo() {
    local path="$1"
    cat > "$path" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == "build" ]]; then
    mkdir -p target/release
    cp "$NEW_FAKE_BIN_SRC" target/release/loom-daemon
    chmod +x target/release/loom-daemon
    echo "[fake cargo] build ok"
    exit 0
fi
echo "[fake cargo] unsupported subcommand: $*" >&2
exit 1
EOF
    chmod +x "$path"
}

# Fake `crontab` (#4697): the update script's idle-shutdown-notice check runs
# `crontab -l` unconditionally on every invocation, so without this stub every
# test in this suite would shell out to the REAL system `crontab` for
# whichever account runs the suite — reading (never writing, but still a real
# information leak/hang risk on a host with no cron daemon configured) actual
# operator state, exactly the class of hazard the launchd/systemd sandboxing
# above exists to prevent. `-l` echoes the contents of
# $FAKE_CRONTAB_CONTENTS_FILE when set+readable, else exits 1 with no output
# (mirrors the common "no crontab for this user" case — silence, not an
# error the caller need alarm on). Any other invocation is a no-op success.
write_fake_crontab() {
    local path="$1"
    cat > "$path" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == "-l" ]]; then
    if [[ -n "${FAKE_CRONTAB_CONTENTS_FILE:-}" && -r "${FAKE_CRONTAB_CONTENTS_FILE:-}" ]]; then
        cat "$FAKE_CRONTAB_CONTENTS_FILE"
        exit 0
    fi
    exit 1
fi
exit 0
EOF
    chmod +x "$path"
}

MINIMAL_PATH="/usr/bin:/bin:/usr/sbin:/sbin"

BASE_WORKDIR="$(mktemp -d)"

# #4011: isolate the autonomy-desired marker + watchdog label suite-wide so a
# restart path that reaches the real loom-daemon-start.sh can never write the
# operator's real ~/.loom/autonomy-desired or provision the real
# com.rjwalters.loom-daemon-watchdog LaunchAgent. Both are exported so every
# sub-invocation (each cd'd into its own W* dir) inherits them.
export LOOM_AUTONOMY_MARKER="$BASE_WORKDIR/autonomy-desired"
export LOOM_WATCHDOG_LABEL="${LOOM_LAUNCHD_LABEL}-watchdog"

# Machine-level provisioning sandbox (#4381 incident — see the checksum-guard
# comment near the top of this file for the full writeup). loom-daemon-update.sh
# resolves its machine-level install destination as
# `${LOOM_DAEMON_BIN_DIR:-$HOME/.local/bin}` whenever LOOM_DAEMON_BIN is unset.
# Most tests below pin LOOM_DAEMON_BIN explicitly (so this is inert for them),
# but the ff-sync/staleness-detection tests (37/39/43) deliberately exercise the
# no-LOOM_DAEMON_BIN path, and until this fix that meant an UNSANDBOXED
# provision_machine_daemon() call landed on the operator's real
# ~/.local/bin/loom-daemon. Exporting this suite-wide closes the hole for every
# current call site AND any future one that forgets to set LOOM_DAEMON_BIN —
# belt-and-braces with the checksum guard at both the top and bottom of this
# file, which would otherwise be the only thing catching a regression.
#
# `unset LOOM_DAEMON_BIN` closes the other half of the hole (#4902): every
# live Loom agent session (Builder/Judge/Doctor, ...) inherits an ambient
# LOOM_DAEMON_BIN pointing at the real production binary, and
# loom-daemon-update.sh's `PROVISION_TARGET="${LOOM_DAEMON_BIN:-$DEST_DIR/...}"`
# lets that ambient value win over the LOOM_DAEMON_BIN_DIR sandbox above,
# silently defeating it. Tests below that need LOOM_DAEMON_BIN pin it inline
# on their own invocation (e.g. `LOOM_DAEMON_BIN="$W1/..." bash ...`), which
# still applies regardless of this ambient unset.
export LOOM_DAEMON_BIN_DIR="$BASE_WORKDIR/machine-level-bin-sandbox"
unset LOOM_DAEMON_BIN

# Binary-format sanity gate bypass (#4397, deferred from #4381's incident
# review): provision_machine_daemon now refuses to install anything that
# isn't a real compiled binary (Mach-O/ELF — see _pmd_is_real_binary in
# scripts/install/provision-daemon.sh). EVERY fake daemon binary this suite
# writes (write_fake_daemon et al.) is a bash script standing in for the real
# compiled binary, so THIS SUITE — and only this suite — sets the explicit,
# auditable bypass suite-wide. Production callers (scripts/install-loom.sh,
# defaults/scripts/cli/loom-daemon-update.sh) never set it; the gate itself is
# exercised directly (both the reject-a-script and accept-a-real-binary
# cases) by tests/install/test-provision-daemon.sh.
export LOOM_PROVISION_ALLOW_SCRIPT=1

# Suite-level decoy (#4078): a process whose argv ends in `/loom-daemon`, which
# the stop script's label-blind `pgrep -f '(^|/)loom-daemon$'` fallback would
# match. The whole update suite runs under a scratch LOOM_LAUNCHD_LABEL and
# LOOM_DAEMON_LAUNCHD=0, so no real launchd lookup or by-name kill may fire; if
# any test regresses into one, this decoy dies and the final assertion fails.
# Spawned OUTSIDE $BASE_WORKDIR's path so the trap's `pkill -f "$BASE_WORKDIR"`
# does not sweep it; killed explicitly below.
DECOY_DIR="$(mktemp -d)"
cat > "$DECOY_DIR/loom-daemon" <<'EOF'
#!/usr/bin/env bash
while true; do sleep 1; done
EOF
chmod +x "$DECOY_DIR/loom-daemon"
# Redirect stdio to /dev/null so the never-exiting decoy cannot hold open a
# captured stdout pipe of the suite (which would block a caller capturing its
# output — the same command-substitution gotcha the sandbox spawner avoids).
"$DECOY_DIR/loom-daemon" >/dev/null 2>&1 &
DECOY_PID=$!
bg_proc_track "$DECOY_PID"

# Best-effort cleanup of any fake-daemon processes left running (matched by
# their script path under $BASE_WORKDIR, which appears in `ps`'s command
# line) — individual tests also kill their own PIDs explicitly, this is a
# backstop for anything a failed assertion left behind. bg_proc_reap kills
# every PID tracked via bg_proc_track (the W5/W15/W26-style sandbox fixtures
# below track their own spawned PIDs directly rather than relying solely on
# the pkill pattern-match); EXIT/INT/TERM (not just EXIT, #4773) so a hard
# interruption of this suite still reaps every tracked/backstopped process.
# NOTE: a bare `trap CMD EXIT INT TERM` runs CMD on INT/TERM but does NOT stop
# the script (only an EXIT-trap firing auto-exits) -- the explicit `exit`
# below is required, else a SIGTERM'd suite would clean up once and then keep
# running every remaining (numbered W*) test case, re-populating
# $BASE_WORKDIR as it goes (observed directly while verifying this fix: an
# untrapped-exit version of this trap let a SIGTERM'd run limp all the way to
# a later scenario, which then hit a real, un-stubbed `cargo build --release`
# once its own fixture dir had already been rm -rf'd out from under it).
trap 'bg_proc_reap; [ -n "$BASE_WORKDIR" ] && pkill -f "$BASE_WORKDIR" >/dev/null 2>&1; rm -rf "$BASE_WORKDIR" "$DECOY_DIR"' EXIT
trap 'bg_proc_reap; [ -n "$BASE_WORKDIR" ] && pkill -f "$BASE_WORKDIR" >/dev/null 2>&1; rm -rf "$BASE_WORKDIR" "$DECOY_DIR"; exit 1' INT TERM

FAKE_BIN_DIR="$BASE_WORKDIR/fakebin"
mkdir -p "$FAKE_BIN_DIR"
write_fake_cargo "$FAKE_BIN_DIR/cargo"
write_fake_crontab "$FAKE_BIN_DIR/crontab"
# Stub launchctl/pgrep onto the front of every test PATH (FAKE_BIN_DIR is the
# first entry of TEST_PATH and TEST_PATH_NO_CODESIGN), recording invocations to
# $SANDBOX_LOG_DIR so the suite can assert no production label was ever named.
SANDBOX_LOG_DIR="$BASE_WORKDIR/sandbox-log"
launchd_sandbox_install_stubs "$FAKE_BIN_DIR" "$SANDBOX_LOG_DIR"
TEST_PATH="$FAKE_BIN_DIR:$MINIMAL_PATH"

# A copy of /usr/bin with every entry EXCEPT `codesign` symlinked in, used by
# the #4016 "codesign absent from PATH" test below. Built once (symlinking is
# effectively instant) rather than per-test.
NO_CODESIGN_DIR="$BASE_WORKDIR/no-codesign-usr-bin"
mkdir -p "$NO_CODESIGN_DIR"
for f in /usr/bin/*; do
    name="$(basename "$f")"
    [[ "$name" == "codesign" ]] && continue
    ln -sf "$f" "$NO_CODESIGN_DIR/$name" 2>/dev/null
done
TEST_PATH_NO_CODESIGN="$FAKE_BIN_DIR:$NO_CODESIGN_DIR:/bin:/usr/sbin:/sbin"

# ============================================================
# 1. --check reports "up to date" (exit 0) when the installed
#    binary's baked-in commit matches the source tree's HEAD.
# ============================================================
W1="$BASE_WORKDIR/w1"
new_fixture "$W1"
HEAD1="$(cd "$W1" && git rev-parse --short HEAD)"
write_fake_daemon "$W1/installed-loom-daemon" "$HEAD1" "$W1/marker"
out=$( cd "$W1" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$W1/installed-loom-daemon" \
    bash "$UPDATE_SCRIPT" --check; echo "EXIT=$?" )
rc=$(echo "$out" | grep -o 'EXIT=[0-9]*' | cut -d= -f2)
assert_eq "0" "$rc" "--check exits 0 when installed commit matches source HEAD"

# ============================================================
# 2. --check reports "update available" (exit 3) when the
#    installed binary's baked-in commit differs from source HEAD.
# ============================================================
W2="$BASE_WORKDIR/w2"
new_fixture "$W2"
write_fake_daemon "$W2/installed-loom-daemon" "deadbee" "$W2/marker"
out=$( cd "$W2" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$W2/installed-loom-daemon" \
    bash "$UPDATE_SCRIPT" --check; echo "EXIT=$?" )
rc=$(echo "$out" | grep -o 'EXIT=[0-9]*' | cut -d= -f2)
assert_eq "3" "$rc" "--check exits 3 when installed commit differs from source HEAD"

# ============================================================
# 3. --check makes NO writes (no rebuild, no provisioning).
# ============================================================
W3="$BASE_WORKDIR/w3"
new_fixture "$W3"
write_fake_daemon "$W3/installed-loom-daemon" "deadbee" "$W3/marker"
( cd "$W3" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$W3/installed-loom-daemon" \
    bash "$UPDATE_SCRIPT" --check >/dev/null 2>&1 )
if [[ -e "$W3/loom-daemon/target/release/loom-daemon" ]]; then
    assert_eq "no-build-dir" "build-dir-present" "--check performs no rebuild"
else
    assert_eq "true" "true" "--check performs no rebuild"
fi

# ============================================================
# 4. --dry-run reports the plan but makes no writes either.
# ============================================================
W4="$BASE_WORKDIR/w4"
new_fixture "$W4"
write_fake_daemon "$W4/installed-loom-daemon" "deadbee" "$W4/marker"
dryrun_out=$( cd "$W4" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$W4/installed-loom-daemon" \
    bash "$UPDATE_SCRIPT" --dry-run 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if [[ -e "$W4/loom-daemon/target/release/loom-daemon" ]]; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --dry-run performs no rebuild"
elif echo "$dryrun_out" | grep -q 'dry-run.*cargo build'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --dry-run performs no rebuild and prints the plan"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --dry-run performs no rebuild and prints the plan"
    echo "  output: $dryrun_out"
fi

# ============================================================
# 5. Full flow: stale binary + a daemon that WAS running with
#    persisted flags (--work-finder) -> rebuild, provision,
#    restart with EXACTLY the persisted flags (never widened).
# ============================================================
W5="$BASE_WORKDIR/w5"
new_fixture "$W5"
HEAD5="$(cd "$W5" && git rev-parse --short HEAD)"
INSTALLED5="$W5/installed/loom-daemon"
mkdir -p "$W5/installed"
write_fake_daemon "$INSTALLED5" "deadbee" "$W5/old-marker"
NEW_FAKE5="$W5/new-fake-daemon"
write_fake_daemon "$NEW_FAKE5" "$HEAD5" "$W5/new-marker"

# Persisted flags from a prior `loom-daemon-start.sh --work-finder` (Issue #3968).
echo "--work-finder" > "$W5/.loom/.daemon.flags"

# Simulate an already-running old daemon: background the OLD fake binary and
# record its PID, exactly like loom-daemon-start.sh would have.
"$INSTALLED5" >/dev/null 2>&1 &
old_pid=$!
bg_proc_track "$old_pid"
sleep 0.3
echo "$old_pid" > "$W5/.loom/.daemon.pid"
TESTS_RUN=$((TESTS_RUN + 1))
if kill -0 "$old_pid" 2>/dev/null; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} fixture: old daemon process is alive before update"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} fixture: old daemon process is alive before update"
fi

( cd "$W5" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$INSTALLED5" NEW_FAKE_BIN_SRC="$NEW_FAKE5" \
    env -u LOOM_WORK_FINDER -u LOOM_MAIN_HEALTH_GATE \
    bash "$UPDATE_SCRIPT" >"$W5/update.log" 2>&1 )
update_rc=$?
assert_eq "0" "$update_rc" "full update run (rebuild+provision+restart) exits 0"

TESTS_RUN=$((TESTS_RUN + 1))
if kill -0 "$old_pid" 2>/dev/null; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} old daemon process was stopped by the restart"
else
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} old daemon process was stopped by the restart"
fi

# Wait briefly for the new (restarted) daemon to write its marker.
for _ in 1 2 3 4 5 6 7 8 9 10; do
    [[ -s "$W5/new-marker" ]] && break
    sleep 0.3
done
new_marker_content="$(cat "$W5/new-marker" 2>/dev/null || echo "<missing>")"
assert_eq "FAKE_DAEMON WF=[1] HG=[0]" "$new_marker_content" \
    "restarted daemon inherited EXACTLY the persisted --work-finder flag (never widened to health-gate too)"

TESTS_RUN=$((TESTS_RUN + 1))
if [[ -x "$INSTALLED5" ]] && "$INSTALLED5" --version 2>/dev/null | grep -q "$HEAD5"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} LOOM_DAEMON_BIN path was provisioned with the freshly-built binary"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} LOOM_DAEMON_BIN path was provisioned with the freshly-built binary"
fi

# Happy-path roll must self-verify both the built binary and the destination
# (#4053): the short-circuit can no longer produce a silent no-op on a real roll.
TESTS_RUN=$((TESTS_RUN + 1))
if grep -qi 'Build verification' "$W5/update.log" \
    && grep -qi 'Post-provision verification' "$W5/update.log"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} happy-path roll self-verifies built commit AND destination binary"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} happy-path roll self-verifies built commit AND destination binary"
    echo "  update.log: $(cat "$W5/update.log" 2>/dev/null)"
fi

# Clean up the restarted daemon (find it via the PID file this run wrote).
if [[ -f "$W5/.loom/.daemon.pid" ]]; then
    kill "$(cat "$W5/.loom/.daemon.pid" 2>/dev/null)" 2>/dev/null || true
fi

# ============================================================
# 5b. Regression: an EMPTY persisted-flags file (a prior bare/FLAGS-OFF
#     start) must restart cleanly with zero flags — no unbound-variable
#     crash from `"${RESTART_ARGS[@]}"` on a zero-element array under
#     `set -u` (bash < 4.4, still /bin/bash on stock macOS).
# ============================================================
W5B="$BASE_WORKDIR/w5b"
new_fixture "$W5B"
HEAD5B="$(cd "$W5B" && git rev-parse --short HEAD)"
INSTALLED5B="$W5B/installed/loom-daemon"
mkdir -p "$W5B/installed"
write_fake_daemon "$INSTALLED5B" "deadbee" "$W5B/old-marker"
NEW_FAKE5B="$W5B/new-fake-daemon"
write_fake_daemon "$NEW_FAKE5B" "$HEAD5B" "$W5B/new-marker"
: > "$W5B/.loom/.daemon.flags" # empty, mirroring a bare prior start

"$INSTALLED5B" >/dev/null 2>&1 &
old_pid5b=$!
bg_proc_track "$old_pid5b"
sleep 0.3
echo "$old_pid5b" > "$W5B/.loom/.daemon.pid"

# Capture to a log rather than /dev/null (#4799): when this scenario wedged in
# CI the tail of the suite output was undiagnostic precisely because its update
# run wrote nowhere, so the failure surfaced as silence. Mirror scenario 5.
( cd "$W5B" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$INSTALLED5B" NEW_FAKE_BIN_SRC="$NEW_FAKE5B" \
    env -u LOOM_WORK_FINDER -u LOOM_MAIN_HEALTH_GATE \
    bash "$UPDATE_SCRIPT" >"$W5B/update.log" 2>&1 )
update5b_rc=$?
if [[ "$update5b_rc" != "0" ]]; then
    echo "  update.log (5b): $(tail -n 20 "$W5B/update.log" 2>/dev/null)"
fi
assert_eq "0" "$update5b_rc" "restart with an EMPTY persisted-flags file exits 0 (#3968 regression)"

for _ in 1 2 3 4 5 6 7 8 9 10; do
    [[ -s "$W5B/new-marker" ]] && break
    sleep 0.3
done
new5b_marker_content="$(cat "$W5B/new-marker" 2>/dev/null || echo "<missing>")"
# A plain (zero-flag) restart forwards no CLI args, so loom-daemon-start.sh's
# own FLAGS-OFF default applies: LOOM_WORK_FINDER=0, LOOM_MAIN_HEALTH_GATE=0.
assert_eq "FAKE_DAEMON WF=[0] HG=[0]" "$new5b_marker_content" \
    "restarted daemon with empty persisted flags runs plain FLAGS-OFF (no args forwarded)"

if [[ -f "$W5B/.loom/.daemon.pid" ]]; then
    kill "$(cat "$W5B/.loom/.daemon.pid" 2>/dev/null)" 2>/dev/null || true
fi

# ============================================================
# 5c. Non-interactive-SSH cargo fallback (#4695): cargo is absent from PATH
#     entirely (the exact non-login-shell-SSH symptom), but present at
#     rustup's default $HOME/.cargo/bin -- the update must still find it via
#     the fallback and rebuild successfully.
# ============================================================
W5C="$BASE_WORKDIR/w5c"
new_fixture "$W5C"
HEAD5C="$(cd "$W5C" && git rev-parse --short HEAD)"
INSTALLED5C="$W5C/installed/loom-daemon"
mkdir -p "$W5C/installed"
write_fake_daemon "$INSTALLED5C" "deadbee" "$W5C/old-marker"
NEW_FAKE5C="$W5C/new-fake-daemon"
write_fake_daemon "$NEW_FAKE5C" "$HEAD5C" "$W5C/new-marker"
# No .loom/.daemon.pid -> WAS_RUNNING resolves false, so this test exercises
# only the rebuild+provision path (the cargo-fallback code under test)
# without needing a background process to restart.

# A PATH carrying the same launchd-sandbox stubs (launchctl/pgrep) as every
# other test, but deliberately WITHOUT cargo anywhere on it.
NO_CARGO_BIN_DIR5C="$BASE_WORKDIR/w5c-no-cargo-bin"
NO_CARGO_LOG_DIR5C="$BASE_WORKDIR/w5c-no-cargo-log"
launchd_sandbox_install_stubs "$NO_CARGO_BIN_DIR5C" "$NO_CARGO_LOG_DIR5C"
TEST_PATH_NO_CARGO5C="$NO_CARGO_BIN_DIR5C:$MINIMAL_PATH"

# The fake cargo lives ONLY under a scratch $HOME's rustup-default location,
# simulating a non-interactive SSH shell that never sourced the profile line
# rustup's installer adds.
HOME5C="$BASE_WORKDIR/w5c-home"
mkdir -p "$HOME5C/.cargo/bin"
write_fake_cargo "$HOME5C/.cargo/bin/cargo"

out5c=$( cd "$W5C" && PATH="$TEST_PATH_NO_CARGO5C" HOME="$HOME5C" \
    LOOM_DAEMON_BIN="$INSTALLED5C" NEW_FAKE_BIN_SRC="$NEW_FAKE5C" \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc5c=$?
assert_eq "0" "$rc5c" "update succeeds when cargo is absent from PATH but present at \$HOME/.cargo/bin (#4695)"

TESTS_RUN=$((TESTS_RUN + 1))
if [[ -x "$INSTALLED5C" ]] && "$INSTALLED5C" --version 2>/dev/null | grep -q "$HEAD5C"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} \$HOME/.cargo/bin fallback cargo actually rebuilt+provisioned the fresh binary"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} \$HOME/.cargo/bin fallback cargo actually rebuilt+provisioned the fresh binary"
    echo "  output: $out5c"
fi

# ============================================================
# 5d. cargo genuinely absent (not on PATH, not under $HOME/.cargo/bin either)
#     -> exit 1 with a failure message that suggests installing via rustup,
#     not just "not found" (#4695 AC).
# ============================================================
W5D="$BASE_WORKDIR/w5d"
new_fixture "$W5D"
HOME5D="$BASE_WORKDIR/w5d-home"
mkdir -p "$HOME5D" # deliberately no .cargo/bin at all
NO_CARGO_BIN_DIR5D="$BASE_WORKDIR/w5d-no-cargo-bin"
NO_CARGO_LOG_DIR5D="$BASE_WORKDIR/w5d-no-cargo-log"
launchd_sandbox_install_stubs "$NO_CARGO_BIN_DIR5D" "$NO_CARGO_LOG_DIR5D"
TEST_PATH_NO_CARGO5D="$NO_CARGO_BIN_DIR5D:$MINIMAL_PATH"

out5d=$( cd "$W5D" && PATH="$TEST_PATH_NO_CARGO5D" HOME="$HOME5D" \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc5d=$?
assert_eq "1" "$rc5d" "update exits 1 when cargo is genuinely absent (no PATH, no \$HOME/.cargo/bin)"

TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out5d" | grep -qi 'rustup'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} genuinely-absent-cargo error message suggests installing via rustup"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} genuinely-absent-cargo error message suggests installing via rustup"
    echo "  output: $out5d"
fi

# ============================================================
# 6. Stale binary but daemon was NOT running -> rebuild+provision
#    happen, but the daemon is NOT started (never widen FLAGS-OFF
#    by starting autonomy — or anything — that wasn't already up).
# ============================================================
W6="$BASE_WORKDIR/w6"
new_fixture "$W6"
HEAD6="$(cd "$W6" && git rev-parse --short HEAD)"
INSTALLED6="$W6/installed/loom-daemon"
mkdir -p "$W6/installed"
write_fake_daemon "$INSTALLED6" "deadbee" "$W6/old-marker"
NEW_FAKE6="$W6/new-fake-daemon"
write_fake_daemon "$NEW_FAKE6" "$HEAD6" "$W6/new-marker"
# No .loom/.daemon.pid -> WAS_RUNNING resolves false.

out6=$( cd "$W6" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$INSTALLED6" NEW_FAKE_BIN_SRC="$NEW_FAKE6" \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc6=$?
assert_eq "0" "$rc6" "update with no prior PID file exits 0 (rebuild+provision only)"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out6" | grep -qi 'not running'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} update reports the daemon was not running (nothing started)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} update reports the daemon was not running (nothing started)"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if [[ -f "$W6/.loom/.daemon.pid" ]]; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} update never starts a daemon that wasn't already running"
else
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} update never starts a daemon that wasn't already running"
fi

# ============================================================
# 7. --no-restart rebuilds + provisions but leaves a running
#    daemon untouched (old process stays alive on the old binary).
# ============================================================
W7="$BASE_WORKDIR/w7"
new_fixture "$W7"
HEAD7="$(cd "$W7" && git rev-parse --short HEAD)"
INSTALLED7="$W7/installed/loom-daemon"
mkdir -p "$W7/installed"
write_fake_daemon "$INSTALLED7" "deadbee" "$W7/old-marker"
NEW_FAKE7="$W7/new-fake-daemon"
write_fake_daemon "$NEW_FAKE7" "$HEAD7" "$W7/new-marker"

"$INSTALLED7" >/dev/null 2>&1 &
old_pid7=$!
bg_proc_track "$old_pid7"
sleep 0.3
echo "$old_pid7" > "$W7/.loom/.daemon.pid"

( cd "$W7" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$INSTALLED7" NEW_FAKE_BIN_SRC="$NEW_FAKE7" \
    bash "$UPDATE_SCRIPT" --no-restart >/dev/null 2>&1 )

TESTS_RUN=$((TESTS_RUN + 1))
if kill -0 "$old_pid7" 2>/dev/null; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --no-restart leaves the running daemon process untouched"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --no-restart leaves the running daemon process untouched"
fi
kill "$old_pid7" 2>/dev/null || true

# ============================================================
# 8. --help documents --check / --dry-run / --force / --no-restart.
# ============================================================
help_out=$(bash "$UPDATE_SCRIPT" --help 2>/dev/null)
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$help_out" | grep -q -- '--check' && echo "$help_out" | grep -q -- '--dry-run' \
    && echo "$help_out" | grep -q -- '--no-restart'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --help documents --check / --dry-run / --no-restart"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --help documents --check / --dry-run / --no-restart"
fi

# ============================================================
# 9. Refuses to run outside a loom-daemon source checkout.
# ============================================================
W9="$BASE_WORKDIR/w9"
mkdir -p "$W9/.loom"
( cd "$W9" && git init -q )
( cd "$W9" && PATH="$TEST_PATH" bash "$UPDATE_SCRIPT" --check >/dev/null 2>&1 )
rc9=$?
assert_eq "1" "$rc9" "refuses to run when loom-daemon/Cargo.toml is absent"

# ============================================================
# 10. Build verification: a freshly-built binary whose embedded
#     commit does NOT match source HEAD (a stale baked-in commit,
#     e.g. from a build.rs watch-set bug) -> exit 4 and NO provision.
#     This is the self-verifying rebuild the whole issue is about:
#     a successful compile that ships the wrong commit is refused,
#     distinguishably from a compile failure (#4053).
# ============================================================
W10="$BASE_WORKDIR/w10"
new_fixture "$W10"
INSTALLED10="$W10/installed/loom-daemon"
mkdir -p "$W10/installed"
write_fake_daemon "$INSTALLED10" "deadbee" "$W10/old-marker"   # stale installed
# The freshly-"built" binary reports a WRONG commit (NOT source HEAD): simulates
# a build.rs watch-set defect that bakes a stale commit into a successful build.
NEW_FAKE10="$W10/new-fake-daemon"
write_fake_daemon "$NEW_FAKE10" "badc0de" "$W10/new-marker"
out10=$( cd "$W10" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$INSTALLED10" NEW_FAKE_BIN_SRC="$NEW_FAKE10" \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc10=$?
assert_eq "4" "$rc10" "stale baked-in commit (build-system defect) exits 4"
TESTS_RUN=$((TESTS_RUN + 1))
if "$INSTALLED10" --version 2>/dev/null | grep -q "deadbee"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} build-commit mismatch does NOT provision (destination left untouched)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} build-commit mismatch does NOT provision (destination left untouched)"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out10" | grep -qi 'Build verification FAILED'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} build-commit mismatch is reported distinguishably from a compile failure"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} build-commit mismatch is reported distinguishably from a compile failure"
fi

# ============================================================
# 11. Post-provision verification: a provision step that reports
#     SUCCESS but leaves the destination stale (the exact "reports
#     success while shipping nothing" hazard) -> exit 5. Exercises
#     the provision_machine_daemon branch via a fake provisioner
#     that returns 0 and points PROVISIONED_DAEMON_BIN at a stale
#     binary — proving the short-circuit can no longer silently
#     no-op a real roll, and that the failure is distinguishable
#     from the pre-existing soft warn (#4053, findings 3 & 4).
# ============================================================
W11="$BASE_WORKDIR/w11"
new_fixture "$W11"
HEAD11="$(cd "$W11" && git rev-parse --short HEAD)"
# The freshly-"built" binary reports the CORRECT source HEAD (passes build
# verification), so the failure below is unambiguously post-provision.
NEW_FAKE11="$W11/new-fake-daemon"
write_fake_daemon "$NEW_FAKE11" "$HEAD11" "$W11/new-marker"
# A STALE destination binary the fake provisioner will dishonestly point at.
STALE_DEST11="$W11/staledest/loom-daemon"
mkdir -p "$W11/staledest"
write_fake_daemon "$STALE_DEST11" "abc0123" "$W11/stale-marker"
# Fake provision-daemon.sh: reports success but leaves the destination stale.
mkdir -p "$W11/scripts/install"
cat > "$W11/scripts/install/provision-daemon.sh" <<EOF
provision_machine_daemon() {
    PROVISIONED_DAEMON_BIN="$STALE_DEST11"
    return 0
}
EOF
# No LOOM_DAEMON_BIN -> the provision_machine_daemon branch. No resolvable
# installed binary -> UPDATE_NEEDED is true -> the fake cargo "builds" NEW_FAKE11.
out11=$( cd "$W11" && PATH="$TEST_PATH" NEW_FAKE_BIN_SRC="$NEW_FAKE11" \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc11=$?
assert_eq "5" "$rc11" "provision reports success but ships a stale destination -> exit 5"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out11" | grep -qi 'Post-provision verification FAILED'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} silent no-op roll is caught and reported distinguishably"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} silent no-op roll is caught and reported distinguishably"
    echo "  output: $out11"
fi

# ============================================================
# 12. Signing helper (#4016) — a codesign FAILURE during the update is
#     non-fatal: the run still exits 0 and the binary is still provisioned,
#     with a warning surfaced. Fakes `uname` as Darwin so this is
#     deterministic regardless of the host actually running this suite.
# ============================================================
W12="$BASE_WORKDIR/w12"
new_fixture "$W12"
HEAD12="$(cd "$W12" && git rev-parse --short HEAD)"
INSTALLED12="$W12/installed/loom-daemon"
mkdir -p "$W12/installed"
write_fake_daemon "$INSTALLED12" "deadbee" "$W12/old-marker"
NEW_FAKE12="$W12/new-fake-daemon"
write_fake_daemon "$NEW_FAKE12" "$HEAD12" "$W12/new-marker"

FAKE_SIGN_FAIL_DIR="$W12/fake-codesign-fail-bin"
mkdir -p "$FAKE_SIGN_FAIL_DIR"
cat > "$FAKE_SIGN_FAIL_DIR/uname" <<'EOF'
#!/usr/bin/env bash
echo "Darwin"
EOF
chmod +x "$FAKE_SIGN_FAIL_DIR/uname"
cat > "$FAKE_SIGN_FAIL_DIR/codesign" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
chmod +x "$FAKE_SIGN_FAIL_DIR/codesign"

out12=$( cd "$W12" && PATH="$FAKE_SIGN_FAIL_DIR:$TEST_PATH" LOOM_DAEMON_BIN="$INSTALLED12" NEW_FAKE_BIN_SRC="$NEW_FAKE12" \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc12=$?
assert_eq "0" "$rc12" "codesign failure during update is non-fatal — exits 0"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out12" | grep -qi 'codesign failed'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} codesign failure surfaces a non-fatal warning"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} codesign failure surfaces a non-fatal warning"
    echo "  output: $out12"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if [[ -x "$INSTALLED12" ]] && "$INSTALLED12" --version 2>/dev/null | grep -q "$HEAD12"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} codesign failure: binary is still provisioned"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} codesign failure: binary is still provisioned"
fi

# ============================================================
# 13. Signing helper (#4016) — non-Darwin skips codesign entirely: the
#     update still succeeds and codesign is NEVER invoked. Fakes `uname`
#     as Linux and a `codesign` that leaves a marker file if ever called.
# ============================================================
W13="$BASE_WORKDIR/w13"
new_fixture "$W13"
HEAD13="$(cd "$W13" && git rev-parse --short HEAD)"
INSTALLED13="$W13/installed/loom-daemon"
mkdir -p "$W13/installed"
write_fake_daemon "$INSTALLED13" "deadbee" "$W13/old-marker"
NEW_FAKE13="$W13/new-fake-daemon"
write_fake_daemon "$NEW_FAKE13" "$HEAD13" "$W13/new-marker"

FAKE_LINUX_DIR13="$W13/fake-linux-bin"
mkdir -p "$FAKE_LINUX_DIR13"
cat > "$FAKE_LINUX_DIR13/uname" <<'EOF'
#!/usr/bin/env bash
echo "Linux"
EOF
chmod +x "$FAKE_LINUX_DIR13/uname"
CODESIGN_MARKER13="$W13/codesign-invoked-marker"
cat > "$FAKE_LINUX_DIR13/codesign" <<EOF
#!/usr/bin/env bash
touch "$CODESIGN_MARKER13"
exit 0
EOF
chmod +x "$FAKE_LINUX_DIR13/codesign"

# shellcheck disable=SC2034  # captured for ad-hoc debugging, not asserted on
out13=$( cd "$W13" && PATH="$FAKE_LINUX_DIR13:$TEST_PATH" LOOM_DAEMON_BIN="$INSTALLED13" NEW_FAKE_BIN_SRC="$NEW_FAKE13" \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc13=$?
assert_eq "0" "$rc13" "non-Darwin: update still exits 0"
TESTS_RUN=$((TESTS_RUN + 1))
if [[ -x "$INSTALLED13" ]] && "$INSTALLED13" --version 2>/dev/null | grep -q "$HEAD13"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} non-Darwin: binary is still provisioned"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} non-Darwin: binary is still provisioned"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if [[ -e "$CODESIGN_MARKER13" ]]; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} non-Darwin: codesign is never invoked"
else
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} non-Darwin: codesign is never invoked"
fi

# ============================================================
# 14. Signing helper (#4016) — codesign absent from PATH entirely: the
#     update still succeeds and provisions the binary (no codesign to
#     invoke at all). Uses a curated PATH built from /usr/bin minus
#     codesign, so this is a genuine absence rather than a stub.
# ============================================================
W14="$BASE_WORKDIR/w14"
new_fixture "$W14"
HEAD14="$(cd "$W14" && git rev-parse --short HEAD)"
INSTALLED14="$W14/installed/loom-daemon"
mkdir -p "$W14/installed"
write_fake_daemon "$INSTALLED14" "deadbee" "$W14/old-marker"
NEW_FAKE14="$W14/new-fake-daemon"
write_fake_daemon "$NEW_FAKE14" "$HEAD14" "$W14/new-marker"

out14=$( cd "$W14" && PATH="$TEST_PATH_NO_CODESIGN" LOOM_DAEMON_BIN="$INSTALLED14" NEW_FAKE_BIN_SRC="$NEW_FAKE14" \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc14=$?
assert_eq "0" "$rc14" "codesign absent from PATH: update still exits 0"
TESTS_RUN=$((TESTS_RUN + 1))
if [[ -x "$INSTALLED14" ]] && "$INSTALLED14" --version 2>/dev/null | grep -q "$HEAD14"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} codesign absent from PATH: binary is still provisioned"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} codesign absent from PATH: binary is still provisioned"
    echo "  output: $out14"
fi

# ============================================================
# 15. Launchd ownership + restart (#4042/#4232): a launchd-managed daemon (stub
#     launchctl reports a LOADED job + pid) with NO .loom/.daemon.pid file ->
#     the updater plans/executes a RESTART (not "was not running"), drives it
#     through the `restart` subcommand (stub records the invocation), does NOT
#     consult .daemon.flags, and exits 0. The stub relaunches onto a NEW, real,
#     live pid the instant the restart is accepted (#4232 case (a): relaunch
#     observed -> success, no kickstart fallback ever invoked).
# ============================================================
W15="$BASE_WORKDIR/w15"
new_fixture "$W15"
HEAD15="$(cd "$W15" && git rev-parse --short HEAD)"
INSTALLED15="$W15/installed/loom-daemon"
mkdir -p "$W15/installed"
# The provisioned (fresh) binary — invoked as `restart` after provisioning —
# reports source HEAD and accepts the restart request (rc 0). No .daemon.pid.
RESTART_MARKER15="$W15/restart-invoked"
write_fake_daemon_restart "$INSTALLED15" "deadbee" "$RESTART_MARKER15" 0
NEW_FAKE15="$W15/new-fake-daemon"
write_fake_daemon_restart "$NEW_FAKE15" "$HEAD15" "$RESTART_MARKER15" 0
# A .daemon.flags that MUST NOT be consulted in launchd mode (would otherwise
# add --work-finder to a stop+start path).
echo "--work-finder" > "$W15/.loom/.daemon.flags"
# A real, live process standing in for "the relaunched job's new pid" — the
# #4232 poll requires a live pid (kill -0), not just a differing number.
sleep 60 >/dev/null 2>&1 &
RELAUNCHED_PID15=$!
bg_proc_track "$RELAUNCHED_PID15"
LD_BIN15="$W15/launchd-bin"
write_fake_launchd_loaded_bin "$LD_BIN15" "$W15/launchctl.log" "$RESTART_MARKER15" "$RELAUNCHED_PID15"

out15=$( cd "$W15" && PATH="$LD_BIN15:$TEST_PATH" LOOM_DAEMON_LAUNCHD=1 \
    LOOM_DAEMON_BIN="$INSTALLED15" NEW_FAKE_BIN_SRC="$NEW_FAKE15" \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc15=$?
kill "$RELAUNCHED_PID15" 2>/dev/null || true
assert_eq "0" "$rc15" "launchd-managed update (no pid file) exits 0"
TESTS_RUN=$((TESTS_RUN + 1))
if [[ -s "$RESTART_MARKER15" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} launchd restart driven through the 'restart' subcommand (not stop+start)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} launchd restart driven through the 'restart' subcommand (not stop+start)"
    echo "  output: $out15"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out15" | grep -qi 'not running'; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} launchd-loaded job is NOT mistaken for 'was not running'"
    echo "  output: $out15"
else
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} launchd-loaded job is NOT mistaken for 'was not running'"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if grep -q "^${RELAUNCHED_PID15}$\|new pid ${RELAUNCHED_PID15}" <<< "$out15" || echo "$out15" | grep -q "new pid ${RELAUNCHED_PID15}"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} success message reports the VERIFIED new pid (#4232)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} success message reports the VERIFIED new pid (#4232)"
    echo "  output: $out15"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if grep -q '^kickstart ' "$W15/launchctl.log" 2>/dev/null; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} relaunch observed immediately -> kickstart fallback is NEVER invoked (#4232 case a)"
    echo "  launchctl.log: $(cat "$W15/launchctl.log" 2>/dev/null)"
else
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} relaunch observed immediately -> kickstart fallback is NEVER invoked (#4232 case a)"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out15" | grep -qi 'FLAGS-OFF'; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} no 'restarting FLAGS-OFF' warning fires for a launchd restart"
    echo "  output: $out15"
else
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} no 'restarting FLAGS-OFF' warning fires for a launchd restart"
fi

# ============================================================
# 16. Launchd restart REFUSED (#4042/#4118): a launchd job is loaded but the
#     running (old) binary rejects the restart request (pre-#4077 binary / dead
#     socket). Without --relaunch the updater must exit NON-ZERO (6) and print
#     the CORRECTED fallback: it names the `--relaunch` re-render path (NOT a
#     bare `launchctl bootstrap` of the stale plist, which was the #4118 bug),
#     warns that `launchctl bootout` terminates in-flight sweeps (AC4), and
#     prefers a graceful `kill -TERM`.
# ============================================================
W16="$BASE_WORKDIR/w16"
new_fixture "$W16"
HEAD16="$(cd "$W16" && git rev-parse --short HEAD)"
INSTALLED16="$W16/installed/loom-daemon"
mkdir -p "$W16/installed"
RESTART_MARKER16="$W16/restart-invoked"
write_fake_daemon_restart "$INSTALLED16" "deadbee" "$RESTART_MARKER16" 1
NEW_FAKE16="$W16/new-fake-daemon"
# Fresh binary's `restart` returns non-zero (request refused by the running daemon).
write_fake_daemon_restart "$NEW_FAKE16" "$HEAD16" "$RESTART_MARKER16" 1
LD_BIN16="$W16/launchd-bin"
write_fake_launchd_loaded_bin "$LD_BIN16" "$W16/launchctl.log"

out16=$( cd "$W16" && PATH="$LD_BIN16:$TEST_PATH" LOOM_DAEMON_LAUNCHD=1 \
    LOOM_DAEMON_BIN="$INSTALLED16" NEW_FAKE_BIN_SRC="$NEW_FAKE16" \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc16=$?
assert_eq "6" "$rc16" "launchd restart refused -> exit 6 (never a silent half-update)"
# (a) Names the --relaunch re-render path and does NOT recommend a bare
#     `launchctl bootstrap` of the stale plist (the #4118 self-perpetuating bug).
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out16" | grep -q -- '--relaunch' && ! echo "$out16" | grep -qi 'launchctl bootstrap'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} refused restart names --relaunch, never a bare bootstrap of the stale plist"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} refused restart names --relaunch, never a bare bootstrap of the stale plist"
    echo "  output: $out16"
fi
# (b) AC4: warns that bootout terminates in-flight sweeps + prefers kill -TERM.
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out16" | grep -qi 'bootout' && echo "$out16" | grep -qi 'sweep' \
    && echo "$out16" | grep -q 'kill -TERM'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} refused restart warns bootout kills in-flight sweeps + prefers kill -TERM (AC4)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} refused restart warns bootout kills in-flight sweeps + prefers kill -TERM (AC4)"
    echo "  output: $out16"
fi

# ============================================================
# 17. --check names the owning manager (#4042): launchd (with label) when a
#     job is loaded; PID-file/nohup when a live pid file exists; not running
#     otherwise. All three are read-only (exit 3 here since the commit differs).
# ============================================================
W17="$BASE_WORKDIR/w17"
new_fixture "$W17"
INSTALLED17="$W17/installed/loom-daemon"
mkdir -p "$W17/installed"
write_fake_daemon "$INSTALLED17" "deadbee" "$W17/marker"
LD_BIN17="$W17/launchd-bin"
write_fake_launchd_loaded_bin "$LD_BIN17" "$W17/launchctl.log"
# (a) launchd loaded -> manager: launchd (names the scratch label).
check_ld_out=$( cd "$W17" && PATH="$LD_BIN17:$TEST_PATH" LOOM_DAEMON_LAUNCHD=1 \
    LOOM_LAUNCHD_LABEL="com.example.scratch-4042" LOOM_DAEMON_BIN="$INSTALLED17" \
    bash "$UPDATE_SCRIPT" --check 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$check_ld_out" | grep -qi 'manager: launchd' && echo "$check_ld_out" | grep -q 'com.example.scratch-4042'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --check names launchd (with label) as the owning manager"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --check names launchd (with label) as the owning manager"
    echo "  output: $check_ld_out"
fi
# (b) LOOM_DAEMON_LAUNCHD=0 + live pid file -> manager: PID-file/nohup.
"$INSTALLED17" >/dev/null 2>&1 &
pid17=$!
bg_proc_track "$pid17"
sleep 0.3
echo "$pid17" > "$W17/.loom/.daemon.pid"
check_pid_out=$( cd "$W17" && PATH="$TEST_PATH" LOOM_DAEMON_LAUNCHD=0 \
    LOOM_DAEMON_BIN="$INSTALLED17" bash "$UPDATE_SCRIPT" --check 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$check_pid_out" | grep -qi 'manager: PID-file'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --check names PID-file/nohup as the owning manager"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --check names PID-file/nohup as the owning manager"
    echo "  output: $check_pid_out"
fi
kill "$pid17" 2>/dev/null || true
rm -f "$W17/.loom/.daemon.pid"
# (c) nothing running -> manager: not running.
check_none_out=$( cd "$W17" && PATH="$TEST_PATH" LOOM_DAEMON_LAUNCHD=0 \
    LOOM_DAEMON_BIN="$INSTALLED17" bash "$UPDATE_SCRIPT" --check 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$check_none_out" | grep -qi 'manager: not running'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --check names 'not running' when no daemon is up"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --check names 'not running' when no daemon is up"
    echo "  output: $check_none_out"
fi

# ============================================================
# 18. --dry-run in launchd mode reports the launchd restart plan and makes no
#     writes (no rebuild, no restart invocation).
# ============================================================
W18="$BASE_WORKDIR/w18"
new_fixture "$W18"
INSTALLED18="$W18/installed/loom-daemon"
mkdir -p "$W18/installed"
RESTART_MARKER18="$W18/restart-invoked"
write_fake_daemon_restart "$INSTALLED18" "deadbee" "$RESTART_MARKER18" 0
LD_BIN18="$W18/launchd-bin"
write_fake_launchd_loaded_bin "$LD_BIN18" "$W18/launchctl.log"
dry18_out=$( cd "$W18" && PATH="$LD_BIN18:$TEST_PATH" LOOM_DAEMON_LAUNCHD=1 \
    LOOM_DAEMON_BIN="$INSTALLED18" bash "$UPDATE_SCRIPT" --dry-run 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$dry18_out" | grep -qi 'launchd-managed' && echo "$dry18_out" | grep -qi 'restart' \
    && [[ ! -s "$RESTART_MARKER18" ]] && [[ ! -e "$W18/loom-daemon/target/release/loom-daemon" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --dry-run reports the launchd restart plan and makes no writes"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --dry-run reports the launchd restart plan and makes no writes"
    echo "  output: $dry18_out"
fi

# ============================================================
# 19. LOOM_DAEMON_LAUNCHD=0 skips all launchd tiers even on a (faked) Darwin
#     host with a launchd job loaded: the daemon is treated as NOT launchd-
#     managed, so with no pid file it is "not running" (no restart attempted).
#     Guards the sandbox invariant that --no-launchd never reaches launchd.
# ============================================================
W19="$BASE_WORKDIR/w19"
new_fixture "$W19"
HEAD19="$(cd "$W19" && git rev-parse --short HEAD)"
INSTALLED19="$W19/installed/loom-daemon"
mkdir -p "$W19/installed"
RESTART_MARKER19="$W19/restart-invoked"
write_fake_daemon_restart "$INSTALLED19" "deadbee" "$RESTART_MARKER19" 0
NEW_FAKE19="$W19/new-fake-daemon"
write_fake_daemon_restart "$NEW_FAKE19" "$HEAD19" "$RESTART_MARKER19" 0
LD_BIN19="$W19/launchd-bin"
write_fake_launchd_loaded_bin "$LD_BIN19" "$W19/launchctl.log"
out19=$( cd "$W19" && PATH="$LD_BIN19:$TEST_PATH" LOOM_DAEMON_LAUNCHD=0 \
    LOOM_DAEMON_BIN="$INSTALLED19" NEW_FAKE_BIN_SRC="$NEW_FAKE19" \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc19=$?
assert_eq "0" "$rc19" "LOOM_DAEMON_LAUNCHD=0 update exits 0 (rebuild+provision, no launchd)"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out19" | grep -qi 'not running' && [[ ! -s "$RESTART_MARKER19" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} LOOM_DAEMON_LAUNCHD=0 skips launchd tiers (no restart driven)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} LOOM_DAEMON_LAUNCHD=0 skips launchd tiers (no restart driven)"
    echo "  output: $out19"
fi

# ============================================================
# Scenarios 21+22 are the only two in this suite that need a REAL `plutil`
# (#4799). They drive loom-daemon-update.sh's `--relaunch` path, whose first
# step is `harvest_plist_env` (lib/daemon-env-harvest.sh) -- which requires
# plutil + jq by contract ("required on the macOS launchd path") and returns 2
# when either is missing, so perform_relaunch aborts with 6 and never
# re-renders the plist. Scenario 22 then reads the result back with
# `plutil -extract` directly. Everything else launchd-flavored in this suite
# runs off the stub `launchctl`/`uname` pair and IS portable; these two are
# not, and plutil is macOS-only (no ubuntu package stands in -- an XML-plist
# parser stub would be testing the stub, not the production path). So skip
# them where plutil is absent rather than fail a Linux CI runner on a genuinely
# Darwin-only code path. They still run on every macOS dev/CI host.
if ! command -v plutil >/dev/null 2>&1; then
    echo -e "${YELLOW}⊘${NC} SKIP scenarios 21-22 (--relaunch plist re-render + env preservation): plutil not available — harvest_plist_env is a macOS-only production path"
else
# ============================================================
# 21. --relaunch re-renders the plist with the SUPERVISED keys (#4118 AC1):
#     after a refused restart, `--relaunch` re-renders via loom-daemon-start.sh,
#     installing KeepAlive:{SuccessfulExit:true} + LOOM_DAEMON_SUPERVISOR=launchd
#     into the plist — the two keys the stale pre-#4077 fixture plist LACKS, so a
#     passing assertion proves the re-render actually happened (not a leftover).
#     HOME is sandboxed so LAUNCHD_PLIST resolves inside the test tree, never the
#     operator's real ~/Library/LaunchAgents (#4078).
# ============================================================
W21="$BASE_WORKDIR/w21"
new_fixture "$W21"
INSTALLED21="$W21/installed/loom-daemon"
mkdir -p "$W21/installed"
RESTART_MARKER21="$W21/restart-invoked"
# Running (old) + fresh binaries both REFUSE restart (pre-#4077) -> exit-6 path.
write_fake_daemon_restart "$INSTALLED21" "deadbee" "$RESTART_MARKER21" 1
NEW_FAKE21="$W21/new-fake-daemon"
HEAD21="$(cd "$W21" && git rev-parse --short HEAD)"
write_fake_daemon_restart "$NEW_FAKE21" "$HEAD21" "$RESTART_MARKER21" 1
LD_BIN21="$W21/launchd-bin"
write_fake_launchd_loaded_bin "$LD_BIN21" "$W21/launchctl.log"
HOME21="$W21/home"
PLIST21="$HOME21/Library/LaunchAgents/${LOOM_LAUNCHD_LABEL}.plist"
write_fixture_plist_pre4077 "$PLIST21" "$LOOM_LAUNCHD_LABEL" "$INSTALLED21" "$HOME21"
( cd "$W21" && PATH="$LD_BIN21:$TEST_PATH" HOME="$HOME21" LOOM_DAEMON_LAUNCHD=1 \
    LOOM_DAEMON_BIN="$INSTALLED21" NEW_FAKE_BIN_SRC="$NEW_FAKE21" \
    bash "$UPDATE_SCRIPT" --relaunch >/dev/null 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if grep -q 'LOOM_DAEMON_SUPERVISOR' "$PLIST21" 2>/dev/null \
    && grep -q 'SuccessfulExit' "$PLIST21" 2>/dev/null; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --relaunch re-renders the plist with KeepAlive:SuccessfulExit + LOOM_DAEMON_SUPERVISOR (AC1)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --relaunch re-renders the plist with KeepAlive:SuccessfulExit + LOOM_DAEMON_SUPERVISOR (AC1)"
    echo "  plist: $(cat "$PLIST21" 2>/dev/null)"
fi

# ============================================================
# 22. --relaunch PRESERVES the live plist's autonomy env across the re-render
#     (#4118 AC3): the four autonomy keys carry through byte-for-byte, and the
#     stale PATH is NOT round-tripped (start.sh rebuilds PATH; harvesting it would
#     grow the string every roll — Curator trap #1).
# ============================================================
W22="$BASE_WORKDIR/w22"
new_fixture "$W22"
INSTALLED22="$W22/installed/loom-daemon"
mkdir -p "$W22/installed"
RESTART_MARKER22="$W22/restart-invoked"
write_fake_daemon_restart "$INSTALLED22" "deadbee" "$RESTART_MARKER22" 1
NEW_FAKE22="$W22/new-fake-daemon"
HEAD22="$(cd "$W22" && git rev-parse --short HEAD)"
write_fake_daemon_restart "$NEW_FAKE22" "$HEAD22" "$RESTART_MARKER22" 1
LD_BIN22="$W22/launchd-bin"
write_fake_launchd_loaded_bin "$LD_BIN22" "$W22/launchctl.log"
HOME22="$W22/home"
PLIST22="$HOME22/Library/LaunchAgents/${LOOM_LAUNCHD_LABEL}.plist"
write_fixture_plist_pre4077 "$PLIST22" "$LOOM_LAUNCHD_LABEL" "$INSTALLED22" "$HOME22"
( cd "$W22" && PATH="$LD_BIN22:$TEST_PATH" HOME="$HOME22" LOOM_DAEMON_LAUNCHD=1 \
    LOOM_DAEMON_BIN="$INSTALLED22" NEW_FAKE_BIN_SRC="$NEW_FAKE22" \
    bash "$UPDATE_SCRIPT" --relaunch >/dev/null 2>&1 )
wf22=$(plutil -extract EnvironmentVariables.LOOM_WORK_FINDER raw -o - "$PLIST22" 2>/dev/null)
hg22=$(plutil -extract EnvironmentVariables.LOOM_MAIN_HEALTH_GATE raw -o - "$PLIST22" 2>/dev/null)
mc22=$(plutil -extract EnvironmentVariables.LOOM_WORK_FINDER_MAX_CONCURRENT raw -o - "$PLIST22" 2>/dev/null)
pt22=$(plutil -extract EnvironmentVariables.LOOM_PER_TOKEN_CONCURRENCY raw -o - "$PLIST22" 2>/dev/null)
TESTS_RUN=$((TESTS_RUN + 1))
if [[ "$wf22" == "1" && "$hg22" == "1" && "$mc22" == "10" && "$pt22" == "5" ]] \
    && ! grep -q 'sentinel-oldpath-4118' "$PLIST22" 2>/dev/null; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --relaunch preserves all 4 autonomy env keys and does NOT round-trip the stale PATH (AC3)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --relaunch preserves all 4 autonomy env keys and does NOT round-trip the stale PATH (AC3)"
    echo "  WF=[$wf22] HG=[$hg22] MAXC=[$mc22] PERTOK=[$pt22]"
    echo "  plist: $(cat "$PLIST22" 2>/dev/null)"
fi
fi # end plutil-availability guard for scenarios 21-22

# ============================================================
# 23. --no-restart on a launchd host does NOT print a bare `launchctl bootstrap`
#     of the stale plist (the second #4118 stale-advice site): it names the
#     --relaunch re-render path and warns that bootout kills in-flight sweeps.
# ============================================================
W23="$BASE_WORKDIR/w23"
new_fixture "$W23"
INSTALLED23="$W23/installed/loom-daemon"
mkdir -p "$W23/installed"
RESTART_MARKER23="$W23/restart-invoked"
write_fake_daemon_restart "$INSTALLED23" "deadbee" "$RESTART_MARKER23" 0
NEW_FAKE23="$W23/new-fake-daemon"
HEAD23="$(cd "$W23" && git rev-parse --short HEAD)"
write_fake_daemon_restart "$NEW_FAKE23" "$HEAD23" "$RESTART_MARKER23" 0
LD_BIN23="$W23/launchd-bin"
write_fake_launchd_loaded_bin "$LD_BIN23" "$W23/launchctl.log"
out23=$( cd "$W23" && PATH="$LD_BIN23:$TEST_PATH" LOOM_DAEMON_LAUNCHD=1 \
    LOOM_DAEMON_BIN="$INSTALLED23" NEW_FAKE_BIN_SRC="$NEW_FAKE23" \
    bash "$UPDATE_SCRIPT" --no-restart 2>&1 )
rc23=$?
assert_eq "0" "$rc23" "--no-restart on a launchd host exits 0"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out23" | grep -q -- '--relaunch' \
    && ! echo "$out23" | grep -qi 'launchctl bootstrap' \
    && echo "$out23" | grep -qi 'bootout' && echo "$out23" | grep -qi 'sweep'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --no-restart names --relaunch, no bare bootstrap, warns bootout kills sweeps"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --no-restart names --relaunch, no bare bootstrap, warns bootout kills sweeps"
    echo "  output: $out23"
fi

# ============================================================
# 24. --relaunch with the live plist ABSENT fails loudly (names the missing path)
#     and refuses to relaunch — it must NEVER silently render FLAGS-OFF defaults
#     (the #4011 class this whole line of work closes). No plist is written.
# ============================================================
W24="$BASE_WORKDIR/w24"
new_fixture "$W24"
INSTALLED24="$W24/installed/loom-daemon"
mkdir -p "$W24/installed"
RESTART_MARKER24="$W24/restart-invoked"
write_fake_daemon_restart "$INSTALLED24" "deadbee" "$RESTART_MARKER24" 1
NEW_FAKE24="$W24/new-fake-daemon"
HEAD24="$(cd "$W24" && git rev-parse --short HEAD)"
write_fake_daemon_restart "$NEW_FAKE24" "$HEAD24" "$RESTART_MARKER24" 1
LD_BIN24="$W24/launchd-bin"
write_fake_launchd_loaded_bin "$LD_BIN24" "$W24/launchctl.log"
HOME24="$W24/home"
mkdir -p "$HOME24/Library/LaunchAgents"   # dir exists, plist deliberately absent
PLIST24="$HOME24/Library/LaunchAgents/${LOOM_LAUNCHD_LABEL}.plist"
out24=$( cd "$W24" && PATH="$LD_BIN24:$TEST_PATH" HOME="$HOME24" LOOM_DAEMON_LAUNCHD=1 \
    LOOM_DAEMON_BIN="$INSTALLED24" NEW_FAKE_BIN_SRC="$NEW_FAKE24" \
    bash "$UPDATE_SCRIPT" --relaunch 2>&1 )
rc24=$?
TESTS_RUN=$((TESTS_RUN + 1))
if [[ "$rc24" -ne 0 ]] && echo "$out24" | grep -qi 'not found' \
    && echo "$out24" | grep -qF "$PLIST24" && [[ ! -e "$PLIST24" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --relaunch with an absent plist fails loudly (names the path) and renders nothing"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --relaunch with an absent plist fails loudly (names the path) and renders nothing"
    echo "  rc=$rc24 plist-exists=$([[ -e "$PLIST24" ]] && echo yes || echo no)"
    echo "  output: $out24"
fi

# ============================================================
# M1. Machine mode (Epic #3835 Phase 3b, #4229): LOOM_MACHINE_CHECKOUT lets
#     `--check` rebuild-detect against the CHECKOUT's source tree even when
#     $PWD is a totally unrelated non-Loom directory -- the concrete Gap 1
#     regression this issue closes ("only works inside a Loom source
#     checkout"). HOME is overridden to a scratch dir so DAEMON_STATE_HOME
#     ($HOME/.loom in machine mode) never touches the real operator ~/.loom,
#     matching the rest of this suite's isolation discipline.
# ============================================================
WM1="$BASE_WORKDIR/wm1-checkout"
new_fixture "$WM1"
HEADM1="$(cd "$WM1" && git rev-parse --short HEAD)"
write_fake_daemon "$WM1/installed-loom-daemon" "$HEADM1" "$WM1/marker"
NON_LOOM_DIR="$BASE_WORKDIR/wm1-non-loom-cwd"
mkdir -p "$NON_LOOM_DIR"
HOME_WM1="$BASE_WORKDIR/wm1-home"
mkdir -p "$HOME_WM1"
out=$( cd "$NON_LOOM_DIR" && PATH="$TEST_PATH" HOME="$HOME_WM1" LOOM_MACHINE_CHECKOUT="$WM1" \
    LOOM_DAEMON_BIN="$WM1/installed-loom-daemon" \
    bash "$UPDATE_SCRIPT" --check; echo "EXIT=$?" )
rc=$(echo "$out" | grep -o 'EXIT=[0-9]*' | cut -d= -f2)
assert_eq "0" "$rc" "machine mode: --check from an unrelated non-Loom \$PWD resolves the checkout as source tree (Gap 1)"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out" | grep -q "only works inside a Loom source checkout"; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} machine mode: never refuses with the dev-mode 'source checkout' error"
    echo "  output: $out"
else
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} machine mode: never refuses with the dev-mode 'source checkout' error"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out" | grep -qF "$WM1"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} machine mode: reports the machine checkout as the resolved source tree"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} machine mode: reports the machine checkout as the resolved source tree"
    echo "  output: $out"
fi

# Dev-mode fallback (scope guard, filed AC5): the SAME non-Loom directory,
# invoked directly (no LOOM_MACHINE_CHECKOUT -- no dispatcher), still refuses
# exactly as before #4229. Machine mode is additive, never a replacement.
out_dev=$( cd "$NON_LOOM_DIR" && PATH="$TEST_PATH" HOME="$HOME_WM1" bash "$UPDATE_SCRIPT" --check 2>&1 )
rc_dev=$?
assert_eq "1" "$rc_dev" "dev-mode fallback unchanged: --check from a non-Loom \$PWD (no dispatcher) still exits 1"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out_dev" | grep -qi "Not in a Loom workspace"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} dev-mode fallback unchanged: reports 'Not in a Loom workspace'"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} dev-mode fallback unchanged: reports 'Not in a Loom workspace'"
fi

# ============================================================
# 26. Systemd ownership + restart (#4260 sub-issue C, mirrors test 15): a
#     systemd-managed daemon (stub systemctl reports an ACTIVE unit + MainPID)
#     with NO .loom/.daemon.pid file -> the updater plans/executes a RESTART
#     (not "was not running"), drives it through the `restart` subcommand
#     (stub records the invocation), does NOT consult .daemon.flags, and exits
#     0. The stub relaunches onto a NEW, real, live pid the instant the restart
#     is accepted (#4950 case (a): relaunch observed -> success, no
#     reset-failed+start self-heal ever invoked). Forced via the
#     LOOM_SYSTEMD_FORCE=1 test seam (lib/systemd-user.sh) since this suite
#     runs on Darwin; LOOM_DAEMON_LAUNCHD=0 is already the suite-wide default
#     so launchd never competes for this tier.
# ============================================================
W26="$BASE_WORKDIR/w26"
new_fixture "$W26"
HEAD26="$(cd "$W26" && git rev-parse --short HEAD)"
INSTALLED26="$W26/installed/loom-daemon"
mkdir -p "$W26/installed"
RESTART_MARKER26="$W26/restart-invoked"
write_fake_daemon_restart "$INSTALLED26" "deadbee" "$RESTART_MARKER26" 0
NEW_FAKE26="$W26/new-fake-daemon"
write_fake_daemon_restart "$NEW_FAKE26" "$HEAD26" "$RESTART_MARKER26" 0
# A .daemon.flags that MUST NOT be consulted in systemd mode (would otherwise
# add --work-finder to a stop+start path).
echo "--work-finder" > "$W26/.loom/.daemon.flags"
# A real, live process standing in for "the relaunched unit's new MainPID" —
# the #4950 poll requires a live pid (kill -0), not just a differing number.
sleep 60 >/dev/null 2>&1 &
RELAUNCHED_PID26=$!
bg_proc_track "$RELAUNCHED_PID26"
SD_BIN26="$W26/systemd-bin"
SD_LOG26="$W26/systemctl.log"
SD_STATE26="$W26/systemd-pid-state"
write_fake_systemd_pid_bin "$SD_BIN26" "$SD_LOG26" "$SD_STATE26" "4242:active:success" \
    "$RESTART_MARKER26" "${RELAUNCHED_PID26}:active:success"

out26=$( cd "$W26" && PATH="$SD_BIN26:$TEST_PATH" LOOM_SYSTEMD_FORCE=1 LOOM_DAEMON_SYSTEMD=1 \
    LOOM_SYSTEMD_UNIT="loom-daemon-test-sd26.service" \
    LOOM_DAEMON_BIN="$INSTALLED26" NEW_FAKE_BIN_SRC="$NEW_FAKE26" \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc26=$?
kill "$RELAUNCHED_PID26" 2>/dev/null || true
assert_eq "0" "$rc26" "systemd-managed update (no pid file) exits 0"
TESTS_RUN=$((TESTS_RUN + 1))
if [[ -s "$RESTART_MARKER26" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} systemd restart driven through the 'restart' subcommand (not stop+start)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} systemd restart driven through the 'restart' subcommand (not stop+start)"
    echo "  output: $out26"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out26" | grep -qi 'not running'; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} systemd-loaded unit is NOT mistaken for 'was not running'"
    echo "  output: $out26"
else
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} systemd-loaded unit is NOT mistaken for 'was not running'"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out26" | grep -qi 'FLAGS-OFF'; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} no 'restarting FLAGS-OFF' warning fires for a systemd restart"
    echo "  output: $out26"
else
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} no 'restarting FLAGS-OFF' warning fires for a systemd restart"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if grep -q "new pid ${RELAUNCHED_PID26}" <<< "$out26"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} success message reports the VERIFIED new MainPID (#4950)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} success message reports the VERIFIED new MainPID (#4950)"
    echo "  output: $out26"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if grep -q 'reset-failed' "$SD_LOG26" 2>/dev/null; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} relaunch observed immediately -> reset-failed+start self-heal is NEVER invoked (#4950 case a)"
    echo "  systemctl.log: $(cat "$SD_LOG26" 2>/dev/null)"
else
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} relaunch observed immediately -> reset-failed+start self-heal is NEVER invoked (#4950 case a)"
fi

# ============================================================
# 27. Systemd restart REFUSED (mirrors test 16): a systemd unit is active but
#     the running (old) binary rejects the restart request (pre-#4267 binary /
#     dead socket). Without --relaunch the updater must exit NON-ZERO (6) and
#     print the systemd fallback: names --relaunch (NOT a bare `systemctl
#     --user stop`, which tears down the cgroup and kills in-flight sweeps),
#     and prefers a graceful `kill -TERM`.
# ============================================================
W27="$BASE_WORKDIR/w27"
new_fixture "$W27"
HEAD27="$(cd "$W27" && git rev-parse --short HEAD)"
INSTALLED27="$W27/installed/loom-daemon"
mkdir -p "$W27/installed"
RESTART_MARKER27="$W27/restart-invoked"
write_fake_daemon_restart "$INSTALLED27" "deadbee" "$RESTART_MARKER27" 1
NEW_FAKE27="$W27/new-fake-daemon"
write_fake_daemon_restart "$NEW_FAKE27" "$HEAD27" "$RESTART_MARKER27" 1
SD_BIN27="$W27/systemd-bin"
SD_LOG27="$W27/systemctl.log"
write_fake_systemd_active_bin "$SD_BIN27" "$SD_LOG27" "4242"

out27=$( cd "$W27" && PATH="$SD_BIN27:$TEST_PATH" LOOM_SYSTEMD_FORCE=1 LOOM_DAEMON_SYSTEMD=1 \
    LOOM_SYSTEMD_UNIT="loom-daemon-test-sd27.service" \
    LOOM_DAEMON_BIN="$INSTALLED27" NEW_FAKE_BIN_SRC="$NEW_FAKE27" \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc27=$?
assert_eq "6" "$rc27" "systemd restart refused -> exit 6 (never a silent half-update)"
# Names --relaunch as the fix and NEVER recommends a bare `systemctl --user
# restart`/`enable` by hand as a shortcut (that would skip the re-render and
# refuse identically forever, the systemd analog of the #4118 bootstrap bug) —
# the ONLY manual systemctl-adjacent mention allowed is the "do NOT ... stop"
# warning itself.
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out27" | grep -q -- '--relaunch' \
    && ! echo "$out27" | grep -qi 'systemctl --user restart' \
    && ! echo "$out27" | grep -qi 'systemctl --user enable'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} refused restart names --relaunch, never a bare manual systemctl restart/enable"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} refused restart names --relaunch, never a bare manual systemctl restart/enable"
    echo "  output: $out27"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out27" | grep -qi 'cgroup' && echo "$out27" | grep -qi 'sweep' \
    && echo "$out27" | grep -q 'kill -TERM'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} refused restart warns stop tears down the cgroup + kills in-flight sweeps, prefers kill -TERM"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} refused restart warns stop tears down the cgroup + kills in-flight sweeps, prefers kill -TERM"
    echo "  output: $out27"
fi

# ============================================================
# 28. --check names systemd (with unit) as the owning manager when a unit is
#     active (mirrors test 17a).
# ============================================================
W28="$BASE_WORKDIR/w28"
new_fixture "$W28"
INSTALLED28="$W28/installed/loom-daemon"
mkdir -p "$W28/installed"
write_fake_daemon "$INSTALLED28" "deadbee" "$W28/marker"
SD_BIN28="$W28/systemd-bin"
SD_LOG28="$W28/systemctl.log"
write_fake_systemd_active_bin "$SD_BIN28" "$SD_LOG28" "4242"
check_sd_out=$( cd "$W28" && PATH="$SD_BIN28:$TEST_PATH" LOOM_SYSTEMD_FORCE=1 LOOM_DAEMON_SYSTEMD=1 \
    LOOM_SYSTEMD_UNIT="loom-daemon-test-sd28.service" LOOM_DAEMON_BIN="$INSTALLED28" \
    bash "$UPDATE_SCRIPT" --check 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$check_sd_out" | grep -qi 'manager: systemd' && echo "$check_sd_out" | grep -q 'loom-daemon-test-sd28.service'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --check names systemd (with unit) as the owning manager"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --check names systemd (with unit) as the owning manager"
    echo "  output: $check_sd_out"
fi

# ============================================================
# 29. --dry-run in systemd mode reports the systemd restart plan and makes no
#     writes (no rebuild, no restart invocation) (mirrors test 18).
# ============================================================
W29="$BASE_WORKDIR/w29"
new_fixture "$W29"
INSTALLED29="$W29/installed/loom-daemon"
mkdir -p "$W29/installed"
RESTART_MARKER29="$W29/restart-invoked"
write_fake_daemon_restart "$INSTALLED29" "deadbee" "$RESTART_MARKER29" 0
SD_BIN29="$W29/systemd-bin"
SD_LOG29="$W29/systemctl.log"
write_fake_systemd_active_bin "$SD_BIN29" "$SD_LOG29" "4242"
dry29_out=$( cd "$W29" && PATH="$SD_BIN29:$TEST_PATH" LOOM_SYSTEMD_FORCE=1 LOOM_DAEMON_SYSTEMD=1 \
    LOOM_SYSTEMD_UNIT="loom-daemon-test-sd29.service" LOOM_DAEMON_BIN="$INSTALLED29" \
    bash "$UPDATE_SCRIPT" --dry-run 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$dry29_out" | grep -qi 'systemd-managed' && echo "$dry29_out" | grep -qi 'restart' \
    && [[ ! -s "$RESTART_MARKER29" ]] && [[ ! -e "$W29/loom-daemon/target/release/loom-daemon" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --dry-run reports the systemd restart plan and makes no writes"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --dry-run reports the systemd restart plan and makes no writes"
    echo "  output: $dry29_out"
fi

# ============================================================
# 30. LOOM_DAEMON_SYSTEMD=0 skips all systemd tiers even with the unit detected
#     as active (LOOM_SYSTEMD_FORCE=1 + stub systemctl on PATH): the daemon is
#     treated as NOT systemd-managed, so with no pid file it is "not running"
#     (no restart attempted). Guards the --no-systemd symmetry contract (#4260
#     sub-issue C AC3, mirrors test 19).
# ============================================================
W30="$BASE_WORKDIR/w30"
new_fixture "$W30"
HEAD30="$(cd "$W30" && git rev-parse --short HEAD)"
INSTALLED30="$W30/installed/loom-daemon"
mkdir -p "$W30/installed"
RESTART_MARKER30="$W30/restart-invoked"
write_fake_daemon_restart "$INSTALLED30" "deadbee" "$RESTART_MARKER30" 0
NEW_FAKE30="$W30/new-fake-daemon"
write_fake_daemon_restart "$NEW_FAKE30" "$HEAD30" "$RESTART_MARKER30" 0
SD_BIN30="$W30/systemd-bin"
SD_LOG30="$W30/systemctl.log"
write_fake_systemd_active_bin "$SD_BIN30" "$SD_LOG30" "4242"
out30=$( cd "$W30" && PATH="$SD_BIN30:$TEST_PATH" LOOM_SYSTEMD_FORCE=1 LOOM_DAEMON_SYSTEMD=0 \
    LOOM_SYSTEMD_UNIT="loom-daemon-test-sd30.service" \
    LOOM_DAEMON_BIN="$INSTALLED30" NEW_FAKE_BIN_SRC="$NEW_FAKE30" \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc30=$?
assert_eq "0" "$rc30" "LOOM_DAEMON_SYSTEMD=0 update exits 0 (rebuild+provision, no systemd)"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out30" | grep -qi 'not running' && [[ ! -s "$RESTART_MARKER30" ]] && [[ ! -s "$SD_LOG30" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} LOOM_DAEMON_SYSTEMD=0 skips systemd tiers (no restart driven, zero systemctl calls)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} LOOM_DAEMON_SYSTEMD=0 skips systemd tiers (no restart driven, zero systemctl calls)"
    echo "  output: $out30"
    echo "  systemctl calls: $(cat "$SD_LOG30" 2>/dev/null)"
fi

# ============================================================
# 31. --relaunch re-renders the unit with the SUPERVISED keys (mirrors test
#     21): after a refused restart, `--relaunch` re-renders via
#     loom-daemon-start.sh, installing Restart=on-success +
#     LOOM_DAEMON_SUPERVISOR=systemd into the unit — the two keys the stale
#     pre-#4267 fixture unit LACKS, so a passing assertion proves the
#     re-render actually happened (not a leftover). HOME is sandboxed so
#     resolve_systemd_unit_path() resolves inside the test tree.
# ============================================================
W31="$BASE_WORKDIR/w31"
new_fixture "$W31"
INSTALLED31="$W31/installed/loom-daemon"
mkdir -p "$W31/installed"
RESTART_MARKER31="$W31/restart-invoked"
# Running (old) + fresh binaries both REFUSE restart (pre-#4267) -> exit-6 path.
write_fake_daemon_restart "$INSTALLED31" "deadbee" "$RESTART_MARKER31" 1
NEW_FAKE31="$W31/new-fake-daemon"
HEAD31="$(cd "$W31" && git rev-parse --short HEAD)"
write_fake_daemon_restart "$NEW_FAKE31" "$HEAD31" "$RESTART_MARKER31" 1
UNIT31="loom-daemon-test-sd31.service"
SD_BIN31="$W31/systemd-bin"
SD_LOG31="$W31/systemctl.log"
write_fake_systemd_active_bin "$SD_BIN31" "$SD_LOG31" "4242"
HOME31="$W31/home"
UNIT_PATH31="$HOME31/.config/systemd/user/${UNIT31}"
write_fixture_unit_pre4267 "$UNIT_PATH31" "$INSTALLED31"
( cd "$W31" && PATH="$SD_BIN31:$TEST_PATH" HOME="$HOME31" LOOM_SYSTEMD_FORCE=1 LOOM_DAEMON_SYSTEMD=1 \
    LOOM_SYSTEMD_UNIT="$UNIT31" \
    LOOM_DAEMON_BIN="$INSTALLED31" NEW_FAKE_BIN_SRC="$NEW_FAKE31" \
    bash "$UPDATE_SCRIPT" --relaunch >/dev/null 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if grep -q 'LOOM_DAEMON_SUPERVISOR=systemd' "$UNIT_PATH31" 2>/dev/null \
    && grep -qx 'Restart=on-success' "$UNIT_PATH31" 2>/dev/null \
    && grep -qx 'KillMode=mixed' "$UNIT_PATH31" 2>/dev/null; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --relaunch re-renders the unit with Restart=on-success + KillMode=mixed (#4862) + LOOM_DAEMON_SUPERVISOR=systemd"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --relaunch re-renders the unit with Restart=on-success + KillMode=mixed + LOOM_DAEMON_SUPERVISOR=systemd"
    echo "  unit: $(cat "$UNIT_PATH31" 2>/dev/null)"
fi

# ============================================================
# 32. --relaunch PRESERVES the live unit's autonomy env across the re-render
#     (mirrors test 22): the four autonomy keys carry through byte-for-byte,
#     and the stale sentinel PATH is NOT round-tripped (start.sh rebuilds
#     PATH).
# ============================================================
W32="$BASE_WORKDIR/w32"
new_fixture "$W32"
INSTALLED32="$W32/installed/loom-daemon"
mkdir -p "$W32/installed"
RESTART_MARKER32="$W32/restart-invoked"
write_fake_daemon_restart "$INSTALLED32" "deadbee" "$RESTART_MARKER32" 1
NEW_FAKE32="$W32/new-fake-daemon"
HEAD32="$(cd "$W32" && git rev-parse --short HEAD)"
write_fake_daemon_restart "$NEW_FAKE32" "$HEAD32" "$RESTART_MARKER32" 1
UNIT32="loom-daemon-test-sd32.service"
SD_BIN32="$W32/systemd-bin"
SD_LOG32="$W32/systemctl.log"
write_fake_systemd_active_bin "$SD_BIN32" "$SD_LOG32" "4242"
HOME32="$W32/home"
UNIT_PATH32="$HOME32/.config/systemd/user/${UNIT32}"
write_fixture_unit_pre4267 "$UNIT_PATH32" "$INSTALLED32"
( cd "$W32" && PATH="$SD_BIN32:$TEST_PATH" HOME="$HOME32" LOOM_SYSTEMD_FORCE=1 LOOM_DAEMON_SYSTEMD=1 \
    LOOM_SYSTEMD_UNIT="$UNIT32" \
    LOOM_DAEMON_BIN="$INSTALLED32" NEW_FAKE_BIN_SRC="$NEW_FAKE32" \
    bash "$UPDATE_SCRIPT" --relaunch >/dev/null 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if grep -qx 'Environment=LOOM_WORK_FINDER=1' "$UNIT_PATH32" 2>/dev/null \
    && grep -qx 'Environment=LOOM_MAIN_HEALTH_GATE=1' "$UNIT_PATH32" 2>/dev/null \
    && grep -qx 'Environment=LOOM_WORK_FINDER_MAX_CONCURRENT=10' "$UNIT_PATH32" 2>/dev/null \
    && grep -qx 'Environment=LOOM_PER_TOKEN_CONCURRENCY=5' "$UNIT_PATH32" 2>/dev/null \
    && ! grep -q 'sentinel-oldpath-4267' "$UNIT_PATH32" 2>/dev/null; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --relaunch preserves all 4 autonomy env keys and does NOT round-trip the stale PATH"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --relaunch preserves all 4 autonomy env keys and does NOT round-trip the stale PATH"
    echo "  unit: $(cat "$UNIT_PATH32" 2>/dev/null)"
fi

# ============================================================
# 33. --no-restart on a systemd host does NOT print a bare 'systemctl --user
#     stop' of the active unit: it names the --relaunch re-render path and
#     warns that stop kills in-flight sweeps (mirrors test 23).
# ============================================================
W33="$BASE_WORKDIR/w33"
new_fixture "$W33"
INSTALLED33="$W33/installed/loom-daemon"
mkdir -p "$W33/installed"
RESTART_MARKER33="$W33/restart-invoked"
write_fake_daemon_restart "$INSTALLED33" "deadbee" "$RESTART_MARKER33" 0
NEW_FAKE33="$W33/new-fake-daemon"
HEAD33="$(cd "$W33" && git rev-parse --short HEAD)"
write_fake_daemon_restart "$NEW_FAKE33" "$HEAD33" "$RESTART_MARKER33" 0
SD_BIN33="$W33/systemd-bin"
SD_LOG33="$W33/systemctl.log"
write_fake_systemd_active_bin "$SD_BIN33" "$SD_LOG33" "4242"
out33=$( cd "$W33" && PATH="$SD_BIN33:$TEST_PATH" LOOM_SYSTEMD_FORCE=1 LOOM_DAEMON_SYSTEMD=1 \
    LOOM_SYSTEMD_UNIT="loom-daemon-test-sd33.service" \
    LOOM_DAEMON_BIN="$INSTALLED33" NEW_FAKE_BIN_SRC="$NEW_FAKE33" \
    bash "$UPDATE_SCRIPT" --no-restart 2>&1 )
rc33=$?
assert_eq "0" "$rc33" "--no-restart on a systemd host exits 0"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out33" | grep -q -- '--relaunch' \
    && ! echo "$out33" | grep -qi 'systemctl --user restart' \
    && ! echo "$out33" | grep -qi 'systemctl --user enable' \
    && echo "$out33" | grep -qi 'cgroup' && echo "$out33" | grep -qi 'sweep'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --no-restart names --relaunch, no bare manual systemctl restart/enable, warns cgroup teardown kills sweeps"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --no-restart names --relaunch, no bare manual systemctl restart/enable, warns cgroup teardown kills sweeps"
    echo "  output: $out33"
fi

# ============================================================
# 34. --relaunch with the live unit file ABSENT fails loudly (names the
#     missing path) and refuses to relaunch — it must NEVER silently render
#     FLAGS-OFF defaults (the #4011 class this whole line of work closes). No
#     unit is written (mirrors test 24).
# ============================================================
W34="$BASE_WORKDIR/w34"
new_fixture "$W34"
INSTALLED34="$W34/installed/loom-daemon"
mkdir -p "$W34/installed"
RESTART_MARKER34="$W34/restart-invoked"
write_fake_daemon_restart "$INSTALLED34" "deadbee" "$RESTART_MARKER34" 1
NEW_FAKE34="$W34/new-fake-daemon"
HEAD34="$(cd "$W34" && git rev-parse --short HEAD)"
write_fake_daemon_restart "$NEW_FAKE34" "$HEAD34" "$RESTART_MARKER34" 1
UNIT34="loom-daemon-test-sd34.service"
SD_BIN34="$W34/systemd-bin"
SD_LOG34="$W34/systemctl.log"
write_fake_systemd_active_bin "$SD_BIN34" "$SD_LOG34" "4242"
HOME34="$W34/home"
mkdir -p "$HOME34/.config/systemd/user"   # dir exists, unit deliberately absent
UNIT_PATH34="$HOME34/.config/systemd/user/${UNIT34}"
out34=$( cd "$W34" && PATH="$SD_BIN34:$TEST_PATH" HOME="$HOME34" LOOM_SYSTEMD_FORCE=1 LOOM_DAEMON_SYSTEMD=1 \
    LOOM_SYSTEMD_UNIT="$UNIT34" \
    LOOM_DAEMON_BIN="$INSTALLED34" NEW_FAKE_BIN_SRC="$NEW_FAKE34" \
    bash "$UPDATE_SCRIPT" --relaunch 2>&1 )
rc34=$?
TESTS_RUN=$((TESTS_RUN + 1))
if [[ "$rc34" -ne 0 ]] && echo "$out34" | grep -qi 'not found' \
    && echo "$out34" | grep -qF "$UNIT_PATH34" && [[ ! -e "$UNIT_PATH34" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --relaunch with an absent unit fails loudly (names the path) and renders nothing"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --relaunch with an absent unit fails loudly (names the path) and renders nothing"
    echo "  rc=$rc34 unit-exists=$([[ -e "$UNIT_PATH34" ]] && echo yes || echo no)"
    echo "  output: $out34"
fi

# ============================================================
# 35. Restart ack'd but launchd never relaunches -> kickstart fallback DOES
#     relaunch it (#4232 case (b)): the restart request is accepted (exit 0)
#     but the reported pid never moves off the pre-restart pid on its own;
#     the updater falls back to a PLAIN 'launchctl kickstart' (never -k),
#     which (per the stub) IS what finally moves the pid — success, with a
#     remediation note in the output. Poll windows are shrunk via env so the
#     test runs fast without changing the production defaults.
# ============================================================
W35="$BASE_WORKDIR/w35"
new_fixture "$W35"
INSTALLED35="$W35/installed/loom-daemon"
mkdir -p "$W35/installed"
RESTART_MARKER35="$W35/restart-invoked"
write_fake_daemon_restart "$INSTALLED35" "deadbee" "$RESTART_MARKER35" 0
NEW_FAKE35="$W35/new-fake-daemon"
HEAD35="$(cd "$W35" && git rev-parse --short HEAD)"
write_fake_daemon_restart "$NEW_FAKE35" "$HEAD35" "$RESTART_MARKER35" 0

sleep 60 >/dev/null 2>&1 &
OLD_PID26=$!
bg_proc_track "$OLD_PID26"
sleep 60 >/dev/null 2>&1 &
KICKSTART_PID35=$!
bg_proc_track "$KICKSTART_PID35"
STATE35="$W35/launchd-pid-state"
echo "$OLD_PID26" > "$STATE35"
LD_BIN35="$W35/launchd-bin"
write_fake_launchd_pid_bin "$LD_BIN35" "$W35/launchctl.log" "$STATE35" "$KICKSTART_PID35"

out35=$( cd "$W35" && PATH="$LD_BIN35:$TEST_PATH" LOOM_DAEMON_LAUNCHD=1 \
    LOOM_DAEMON_BIN="$INSTALLED35" NEW_FAKE_BIN_SRC="$NEW_FAKE35" \
    LOOM_DAEMON_RESTART_POLL_SECS=1 LOOM_DAEMON_RESTART_POLL_INTERVAL=0.2 \
    LOOM_DAEMON_RESTART_KICKSTART_POLL_SECS=3 \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc26=$?
kill "$OLD_PID26" "$KICKSTART_PID35" 2>/dev/null || true
assert_eq "0" "$rc26" "no spontaneous relaunch -> kickstart fallback relaunches it -> exit 0 (#4232 case b)"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out35" | grep -qi 'kickstart' && echo "$out35" | grep -qi 'remediation'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} success output names the kickstart fallback + a remediation note"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} success output names the kickstart fallback + a remediation note"
    echo "  output: $out35"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if grep -qE '^kickstart [^ ]+$' "$W35/launchctl.log" 2>/dev/null; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} kickstart is invoked PLAIN — never with -k"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} kickstart is invoked PLAIN — never with -k"
    echo "  launchctl.log: $(cat "$W35/launchctl.log" 2>/dev/null)"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if grep -q -- '-k' "$W35/launchctl.log" 2>/dev/null; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} kickstart is NEVER invoked with -k (would risk killing a daemon that relaunched during the race window)"
    echo "  launchctl.log: $(cat "$W35/launchctl.log" 2>/dev/null)"
else
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} kickstart is NEVER invoked with -k (would risk killing a daemon that relaunched during the race window)"
fi

# ============================================================
# 36. Restart ack'd, launchd never relaunches, AND the kickstart fallback also
#     never brings it up -> exit NON-ZERO (7), loudly, rather than a silent
#     half-update success (#4232 case (c)).
# ============================================================
W36="$BASE_WORKDIR/w36"
new_fixture "$W36"
INSTALLED36="$W36/installed/loom-daemon"
mkdir -p "$W36/installed"
RESTART_MARKER36="$W36/restart-invoked"
write_fake_daemon_restart "$INSTALLED36" "deadbee" "$RESTART_MARKER36" 0
NEW_FAKE36="$W36/new-fake-daemon"
HEAD36="$(cd "$W36" && git rev-parse --short HEAD)"
write_fake_daemon_restart "$NEW_FAKE36" "$HEAD36" "$RESTART_MARKER36" 0

sleep 60 >/dev/null 2>&1 &
OLD_PID27=$!
bg_proc_track "$OLD_PID27"
STATE36="$W36/launchd-pid-state"
echo "$OLD_PID27" > "$STATE36"
LD_BIN36="$W36/launchd-bin"
# No kickstart_new_pid given -> kickstart is a no-op; the pid never moves.
write_fake_launchd_pid_bin "$LD_BIN36" "$W36/launchctl.log" "$STATE36"

out36=$( cd "$W36" && PATH="$LD_BIN36:$TEST_PATH" LOOM_DAEMON_LAUNCHD=1 \
    LOOM_DAEMON_BIN="$INSTALLED36" NEW_FAKE_BIN_SRC="$NEW_FAKE36" \
    LOOM_DAEMON_RESTART_POLL_SECS=1 LOOM_DAEMON_RESTART_POLL_INTERVAL=0.2 \
    LOOM_DAEMON_RESTART_KICKSTART_POLL_SECS=1 \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc27=$?
kill "$OLD_PID27" 2>/dev/null || true
assert_eq "7" "$rc27" "no relaunch even after kickstart -> exit 7 (never a silent half-update, #4232 case c)"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out36" | grep -qi 'FAILED' && echo "$out36" | grep -qi 'kickstart'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} failure output loudly reports the exhausted kickstart fallback"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} failure output loudly reports the exhausted kickstart fallback"
    echo "  output: $out36"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out36" | grep -qi 'last exit status'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} failure output includes launchctl print diagnostics (state/last exit status)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} failure output includes launchctl print diagnostics (state/last exit status)"
    echo "  output: $out36"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if grep -q -- '-k' "$W36/launchctl.log" 2>/dev/null; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} exhausted kickstart fallback was still never invoked with -k"
else
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} exhausted kickstart fallback was still never invoked with -k"
fi

# ============================================================
# 37. ff-first default (#4330): local main is behind origin/main with a
#     CLEAN tree -> the ff-merge applies, and the rebuild below builds the
#     freshly-synced (post-merge) HEAD, not the stale pre-merge one.
# ============================================================
W37="$BASE_WORKDIR/w37"
BARE37="$BASE_WORKDIR/w37-origin.git"
new_fixture_with_origin "$W37" "$BARE37"
ORIGIN_TIP37="$(push_extra_commits_to_origin "$BARE37" 2)"
NEW_FAKE37="$W37/new-fake-daemon"
write_fake_daemon "$NEW_FAKE37" "$ORIGIN_TIP37" "$W37/new-marker"
out37=$( cd "$W37" && PATH="$TEST_PATH" NEW_FAKE_BIN_SRC="$NEW_FAKE37" \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc37=$?
assert_eq "0" "$rc37" "ff-first: behind origin + clean tree -> update succeeds (ff-merge applied)"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out37" | grep -qi 'Fast-forwarded'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} ff-first: reports the fast-forward sync"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} ff-first: reports the fast-forward sync"
    echo "  output: $out37"
fi
HEAD_AFTER37="$(cd "$W37" && git rev-parse --short HEAD)"
assert_eq "$ORIGIN_TIP37" "$HEAD_AFTER37" "ff-first: local HEAD now equals origin tip after the sync"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out37" | grep -qi "Build verification: freshly-built binary embeds source HEAD commit (${ORIGIN_TIP37})"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} ff-first: the rebuild is verified against the POST-merge HEAD, not the stale pre-merge one"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} ff-first: the rebuild is verified against the POST-merge HEAD, not the stale pre-merge one"
    echo "  output: $out37"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out37" | grep -qE "Installed: ${ORIGIN_TIP37} \(matches origin/main\)"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} final installed line states the built commit AND that it matches origin/main (AC4)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} final installed line states the built commit AND that it matches origin/main (AC4)"
    echo "  output: $out37"
fi

# ============================================================
# 38. ff-first default: local main is behind origin/main AND has a DIVERGED
#     local commit whose CONTENT genuinely differs from origin's (not merely
#     its commit history) -> the ff-merge cannot apply -> abort exit 1, no
#     merge, no build, working tree untouched. Never guesses or hard-resets.
#     This is the #4951 regression case: real (non-empty) content divergence
#     must NOT be reclassified as the content-identical case covered by tests
#     57/58 below — both sides here add a real, DIFFERENT file, so
#     `git diff origin/main...main` is genuinely non-empty.
# ============================================================
W38="$BASE_WORKDIR/w38"
BARE38="$BASE_WORKDIR/w38-origin.git"
new_fixture_with_origin "$W38" "$BARE38"
TMPCLONE38="$(mktemp -d)"
git clone -q "$BARE38" "$TMPCLONE38"
echo "origin-value" > "$TMPCLONE38/origin-only-file.txt"
( cd "$TMPCLONE38" && git add origin-only-file.txt && git -c user.email=test@test -c user.name=test commit -q -m "origin real change" && git push -q origin HEAD:refs/heads/main )
rm -rf "$TMPCLONE38"
# Diverge: a local commit that is NOT on origin, adding DIFFERENT real
# content (so the merge is not a plain fast-forward, AND the resulting
# content diff vs origin is genuinely non-empty).
echo "local-value" > "$W38/local-only-file.txt"
( cd "$W38" && git add local-only-file.txt && git -c user.email=test@test -c user.name=test commit -q -m "local diverged commit (real content)" )
HEAD_BEFORE38="$(cd "$W38" && git rev-parse --short HEAD)"
out38=$( cd "$W38" && PATH="$TEST_PATH" bash "$UPDATE_SCRIPT" 2>&1 )
rc38=$?
assert_eq "1" "$rc38" "ff-first: diverged local commit -> abort exit 1 (never guess or hard-reset)"
HEAD_AFTER38="$(cd "$W38" && git rev-parse --short HEAD)"
assert_eq "$HEAD_BEFORE38" "$HEAD_AFTER38" "ff-first: diverged case leaves local HEAD completely untouched"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out38" | grep -qi 'Refusing to guess or hard-reset'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} ff-first: diverged case reports the abort rationale"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} ff-first: diverged case reports the abort rationale"
    echo "  output: $out38"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if [[ -e "$W38/loom-daemon/target/release/loom-daemon" ]]; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} ff-first: diverged case performs no build"
else
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} ff-first: diverged case performs no build"
fi

# ============================================================
# 39. --allow-stale restores the pre-#4330 build-what's-here behavior: the
#     ff-merge is skipped entirely, the current (stale) checkout is built,
#     and the behind-origin advisory warning is still printed.
# ============================================================
W39="$BASE_WORKDIR/w39"
BARE39="$BASE_WORKDIR/w39-origin.git"
new_fixture_with_origin "$W39" "$BARE39"
push_extra_commits_to_origin "$BARE39" 3 >/dev/null
HEAD_BEFORE39="$(cd "$W39" && git rev-parse --short HEAD)"
NEW_FAKE39="$W39/new-fake-daemon"
write_fake_daemon "$NEW_FAKE39" "$HEAD_BEFORE39" "$W39/new-marker"
out39=$( cd "$W39" && PATH="$TEST_PATH" NEW_FAKE_BIN_SRC="$NEW_FAKE39" \
    bash "$UPDATE_SCRIPT" --allow-stale 2>&1 )
rc39=$?
assert_eq "0" "$rc39" "--allow-stale: builds the current (stale) checkout, exits 0"
HEAD_AFTER39="$(cd "$W39" && git rev-parse --short HEAD)"
assert_eq "$HEAD_BEFORE39" "$HEAD_AFTER39" "--allow-stale: local HEAD is never merged/advanced"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out39" | grep -qi 'behind origin/main.*--allow-stale'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --allow-stale: the behind-origin advisory warning is still printed"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --allow-stale: the behind-origin advisory warning is still printed"
    echo "  output: $out39"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out39" | grep -qE "Installed: ${HEAD_BEFORE39} \(origin/main is at .* does NOT match"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --allow-stale: final installed line reports the built commit does NOT match origin/main (AC4)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --allow-stale: final installed line reports the built commit does NOT match origin/main (AC4)"
    echo "  output: $out39"
fi

# ============================================================
# 40. --check / --dry-run stay read-only even when behind origin: no fetch
#     result may write to the tree — HEAD is unchanged after either mode, and
#     --check reports the behind-origin status informationally while
#     --dry-run's printed plan mentions the ff-sync.
# ============================================================
W40="$BASE_WORKDIR/w40"
BARE40="$BASE_WORKDIR/w40-origin.git"
new_fixture_with_origin "$W40" "$BARE40"
push_extra_commits_to_origin "$BARE40" 1 >/dev/null
HEAD_BEFORE40="$(cd "$W40" && git rev-parse --short HEAD)"
out40_check=$( cd "$W40" && PATH="$TEST_PATH" bash "$UPDATE_SCRIPT" --check 2>&1 )
HEAD_AFTER40_CHECK="$(cd "$W40" && git rev-parse --short HEAD)"
assert_eq "$HEAD_BEFORE40" "$HEAD_AFTER40_CHECK" "--check while behind origin: HEAD unchanged (no writes contract holds)"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out40_check" | grep -qi 'behind origin/main'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --check reports the behind-origin status informationally"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --check reports the behind-origin status informationally"
    echo "  output: $out40_check"
fi
out40_dry=$( cd "$W40" && PATH="$TEST_PATH" bash "$UPDATE_SCRIPT" --dry-run 2>&1 )
HEAD_AFTER40_DRY="$(cd "$W40" && git rev-parse --short HEAD)"
assert_eq "$HEAD_BEFORE40" "$HEAD_AFTER40_DRY" "--dry-run while behind origin: HEAD unchanged (no writes contract holds)"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out40_dry" | grep -qi 'fast-forward.*origin/main'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --dry-run's printed plan mentions the ff-sync"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --dry-run's printed plan mentions the ff-sync"
    echo "  output: $out40_dry"
fi

# ============================================================
# 41. HEAD on a non-default branch while behind origin -> abort, naming
#     --allow-stale (an operator deliberately elsewhere — e.g. bisecting — is
#     exactly the --allow-stale use case, never a guess-which-branch merge).
# ============================================================
W41="$BASE_WORKDIR/w41"
BARE41="$BASE_WORKDIR/w41-origin.git"
new_fixture_with_origin "$W41" "$BARE41"
push_extra_commits_to_origin "$BARE41" 1 >/dev/null
( cd "$W41" && git checkout -q -b feature-branch )
HEAD_BEFORE41="$(cd "$W41" && git rev-parse --short HEAD)"
out41=$( cd "$W41" && PATH="$TEST_PATH" bash "$UPDATE_SCRIPT" 2>&1 )
rc41=$?
assert_eq "1" "$rc41" "non-default branch while behind -> abort exit 1"
HEAD_AFTER41="$(cd "$W41" && git rev-parse --short HEAD)"
assert_eq "$HEAD_BEFORE41" "$HEAD_AFTER41" "non-default branch while behind: HEAD untouched"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out41" | grep -q -- '--allow-stale'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} abort message names --allow-stale as the deliberate escape hatch"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} abort message names --allow-stale as the deliberate escape hatch"
    echo "  output: $out41"
fi

# ============================================================
# 42. Fetch failure (origin unreachable) must NOT make the script
#     network-dependent: warn and proceed with local HEAD as-is, exactly like
#     today's behavior — never abort, never hang past the bounded fetch.
# ============================================================
W42="$BASE_WORKDIR/w42"
BARE42="$BASE_WORKDIR/w42-origin.git"
new_fixture_with_origin "$W42" "$BARE42"
# refs/remotes/origin/HEAD is now cached locally (set by new_fixture_with_origin
# above), so loom_default_branch() can still name "main" via its offline,
# no-network tier even though the URL below makes the ACTUAL fetch fail —
# isolating "fetch failed" from "branch unresolvable" (test 43 covers the latter).
( cd "$W42" && git remote set-url origin "$BASE_WORKDIR/does-not-exist-w42.git" )
HEAD42="$(cd "$W42" && git rev-parse --short HEAD)"
INSTALLED42="$W42/installed/loom-daemon"
mkdir -p "$W42/installed"
write_fake_daemon "$INSTALLED42" "$HEAD42" "$W42/old-marker"
out42=$( cd "$W42" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$INSTALLED42" \
    bash "$UPDATE_SCRIPT" --check 2>&1 )
rc42=$?
assert_eq "0" "$rc42" "fetch failure: proceeds as today (installed already matches local HEAD) -> exit 0"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out42" | grep -qi 'could not reach origin'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} fetch failure surfaces a warning instead of aborting"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} fetch failure surfaces a warning instead of aborting"
    echo "  output: $out42"
fi

# ============================================================
# 43. Signaling "unknown currency" honestly: a fixture with NO origin remote
#     at all (loom_default_branch cannot resolve a default branch) still
#     completes normally, and the final installed line says so explicitly
#     rather than silently omitting the currency claim.
# ============================================================
W43="$BASE_WORKDIR/w43"
new_fixture "$W43"
HEAD43="$(cd "$W43" && git rev-parse --short HEAD)"
NEW_FAKE43="$W43/new-fake-daemon"
write_fake_daemon "$NEW_FAKE43" "$HEAD43" "$W43/new-marker"
out43=$( cd "$W43" && PATH="$TEST_PATH" NEW_FAKE_BIN_SRC="$NEW_FAKE43" \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc43=$?
assert_eq "0" "$rc43" "no origin remote at all: update still succeeds"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out43" | grep -qi 'currency vs origin/<default-branch> unknown'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} final installed line honestly reports unknown currency when origin is unresolvable"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} final installed line honestly reports unknown currency when origin is unresolvable"
    echo "  output: $out43"
fi

# ============================================================
# 45-48. Stale `loom-*` entry-point advisory (#4079 hardening, epic #4081
#     Phase 4 / #4557). The update script scans PATH for `loom-*` executables
#     that do not resolve to the loom-daemon binary it resolved, and warns.
#
#     Fixture PATH holds, in one directory:
#       - loom-daemon           : the resolved binary itself (never flagged)
#       - loom-clean            : a legit auto-generated shim exec'ing the
#                                 sibling loom-daemon (never flagged)
#       - loom-tokens           : a STALE pip console script (MUST be flagged)
#       - loom-agent-spawn      : a second stale pip console script (flagged)
#       - loom-search           : a third stale console script (flagged) — the
#                                 `loom-search` carve-out was itself retired in
#                                 #4970, so it is no longer allowlisted and a
#                                 leftover binary is exactly the #4079 shape.
#
#     LOOM_SKIP_STALE_ENTRY_POINT_CHECK=1 must silence all of it, and the
#     advisory must never change the exit code.
# ============================================================
W45="$BASE_WORKDIR/w45"
new_fixture "$W45"
HEAD45="$(cd "$W45" && git rev-parse --short HEAD)"
STALE_BIN_DIR="$W45/stale-bin"
mkdir -p "$STALE_BIN_DIR"

# The resolved daemon binary, on PATH.
write_fake_daemon "$STALE_BIN_DIR/loom-daemon" "$HEAD45" "$W45/marker45"

# A legitimate auto-generated PATH shim (provision-daemon.sh's template).
cat > "$STALE_BIN_DIR/loom-clean" <<'SHIM'
#!/usr/bin/env bash
# Auto-generated PATH shim (issue #4272) — do not edit by hand.
exec "$(dirname "$0")/loom-daemon" clean "$@"
SHIM
chmod +x "$STALE_BIN_DIR/loom-clean"

# Three stale pip/PATH console scripts of the #4079 shape (including
# `loom-search`, no longer allowlisted post-#4970).
for _stale in loom-tokens loom-agent-spawn loom-search; do
    cat > "$STALE_BIN_DIR/$_stale" <<'STALEPY'
#!/usr/bin/python3
# -*- coding: utf-8 -*-
import sys
sys.exit(0)
STALEPY
    chmod +x "$STALE_BIN_DIR/$_stale"
done

out45=$( cd "$W45" && PATH="$STALE_BIN_DIR:$TEST_PATH" \
    LOOM_DAEMON_BIN="$STALE_BIN_DIR/loom-daemon" \
    bash "$UPDATE_SCRIPT" --check 2>&1; echo "EXIT=$?" )
rc45=$(echo "$out45" | grep -o 'EXIT=[0-9]*' | cut -d= -f2)

# 45. The three stale console scripts are named.
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out45" | grep -q "Stale 'loom-\*' entry points found on PATH" \
   && echo "$out45" | grep -q "$STALE_BIN_DIR/loom-tokens" \
   && echo "$out45" | grep -q "$STALE_BIN_DIR/loom-agent-spawn" \
   && echo "$out45" | grep -q "$STALE_BIN_DIR/loom-search"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} stale loom-* PATH entry points are warned about by path (#4079/#4557/#4970)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} stale loom-* PATH entry points are warned about by path (#4079/#4557/#4970)"
    echo "  output: $out45"
fi

# 46. The legit shim and the resolved binary are NOT flagged (a false positive
#     here would train operators to ignore the check).
TESTS_RUN=$((TESTS_RUN + 1))
_stale_block="$(echo "$out45" | sed -n "/Stale 'loom-\*' entry points/,/Suppress this check/p")"
if ! echo "$_stale_block" | grep -q 'loom-clean' \
   && ! echo "$_stale_block" | grep -qE '(^|/)loom-daemon —'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} the current shim and the resolved binary are not flagged as stale"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} the current shim and the resolved binary are not flagged as stale"
    echo "  stale block: $_stale_block"
fi

# 47. The advisory does NOT change the exit code (--check with a matching commit
#     is still a clean exit 0).
assert_eq "0" "$rc45" "the stale-entry-point advisory never changes the exit code"

# 48. LOOM_SKIP_STALE_ENTRY_POINT_CHECK=1 silences it entirely.
out48=$( cd "$W45" && PATH="$STALE_BIN_DIR:$TEST_PATH" \
    LOOM_DAEMON_BIN="$STALE_BIN_DIR/loom-daemon" \
    LOOM_SKIP_STALE_ENTRY_POINT_CHECK=1 \
    bash "$UPDATE_SCRIPT" --check 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if ! echo "$out48" | grep -q "Stale 'loom-\*' entry points"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} LOOM_SKIP_STALE_ENTRY_POINT_CHECK=1 suppresses the advisory"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} LOOM_SKIP_STALE_ENTRY_POINT_CHECK=1 suppresses the advisory"
    echo "  output: $out48"
fi

# ============================================================
# 49-53. Idle-shutdown cron-guard post-update notice (#4697): the update
#     script should warn, post-update, that this host will power itself off
#     after N idle minutes when the stage-2 `fleet add-worker
#     --idle-shutdown-minutes` cron guard is installed — and stay silent
#     when it is not. Uses --check (fast, no rebuild) exactly like the
#     stale-entry-point block above; a sandboxed $HOME49 keeps the
#     `$HOME/.local/bin/loom-idle-shutdown.sh` lookup off the real operator
#     account (belt-and-braces alongside the fake `crontab` stub above).
# ============================================================
W49="$BASE_WORKDIR/w49"
new_fixture "$W49"
HEAD49="$(cd "$W49" && git rev-parse --short HEAD)"
write_fake_daemon "$W49/resolved-daemon" "$HEAD49" "$W49/marker49"

HOME49="$BASE_WORKDIR/home49"
mkdir -p "$HOME49/.local/bin"
cat > "$HOME49/.local/bin/loom-idle-shutdown.sh" <<'GUARD'
#!/usr/bin/env bash
set -euo pipefail
LIMIT=45
GUARD
chmod +x "$HOME49/.local/bin/loom-idle-shutdown.sh"

CRONFIX49="$BASE_WORKDIR/cron49-with-guard"
echo "*/5 * * * * $HOME49/.local/bin/loom-idle-shutdown.sh >/dev/null 2>&1" > "$CRONFIX49"

# 49. Guard installed + configured window readable -> notice names "45".
out49=$( cd "$W49" && PATH="$TEST_PATH" HOME="$HOME49" \
    LOOM_DAEMON_BIN="$W49/resolved-daemon" \
    FAKE_CRONTAB_CONTENTS_FILE="$CRONFIX49" \
    bash "$UPDATE_SCRIPT" --check 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out49" | grep -q 'idle-shutdown cron guard installed' \
   && echo "$out49" | grep -q -- '--idle-shutdown-minutes 45)'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} idle-shutdown guard installed -> post-update notice names the configured minutes (#4697)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} idle-shutdown guard installed -> post-update notice names the configured minutes (#4697)"
    echo "  output: $out49"
fi

# 50. No guard installed (empty crontab fixture) -> completely silent.
CRONFIX50="$BASE_WORKDIR/cron50-empty"
: > "$CRONFIX50"
out50=$( cd "$W49" && PATH="$TEST_PATH" HOME="$HOME49" \
    LOOM_DAEMON_BIN="$W49/resolved-daemon" \
    FAKE_CRONTAB_CONTENTS_FILE="$CRONFIX50" \
    bash "$UPDATE_SCRIPT" --check 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if ! echo "$out50" | grep -qi 'idle-shutdown'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} no idle-shutdown guard installed -> no notice at all (#4697)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} no idle-shutdown guard installed -> no notice at all (#4697)"
    echo "  output: $out50"
fi

# 51. LOOM_SKIP_IDLE_SHUTDOWN_NOTICE=1 suppresses it even with the guard present.
out51=$( cd "$W49" && PATH="$TEST_PATH" HOME="$HOME49" \
    LOOM_DAEMON_BIN="$W49/resolved-daemon" \
    FAKE_CRONTAB_CONTENTS_FILE="$CRONFIX49" \
    LOOM_SKIP_IDLE_SHUTDOWN_NOTICE=1 \
    bash "$UPDATE_SCRIPT" --check 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if ! echo "$out51" | grep -qi 'idle-shutdown'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} LOOM_SKIP_IDLE_SHUTDOWN_NOTICE=1 suppresses the notice"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} LOOM_SKIP_IDLE_SHUTDOWN_NOTICE=1 suppresses the notice"
    echo "  output: $out51"
fi

# 52. The notice never changes the exit code (still exit 0 -- up to date).
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out49" | grep -q . && ( cd "$W49" && PATH="$TEST_PATH" HOME="$HOME49" \
    LOOM_DAEMON_BIN="$W49/resolved-daemon" \
    FAKE_CRONTAB_CONTENTS_FILE="$CRONFIX49" \
    bash "$UPDATE_SCRIPT" --check >/dev/null 2>&1 ); then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} the idle-shutdown notice never changes the exit code"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} the idle-shutdown notice never changes the exit code"
fi

# 53. Guard entry present in cron but the guard SCRIPT (and thus its LIMIT=)
#     unreadable -- e.g. a hand-installed or relocated guard. The notice must
#     still fire (the host still powers itself off) with the honest
#     "window unknown" wording rather than silently degrading to nothing or
#     inventing a minutes value. $HOME53 has no .local/bin/loom-idle-shutdown.sh.
HOME53="$BASE_WORKDIR/home53"
mkdir -p "$HOME53/.local/bin"
out53=$( cd "$W49" && PATH="$TEST_PATH" HOME="$HOME53" \
    LOOM_DAEMON_BIN="$W49/resolved-daemon" \
    FAKE_CRONTAB_CONTENTS_FILE="$CRONFIX49" \
    bash "$UPDATE_SCRIPT" --check 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out53" | grep -q 'idle-shutdown cron guard installed' \
   && echo "$out53" | grep -q 'could not be read from'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} unreadable guard script -> notice still fires with an honest unknown window (#4697)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} unreadable guard script -> notice still fires with an honest unknown window (#4697)"
    echo "  output: $out53"
fi

# ============================================================
# 54. Systemd restart ack'd but the unit never relaunches on its own AND lands
#     in 'failed (Result: timeout)' -> the #4950 self-heal ('systemctl --user
#     reset-failed <unit> && systemctl --user start <unit>') DOES recover it:
#     the restart request is accepted (exit 0) but the reported MainPID never
#     moves off the pre-restart pid until AFTER reset-failed+start are invoked
#     -- success, with a remediation note in the output. This is the exact
#     2026-08-02 incident shape this issue closes. Poll windows are shrunk via
#     env so the test runs fast without changing the production defaults.
# ============================================================
W54="$BASE_WORKDIR/w54"
new_fixture "$W54"
INSTALLED54="$W54/installed/loom-daemon"
mkdir -p "$W54/installed"
RESTART_MARKER54="$W54/restart-invoked"
write_fake_daemon_restart "$INSTALLED54" "deadbee" "$RESTART_MARKER54" 0
NEW_FAKE54="$W54/new-fake-daemon"
HEAD54="$(cd "$W54" && git rev-parse --short HEAD)"
write_fake_daemon_restart "$NEW_FAKE54" "$HEAD54" "$RESTART_MARKER54" 0

sleep 60 >/dev/null 2>&1 &
RECOVERED_PID54=$!
bg_proc_track "$RECOVERED_PID54"
SD_BIN54="$W54/systemd-bin"
SD_LOG54="$W54/systemctl.log"
SD_STATE54="$W54/systemd-pid-state"
# pre_state: OLD pid, active/success. post_state (once restart is requested):
# 0:failed:timeout -- Restart=on-success never fires for a failed unit.
# recovery_state: only reached via reset-failed + start -- a NEW, live pid.
write_fake_systemd_pid_bin "$SD_BIN54" "$SD_LOG54" "$SD_STATE54" "9999:active:success" \
    "$RESTART_MARKER54" "0:failed:timeout" "${RECOVERED_PID54}:active:success"

out54=$( cd "$W54" && PATH="$SD_BIN54:$TEST_PATH" LOOM_SYSTEMD_FORCE=1 LOOM_DAEMON_SYSTEMD=1 \
    LOOM_SYSTEMD_UNIT="loom-daemon-test-sd54.service" \
    LOOM_DAEMON_BIN="$INSTALLED54" NEW_FAKE_BIN_SRC="$NEW_FAKE54" \
    LOOM_DAEMON_RESTART_POLL_SECS=1 LOOM_DAEMON_RESTART_POLL_INTERVAL=0.2 \
    LOOM_DAEMON_RESTART_KICKSTART_POLL_SECS=3 \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc54=$?
kill "$RECOVERED_PID54" 2>/dev/null || true
assert_eq "0" "$rc54" "no spontaneous relaunch, unit failed -> reset-failed+start recovers it -> exit 0 (#4950)"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out54" | grep -qi 'reset-failed' && echo "$out54" | grep -qi 'remediation'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} success output names the reset-failed+start self-heal + a remediation note"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} success output names the reset-failed+start self-heal + a remediation note"
    echo "  output: $out54"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if grep -q -- 'reset-failed' "$SD_LOG54" 2>/dev/null \
    && grep -qE -- '(^|[[:space:]])start([[:space:]]|$)' "$SD_LOG54" 2>/dev/null; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} self-heal invokes the EXACT documented recovery: reset-failed then start"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} self-heal invokes the EXACT documented recovery: reset-failed then start"
    echo "  systemctl.log: $(cat "$SD_LOG54" 2>/dev/null)"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if grep -q "new pid ${RECOVERED_PID54}" <<< "$out54"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} success message reports the pid recovered by the self-heal"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} success message reports the pid recovered by the self-heal"
    echo "  output: $out54"
fi

# ============================================================
# 55. Systemd restart ack'd, unit never relaunches, lands in 'failed
#     (Result: timeout)', AND the reset-failed+start self-heal ALSO fails to
#     bring it up -> exit NON-ZERO (7), loudly, rather than a silent
#     half-update success (#4950 case (c), mirrors test 36).
# ============================================================
W55="$BASE_WORKDIR/w55"
new_fixture "$W55"
INSTALLED55="$W55/installed/loom-daemon"
mkdir -p "$W55/installed"
RESTART_MARKER55="$W55/restart-invoked"
write_fake_daemon_restart "$INSTALLED55" "deadbee" "$RESTART_MARKER55" 0
NEW_FAKE55="$W55/new-fake-daemon"
HEAD55="$(cd "$W55" && git rev-parse --short HEAD)"
write_fake_daemon_restart "$NEW_FAKE55" "$HEAD55" "$RESTART_MARKER55" 0

SD_BIN55="$W55/systemd-bin"
SD_LOG55="$W55/systemctl.log"
SD_STATE55="$W55/systemd-pid-state"
# No recovery_state given -> reset-failed+start leaves the unit inactive; the
# pid never moves.
write_fake_systemd_pid_bin "$SD_BIN55" "$SD_LOG55" "$SD_STATE55" "9999:active:success" \
    "$RESTART_MARKER55" "0:failed:timeout"

out55=$( cd "$W55" && PATH="$SD_BIN55:$TEST_PATH" LOOM_SYSTEMD_FORCE=1 LOOM_DAEMON_SYSTEMD=1 \
    LOOM_SYSTEMD_UNIT="loom-daemon-test-sd55.service" \
    LOOM_DAEMON_BIN="$INSTALLED55" NEW_FAKE_BIN_SRC="$NEW_FAKE55" \
    LOOM_DAEMON_RESTART_POLL_SECS=1 LOOM_DAEMON_RESTART_POLL_INTERVAL=0.2 \
    LOOM_DAEMON_RESTART_KICKSTART_POLL_SECS=1 \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc55=$?
assert_eq "7" "$rc55" "no relaunch even after reset-failed+start -> exit 7 (never a silent half-update, #4950 case c)"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out55" | grep -qi 'FAILED' && echo "$out55" | grep -qi 'reset-failed'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} failure output loudly reports the exhausted self-heal fallback"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} failure output loudly reports the exhausted self-heal fallback"
    echo "  output: $out55"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out55" | grep -qi 'Active:' && echo "$out55" | grep -qi 'Result:'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} failure output includes systemctl status diagnostics (Active:/Result:)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} failure output includes systemctl status diagnostics (Active:/Result:)"
    echo "  output: $out55"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if grep -q -- 'reset-failed' "$SD_LOG55" 2>/dev/null; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} exhausted self-heal still attempted the documented reset-failed recovery"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} exhausted self-heal still attempted the documented reset-failed recovery"
    echo "  systemctl.log: $(cat "$SD_LOG55" 2>/dev/null)"
fi

# ============================================================
# 56. Systemd restart ack'd but never relaunches, and the unit is NOT in a
#     'failed' state (some other stall, e.g. still 'activating') -> the
#     updater refuses to guess at a recovery action (no reset-failed/start
#     invoked) and exits non-zero (7) with diagnostics (#4950 AC2 scope: the
#     self-heal is gated on a CONFIRMED 'failed' ActiveState).
# ============================================================
W56="$BASE_WORKDIR/w56"
new_fixture "$W56"
INSTALLED56="$W56/installed/loom-daemon"
mkdir -p "$W56/installed"
RESTART_MARKER56="$W56/restart-invoked"
write_fake_daemon_restart "$INSTALLED56" "deadbee" "$RESTART_MARKER56" 0
NEW_FAKE56="$W56/new-fake-daemon"
HEAD56="$(cd "$W56" && git rev-parse --short HEAD)"
write_fake_daemon_restart "$NEW_FAKE56" "$HEAD56" "$RESTART_MARKER56" 0

SD_BIN56="$W56/systemd-bin"
SD_LOG56="$W56/systemctl.log"
SD_STATE56="$W56/systemd-pid-state"
# post_state is 'activating', not 'failed' -- the pid never moves, but the
# self-heal gate must NOT fire since the unit was never confirmed failed.
write_fake_systemd_pid_bin "$SD_BIN56" "$SD_LOG56" "$SD_STATE56" "9999:active:success" \
    "$RESTART_MARKER56" "0:activating:success"

out56=$( cd "$W56" && PATH="$SD_BIN56:$TEST_PATH" LOOM_SYSTEMD_FORCE=1 LOOM_DAEMON_SYSTEMD=1 \
    LOOM_SYSTEMD_UNIT="loom-daemon-test-sd56.service" \
    LOOM_DAEMON_BIN="$INSTALLED56" NEW_FAKE_BIN_SRC="$NEW_FAKE56" \
    LOOM_DAEMON_RESTART_POLL_SECS=1 LOOM_DAEMON_RESTART_POLL_INTERVAL=0.2 \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc56=$?
assert_eq "7" "$rc56" "unit stalled but never confirmed failed -> exit 7, no guessed recovery"
TESTS_RUN=$((TESTS_RUN + 1))
if grep -q -- 'reset-failed' "$SD_LOG56" 2>/dev/null \
    || grep -qE -- '(^|[[:space:]])start([[:space:]]|$)' "$SD_LOG56" 2>/dev/null; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} self-heal is NEVER invoked when ActiveState is not confirmed 'failed'"
    echo "  systemctl.log: $(cat "$SD_LOG56" 2>/dev/null)"
else
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} self-heal is NEVER invoked when ActiveState is not confirmed 'failed'"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out56" | grep -qi 'FAILED'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} failure output loudly reports the unconfirmed relaunch"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} failure output loudly reports the unconfirmed relaunch"
    echo "  output: $out56"
fi

# ============================================================
# 57. ff-abort classification (#4951): local <default> has DIVERGED from
#     origin/<default> in commit history, but the two are content-IDENTICAL
#     (`git diff origin/main...main` is empty — e.g. a local resync commit
#     and its own revert that net to no change, the robb-STUDIO 2026-08-02
#     incident shape). Default (no --auto-resolve-safe-abort): still hard
#     aborts (exit 1, HEAD untouched) but now names the exact safe command
#     instead of the old bare "resolve manually" message.
# ============================================================
W57="$BASE_WORKDIR/w57"
BARE57="$BASE_WORKDIR/w57-origin.git"
new_fixture_with_origin "$W57" "$BARE57"
push_extra_commits_to_origin "$BARE57" 2 >/dev/null
# Diverge locally with an equally content-free commit -- both sides' extra
# commits are --allow-empty, so the FINAL tree state matches even though the
# commit graphs have diverged (git merge --ff-only still refuses).
( cd "$W57" && git -c user.email=test@test -c user.name=test commit -q --allow-empty -m "local commit that nets to no change" )
HEAD_BEFORE57="$(cd "$W57" && git rev-parse --short HEAD)"
out57=$( cd "$W57" && PATH="$TEST_PATH" bash "$UPDATE_SCRIPT" 2>&1 )
rc57=$?
assert_eq "1" "$rc57" "content-identical divergence (no flag): still exits 1 by default"
HEAD_AFTER57="$(cd "$W57" && git rev-parse --short HEAD)"
assert_eq "$HEAD_BEFORE57" "$HEAD_AFTER57" "content-identical divergence (no flag): HEAD completely untouched"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out57" | grep -qi 'content-IDENTICAL'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} content-identical divergence is classified and named explicitly"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} content-identical divergence is classified and named explicitly"
    echo "  output: $out57"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out57" | grep -q -- 'reset --hard origin/main'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} abort message names the exact safe reset command"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} abort message names the exact safe reset command"
    echo "  output: $out57"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out57" | grep -q -- '--auto-resolve-safe-abort'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} abort message names --auto-resolve-safe-abort as the automatic path"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} abort message names --auto-resolve-safe-abort as the automatic path"
    echo "  output: $out57"
fi

# ============================================================
# 58. Same content-identical-divergence shape as test 57, but WITH
#     --auto-resolve-safe-abort: the script performs `git reset --hard
#     origin/main` itself, then proceeds to rebuild the now-current HEAD ->
#     exit 0, HEAD now equals origin's tip.
# ============================================================
W58="$BASE_WORKDIR/w58"
BARE58="$BASE_WORKDIR/w58-origin.git"
new_fixture_with_origin "$W58" "$BARE58"
ORIGIN_TIP58="$(push_extra_commits_to_origin "$BARE58" 2)"
( cd "$W58" && git -c user.email=test@test -c user.name=test commit -q --allow-empty -m "local commit that nets to no change" )
NEW_FAKE58="$W58/new-fake-daemon"
write_fake_daemon "$NEW_FAKE58" "$ORIGIN_TIP58" "$W58/new-marker"
out58=$( cd "$W58" && PATH="$TEST_PATH" NEW_FAKE_BIN_SRC="$NEW_FAKE58" \
    bash "$UPDATE_SCRIPT" --auto-resolve-safe-abort 2>&1 )
rc58=$?
assert_eq "0" "$rc58" "content-identical divergence + --auto-resolve-safe-abort: auto-resolves and succeeds"
HEAD_AFTER58="$(cd "$W58" && git rev-parse --short HEAD)"
assert_eq "$ORIGIN_TIP58" "$HEAD_AFTER58" "content-identical divergence + --auto-resolve-safe-abort: HEAD now equals origin/main's tip"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out58" | grep -qi 'Auto-resolved'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} auto-resolve reports what it did"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} auto-resolve reports what it did"
    echo "  output: $out58"
fi

# ============================================================
# 59. ff-abort classification (#4951): the ff-merge is blocked by a dirty
#     TRACKED file, but it is a Loom-managed installed copy (under
#     .loom/docs/, regenerated from defaults/ by resync-installed.sh) whose
#     incoming origin change conflicts with the local edit -- not real local
#     work (the loom-worker-1 2026-08-02 incident shape). Default (no
#     --auto-resolve-safe-abort): still hard aborts (exit 1, dirty file left
#     untouched) but now names the exact safe commands instead of the old
#     bare "resolve manually" message.
# ============================================================
W59="$BASE_WORKDIR/w59"
BARE59="$BASE_WORKDIR/w59-origin.git"
new_fixture_with_origin "$W59" "$BARE59"
mkdir -p "$W59/.loom/docs"
echo "docv1" > "$W59/.loom/docs/example.md"
( cd "$W59" && git add .loom/docs/example.md && git -c user.email=test@test -c user.name=test commit -q -m "add managed doc" && git push -q origin HEAD:refs/heads/main )
TMPCLONE59="$(mktemp -d)"
git clone -q "$BARE59" "$TMPCLONE59"
echo "docv2-from-origin" > "$TMPCLONE59/.loom/docs/example.md"
( cd "$TMPCLONE59" && git commit -aq -m "origin updates the managed doc" && git push -q origin HEAD:refs/heads/main )
rm -rf "$TMPCLONE59"
# Dirty (uncommitted) that SAME managed file with a conflicting edit, so
# `git merge --ff-only` genuinely refuses (an unrelated dirty file would not
# block a fast-forward).
echo "docv1-local-edit" > "$W59/.loom/docs/example.md"
HEAD_BEFORE59="$(cd "$W59" && git rev-parse --short HEAD)"
out59=$( cd "$W59" && PATH="$TEST_PATH" bash "$UPDATE_SCRIPT" 2>&1 )
rc59=$?
assert_eq "1" "$rc59" "managed-only dirty file (no flag): still exits 1 by default"
HEAD_AFTER59="$(cd "$W59" && git rev-parse --short HEAD)"
assert_eq "$HEAD_BEFORE59" "$HEAD_AFTER59" "managed-only dirty file (no flag): HEAD untouched"
TESTS_RUN=$((TESTS_RUN + 1))
if [[ "$(cat "$W59/.loom/docs/example.md")" == "docv1-local-edit" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} dirty managed file is left untouched by default (no checkout performed)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} dirty managed file is left untouched by default (no checkout performed)"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out59" | grep -qi 'Loom-managed installed copies'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} managed-only dirty state is classified and named explicitly"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} managed-only dirty state is classified and named explicitly"
    echo "  output: $out59"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out59" | grep -q -- 'checkout -- .loom/docs/example.md' \
    && echo "$out59" | grep -q -- 'resync-installed.sh'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} abort message names the exact safe checkout + resync commands"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} abort message names the exact safe checkout + resync commands"
    echo "  output: $out59"
fi

# ============================================================
# 60. Same managed-only-dirty shape as test 59, but WITH
#     --auto-resolve-safe-abort: the script discards the local edit
#     (`git checkout --`), fast-forwards, and invokes resync-installed.sh
#     post-roll -> exit 0, the managed file now matches origin's content, and
#     the (stubbed) resync-installed.sh was actually invoked.
# ============================================================
W60="$BASE_WORKDIR/w60"
BARE60="$BASE_WORKDIR/w60-origin.git"
new_fixture_with_origin "$W60" "$BARE60"
mkdir -p "$W60/.loom/docs"
echo "docv1" > "$W60/.loom/docs/example.md"
( cd "$W60" && git add .loom/docs/example.md && git -c user.email=test@test -c user.name=test commit -q -m "add managed doc" && git push -q origin HEAD:refs/heads/main )
TMPCLONE60="$(mktemp -d)"
git clone -q "$BARE60" "$TMPCLONE60"
echo "docv2-from-origin" > "$TMPCLONE60/.loom/docs/example.md"
( cd "$TMPCLONE60" && git commit -aq -m "origin updates the managed doc" && git push -q origin HEAD:refs/heads/main )
ORIGIN_TIP60="$(cd "$TMPCLONE60" && git rev-parse --short HEAD)"
rm -rf "$TMPCLONE60"
echo "docv1-local-edit" > "$W60/.loom/docs/example.md"
# Stub resync-installed.sh at the installed-copy resolution path so we can
# prove it was actually invoked post-roll, without exercising the real
# script's defaults/-tree assumptions inside this minimal fixture.
mkdir -p "$W60/.loom/scripts"
RESYNC_MARKER60="$W60/resync-invoked-marker"
cat > "$W60/.loom/scripts/resync-installed.sh" <<RESYNCEOF
#!/usr/bin/env bash
echo invoked > "$RESYNC_MARKER60"
RESYNCEOF
chmod +x "$W60/.loom/scripts/resync-installed.sh"
NEW_FAKE60="$W60/new-fake-daemon"
write_fake_daemon "$NEW_FAKE60" "$ORIGIN_TIP60" "$W60/new-marker"
out60=$( cd "$W60" && PATH="$TEST_PATH" NEW_FAKE_BIN_SRC="$NEW_FAKE60" \
    bash "$UPDATE_SCRIPT" --auto-resolve-safe-abort 2>&1 )
rc60=$?
assert_eq "0" "$rc60" "managed-only dirty file + --auto-resolve-safe-abort: auto-resolves and succeeds"
HEAD_AFTER60="$(cd "$W60" && git rev-parse --short HEAD)"
assert_eq "$ORIGIN_TIP60" "$HEAD_AFTER60" "managed-only dirty file + --auto-resolve-safe-abort: HEAD now equals origin/main's tip"
TESTS_RUN=$((TESTS_RUN + 1))
if [[ "$(cat "$W60/.loom/docs/example.md" 2>/dev/null)" == "docv2-from-origin" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} local edit was discarded; managed file now matches origin's post-merge content"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} local edit was discarded; managed file now matches origin's post-merge content"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if [[ -f "$RESYNC_MARKER60" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} post-roll resync-installed.sh was actually invoked"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} post-roll resync-installed.sh was actually invoked"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out60" | grep -qi 'Auto-resolved'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} auto-resolve reports what it did"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} auto-resolve reports what it did"
    echo "  output: $out60"
fi

# ============================================================
# 61. Regression (#4951 safety floor): a dirty tracked file OUTSIDE the
#     Loom-managed set (a top-level repo file, conflicting with origin's
#     change) alongside ANOTHER dirty file that IS managed -- the "every
#     blocking file" wording is conjunctive, not "any": one unmanaged dirty
#     file must still force the generic hard abort, with NO auto-resolve
#     attempted, even when --auto-resolve-safe-abort is passed.
# ============================================================
W61="$BASE_WORKDIR/w61"
BARE61="$BASE_WORKDIR/w61-origin.git"
new_fixture_with_origin "$W61" "$BARE61"
mkdir -p "$W61/.loom/docs"
echo "docv1" > "$W61/.loom/docs/example.md"
echo "unmanagedv1" > "$W61/unmanaged-file.txt"
( cd "$W61" && git add .loom/docs/example.md unmanaged-file.txt && git -c user.email=test@test -c user.name=test commit -q -m "add managed doc + unmanaged file" && git push -q origin HEAD:refs/heads/main )
TMPCLONE61="$(mktemp -d)"
git clone -q "$BARE61" "$TMPCLONE61"
echo "unmanagedv2-from-origin" > "$TMPCLONE61/unmanaged-file.txt"
( cd "$TMPCLONE61" && git commit -aq -m "origin updates the unmanaged file" && git push -q origin HEAD:refs/heads/main )
rm -rf "$TMPCLONE61"
# Dirty the UNMANAGED tracked file with a conflicting edit (blocks the
# ff-merge) alongside a dirty MANAGED file (does not itself conflict, but
# must not let the unmanaged one slip through).
echo "unmanagedv1-local-edit" > "$W61/unmanaged-file.txt"
echo "docv1-local-edit" > "$W61/.loom/docs/example.md"
HEAD_BEFORE61="$(cd "$W61" && git rev-parse --short HEAD)"
out61=$( cd "$W61" && PATH="$TEST_PATH" bash "$UPDATE_SCRIPT" --auto-resolve-safe-abort 2>&1 )
rc61=$?
assert_eq "1" "$rc61" "unmanaged dirty file alongside a managed one, EVEN with --auto-resolve-safe-abort: still hard-aborts"
HEAD_AFTER61="$(cd "$W61" && git rev-parse --short HEAD)"
assert_eq "$HEAD_BEFORE61" "$HEAD_AFTER61" "unmanaged dirty file case: HEAD untouched"
TESTS_RUN=$((TESTS_RUN + 1))
if [[ "$(cat "$W61/unmanaged-file.txt")" == "unmanagedv1-local-edit" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} unmanaged dirty file is left untouched -- no auto-resolve attempted"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} unmanaged dirty file is left untouched -- no auto-resolve attempted"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out61" | grep -qi 'Refusing to guess or hard-reset'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} mixed managed+unmanaged dirty: falls through to the generic hard-abort message"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} mixed managed+unmanaged dirty: falls through to the generic hard-abort message"
    echo "  output: $out61"
fi

# ============================================================
# 62. Regression (#4951 safety floor, Judge finding on PR #4965): the
#     content-identical classifier compares only COMMITTED refs, so a host can
#     simultaneously have (a) diverged local commits that net to zero content
#     diff vs. origin (the test-57 shape) AND (b) an entirely unrelated dirty
#     tracked file OUTSIDE the Loom-managed set. `git merge --ff-only` fails on
#     the history divergence alone, so this branch is reached — and before the
#     fix, _ff_abort_content_identical returned true regardless of the working
#     tree, letting --auto-resolve-safe-abort `git reset --hard` silently
#     destroy that file's uncommitted changes. It must hard-abort instead, with
#     the dirty file byte-for-byte intact.
# ============================================================
W62="$BASE_WORKDIR/w62"
BARE62="$BASE_WORKDIR/w62-origin.git"
new_fixture_with_origin "$W62" "$BARE62"
echo "scratchv1" > "$W62/unmanaged-scratch.txt"
( cd "$W62" && git add unmanaged-scratch.txt && git -c user.email=test@test -c user.name=test commit -q -m "add unmanaged scratch file" && git push -q origin HEAD:refs/heads/main )
# Origin advances with content-free commits, so the two sides' TREES stay
# identical -- exactly the content-identical divergence of tests 57/58.
push_extra_commits_to_origin "$BARE62" 2 >/dev/null
( cd "$W62" && git -c user.email=test@test -c user.name=test commit -q --allow-empty -m "local commit that nets to no change" )
# ... but now an unrelated, UNMANAGED tracked file is also dirty in the
# working tree (an operator's manual scratch edit).
echo "scratchv1-local-edit" > "$W62/unmanaged-scratch.txt"
HEAD_BEFORE62="$(cd "$W62" && git rev-parse --short HEAD)"
out62=$( cd "$W62" && PATH="$TEST_PATH" bash "$UPDATE_SCRIPT" --auto-resolve-safe-abort 2>&1 )
rc62=$?
assert_eq "1" "$rc62" "content-identical divergence + dirty UNMANAGED file, EVEN with --auto-resolve-safe-abort: still hard-aborts"
HEAD_AFTER62="$(cd "$W62" && git rev-parse --short HEAD)"
assert_eq "$HEAD_BEFORE62" "$HEAD_AFTER62" "content-identical divergence + dirty unmanaged file: HEAD untouched (no reset --hard)"
TESTS_RUN=$((TESTS_RUN + 1))
if [[ "$(cat "$W62/unmanaged-scratch.txt")" == "scratchv1-local-edit" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} uncommitted edit to an unmanaged file survives a content-identical divergence"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} uncommitted edit to an unmanaged file survives a content-identical divergence"
    echo "  file now: $(cat "$W62/unmanaged-scratch.txt" 2>/dev/null)"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out62" | grep -qi 'Refusing to guess or hard-reset'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} content-identical + dirty unmanaged file falls through to the generic hard-abort message"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} content-identical + dirty unmanaged file falls through to the generic hard-abort message"
    echo "  output: $out62"
fi

# ============================================================
# 63. Same combined shape as test 62, but the co-existing dirty tracked file IS
#     Loom-managed. The content-identical classifier must STILL decline (its
#     `git reset --hard` is only vouched-for on a clean tree), handing the case
#     to the managed-dirty classifier instead. That one discards the managed
#     edit it is authorized to discard and retries the fast-forward, which
#     still cannot apply over the history divergence -- so the run hard-aborts
#     (exit 1) with local commits intact, rather than silently hard-resetting
#     from the content-identical branch. Deliberately NOT resolved end-to-end:
#     widening either classifier to cover the combination is exactly the kind
#     of "when genuinely unsure" widening the #4381 safety note forbids.
# ============================================================
W63="$BASE_WORKDIR/w63"
BARE63="$BASE_WORKDIR/w63-origin.git"
new_fixture_with_origin "$W63" "$BARE63"
mkdir -p "$W63/.loom/docs"
echo "docv1" > "$W63/.loom/docs/example.md"
( cd "$W63" && git add .loom/docs/example.md && git -c user.email=test@test -c user.name=test commit -q -m "add managed doc" && git push -q origin HEAD:refs/heads/main )
push_extra_commits_to_origin "$BARE63" 2 >/dev/null
( cd "$W63" && git -c user.email=test@test -c user.name=test commit -q --allow-empty -m "local commit that nets to no change" )
echo "docv1-local-edit" > "$W63/.loom/docs/example.md"
HEAD_BEFORE63="$(cd "$W63" && git rev-parse --short HEAD)"
out63=$( cd "$W63" && PATH="$TEST_PATH" bash "$UPDATE_SCRIPT" --auto-resolve-safe-abort 2>&1 )
rc63=$?
assert_eq "1" "$rc63" "content-identical divergence + dirty MANAGED file, EVEN with --auto-resolve-safe-abort: still hard-aborts"
HEAD_AFTER63="$(cd "$W63" && git rev-parse --short HEAD)"
assert_eq "$HEAD_BEFORE63" "$HEAD_AFTER63" "content-identical divergence + dirty managed file: HEAD untouched (no reset --hard)"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out63" | grep -qi 'reset local main to origin/main'; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} content-identical branch never claims a reset --hard while the tree is dirty"
    echo "  output: $out63"
else
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} content-identical branch never claims a reset --hard while the tree is dirty"
fi

# ============================================================
# 25. Launchd-sandbox guards (#4078): the whole suite exercises the REAL
#     start/stop scripts, so prove it never reached the operator's live daemon.
#     (a) The suite-level decoy loom-daemon is still alive — no by-name kill
#         fired anywhere in the suite.
#     (b) No recorded launchctl invocation ever named a com.rjwalters.* label.
# ============================================================
TESTS_RUN=$((TESTS_RUN + 1))
if kill -0 "$DECOY_PID" 2>/dev/null; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} suite-level decoy loom-daemon survived the whole update suite"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} suite-level decoy loom-daemon survived the whole update suite"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if launchd_sandbox_assert_no_production_label "$SANDBOX_LOG_DIR/launchctl-invocations.log"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} no launchctl invocation named a com.rjwalters.* label"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} no launchctl invocation named a com.rjwalters.* label"
    echo "  launchctl invocations: $(cat "$SANDBOX_LOG_DIR/launchctl-invocations.log" 2>/dev/null)"
fi

# ============================================================
# 44. Production-binary checksum guard (#4381 incident): the REAL
#     ~/.local/bin/loom-daemon must be byte-for-byte unchanged after the whole
#     suite runs. This is the direct regression test for the 2026-07-29
#     incident (a test fixture stub overwrote the production binary for ~9
#     hours) — it fails LOUDLY if the LOOM_DAEMON_BIN_DIR sandbox above is ever
#     bypassed, removed, or a new call site is added that skips it.
# ============================================================
_PROD_DAEMON_CHECKSUM_AFTER="$(_prod_daemon_checksum)"
TESTS_RUN=$((TESTS_RUN + 1))
if [[ "$_PROD_DAEMON_CHECKSUM_BEFORE" == "$_PROD_DAEMON_CHECKSUM_AFTER" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} real ${_PROD_DAEMON_BIN} is byte-identical before/after the suite (#4381 regression guard)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} real ${_PROD_DAEMON_BIN} CHANGED during this test run (#4381 regression!)"
    echo "  before: $_PROD_DAEMON_CHECKSUM_BEFORE"
    echo "  after:  $_PROD_DAEMON_CHECKSUM_AFTER"
fi

# ---------- summary ----------
echo
echo "Ran $TESTS_RUN tests: $TESTS_PASSED passed, $TESTS_FAILED failed"
[[ "$TESTS_FAILED" -eq 0 ]]
