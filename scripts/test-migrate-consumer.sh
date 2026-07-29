#!/usr/bin/env bash
# Test suite for scripts/install/migrate-consumer.sh (Epic #3835 Phase 6, #4254).
#
# Builds throwaway git fixtures that simulate a historical 0.12-style file-copy
# Loom install (install-metadata.json + installed_files manifest, committed
# implementation, a pinned locally-edited file, deprecated tombstone artifacts,
# sweep.modelAliases in the legacy config, and untracked runtime state) and
# drives `loom migrate` against them, asserting the machine-model target layout.
#
# Fast + hermetic: temp git repos, no network, no gh auth, no daemon. Workspace
# registration is best-effort and skipped when loom-daemon is absent (asserted).
#
# Usage: bash scripts/test-migrate-consumer.sh

set -uo pipefail

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
passed=0; failed=0
pass() { echo -e "${GREEN}✓${NC} $1"; passed=$((passed + 1)); }
fail() { echo -e "${RED}✗${NC} $1"; failed=$((failed + 1)); }
warn() { echo -e "${YELLOW}!${NC} $1"; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOOM_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
MIGRATE="$LOOM_ROOT/scripts/install/migrate-consumer.sh"
export LOOM_ROOT   # so migrate-consumer.sh resolves defaults/ from this checkout

TEST_DIR="$(mktemp -d)"
cleanup() { [[ -n "${TEST_DIR:-}" && -d "$TEST_DIR" ]] && rm -rf "$TEST_DIR"; }
trap cleanup EXIT

# Run the migration with an isolated HOME so `loom-daemon workspace add` writes
# to a throwaway registry under TEST_DIR instead of polluting the real
# ~/.loom/workspaces.json with paths that vanish on cleanup.
FAKE_HOME="$TEST_DIR/home"
mkdir -p "$FAKE_HOME"
run_migrate() { HOME="$FAKE_HOME" bash "$MIGRATE" "$@"; }

# Build a fixture repo with a committed 0.12-style install.
#   - manifest lists current-impl files, deprecated tombstones, a shim, and a
#     repo-level file
#   - legacy .loom/config.json carries guards/buildGate/worktree.root AND
#     sweep.modelAliases (the scope-guard-1 exclusion target)
#   - .loom/resync-ignore pins a locally-edited script
#   - untracked runtime state under .loom/logs, .loom/tokens, .loom/sweep-checkpoint
make_fixture() {
  local repo="$1"
  mkdir -p "$repo"/.loom/{scripts,hooks,bin,logs,tokens,sweep-checkpoint} \
           "$repo"/.claude/commands/loom "$repo"/.claude/agents "$repo"/.github
  git -C "$repo" init --quiet
  git -C "$repo" config user.email "t@e.com"; git -C "$repo" config user.name "T"

  # Legacy config with the modelAliases key that must NOT migrate.
  cat > "$repo/.loom/config.json" <<'JSON'
{
  "guards": { "sqlDdl": true, "cloudCli": false },
  "buildGate": { "enabled": true, "command": "make test" },
  "worktree": { "root": "/mnt/scratch" },
  "sweep": { "modelAliases": { "opus": "claude-opus-4-8" }, "maxParallel": 3 }
}
JSON

  echo 'echo worktree' > "$repo/.loom/scripts/worktree.sh"
  echo 'guard'         > "$repo/.loom/hooks/guard-destructive.sh"
  echo 'pool manager'  > "$repo/.loom/bin/loom"
  echo 'builder skill' > "$repo/.claude/commands/loom/builder.md"
  echo 'DEPRECATED iteration' > "$repo/.claude/commands/loom/loom-iteration.md"
  echo 'DEPRECATED parent'    > "$repo/.claude/commands/loom/loom-parent.md"
  echo 'builder agent'  > "$repo/.claude/agents/loom-builder.md"
  echo 'exec loom "$@"' > "$repo/loom.sh"
  echo 'consumer labels' > "$repo/.github/labels.yml"
  echo 'node_modules/'   > "$repo/.gitignore"

  # Pinned + locally edited script.
  echo 'custom-pinned'  > "$repo/.loom/scripts/custom.sh"
  echo 'scripts/custom.sh' > "$repo/.loom/resync-ignore"

  # CLAUDE.md with the marker section.
  cat > "$repo/CLAUDE.md" <<'MD'
# My Project

<!-- BEGIN LOOM ORCHESTRATION -->
Old MCP-observer loop content.
<!-- END LOOM ORCHESTRATION -->

More project docs.
MD

  # Historical manifest.
  cat > "$repo/.loom/install-metadata.json" <<'JSON'
{
  "loom_version": "0.12.0",
  "loom_commit": "abc1234",
  "install_date": "2026-07-21",
  "installed_files": [
    ".loom/config.json",
    ".loom/scripts/worktree.sh",
    ".loom/scripts/custom.sh",
    ".loom/hooks/guard-destructive.sh",
    ".loom/bin/loom",
    ".claude/commands/loom/builder.md",
    ".claude/commands/loom/loom-iteration.md",
    ".claude/commands/loom/loom-parent.md",
    ".claude/agents/loom-builder.md",
    ".github/labels.yml",
    "loom.sh",
    "CLAUDE.md"
  ]
}
JSON

  git -C "$repo" add -A
  git -C "$repo" commit -q -m "committed 0.12-style loom install"

  # Untracked runtime state (created AFTER the commit so it stays untracked).
  echo 'log line'  > "$repo/.loom/logs/sweep.log"
  echo 'token'     > "$repo/.loom/tokens/acct.env"
  echo 'ckpt'      > "$repo/.loom/sweep-checkpoint/issue-1.json"
}

echo "======================================"
echo "migrate-consumer.sh tests"
echo "======================================"
echo ""

# ==========================================================================
# 1. Refusal: no historical install (no metadata)
# ==========================================================================
echo "--- refusal + guards ---"
R0="$TEST_DIR/no-metadata"
mkdir -p "$R0"; git -C "$R0" init --quiet
git -C "$R0" config user.email t@e.com; git -C "$R0" config user.name T
echo x > "$R0/f"; git -C "$R0" add -A; git -C "$R0" commit -q -m init
before="$(git -C "$R0" status --porcelain)"
if run_migrate "$R0" > "$TEST_DIR/r0.out" 2>&1; then
  fail "missing metadata should refuse (non-zero exit)"
else
  pass "missing metadata refuses (non-zero exit)"
fi
after="$(git -C "$R0" status --porcelain)"
[[ "$before" == "$after" ]] && pass "refusal made zero changes" || fail "refusal changed the tree"

# Dirty-tree guard.
D0="$TEST_DIR/dirty"; make_fixture "$D0"
echo 'dirty edit' >> "$D0/.github/labels.yml"   # uncommitted tracked change
if run_migrate "$D0" > "$TEST_DIR/dirty.out" 2>&1; then
  fail "dirty tree should refuse without --force"
else
  pass "dirty tree refuses without --force"
fi
echo ""

# ==========================================================================
# 2. Dry run: full plan, zero changes
# ==========================================================================
echo "--- dry run ---"
DR="$TEST_DIR/dryrun"; make_fixture "$DR"
before="$(git -C "$DR" status --porcelain)"
run_migrate "$DR" --dry-run > "$TEST_DIR/dry.out" 2>&1 && rc=0 || rc=$?
[[ "$rc" -eq 0 ]] && pass "--dry-run exits 0" || fail "--dry-run exits 0 (rc=$rc)"
after="$(git -C "$DR" status --porcelain)"
[[ "$before" == "$after" ]] && pass "--dry-run makes zero changes" || fail "--dry-run mutated the tree"
[[ ! -f "$DR/.loom-project/project.json" ]] && pass "--dry-run does not write project.json" || fail "--dry-run wrote project.json"
grep -q "would untrack" "$TEST_DIR/dry.out" && pass "--dry-run prints an untrack plan" || fail "--dry-run untrack plan missing"
grep -q "would remove" "$TEST_DIR/dry.out" && pass "--dry-run prints a deprecated-removal plan" || fail "--dry-run removal plan missing"
grep -q "would create.*project.json" "$TEST_DIR/dry.out" && pass "--dry-run plans project.json" || fail "--dry-run project.json plan missing"
echo ""

# ==========================================================================
# 3. Apply: target layout
# ==========================================================================
echo "--- apply (target layout) ---"
A="$TEST_DIR/apply"; make_fixture "$A"
run_migrate "$A" > "$TEST_DIR/apply.out" 2>&1 && rc=0 || rc=$?
[[ "$rc" -eq 0 ]] && pass "apply exits 0" || { fail "apply exits 0 (rc=$rc)"; cat "$TEST_DIR/apply.out"; }

# 3a. project.json extracted, tracked, and modelAliases EXCLUDED.
if [[ -f "$A/.loom-project/project.json" ]] \
   && [[ -n "$(git -C "$A" ls-files -- .loom-project/project.json)" ]]; then
  pass "project.json created + tracked"
else
  fail "project.json not created/tracked"
fi
if command -v jq >/dev/null 2>&1; then
  if jq -e '.sweep.modelAliases' "$A/.loom-project/project.json" >/dev/null 2>&1; then
    fail "project.json still contains sweep.modelAliases (scope guard 1 violated)"
  else
    pass "project.json excludes sweep.modelAliases (scope guard 1)"
  fi
  jq -e '.guards.sqlDdl == true and .buildGate.command == "make test"' \
     "$A/.loom-project/project.json" >/dev/null 2>&1 \
     && pass "project.json carries guards + buildGate" \
     || fail "project.json missing guards/buildGate"
  # sweep.maxParallel (a non-modelAliases sweep key) is retained.
  jq -e '.sweep.maxParallel == 3' "$A/.loom-project/project.json" >/dev/null 2>&1 \
     && pass "project.json keeps non-modelAliases sweep keys" \
     || fail "project.json dropped sweep.maxParallel"
else
  warn "jq unavailable — skipping project.json content assertions"
fi

# 3b. current implementation untracked but still on disk.
if [[ -z "$(git -C "$A" ls-files -- .loom/scripts/worktree.sh)" ]] \
   && [[ -z "$(git -C "$A" ls-files -- .claude/commands/loom/builder.md)" ]] \
   && [[ -f "$A/.loom/scripts/worktree.sh" ]] \
   && [[ -f "$A/.claude/commands/loom/builder.md" ]]; then
  pass "implementation untracked + kept on disk"
else
  fail "implementation untrack/keep failed"
fi

# 3c. gitignore block written.
grep -qF "/.loom/" "$A/.gitignore" && grep -qF "/.claude/commands/loom/" "$A/.gitignore" \
  && pass "gitignore implementation block written" || fail "gitignore block missing"

# 3d. shims preserved (tracked + on disk).
if [[ -n "$(git -C "$A" ls-files -- loom.sh)" ]] \
   && [[ -n "$(git -C "$A" ls-files -- .loom/bin/loom)" ]] \
   && [[ -f "$A/loom.sh" ]] && [[ -f "$A/.loom/bin/loom" ]]; then
  pass "loom.sh + .loom/bin/loom shims preserved (tracked)"
else
  fail "shims not preserved"
fi

# 3e. repo-level file stays tracked.
[[ -n "$(git -C "$A" ls-files -- .github/labels.yml)" ]] \
  && pass ".github/labels.yml stays tracked" || fail ".github/labels.yml untracked"

# 3f. pinned file left tracked (not untracked).
[[ -n "$(git -C "$A" ls-files -- .loom/scripts/custom.sh)" ]] \
  && pass "pinned .loom/scripts/custom.sh stays tracked" || fail "pinned file was untracked"
grep -q "pinned in .loom/resync-ignore" "$TEST_DIR/apply.out" \
  && pass "pinned file surfaced in report" || fail "pinned file not surfaced"

# 3g. deprecated tombstones removed (disk + index).
if [[ ! -f "$A/.claude/commands/loom/loom-iteration.md" ]] \
   && [[ ! -f "$A/.claude/commands/loom/loom-parent.md" ]] \
   && [[ -z "$(git -C "$A" ls-files -- .claude/commands/loom/loom-iteration.md)" ]]; then
  pass "deprecated tombstones removed (disk + index)"
else
  fail "deprecated tombstones not removed"
fi

# 3h. runtime state untouched.
if [[ -f "$A/.loom/logs/sweep.log" ]] && [[ -f "$A/.loom/tokens/acct.env" ]] \
   && [[ -f "$A/.loom/sweep-checkpoint/issue-1.json" ]]; then
  pass "runtime state (logs/tokens/sweep-checkpoint) untouched"
else
  fail "runtime state disturbed"
fi

# 3i. metadata re-stamped.
if command -v jq >/dev/null 2>&1; then
  jq -e '.migrated_to_machine_model and .install_model == "machine-daemon"' \
     "$A/.loom/install-metadata.json" >/dev/null 2>&1 \
     && pass "install-metadata.json re-stamped" || fail "metadata not re-stamped"
fi

# 3j. CLAUDE.md marker section refreshed.
grep -q "machine-level daemon model" "$A/CLAUDE.md" \
  && grep -qc "<!-- BEGIN LOOM ORCHESTRATION -->" "$A/CLAUDE.md" >/dev/null \
  && [[ "$(grep -cF "<!-- BEGIN LOOM ORCHESTRATION -->" "$A/CLAUDE.md")" -eq 1 ]] \
  && pass "CLAUDE.md marker section refreshed (single block)" \
  || fail "CLAUDE.md marker section not refreshed"

# 3k. workspace registration reported (skipped when daemon absent).
if command -v loom-daemon >/dev/null 2>&1; then
  grep -q "registered" "$TEST_DIR/apply.out" && pass "workspace registered" || warn "workspace registration not reported"
else
  grep -q "loom-daemon not on PATH" "$TEST_DIR/apply.out" \
    && pass "workspace registration cleanly skipped (no daemon)" \
    || fail "missing-daemon path not handled"
fi
echo ""

# ==========================================================================
# 4. Idempotency: second apply is a no-op
# ==========================================================================
echo "--- idempotency ---"
git -C "$A" add -A; git -C "$A" commit -q -m "migrated" 2>/dev/null || true
before="$(git -C "$A" status --porcelain)"
run_migrate "$A" > "$TEST_DIR/apply2.out" 2>&1 && rc=0 || rc=$?
[[ "$rc" -eq 0 ]] && pass "second apply exits 0" || fail "second apply exits 0 (rc=$rc)"
after="$(git -C "$A" status --porcelain)"
[[ "$before" == "$after" ]] && pass "second apply is a no-op (clean tree)" || { fail "second apply mutated the tree"; git -C "$A" status --porcelain; }
grep -q "already present" "$TEST_DIR/apply2.out" \
  && pass "second run reports project.json already present" || warn "no 'already present' note on rerun"
echo ""

# ==========================================================================
# 5. Edge: repo with no .loom/resync-ignore
# ==========================================================================
echo "--- edge: no resync-ignore ---"
E="$TEST_DIR/no-ignore"; make_fixture "$E"
rm -f "$E/.loom/resync-ignore"
git -C "$E" rm -q --cached .loom/resync-ignore >/dev/null 2>&1 || true
git -C "$E" commit -q -am "drop resync-ignore" 2>/dev/null || true
run_migrate "$E" > "$TEST_DIR/e.out" 2>&1 && rc=0 || rc=$?
[[ "$rc" -eq 0 ]] && pass "migrate with no resync-ignore exits 0" || fail "no-resync-ignore rc=$rc"
[[ -z "$(git -C "$E" ls-files -- .loom/scripts/custom.sh)" ]] \
  && pass "unpinned custom.sh untracked when no resync-ignore" || fail "custom.sh handling wrong"
echo ""

echo "======================================"
echo -e "${GREEN}Passed: $passed${NC}   ${RED}Failed: $failed${NC}"
echo "======================================"
[[ "$failed" -eq 0 ]] && { echo -e "${GREEN}All tests passed!${NC}"; exit 0; } || { echo -e "${RED}Some tests failed.${NC}"; exit 1; }
