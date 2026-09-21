#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — the
# session-owned scratch directory carve-out in the rm-scope check (#8460,
# parent #8453 item 5).
#
# What is under test: `guards.rmScope=repo` refuses every `rm` outside the
# repo, including the private `CARGO_TARGET_DIR` an agent builds into to get a
# hermetic test run. Three agents each abandoned 3.5-11 GB of private build
# output on one fleet host in a single day because of it. The carve-out admits
# EXACTLY ONE extra path — `<scratch-root>/<this session's id>` and anything
# under it, proven by an ownership marker — and nothing else.
#
# The assertions below are organised around the ways that proof can fail,
# because the expensive mistake here is not "the allow does not work", it is
# "the allow works for a path it should not have": a session-ownership check
# keyed on the path alone, or on an identity the acting session can forge,
# silently lets one agent delete another agent's private directory.
#
# FIXTURE LOCATION MATTERS. Every fixture lives under $HOME, never under
# $TMPDIR: `/tmp/*`, `/var/tmp/*` and `/var/folders/*` are on the rm-scope
# ephemeral allowlist, so a fixture there would be admitted by that allowlist
# BEFORE the carve-out is ever consulted and every refusal assertion here would
# pass vacuously. The same reason `make_sql_repo` (an mktemp repo) is not used
# for the config-tier cases: its own path would be ephemeral-allowlisted.
#
# Usage: ./tests/hooks/test-guard-destructive-session-scratch.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

echo -e "${YELLOW}--- Session-owned scratch dir carve-out (rm-scope, #8460) ---${NC}"

# --- Fixture ---------------------------------------------------------------
SS_BASE="$HOME/.loom-guard-session-scratch-test.$$"
cleanup_session_scratch_fixture() { rm -rf "$SS_BASE"; }
trap cleanup_session_scratch_fixture EXIT

SS_ROOT="$SS_BASE/scratch"
SID_A="aaaaaaaa-1111-2222-3333-444444444444"   # the acting session
SID_B="bbbbbbbb-5555-6666-7777-888888888888"   # a concurrent peer session
MARKER=".loom-session-scratch"

mkdir -p "$SS_ROOT/$SID_A/debug/deps" "$SS_ROOT/$SID_B/debug"
printf 'session=%s\n' "$SID_A" >"$SS_ROOT/$SID_A/$MARKER"
printf 'session=%s\n' "$SID_B" >"$SS_ROOT/$SID_B/$MARKER"

# A directory outside the scratch root entirely: the target of every escape
# attempt below (a symlink tunnel, a `..` traversal).
mkdir -p "$SS_BASE/precious"
: >"$SS_BASE/precious/do-not-delete"

# A throwaway git repo under $SS_BASE (NOT mktemp — see the header note) whose
# .loom/config.json carries guards.scratchRoot, for the config-tier cases.
SS_CFG_REPO="$SS_BASE/cfgrepo"
mkdir -p "$SS_CFG_REPO/.loom"
git -C "$SS_CFG_REPO" init -q >/dev/null 2>&1
jq -nc --arg r "$SS_ROOT" '{guards:{scratchRoot:$r}}' >"$SS_CFG_REPO/.loom/config.json"

# --- Suite-local assertions ------------------------------------------------
#
# The shared harness's run_guard_env() takes exactly one VAR=VALUE and always
# uses $TEST_REPO as cwd; this suite needs two assignments (the default-root
# case overrides HOME) and a per-case cwd (the config-tier cases). Everything
# else — counters, colours, make_input and its GUARD_TEST_SESSION_ID hook — is
# the shared harness's.
SS_CWD=""   # empty => $TEST_REPO (the hermetic no-config repo)

ss_run() {
    local cmd="$1"
    shift
    make_input "$cmd" "${SS_CWD:-$TEST_REPO}" | env "$@" "$GUARD" 2>&1 || true
}

ss_decision() { echo "$1" | jq -r '.hookSpecificOutput.permissionDecision // empty' 2>/dev/null; }
ss_reason()   { echo "$1" | jq -r '.hookSpecificOutput.permissionDecisionReason // empty' 2>/dev/null; }

ss_record() {   # <ok:0|1> <desc> <expected> <cmd> <output>
    TOTAL=$((TOTAL + 1))
    if [[ "$1" == 0 ]]; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $2"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $2"
        echo -e "       Command: $4"
        echo -e "       Expected: $3"
        echo -e "       Got: $5"
    fi
}

# ss_assert_allow <desc> <cmd> [env-kv...] — allow means no decision at all.
ss_assert_allow() {
    local description="$1" cmd="$2"
    shift 2
    local output rc=1
    output=$(ss_run "$cmd" "$@")
    [[ -z "$(ss_decision "$output")" ]] && rc=0
    ss_record "$rc" "$description" "allow (no decision)" "$cmd" "$output"
}

# ss_assert_deny <desc> <cmd> [env-kv...]
ss_assert_deny() {
    local description="$1" cmd="$2"
    shift 2
    local output rc=1
    output=$(ss_run "$cmd" "$@")
    [[ "$(ss_decision "$output")" == deny ]] && rc=0
    ss_record "$rc" "$description" "deny" "$cmd" "$output"
}

# ss_assert_deny_reason <desc> <cmd> <ere> [env-kv...]
ss_assert_deny_reason() {
    local description="$1" cmd="$2" pattern="$3"
    shift 3
    local output reason rc=1
    output=$(ss_run "$cmd" "$@")
    reason=$(ss_reason "$output")
    # Herestring, not a pipe: `grep -q` exits early and would SIGPIPE its
    # producer under `set -o pipefail` (scripts/check-pipefail-early-exit.sh).
    if [[ "$(ss_decision "$output")" == deny ]] && grep -qE "$pattern" <<<"$reason"; then
        rc=0
    fi
    ss_record "$rc" "$description" "deny with reason matching /$pattern/" "$cmd" "$output"
}

# ss_assert_deny_reason_not <desc> <cmd> <ere> [env-kv...] — denied, but the
# reason must NOT match: proves the new remediation hint stays off unrelated
# denials, i.e. every other out-of-repo message is unchanged.
ss_assert_deny_reason_not() {
    local description="$1" cmd="$2" pattern="$3"
    shift 3
    local output reason rc=1
    output=$(ss_run "$cmd" "$@")
    reason=$(ss_reason "$output")
    # Herestring, not a pipe — see ss_assert_deny_reason above.
    if [[ "$(ss_decision "$output")" == deny ]] && ! grep -qE "$pattern" <<<"$reason"; then
        rc=0
    fi
    ss_record "$rc" "$description" "deny with reason NOT matching /$pattern/" "$cmd" "$output"
}

ENV_ROOT="LOOM_GUARD_SCRATCH_ROOT=$SS_ROOT"
# `env` needs at least one operand in the suite-local runner; this name is
# never read by the guard and exists only as an inert placeholder.
ENV_NONE="LOOM_GUARD_SESSION_SCRATCH_TEST_PLACEHOLDER=1"

# =========================================================================
# 1. THE ALLOW — the whole point: a session removes the directory it created.
# =========================================================================
GUARD_TEST_SESSION_ID="$SID_A"

ss_assert_allow "own session dir is removable" \
    "rm -rf $SS_ROOT/$SID_A" "$ENV_ROOT"
ss_assert_allow "a path UNDER the own session dir is removable" \
    "rm -rf $SS_ROOT/$SID_A/debug/deps" "$ENV_ROOT"
ss_assert_allow "a trailing slash normalizes to the same admitted dir" \
    "rm -rf $SS_ROOT/$SID_A/" "$ENV_ROOT"

# =========================================================================
# 2. THE REFUSALS THAT MATTER MOST — another session's dir, and the root.
# =========================================================================
ss_assert_deny "a PEER session's dir under the SAME root is NOT removable" \
    "rm -rf $SS_ROOT/$SID_B" "$ENV_ROOT"
ss_assert_deny "a path under a peer session's dir is NOT removable" \
    "rm -rf $SS_ROOT/$SID_B/debug" "$ENV_ROOT"
ss_assert_deny "the scratch ROOT itself is never removable" \
    "rm -rf $SS_ROOT" "$ENV_ROOT"
ss_assert_deny "a '..' traversal out of the own dir into a peer's is refused" \
    "rm -rf $SS_ROOT/$SID_A/../$SID_B" "$ENV_ROOT"
ss_assert_deny "a '..' traversal out of the scratch root entirely is refused" \
    "rm -rf $SS_ROOT/$SID_A/../../precious" "$ENV_ROOT"
ss_assert_deny "an unrelated out-of-repo path is still refused" \
    "rm -rf /opt/some-vendor/important" "$ENV_ROOT"
ss_assert_deny "a second, non-admitted target in the SAME command still denies" \
    "rm -rf $SS_ROOT/$SID_A /opt/some-vendor/important" "$ENV_ROOT"

# =========================================================================
# 3. IDENTITY — admission is keyed on the harness-supplied session_id, which
#    the acting session cannot set. No id, or an implausible one, is a
#    refusal, never a free pass.
# =========================================================================
GUARD_TEST_SESSION_ID=""
ss_assert_deny "no session_id on stdin: carve-out inert, own dir NOT removable" \
    "rm -rf $SS_ROOT/$SID_A" "$ENV_ROOT"

GUARD_TEST_SESSION_ID=".."
ss_assert_deny "session_id '..' is rejected by the shape gate: carve-out inert" \
    "rm -rf $SS_ROOT/$SID_A" "$ENV_ROOT"

GUARD_TEST_SESSION_ID="a/b/../.."
ss_assert_deny "a slash-bearing session_id is rejected by the shape gate" \
    "rm -rf $SS_ROOT/$SID_A" "$ENV_ROOT"

GUARD_TEST_SESSION_ID="ab"
mkdir -p "$SS_ROOT/ab"
printf 'session=ab\n' >"$SS_ROOT/ab/$MARKER"
ss_assert_deny "an implausibly short session_id is rejected by the shape gate" \
    "rm -rf $SS_ROOT/ab" "$ENV_ROOT"

GUARD_TEST_SESSION_ID="$SID_B"
ss_assert_deny "session B cannot remove session A's dir (the mirror case)" \
    "rm -rf $SS_ROOT/$SID_A" "$ENV_ROOT"
ss_assert_allow "...while session B CAN remove its own" \
    "rm -rf $SS_ROOT/$SID_B" "$ENV_ROOT"

# =========================================================================
# 4. THE MARKER — missing, malformed, mis-named, or a symlink: fail CLOSED.
# =========================================================================
SID_NOMARK="cccccccc-1111-2222-3333-444444444444"
mkdir -p "$SS_ROOT/$SID_NOMARK"
GUARD_TEST_SESSION_ID="$SID_NOMARK"
ss_assert_deny "a dir with NO ownership marker is not removable (fail closed)" \
    "rm -rf $SS_ROOT/$SID_NOMARK" "$ENV_ROOT"

SID_BADMARK="dddddddd-1111-2222-3333-444444444444"
mkdir -p "$SS_ROOT/$SID_BADMARK"
printf 'this file has no session= line at all\n' >"$SS_ROOT/$SID_BADMARK/$MARKER"
GUARD_TEST_SESSION_ID="$SID_BADMARK"
ss_assert_deny "a MALFORMED marker is not removable (fail closed)" \
    "rm -rf $SS_ROOT/$SID_BADMARK" "$ENV_ROOT"

SID_WRONGMARK="eeeeeeee-1111-2222-3333-444444444444"
mkdir -p "$SS_ROOT/$SID_WRONGMARK"
printf 'session=%s\n' "$SID_B" >"$SS_ROOT/$SID_WRONGMARK/$MARKER"
GUARD_TEST_SESSION_ID="$SID_WRONGMARK"
ss_assert_deny "a marker naming a DIFFERENT session is not removable" \
    "rm -rf $SS_ROOT/$SID_WRONGMARK" "$ENV_ROOT"

SID_LINKMARK="ffffffff-1111-2222-3333-444444444444"
mkdir -p "$SS_ROOT/$SID_LINKMARK"
printf 'session=%s\n' "$SID_LINKMARK" >"$SS_BASE/planted-marker"
ln -s "$SS_BASE/planted-marker" "$SS_ROOT/$SID_LINKMARK/$MARKER"
GUARD_TEST_SESSION_ID="$SID_LINKMARK"
ss_assert_deny "a SYMLINKED marker is not removable (must be a regular file)" \
    "rm -rf $SS_ROOT/$SID_LINKMARK" "$ENV_ROOT"

# =========================================================================
# 5. SYMLINKS — neither the session dir nor a path inside it may be used as a
#    tunnel to something outside the session dir.
# =========================================================================
SID_DIRLINK="99999999-1111-2222-3333-444444444444"
ln -s "$SS_BASE/precious" "$SS_ROOT/$SID_DIRLINK"
GUARD_TEST_SESSION_ID="$SID_DIRLINK"
ss_assert_deny "a session dir that is itself a SYMLINK is not removable" \
    "rm -rf $SS_ROOT/$SID_DIRLINK" "$ENV_ROOT"
ss_assert_deny "...and neither is a path through that symlinked session dir" \
    "rm -rf $SS_ROOT/$SID_DIRLINK/do-not-delete" "$ENV_ROOT"

ln -s "$SS_BASE/precious" "$SS_ROOT/$SID_A/tunnel"
GUARD_TEST_SESSION_ID="$SID_A"
ss_assert_deny "a symlink INSIDE the own session dir is not a tunnel out" \
    "rm -rf $SS_ROOT/$SID_A/tunnel/do-not-delete" "$ENV_ROOT"
# Conservative, and deliberately so: `rm` would unlink the symlink rather than
# follow it, but the physical-containment test cannot distinguish the two, and
# refusing is lossless (removing the whole session dir, which unlinks it, is
# still admitted — asserted next).
ss_assert_deny "unlinking a symlink pointing OUT is refused (fail closed)" \
    "rm -rf $SS_ROOT/$SID_A/tunnel" "$ENV_ROOT"
ss_assert_allow "the whole session dir stays removable despite holding that symlink" \
    "rm -rf $SS_ROOT/$SID_A" "$ENV_ROOT"

# =========================================================================
# 6. ROOT SANITY SCREEN — a misconfigured root makes the carve-out INERT,
#    never broad. Each root below would otherwise make `<root>/<id>` name
#    something that is not a private scratch directory.
# =========================================================================
ss_assert_deny "scratch root '/' is rejected: carve-out inert" \
    "rm -rf /$SID_A" "LOOM_GUARD_SCRATCH_ROOT=/"
ss_assert_deny "scratch root \$HOME is rejected: carve-out inert" \
    "rm -rf $HOME/$SID_A" "LOOM_GUARD_SCRATCH_ROOT=$HOME"
ss_assert_deny "a single-segment system root (/opt) is rejected: carve-out inert" \
    "rm -rf /opt/$SID_A" "LOOM_GUARD_SCRATCH_ROOT=/opt"
ss_assert_deny "a relative scratch root is dropped: carve-out inert" \
    "rm -rf $SS_ROOT/$SID_A" "LOOM_GUARD_SCRATCH_ROOT=relative/path"

# A root that CONTAINS the repo would make the repo's own siblings candidates.
SS_CWD="$SS_CFG_REPO"
ss_assert_deny "a scratch root that is an ancestor of the repo is rejected" \
    "rm -rf $SS_BASE/$SID_A" "LOOM_GUARD_SCRATCH_ROOT=$SS_BASE"
SS_CWD=""

# =========================================================================
# 7. THE DENIAL FLOOR IS UNTOUCHED — the unconditional catastrophic-path deny
#    runs BEFORE this carve-out is consulted.
# =========================================================================
ss_assert_deny "rm -rf / is still denied with a scratch root configured" \
    "rm -rf /" "$ENV_ROOT"
ss_assert_deny "rm -rf \$HOME is still denied with a scratch root configured" \
    "rm -rf $HOME" "$ENV_ROOT"
ss_assert_deny "a bare single-segment path is still denied" \
    "rm -rf /usr" "$ENV_ROOT"

# =========================================================================
# 8. THE REMEDIATION HINT — present for a scratch-root path this session
#    cannot prove it owns (the leak case), absent everywhere else so no other
#    out-of-repo denial's wording changes.
# =========================================================================
ss_assert_deny_reason "peer-session denial names the ownership requirement" \
    "rm -rf $SS_ROOT/$SID_B" "session-scratch root" "$ENV_ROOT"
ss_assert_deny_reason_not "an unrelated out-of-repo denial keeps its old wording" \
    "rm -rf /opt/some-vendor/important" "session-scratch root" "$ENV_ROOT"

# =========================================================================
# 9. DEFAULT ROOT — with neither env nor config set, the root is
#    $HOME/.cache/loom/session-scratch. Exercised against a FAKE $HOME so the
#    real one is never touched.
# =========================================================================
FAKE_HOME="$SS_BASE/fakehome"
FAKE_SCRATCH="$FAKE_HOME/.cache/loom/session-scratch"
mkdir -p "$FAKE_SCRATCH/$SID_A" "$FAKE_SCRATCH/$SID_B"
printf 'session=%s\n' "$SID_A" >"$FAKE_SCRATCH/$SID_A/$MARKER"
printf 'session=%s\n' "$SID_B" >"$FAKE_SCRATCH/$SID_B/$MARKER"

ss_assert_allow "default root (\$HOME/.cache/loom/session-scratch): own dir removable" \
    "rm -rf $FAKE_SCRATCH/$SID_A" "HOME=$FAKE_HOME"
ss_assert_deny "default root: a peer's dir is still refused" \
    "rm -rf $FAKE_SCRATCH/$SID_B" "HOME=$FAKE_HOME"
ss_assert_deny "default root: the root itself is still refused" \
    "rm -rf $FAKE_SCRATCH" "HOME=$FAKE_HOME"

# =========================================================================
# 10. CONFIG TIER — guards.scratchRoot in .loom/config.json, and env beats it.
# =========================================================================
SS_CWD="$SS_CFG_REPO"
ss_assert_allow "guards.scratchRoot config admits the own session dir" \
    "rm -rf $SS_ROOT/$SID_A" "$ENV_NONE"
ss_assert_deny "guards.scratchRoot config still refuses a peer's dir" \
    "rm -rf $SS_ROOT/$SID_B" "$ENV_NONE"
ss_assert_deny "env LOOM_GUARD_SCRATCH_ROOT overrides guards.scratchRoot" \
    "rm -rf $SS_ROOT/$SID_A" "LOOM_GUARD_SCRATCH_ROOT=$SS_BASE/other-root"
SS_CWD=""

GUARD_TEST_SESSION_ID=""

print_summary
