#!/usr/bin/env bash
# test-guide-post-stage-hook.sh - Regression test for issue #13892
# (Guide docs tick's fixed staging set cannot satisfy a consuming repo's CI
# integrity check on a staged file).
#
# Executes the prompt's OWN code, extracted from guide.md, against a throwaway
# git repo. Verifies create_docs_pr()'s post-stage hook block:
#   1. no hook present -> staging is exactly the fixed set (behaviour
#      identical to a repo without the hook), lock untouched;
#   2. hook that creates a file and prints it -> staged by exact path;
#   3. hook that modifies a tracked file and prints it -> staged;
#   4. hook that writes a file but prints NOTHING -> the file is NOT staged
#      (nothing unprinted is swept in);
#   5. blank lines among the printed paths are ignored;
#   6. hook runs with cwd = $DOCS_WT and $DOCS_WT exported;
#   7. hook that exits non-zero -> fail-closed: rc 1, staged diff discarded,
#      lock released;
#   8. hook that exists but is not an executable regular file -> fail-closed;
#   9. hook that prints an absolute path -> fail-closed, not staged;
#  10. hook that prints a `..` path -> fail-closed, nothing outside staged;
#  11. hook that prints a path that cannot be staged -> fail-closed.
# Hermetic: no forge or network.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
if [[ -f "$REPO_ROOT/.claude/commands/loom/guide.md" ]]; then
    GUIDE_MD="$REPO_ROOT/.claude/commands/loom/guide.md"
else
    GUIDE_MD="$REPO_ROOT/defaults/.claude/commands/loom/guide.md"
fi

RED='\033[0;31m'; GREEN='\033[0;32m'; NC='\033[0m'
TESTS_RUN=0; TESTS_FAILED=0
pass() { TESTS_RUN=$((TESTS_RUN + 1)); echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo -e "  ${RED}FAIL${NC}: $1"; }
assert_eq() { if [[ "$1" == "$2" ]]; then pass "$3"; else fail "$3 (got '$1', expected '$2')"; fi; }
assert_grep() { if grep -qE "$1" "$2"; then pass "$3"; else fail "$3 (missing: $1)"; fi; }
assert_not_grep() { if grep -qE "$1" "$2"; then fail "$3 (unexpectedly present: $1)"; else pass "$3"; fi; }

SANDBOX="$(mktemp -d)"
trap 'rm -rf "$SANDBOX"' EXIT

# --- extract the prompt's hook-bearing staging block verbatim ------------
# From `local archive_file` through the closing `fi` of the hook block, i.e.
# everything before the DOCS_CHANGED_FILES snapshot comment.
{ echo 'stage_with_hook() {'; awk '/local archive_file/ {p=1} p && /^  # Snapshot which of the three files/ {exit} p {print}' "$GUIDE_MD"; echo '}'; } > "$SANDBOX/stage.sh"
grep -m1 "^WORK_LOG_ARCHIVE_GLOB=" "$GUIDE_MD" > "$SANDBOX/glob.sh"
[[ -s "$SANDBOX/stage.sh" ]] && pass "hook-bearing staging block extracted" || fail "hook-bearing staging block extracted"
grep -q 'docs-post-stage' "$SANDBOX/stage.sh" && pass "extracted block contains the hook" || fail "extracted block contains the hook"

# --- stub lock script -----------------------------------------------------
# The fail-closed paths call ./.loom/scripts/docs-guide-lock.sh release
# relative to the caller's cwd; run_stage() executes from a scratch dir
# carrying this stub so "lock released" is observable.
RUN_DIR="$SANDBOX/run"; mkdir -p "$RUN_DIR/.loom/scripts"
cat > "$RUN_DIR/.loom/scripts/docs-guide-lock.sh" <<'STUB'
#!/usr/bin/env bash
echo "$1" >> "$LOCK_CALLS"
STUB
chmod +x "$RUN_DIR/.loom/scripts/docs-guide-lock.sh"
LOCK_CALLS="$SANDBOX/lock-calls"; export LOCK_CALLS
OUT="$SANDBOX/out"

# --- origin + docs worktree -----------------------------------------------
ORIGIN="$SANDBOX/origin.git"; DOCS_WT="$SANDBOX/wt"
git init -q --bare "$ORIGIN"
git clone -q "$ORIGIN" "$DOCS_WT" 2>/dev/null
git -C "$DOCS_WT" config user.email t@t; git -C "$DOCS_WT" config user.name t
printf '# Work Log\n\n### 2026-11-01\n\n- **PR #9**: recent\n' > "$DOCS_WT/WORK_LOG.md"
echo plan > "$DOCS_WT/WORK_PLAN.md"; echo readme > "$DOCS_WT/README.md"
git -C "$DOCS_WT" add -A; git -C "$DOCS_WT" commit -qm base; git -C "$DOCS_WT" push -q origin HEAD:main 2>/dev/null
export DOCS_WT

# shellcheck disable=SC1090
source "$SANDBOX/glob.sh"
# shellcheck disable=SC1090
source "$SANDBOX/stage.sh"   # defines stage_with_hook()

# Re-arm one tick's worth of state: dirty WORK_LOG.md, clean index, empty
# lock-call log, no hook on disk.
rearm() {
    git -C "$DOCS_WT" reset --hard --quiet HEAD; git -C "$DOCS_WT" clean -qfd
    printf '# Work Log\n\n### 2026-11-01\n\n- **PR #9**: recent\n\n- **PR #10**: newer\n' > "$DOCS_WT/WORK_LOG.md"
    : > "$LOCK_CALLS"
    rm -f "$DOCS_WT/.loom/docs-post-stage.sh"
}
run_stage() { (cd "$RUN_DIR" && git -C "$DOCS_WT" add WORK_LOG.md WORK_PLAN.md README.md && stage_with_hook) >"$OUT" 2>&1; echo $?; }
staged() { git -C "$DOCS_WT" diff --cached --name-only | sort | paste -sd, -; }
index_clean() { git -C "$DOCS_WT" diff --cached --quiet && echo yes || echo no; }

echo "Test 1: no hook present -> fixed staging set, lock untouched"
rearm
assert_eq "$(run_stage)" "0" "no hook: staging succeeds"
assert_eq "$(staged)" "WORK_LOG.md" "no hook: exactly the fixed set is staged"
assert_eq "$(cat "$LOCK_CALLS")" "" "no hook: the lock is never released by this path"
assert_not_grep 'docs-post-stage' "$OUT" "no hook: nothing about the hook is printed"

echo "Test 2: hook creates a file and prints it -> staged by exact path"
rearm
mkdir -p "$DOCS_WT/.loom"
cat > "$DOCS_WT/.loom/docs-post-stage.sh" <<'HOOK'
#!/usr/bin/env bash
printf '{"hashes": {}}' > envelope.json
echo envelope.json
HOOK
chmod +x "$DOCS_WT/.loom/docs-post-stage.sh"
assert_eq "$(run_stage)" "0" "hook writing a printed file succeeds"
assert_eq "$(staged)" "WORK_LOG.md,envelope.json" "the printed file is staged alongside the fixed set"

echo "Test 3: hook modifies a tracked file and prints it -> staged"
rearm; mkdir -p "$DOCS_WT/.loom"
cat > "$DOCS_WT/.loom/docs-post-stage.sh" <<'HOOK'
#!/usr/bin/env bash
echo refreshed >> README.md
echo README.md
HOOK
chmod +x "$DOCS_WT/.loom/docs-post-stage.sh"
assert_eq "$(run_stage)" "0" "hook modifying a tracked file succeeds"
assert_eq "$(staged)" "README.md,WORK_LOG.md" "the modified tracked file is staged"

echo "Test 4: hook writes a file but prints nothing -> NOT staged"
rearm; mkdir -p "$DOCS_WT/.loom"
cat > "$DOCS_WT/.loom/docs-post-stage.sh" <<'HOOK'
#!/usr/bin/env bash
printf '{"hashes": {}}' > envelope.json
HOOK
chmod +x "$DOCS_WT/.loom/docs-post-stage.sh"
assert_eq "$(run_stage)" "0" "silent hook succeeds (exit 0, no paths)"
assert_eq "$(staged)" "WORK_LOG.md" "an unprinted file is never swept in"

echo "Test 5: blank lines among printed paths are ignored"
rearm; mkdir -p "$DOCS_WT/.loom"
cat > "$DOCS_WT/.loom/docs-post-stage.sh" <<'HOOK'
#!/usr/bin/env bash
printf '{"hashes": {}}' > envelope.json
echo ""
echo envelope.json
echo ""
HOOK
chmod +x "$DOCS_WT/.loom/docs-post-stage.sh"
assert_eq "$(run_stage)" "0" "blank lines do not fail the staging loop"
assert_eq "$(staged)" "WORK_LOG.md,envelope.json" "the real path still lands"

echo "Test 6: hook runs with cwd = \$DOCS_WT and \$DOCS_WT exported"
rearm; mkdir -p "$DOCS_WT/.loom"
cat > "$DOCS_WT/.loom/docs-post-stage.sh" <<'HOOK'
#!/usr/bin/env bash
[[ "$PWD" == "$DOCS_WT" ]] && [[ -n "$DOCS_WT" ]] && [[ -d "$DOCS_WT/.git" ]] || exit 7
printf '{"ok": true}' > contract.json
echo contract.json
HOOK
chmod +x "$DOCS_WT/.loom/docs-post-stage.sh"
assert_eq "$(run_stage)" "0" "contract check (cwd, env) holds"
assert_eq "$(staged)" "WORK_LOG.md,contract.json" "hook output staged after the contract check"

echo "Test 7: hook exits non-zero -> fail-closed"
rearm; mkdir -p "$DOCS_WT/.loom"
cat > "$DOCS_WT/.loom/docs-post-stage.sh" <<'HOOK'
#!/usr/bin/env bash
printf '{"partial": true}' > envelope.json
echo envelope.json
exit 3
HOOK
chmod +x "$DOCS_WT/.loom/docs-post-stage.sh"
assert_eq "$(run_stage)" "1" "failing hook aborts with rc 1"
assert_eq "$(index_clean)" "yes" "staged diff discarded on hook failure"
assert_eq "$(cat "$LOCK_CALLS")" "release" "lock released on hook failure"
assert_grep 'exited 3' "$OUT" "tick output names the failure"

echo "Test 8: non-executable hook -> fail-closed"
rearm; mkdir -p "$DOCS_WT/.loom"
printf '#!/usr/bin/env bash\necho WORK_LOG.md\n' > "$DOCS_WT/.loom/docs-post-stage.sh"
chmod 644 "$DOCS_WT/.loom/docs-post-stage.sh"
assert_eq "$(run_stage)" "1" "non-executable hook aborts with rc 1"
assert_eq "$(index_clean)" "yes" "staged diff discarded"
assert_eq "$(cat "$LOCK_CALLS")" "release" "lock released"
assert_grep 'not an executable regular file' "$OUT" "tick output says chmod +x or remove"

echo "Test 9: absolute path printed -> fail-closed"
rearm; mkdir -p "$DOCS_WT/.loom"
cat > "$DOCS_WT/.loom/docs-post-stage.sh" <<HOOK
#!/usr/bin/env bash
echo "$SANDBOX/outside-target"
HOOK
chmod +x "$DOCS_WT/.loom/docs-post-stage.sh"
touch "$SANDBOX/outside-target"
assert_eq "$(run_stage)" "1" "absolute path aborts with rc 1"
assert_eq "$(index_clean)" "yes" "staged diff discarded"
assert_eq "$(cat "$LOCK_CALLS")" "release" "lock released"

echo "Test 10: '..' path printed -> fail-closed, nothing outside staged"
rearm; mkdir -p "$DOCS_WT/.loom"
cat > "$DOCS_WT/.loom/docs-post-stage.sh" <<'HOOK'
#!/usr/bin/env bash
printf '{"escape": true}' > ../outside-staged
echo ../outside-staged
HOOK
chmod +x "$DOCS_WT/.loom/docs-post-stage.sh"
assert_eq "$(run_stage)" "1" "'..' path aborts with rc 1"
assert_eq "$(index_clean)" "yes" "staged diff discarded"
assert_eq "$(cat "$LOCK_CALLS")" "release" "lock released"

echo "Test 11: printed path that cannot be staged -> fail-closed"
rearm; mkdir -p "$DOCS_WT/.loom"
cat > "$DOCS_WT/.loom/docs-post-stage.sh" <<'HOOK'
#!/usr/bin/env bash
echo missing.json
HOOK
chmod +x "$DOCS_WT/.loom/docs-post-stage.sh"
assert_eq "$(run_stage)" "1" "unstagedable path aborts with rc 1"
assert_eq "$(index_clean)" "yes" "staged diff discarded"
assert_eq "$(cat "$LOCK_CALLS")" "release" "lock released"
assert_grep "could not be staged" "$OUT" "tick output names the unstagedable path"

echo ""
echo "Tests run: $TESTS_RUN, failed: $TESTS_FAILED"
[[ "$TESTS_FAILED" -eq 0 ]]
