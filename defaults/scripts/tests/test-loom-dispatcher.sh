#!/usr/bin/env bash
# test-loom-dispatcher.sh — tests for the machine-level `loom` dispatcher and its
# provisioning (Epic #3835 Phase 3a, #4157).
#
# Covers:
#   - scripts/loom — checkout resolution (AC1), collision resolution (AC3),
#     the three status contexts (AC7), config resolution via the Phase 2 tier
#     resolver incl. the jq-absent case (AC5), and the thin `update` boundary
#     (Finding 3).
#   - scripts/install/provision-dispatcher.sh — the verifiable-globals contract
#     (#4053), the symlink checkout (AC1), and the console-script invariant (AC6).
#   - the no-shadow regression proving the two `loom` invocation forms stay
#     disjoint (AC4).
#
# Throwaway `mktemp -d` scratch per case, matching test-config-resolver.sh.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# defaults/scripts/tests -> defaults/scripts/tests/../../.. -> repo root
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

DISPATCHER="$REPO_ROOT/scripts/loom"
PROVISION_LIB="$REPO_ROOT/scripts/install/provision-dispatcher.sh"
REAL_RESOLVER="$REPO_ROOT/defaults/scripts/lib/config-resolver.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

pass() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1)); echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo -e "  ${RED}FAIL${NC}: $1"; }

assert_contains() {
    if [[ "$1" == *"$2"* ]]; then pass "$3"; else fail "$3 (missing substring: '$2' in: $1)"; fi
}
assert_not_contains() {
    if [[ "$1" != *"$2"* ]]; then pass "$3"; else fail "$3 (unexpected substring: '$2')"; fi
}
assert_eq() {
    if [[ "$1" == "$2" ]]; then pass "$3"; else fail "$3 (expected '$2', got '$1')"; fi
}

[[ -f "$DISPATCHER" ]]    || { echo "dispatcher not found at $DISPATCHER"; exit 1; }
[[ -f "$PROVISION_LIB" ]] || { echo "provisioning lib not found at $PROVISION_LIB"; exit 1; }

# Build a fake machine-level checkout with stub daemon-lifecycle scripts and a
# real copy of the config resolver. Echoes the checkout path.
make_checkout() {
    local c; c="$(mktemp -d)"
    mkdir -p "$c/defaults/scripts/cli" "$c/defaults/scripts/lib"
    cat > "$c/defaults/scripts/cli/loom-daemon-start.sh" <<'EOF'
#!/usr/bin/env bash
echo "STUB_START args=[$*]"
EOF
    cat > "$c/defaults/scripts/cli/loom-daemon-stop.sh" <<'EOF'
#!/usr/bin/env bash
echo "STUB_STOP args=[$*]"
EOF
    cat > "$c/defaults/scripts/cli/loom-daemon-update.sh" <<'EOF'
#!/usr/bin/env bash
echo "STUB_UPDATE args=[$*]"
EOF
    chmod +x "$c/defaults/scripts/cli/"*.sh
    cp "$REAL_RESOLVER" "$c/defaults/scripts/lib/config-resolver.sh"
    printf '%s\n' "$c"
}

# Build a fake consumer repo with a pool-manager stub + config tiers.
make_consumer_repo() {
    local r; r="$(mktemp -d)"
    mkdir -p "$r/.loom/bin"
    cat > "$r/.loom/bin/loom" <<'EOF'
#!/usr/bin/env bash
echo "POOL_MANAGER args=[$*]"
EOF
    chmod +x "$r/.loom/bin/loom"
    printf '%s\n' "$r"
}

# ── AC1 / AC7: checkout resolution + status ABSENT checkout ───────────────────
echo "Test 1: status reports an ABSENT machine checkout (AC1 resolution)"
out=$(LOOM_HOME="/nonexistent/loom/checkout/xyz" bash "$DISPATCHER" status 2>&1)
assert_contains "$out" "ABSENT" "status shows ABSENT when checkout missing"
assert_contains "$out" "machine-level dispatcher" "status self-identifies as machine-level"

# ── AC7: three distinguishable status contexts ───────────────────────────────
echo "Test 2: status contexts are correct and distinguishable (AC7)"
CHK=$(make_checkout)
CONSUMER=$(make_consumer_repo)

# (1) consumer repo root
out=$(cd "$CONSUMER" && LOOM_HOME="$CHK" bash "$DISPATCHER" status 2>&1)
assert_contains "$out" "consumer-repo" "consumer repo context labelled consumer-repo"
assert_contains "$out" "pool manager:  present" "consumer repo notes the pool manager"

# (2) git worktree — a real Loom worktree of a repo that commits .loom/ carries
# its own checked-out .loom/ directory, so $WT gets one too. This exercises the
# ordering fix: the linked-worktree (.git file) check must win over the
# consumer-repo (.loom/ dir) check, else the worktree is misclassified as a
# consumer-repo rooted at itself instead of at the main checkout.
WT=$(mktemp -d); mkdir -p "$WT/.loom"
MAIN=$(mktemp -d); mkdir -p "$MAIN/.loom"
printf 'gitdir: %s/.git/worktrees/wt\n' "$MAIN" > "$WT/.git"
out=$(cd "$WT" && LOOM_HOME="$CHK" bash "$DISPATCHER" status 2>&1)
assert_contains "$out" "git-worktree" "worktree context labelled git-worktree"
assert_contains "$out" "$MAIN" "worktree resolves LOOM_CTX_ROOT to the main checkout, not the worktree"

# (3) non-repo directory
NR=$(mktemp -d)
out=$(cd "$NR" && LOOM_HOME="$CHK" bash "$DISPATCHER" status 2>&1)
assert_contains "$out" "non-repo" "non-repo context labelled non-repo"

# ── AC3: collision resolution for the overlapping verbs ──────────────────────
echo "Test 3: bare 'start' inside a consumer repo disambiguates, never runs silently (AC3)"
set +e
out=$(cd "$CONSUMER" && LOOM_HOME="$CHK" bash "$DISPATCHER" start 2>&1)
rc=$?
set -e 2>/dev/null || true
assert_eq "$rc" "3" "bare 'loom start' in a repo exits 3 (disambiguating non-zero)"
assert_contains "$out" ".loom/bin/loom start" "disambiguation names the per-repo pool surface"
assert_contains "$out" "loom start --machine" "disambiguation names the machine surface"
assert_not_contains "$out" "STUB_START" "bare 'loom start' did NOT run the daemon start (no silent surface)"

echo "Test 4: 'start --machine' bypasses the guard and delegates (strips --machine)"
set +e
out=$(cd "$CONSUMER" && LOOM_HOME="$CHK" bash "$DISPATCHER" start --machine 2>&1)
rc=$?
set -e 2>/dev/null || true
assert_eq "$rc" "0" "'loom start --machine' exits 0"
assert_contains "$out" "STUB_START" "'--machine' delegates to loom-daemon-start.sh"
assert_contains "$out" "args=[]" "the --machine selector is stripped before delegating"

# ── Finding 3: thin update verb ──────────────────────────────────────────────
echo "Test 5: 'update' is a thin delegator with no rebuild logic of its own (Finding 3)"
set +e
out=$(LOOM_HOME="$CHK" bash "$DISPATCHER" update --check 2>&1)
rc=$?
set -e 2>/dev/null || true
assert_eq "$rc" "0" "'loom update' exits 0 via delegation"
assert_contains "$out" "STUB_UPDATE args=[--check]" "'update' delegates to loom-daemon-update.sh, passing args"
# Structural: the dispatcher must not itself implement rebuild/restart.
src="$(cat "$DISPATCHER")"
assert_not_contains "$src" "cargo build" "dispatcher source contains no 'cargo build' (thin boundary)"
assert_not_contains "$src" "launchctl" "dispatcher source contains no 'launchctl' (thin boundary)"
assert_not_contains "$src" "git pull" "dispatcher source contains no 'git pull' (thin boundary)"

# ── AC5: config resolution through the Phase 2 tier resolver ─────────────────
if command -v jq >/dev/null 2>&1; then
    echo "Test 6: status resolves config through the tier chain — local tier wins (AC5)"
    RTIER=$(make_consumer_repo)
    mkdir -p "$RTIER/.loom" "$RTIER/.loom-local"
    echo '{"autonomous":{"workFinder":{"enabled":false}}}' > "$RTIER/.loom/config.json"
    echo '{"autonomous":{"workFinder":{"enabled":true}}}'  > "$RTIER/.loom-local/local.json"
    out=$(cd "$RTIER" && LOOM_CONFIG_DEFAULTS_FILE="" LOOM_HOME="$CHK" bash "$DISPATCHER" status 2>&1)
    assert_contains "$out" "autonomous.workFinder.enabled = true" ".loom-local/local.json overrides .loom/config.json (resolver, not direct read)"
else
    echo "Test 6: SKIP (jq not on PATH)"
fi

echo "Test 7: status is explicit when jq is unavailable — not masqueraded as 'no config' (AC5)"
JQLESS_BIN=$(mktemp -d)
for t in sed dirname readlink; do
    p="$(command -v "$t" 2>/dev/null || true)"
    [[ -n "$p" ]] && ln -s "$p" "$JQLESS_BIN/$t"
done
REAL_BASH="$(command -v bash)"
set +e
out=$(cd "$NR" && PATH="$JQLESS_BIN" HOME="$NR" LOOM_HOME="$CHK" "$REAL_BASH" "$DISPATCHER" status 2>&1)
set -e 2>/dev/null || true
assert_contains "$out" "jq not available" "jq-absent status says so explicitly"

# ── AC4: no-shadow regression — the two invocation forms stay disjoint ───────
echo "Test 8: bare 'loom' and './.loom/bin/loom' resolve to different surfaces in the same shell (AC4)"
# Provision the dispatcher into a temp bin dir and put ONLY that dir on PATH.
BIN=$(mktemp -d)
HOME_T=$(mktemp -d)
# shellcheck source=/dev/null
source "$PROVISION_LIB"
provision_loom_dispatcher "$REPO_ROOT" "$BIN" "$HOME_T/.local/share/loom" >/dev/null 2>&1 || true
# Same shell: PATH has the dispatcher's bin dir but NOT any repo's .loom/bin.
run_path="$BIN:/usr/bin:/bin"
assert_not_contains ":$run_path:" "/.loom/bin:" "'.loom/bin' is absent from the test PATH"
# bare 'loom status' -> the machine dispatcher
set +e
bare_out=$(cd "$CONSUMER" && PATH="$run_path" HOME="$HOME_T" LOOM_HOME="$CHK" loom status 2>&1)
set -e 2>/dev/null || true
assert_contains "$bare_out" "machine-level dispatcher" "bare 'loom status' resolves to the machine dispatcher"
# path-qualified './.loom/bin/loom status' -> the pool manager
pool_out=$(cd "$CONSUMER" && PATH="$run_path" ./.loom/bin/loom status 2>&1)
assert_contains "$pool_out" "POOL_MANAGER" "'./.loom/bin/loom status' resolves to the pool manager"
assert_not_contains "$pool_out" "machine-level dispatcher" "the two forms are disjoint (no shadowing)"

# ── #4053 contract + AC1 symlink + AC6 console-script invariant ──────────────
echo "Test 9: provisioning exposes verifiable globals and links the checkout (AC1, #4053)"
assert_eq "$PROVISIONED_DISPATCHER_BIN" "$BIN/loom" "PROVISIONED_DISPATCHER_BIN points at the installed dispatcher"
assert_eq "$PROVISIONED_LOOM_CHECKOUT" "$HOME_T/.local/share/loom" "PROVISIONED_LOOM_CHECKOUT points at the checkout"
[[ -x "$BIN/loom" ]] && pass "dispatcher installed and executable" || fail "dispatcher not installed/executable"
if [[ -L "$HOME_T/.local/share/loom" ]]; then
    tgt="$(readlink "$HOME_T/.local/share/loom")"
    assert_eq "$tgt" "$REPO_ROOT" "checkout is a symlink to the source checkout (cannot diverge)"
else
    fail "checkout was not established as a symlink"
fi
# Idempotent second run.
set +e
provision_loom_dispatcher "$REPO_ROOT" "$BIN" "$HOME_T/.local/share/loom" >/dev/null 2>&1
rc=$?
set -e 2>/dev/null || true
assert_eq "$rc" "0" "provisioning is idempotent (second run returns 0)"

echo "Test 10: provisioning never touches the 23 loom-* console-script symlinks (AC6)"
BIN2=$(mktemp -d)
HOME2=$(mktemp -d)
# Seed 23 dummy console-script files, snapshot their checksums.
for i in $(seq 1 23); do echo "console-script-$i" > "$BIN2/loom-fake$i"; done
before=$(cd "$BIN2" && for f in loom-fake*; do printf '%s:%s\n' "$f" "$(cksum < "$f")"; done | sort)
source "$PROVISION_LIB"
provision_loom_dispatcher "$REPO_ROOT" "$BIN2" "$HOME2/.local/share/loom" >/dev/null 2>&1 || true
after=$(cd "$BIN2" && for f in loom-fake*; do printf '%s:%s\n' "$f" "$(cksum < "$f")"; done | sort)
assert_eq "$after" "$before" "all 23 loom-* entries are byte-identical before/after provisioning"
[[ -f "$BIN2/loom" ]] && pass "only the 'loom' dispatcher was added" || fail "dispatcher not added to bin dir"
count=$(cd "$BIN2" && ls loom-fake* 2>/dev/null | wc -l | tr -d ' ')
assert_eq "$count" "23" "console-script count unchanged (23)"

echo ""
echo "======================================"
echo "test-loom-dispatcher.sh: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"
echo "======================================"
[[ "$TESTS_FAILED" -eq 0 ]]
