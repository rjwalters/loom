#!/usr/bin/env bash
# Test suite for the guard-destructive.sh DISPATCHER (issue #4041, #4894, #5916).
#
# Usage: ./tests/hooks/test-guard-destructive-dispatcher.sh
#
# guard-destructive.sh is a thin dispatcher that chooses which generic guard to
# run at runtime, requiring ALL THREE of the following probes to pass before it
# defers to the canonical Repo Skills guard (#4894, #5916):
#   1. VERSION probe — the canonical guard (.claude/skills/repo/hooks/
#      guard-destructive.sh) exists AND carries the `repo#29` marker.
#   2. CAPABILITY probe (b) — the canonical guard ALSO carries the
#      `worktree-write-confinement` decision tag, i.e. it actually implements
#      the Loom-only Bash-tool write-confinement category (issue #4178), not
#      just the unrelated repo#29 curl-pipe fix.
#   3. CAPABILITY probe (c, #5916) — the canonical guard ALSO carries BOTH the
#      `--comment|--search` AND `--arg|--argjson` regex-alternation substrings,
#      i.e. it masks `gh --search` and `jq --arg`/`--argjson` quoted values
#      before the catastrophic/ask substring scans (the #5797/#5803/#5809 fix),
#      not just the version/write-confinement fixes.
# If any probe fails, the dispatcher falls back to the vendored generic
# guard (guard-destructive-generic.sh) shipped alongside it, which always
# carries all three.
#
# Before #4894 only the version probe existed, so a canonical guard that
# picked up `repo#29` WITHOUT write-confinement (Repo Skills 0.7.0) was
# exec'd anyway and the Bash-tool worktree-isolation category silently
# stopped running. Cases 6-7 below are the regression tests for that gap.
# Before #5916 only probes (a)-(b) existed, so a canonical guard that picked up
# `repo#29` and write-confinement WITHOUT the search/jq masking fix was exec'd
# anyway and false-DENYed commands like `gh issue list --search "..." --jq
# '... | ...'`. Cases 8-9 below are the regression/positive tests for that gap.
#
# These tests build an isolated fake repo tree, drop the real dispatcher +
# vendored generic into <repo>/.loom/hooks/, and stub the canonical guard so we
# can assert which one the dispatcher execs across the marker-present / marker-
# absent / canonical-absent cases. Exit 0 = all pass, 1 = failures.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
DISPATCHER_SRC="$REPO_ROOT/defaults/hooks/guard-destructive.sh"
GENERIC_SRC="$REPO_ROOT/defaults/hooks/guard-destructive-generic.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'
PASS=0
FAIL=0

check() {
  local desc="$1" expected="$2" actual="$3"
  if [[ "$expected" == "$actual" ]]; then
    echo -e "  ${GREEN}PASS${NC}: $desc"
    PASS=$((PASS + 1))
  else
    echo -e "  ${RED}FAIL${NC}: $desc (expected '$expected', got '$actual')"
    FAIL=$((FAIL + 1))
  fi
}

# A dangerous payload assembled so no literal appears in this file (which would
# trip a running guard when the file is edited/scanned elsewhere).
DANGER="rm -r""f /"

# Build an isolated fake repo; returns its path on stdout.
make_repo() {
  local repo
  repo="$(mktemp -d)"
  mkdir -p "$repo/.loom/hooks" "$repo/.claude/skills/repo/hooks"
  cp "$DISPATCHER_SRC" "$repo/.loom/hooks/guard-destructive.sh"
  cp "$GENERIC_SRC" "$repo/.loom/hooks/guard-destructive-generic.sh"
  chmod +x "$repo/.loom/hooks/"*.sh
  printf '%s' "$repo"
}

# Same as make_repo, but ALSO a real git repo carrying a Loom-managed worktree
# fixture (<repo>/.loom/worktrees/issue-1/.loom-managed) — the shape
# guard-destructive-generic.sh's write-confinement category needs to derive a
# main-checkout root via `git rev-parse --git-common-dir` and see at least one
# managed worktree. Used by the #4894 capability-probe regression cases below.
make_repo_gitwt() {
  local repo
  repo="$(make_repo)"
  # Canonicalize (mktemp -d can return a macOS /var/folders symlink whose real
  # target is /private/var/folders; the guard's git-resolved root is always
  # the symlink-resolved form, so comparisons must start from the same form).
  local real_repo
  real_repo="$(cd "$repo" && pwd -P)"
  if [[ "$real_repo" != "$repo" ]]; then
    rm -rf "$repo"
    repo="$real_repo"
    mkdir -p "$repo/.loom/hooks" "$repo/.claude/skills/repo/hooks"
    cp "$DISPATCHER_SRC" "$repo/.loom/hooks/guard-destructive.sh"
    cp "$GENERIC_SRC" "$repo/.loom/hooks/guard-destructive-generic.sh"
    chmod +x "$repo/.loom/hooks/"*.sh
  fi
  git -C "$repo" init -q >/dev/null 2>&1
  mkdir -p "$repo/.loom/worktrees/issue-1/src" "$repo/defaults/hooks"
  : > "$repo/.loom/worktrees/issue-1/.loom-managed"
  printf '%s' "$repo"
}

# Run the dispatcher in $1 with command $2; print its stdout.
run_dispatcher() {
  local repo="$1" cmd="$2"
  printf '{"tool_input":{"command":"%s"},"cwd":"%s"}\n' "$cmd" "$repo" \
    | bash "$repo/.loom/hooks/guard-destructive.sh" 2>/dev/null
}

# Same as run_dispatcher, but JSON-encodes $2 via jq -n so a command containing
# literal double/single quotes (e.g. Case 9's `gh --search "..." --jq '...'`)
# round-trips safely instead of corrupting the hand-built JSON above.
run_dispatcher_json() {
  local repo="$1" cmd="$2"
  jq -n --arg cmd "$cmd" --arg cwd "$repo" \
    '{tool_input: {command: $cmd}, cwd: $cwd}' \
    | bash "$repo/.loom/hooks/guard-destructive.sh" 2>/dev/null
}

# --- Case 1: canonical absent → vendored generic denies the dangerous payload ---
echo "Case 1: canonical absent → vendored generic runs"
REPO="$(make_repo)"
OUT="$(run_dispatcher "$REPO" "$DANGER")"
check "dangerous payload denied via vendored generic" \
  "deny" \
  "$(printf '%s' "$OUT" | jq -r '.hookSpecificOutput.permissionDecision' 2>/dev/null)"
OUT="$(run_dispatcher "$REPO" "ls -la")"
check "benign command allowed (empty output)" "" "$OUT"
rm -rf "$REPO"

# --- Case 2: canonical present but NO marker → still uses vendored generic ---
echo "Case 2: canonical present without repo#29 marker → vendored generic runs"
REPO="$(make_repo)"
printf '#!/usr/bin/env bash\necho CANON-RAN\nexit 0\n' \
  > "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
chmod +x "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
OUT="$(run_dispatcher "$REPO" "$DANGER")"
check "canonical without marker is NOT run" "" \
  "$(printf '%s' "$OUT" | grep -o 'CANON-RAN' | head -1)"
check "dangerous payload still denied via vendored generic" \
  "deny" \
  "$(printf '%s' "$OUT" | jq -r '.hookSpecificOutput.permissionDecision' 2>/dev/null)"
rm -rf "$REPO"

# --- Case 3: canonical present WITH ALL THREE markers → dispatcher execs canonical ---
echo "Case 3: canonical present with repo#29 marker, write-confinement marker, AND search/jq-mask markers → canonical runs"
REPO="$(make_repo)"
# assemble the marker so this file itself has no literal 'repo#29' token
MARK="repo#""29"
printf '#!/usr/bin/env bash\n# fixed per %s\n# implements worktree-write-confinement\n# masks --comment|--search and --arg|--argjson\necho CANON-RAN\nexit 0\n' "$MARK" \
  > "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
chmod +x "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
OUT="$(run_dispatcher "$REPO" "$DANGER")"
check "canonical guard is exec'd (all three probes pass)" "CANON-RAN" \
  "$(printf '%s' "$OUT" | grep -o 'CANON-RAN' | head -1)"
rm -rf "$REPO"

# --- Case 8: canonical has repo#29 + write-confinement but NOT search/jq-mask markers (#5916) → still vendored ---
# This is the exact real-world shape that motivated #5916: as of this writing
# rjwalters/repo has not ported the #5797/#5803/#5809 search/jq masking fix
# upstream, so a canonical guard can pass probes (a) and (b) yet still
# false-DENY `gh --search`/`jq --arg`/`--argjson` commands. Probe (c) closes
# this the same way probe (b) closed the #4894 gap.
echo "Case 8: canonical has repo#29 + write-confinement markers but NOT search/jq-mask markers → vendored generic runs (#5916)"
REPO="$(make_repo)"
printf '#!/usr/bin/env bash\n# fixed per %s\n# implements worktree-write-confinement\necho CANON-RAN\nexit 0\n' "$MARK" \
  > "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
chmod +x "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
OUT="$(run_dispatcher "$REPO" "$DANGER")"
check "canonical WITHOUT search/jq-mask markers is NOT run" "" \
  "$(printf '%s' "$OUT" | grep -o 'CANON-RAN' | head -1)"
check "dangerous payload still denied via vendored generic" \
  "deny" \
  "$(printf '%s' "$OUT" | jq -r '.hookSpecificOutput.permissionDecision' 2>/dev/null)"
rm -rf "$REPO"

# --- Case 9: regression AC (#5916) — search/jq-mask capability gap must not defeat #5797/#5803/#5809 ---
# With a canonical guard that carries repo#29 + write-confinement but NOT the
# search/jq-mask markers (today's real-world Repo Skills shape), the
# dispatcher's own issue-#5916 repro command — a `gh --search` value followed
# by a trailing single-quoted `jq --jq`/`.[] | .number` argument — must ALLOW
# through the dispatcher, proving the vendored fallback's already-fixed
# strip_literal_text() ran, not the stub canonical guard.
echo "Case 9: search/jq-mask capability-gap canonical guard → gh --search + jq pipe command still allowed through dispatcher (#5916 regression)"
REPO="$(make_repo)"
printf '#!/usr/bin/env bash\n# fixed per %s\n# implements worktree-write-confinement\necho CANON-RAN\nexit 0\n' "$MARK" \
  > "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
chmod +x "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
SEARCH_CMD='gh issue list --search "docker system prune" --jq '"'"'.[] | .number'"'"''
OUT="$(run_dispatcher_json "$REPO" "$SEARCH_CMD")"
check "search/jq-mask capability-gap canonical guard is NOT run" "" \
  "$(printf '%s' "$OUT" | grep -o 'CANON-RAN' | head -1)"
check "gh --search + jq pipe command allowed (not denied) via vendored generic's masking fix" "" \
  "$(printf '%s' "$OUT" | jq -r '.hookSpecificOutput.permissionDecision' 2>/dev/null)"
rm -rf "$REPO"

# --- Case 6: canonical has repo#29 but NOT write-confinement (#4894) → still vendored ---
# This is the exact Repo Skills 0.7.0 shape that motivated #4894: the version
# probe alone used to be sufficient, so the dispatcher would exec a canonical
# guard that never implements the Loom-only Bash-tool write-confinement
# category, silently dropping that coverage. The capability probe closes this.
echo "Case 6: canonical has repo#29 marker but NOT the write-confinement marker → vendored generic runs (#4894)"
REPO="$(make_repo)"
printf '#!/usr/bin/env bash\n# fixed per %s\necho CANON-RAN\nexit 0\n' "$MARK" \
  > "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
chmod +x "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
OUT="$(run_dispatcher "$REPO" "$DANGER")"
check "canonical WITHOUT write-confinement marker is NOT run" "" \
  "$(printf '%s' "$OUT" | grep -o 'CANON-RAN' | head -1)"
check "dangerous payload still denied via vendored generic" \
  "deny" \
  "$(printf '%s' "$OUT" | jq -r '.hookSpecificOutput.permissionDecision' 2>/dev/null)"
rm -rf "$REPO"

# --- Case 7: regression AC (#4894) — capability gap must not defeat #4178 ---
# With a canonical guard that carries the version marker but NOT the
# write-confinement marker (the Repo Skills 0.7.0 shape), AND a Loom-managed
# worktree present, a Bash-tool write into the main checkout must still
# produce a deny — proving the vendored fallback's real write-confinement
# category ran, not the stub canonical guard.
echo "Case 7: capability-gap canonical guard + managed worktree present → Bash-tool write to main checkout still denies (#4894 regression)"
REPO="$(make_repo_gitwt)"
printf '#!/usr/bin/env bash\n# fixed per %s\necho CANON-RAN\nexit 0\n' "$MARK" \
  > "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
chmod +x "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
OUT="$(run_dispatcher "$REPO" "echo hi > $REPO/defaults/hooks/f.sh")"
check "capability-gap canonical guard is NOT run" "" \
  "$(printf '%s' "$OUT" | grep -o 'CANON-RAN' | head -1)"
check "Bash-tool write to main checkout still denied (worktree-write-confinement, #4178)" \
  "deny" \
  "$(printf '%s' "$OUT" | jq -r '.hookSpecificOutput.permissionDecision' 2>/dev/null)"
rm -rf "$REPO"

# --- Case 4: neither guard available → fail-open allow (exit 0, no output) ---
echo "Case 4: no guard available → fail-open allow"
REPO="$(mktemp -d)"
mkdir -p "$REPO/.loom/hooks"
cp "$DISPATCHER_SRC" "$REPO/.loom/hooks/guard-destructive.sh"
chmod +x "$REPO/.loom/hooks/guard-destructive.sh"
OUT="$(run_dispatcher "$REPO" "$DANGER")"
RC=$?
check "no guard → empty output" "" "$OUT"
check "no guard → exit 0" "0" "$RC"
rm -rf "$REPO"


# --- Case 5: machine-level layout (Epic #3835 Phase 5, #4262) --------------
# When the dispatcher runs from a checkout (SCRIPT_DIR does NOT sit at
# <repo>/.loom/hooks), the SCRIPT_DIR-relative "../../" resolution would point
# outside the consuming repo entirely. The user-scope command wrapper
# resolves the repo root itself and passes it via LOOM_PROJECT_ROOT — confirm
# the dispatcher prefers that over its SCRIPT_DIR-relative fallback.
echo "Case 5: LOOM_PROJECT_ROOT env fallback resolves canonical guard from a checkout-shaped SCRIPT_DIR"
CHECKOUT="$(mktemp -d)"
mkdir -p "$CHECKOUT/defaults/hooks"
cp "$DISPATCHER_SRC" "$CHECKOUT/defaults/hooks/guard-destructive.sh"
cp "$GENERIC_SRC" "$CHECKOUT/defaults/hooks/guard-destructive-generic.sh"
chmod +x "$CHECKOUT/defaults/hooks/"*.sh
REPO="$(mktemp -d)"
mkdir -p "$REPO/.claude/skills/repo/hooks"
MARK="repo#""29"
printf '#!/usr/bin/env bash\n# fixed per %s\n# implements worktree-write-confinement\n# masks --comment|--search and --arg|--argjson\necho CANON-RAN\nexit 0\n' "$MARK" \
  > "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
chmod +x "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
OUT="$(printf '{"tool_input":{"command":"%s"},"cwd":"%s"}\n' "$DANGER" "$REPO" \
  | LOOM_PROJECT_ROOT="$REPO" bash "$CHECKOUT/defaults/hooks/guard-destructive.sh" 2>/dev/null)"
check "canonical guard resolved via LOOM_PROJECT_ROOT (checkout-shaped SCRIPT_DIR, all three probes pass)" "CANON-RAN" \
  "$(printf '%s' "$OUT" | grep -o 'CANON-RAN' | head -1)"
rm -rf "$CHECKOUT" "$REPO"

echo ""
echo "========================================="
echo -e "Results: ${PASS} passed, ${FAIL} failed"
echo "========================================="
[[ "$FAIL" -eq 0 ]] || exit 1
