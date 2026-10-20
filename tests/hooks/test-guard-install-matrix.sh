#!/usr/bin/env bash
# test-guard-install-matrix.sh — installer-matrix receipts for the shared
# generic-guard design (#8570, docs/design/loom-guard-extraction.md section 6).
#
# Usage: ./tests/hooks/test-guard-install-matrix.sh
#
# Exercises the six install-matrix cells in throwaway repos, with a sandboxed
# $HOME/$LOOM_HOME (nothing under the real ~/.claude or ~/.local is touched) and
# a STUB Repo Skills tree (.claude/skills/repo/hooks/guard-destructive.sh) that
# carries the five dispatcher probe markers. Hermetic: no Repo Skills checkout,
# network or forge is needed, so this runs in CI.
#
# What is REAL here: scripts/install/provision-hooks.sh (provision_loom_hooks,
# deprovision_loom_hooks, ensure_project_hook_wiring), the user-scope command
# wrapper it writes (executed verbatim from settings.json), the real dispatcher
# defaults/hooks/guard-destructive.sh, the real vendored engine, and the real
# defaults/scripts/check-guards-installed.sh.
# What is SIMULATED: the Repo Skills installer (a stub tree is copied in/out) and
# `loom-daemon init`'s file copy (a `cp` of defaults/hooks into .loom/hooks).
# Nothing here proves how the real /repo installer behaves; that is "not
# measured" in the design.
#
# Invariant asserted after every operation: exactly ONE engine decides (the stub
# canonical logs each invocation; the vendored engine is identified by a deny
# that does not carry the stub sentinel), and check-guards-installed.sh reports
# no BROKEN hook.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
PROVISION="$REPO_ROOT/scripts/install/provision-hooks.sh"
CHECK="$REPO_ROOT/defaults/scripts/check-guards-installed.sh"
HOOKS_SRC="$REPO_ROOT/defaults/hooks"

command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 2; }
for f in "$PROVISION" "$CHECK" "$HOOKS_SRC/guard-destructive.sh" "$HOOKS_SRC/guard-destructive-generic.sh"; do
    [[ -r "$f" ]] || { echo "missing prerequisite: $f" >&2; exit 2; }
done

# Hermetic env: ambient overrides must not steer either engine or the sandbox.
unset LOOM_FORCE_SCOPE LOOM_GUARD_OVERRIDE LOOM_GUARD_DECISION_LOG LOOM_HOOKS_HOME \
      LOOM_PROJECT_ROOT LOOM_WORKTREE_PATH LOOM_ROLE LOOM_GUARD_WORKTREE_ISOLATION

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
export HOME="$WORK/home"
export LOOM_HOME="$WORK/loom-home"
mkdir -p "$HOME/.claude" "$LOOM_HOME"
# Machine checkout stand-in: the wrapper execs $LOOM_HOME/defaults/hooks/<name>.
ln -s "$REPO_ROOT/defaults" "$LOOM_HOME/defaults"
export ENGINE_LOG="$WORK/engine.log"

PASS=0; FAIL=0
ok()   { PASS=$((PASS + 1)); printf '  PASS: %s\n' "$1"; }
bad()  { FAIL=$((FAIL + 1)); printf '  FAIL: %s\n' "$1"; }
expect() { # <description> <actual> <expected>
    if [[ "$2" == "$3" ]]; then ok "$1 [$2]"; else bad "$1 (got '$2', want '$3')"; fi
}

# --- repo builders -----------------------------------------------------------
new_repo() { # <name> -> prints path of a fresh Loom-workspace git repo
    local r="$WORK/$1"
    mkdir -p "$r/.loom"
    git -C "$r" init -q
    git -C "$r" checkout -q -b main 2>/dev/null || true
    git -C "$r" -c user.email=t@t -c user.name=t commit -q --allow-empty -m init
    echo '{}' >"$r/.loom/config.json"
    printf '%s' "$(cd "$r" && pwd -P)"
}

# Stub Repo Skills guard: carries every dispatcher marker, logs each run, and
# denies everything with a sentinel reason so the engine is identifiable.
write_stub_repo_skills() { # <repo> [omit-marker]
    local d="$1/.claude/skills/repo/hooks"
    mkdir -p "$d"
    {
        echo '#!/usr/bin/env bash'
        echo '# stub canonical guard: repo#29 worktree-write-confinement'
        [[ "${2:-}" == "search" ]] || echo '# markers: --comment|--search and --arg|--argjson'
        [[ "${2:-}" == "bodyat" ]] || echo '# marker: gh-comment-body-literal-at'
        echo 'cat >/dev/null'
        echo 'echo canonical >>"$ENGINE_LOG"'
        echo "echo '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"STUB-CANONICAL\"}}'"
    } >"$d/guard-destructive.sh"
    chmod +x "$d/guard-destructive.sh"
}
remove_stub_repo_skills() { rm -rf "$1/.claude/skills/repo"; }

copy_project_hooks() { # <repo>: simulate `loom-daemon init` per-repo copy (quick install)
    mkdir -p "$1/.loom/hooks"
    cp -p "$HOOKS_SRC"/*.sh "$1/.loom/hooks/"
}

PROBE='{"tool_name":"Bash","tool_input":{"command":"rm -rf /"}}'

# engine_of_cmd <repo> <command...>: run an entry point in the repo and name the
# engine that decided: canonical | vendored | none | MULTI. Also prints the
# number of decision objects as the second word.
engine_of_cmd() {
    local repo="$1"; shift
    : >"$ENGINE_LOG"
    local out
    out="$(cd "$repo" && printf '%s' "$(jq -c --arg d "$repo" '. + {cwd:$d}' <<<"$PROBE")" | "$@" 2>/dev/null)"
    local n runs
    n="$(printf '%s' "$out" | jq -s 'length' 2>/dev/null || echo 0)"
    runs="$(grep -c canonical "$ENGINE_LOG" 2>/dev/null || true)"
    if [[ "$n" -eq 0 ]]; then echo "none 0"; return; fi
    if [[ "$n" -gt 1 || "$runs" -gt 1 ]]; then echo "MULTI $n"; return; fi
    if printf '%s' "$out" | grep -q STUB-CANONICAL; then echo "canonical 1"; return; fi
    if printf '%s' "$out" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1; then
        echo "vendored 1"; return
    fi
    echo "none 0"
}

# The user-scope wrapper exactly as provision_loom_hooks wrote it to settings.json.
user_scope_cmd() {
    jq -r '[.hooks.PreToolUse[]?.hooks[]?.command | select(contains("guard-destructive.sh"))][0] // empty' \
        "$HOME/.claude/settings.json" 2>/dev/null
}
engine_user_scope() { # <repo>
    local cmd; cmd="$(user_scope_cmd)"
    [[ -n "$cmd" ]] || { echo "none 0"; return; }
    engine_of_cmd "$1" bash -c "$cmd"
}
engine_project() { engine_of_cmd "$1" bash "$1/.loom/hooks/guard-destructive.sh"; }

check_installed() { # <repo> -> exit code of check-guards-installed.sh
    bash "$CHECK" --root "$1" --quiet >/dev/null 2>&1; echo $?
}
loom_entries() { # count of Loom-owned (/defaults/hooks/) commands in user settings
    jq '[.. | objects | .command? // empty | select(contains("/defaults/hooks/"))] | length' \
        "$HOME/.claude/settings.json" 2>/dev/null || echo 0
}

# shellcheck source=scripts/install/provision-hooks.sh
source "$PROVISION"
provision() { provision_loom_hooks "$HOME/.claude" >/dev/null 2>&1; }
deprovision() { deprovision_loom_hooks "$HOME/.claude" >/dev/null 2>&1; }
reset_user_scope() { rm -f "$HOME/.claude/settings.json"; rm -f "$HOME/.claude/settings.json".bak* 2>/dev/null; }

echo "=== Cell 1: /loom only, full install (user-scope wiring, machine checkout) ==="
R1="$(new_repo cell1)"
reset_user_scope
provision
N1="$(loom_entries)"
[[ "$N1" -gt 0 ]] && ok "install wired $N1 Loom-owned hook commands" || bad "install wired no Loom hooks"
expect "install: one engine (vendored fallback)" "$(engine_user_scope "$R1")" "vendored 1"
expect "install: check-guards-installed (no project copies, machine copy present)" "$(check_installed "$R1")" "0"
provision
expect "update: re-provision is idempotent (entry count unchanged)" "$(loom_entries)" "$N1"
expect "update: still one engine" "$(engine_user_scope "$R1")" "vendored 1"
write_stub_repo_skills "$R1"
expect "rollback-forward: compatible Repo Skills appears, still exactly one engine" "$(engine_user_scope "$R1")" "canonical 1"
remove_stub_repo_skills "$R1"
expect "rollback: Repo Skills artifact removed, dispatcher falls back to vendored" "$(engine_user_scope "$R1")" "vendored 1"
jq '.userPref = {"keep": true}' "$HOME/.claude/settings.json" >"$WORK/s.json" && mv "$WORK/s.json" "$HOME/.claude/settings.json"
deprovision
expect "uninstall: no Loom-owned hook commands remain" "$(loom_entries)" "0"
expect "uninstall: user-owned settings key preserved" "$(jq -r '.userPref.keep' "$HOME/.claude/settings.json")" "true"
expect "uninstall: user-scope wrapper gone, so no engine is wired" "$(engine_user_scope "$R1")" "none 0"

echo "=== Cell 2: /loom only, quick install (per-repo .loom/hooks copy + project settings) ==="
R2="$(new_repo cell2)"
reset_user_scope
copy_project_hooks "$R2"
ensure_project_hook_wiring "$R2" >/dev/null 2>&1
# PROJECT_HOOKS_WIRED is the number of project-level entries asserted, not a boolean.
[[ "${PROJECT_HOOKS_WIRED:-0}" -gt 0 ]] && ok "install: project wiring recorded (${PROJECT_HOOKS_WIRED} entries)" || bad "install: no project wiring recorded"
expect "install: one engine (vendored fallback)" "$(engine_project "$R2")" "vendored 1"
expect "install: check-guards-installed" "$(check_installed "$R2")" "0"
chmod -x "$R2/.loom/hooks/guard-destructive-generic.sh"
CHECK2_NONEXEC_ENGINE="$(check_installed "$R2")"
chmod +x "$R2/.loom/hooks/guard-destructive-generic.sh"
mv "$R2/.loom/hooks/guard-destructive-generic.sh" "$WORK/generic.saved"
ZERO_ENGINE="$(engine_project "$R2")"
CHECK2_ZERO="$(check_installed "$R2")"
mv "$WORK/generic.saved" "$R2/.loom/hooks/guard-destructive-generic.sh"
expect "zero-engine case: dispatcher fails open (no decision, no crash)" "$ZERO_ENGINE" "none 0"
echo "  RECEIPT: check-guards-installed exit with the vendored engine file deleted = $CHECK2_ZERO (0 means it does NOT report a missing engine; design section 6 records this)"
echo "  RECEIPT: check-guards-installed exit with the vendored engine file non-executable = $CHECK2_NONEXEC_ENGINE"
mv "$R2/.loom/hooks/guard-destructive.sh" "$WORK/dispatcher.saved"
expect "wired dispatcher copy missing, machine checkout present: DEGRADED, not BROKEN (exit 0)" "$(check_installed "$R2")" "0"
EMPTY_LOOM_HOME="$WORK/empty-loom-home"; mkdir -p "$EMPTY_LOOM_HOME"
expect "negative control: wired dispatcher missing AND no machine checkout is BROKEN (exit 2)" \
    "$(LOOM_HOME="$EMPTY_LOOM_HOME" check_installed "$R2")" "2"
mv "$WORK/dispatcher.saved" "$R2/.loom/hooks/guard-destructive.sh"
expect "update: re-copy hooks keeps one engine" "$(copy_project_hooks "$R2"; engine_project "$R2")" "vendored 1"
rm -rf "$R2/.loom/hooks"
expect "uninstall: per-repo hooks removed, project settings still wire them, machine checkout present: DEGRADED (exit 0)" "$(check_installed "$R2")" "0"
expect "uninstall: same, but no machine checkout: BROKEN (exit 2)" "$(LOOM_HOME="$WORK/empty-loom-home" check_installed "$R2")" "2"

echo "=== Cell 3: /repo only (no Loom) ==="
R3="$WORK/cell3"
mkdir -p "$R3"
git -C "$R3" init -q
write_stub_repo_skills "$R3"
expect "install: Repo Skills guard decides alone" "$(engine_of_cmd "$R3" bash "$R3/.claude/skills/repo/hooks/guard-destructive.sh")" "canonical 1"
expect "install: check-guards-installed treats a non-Loom repo as a clean no-op" "$(check_installed "$R3")" "0"
reset_user_scope
expect "no Loom wiring present, so no Loom engine can double-fire" "$(engine_user_scope "$R3")" "none 0"
remove_stub_repo_skills "$R3"
expect "uninstall: Repo Skills removal leaves no engine (the /repo side owns this)" "$(engine_of_cmd "$R3" bash "$R3/.claude/skills/repo/hooks/guard-destructive.sh")" "none 0"

echo "=== Cell 4: both, /repo installed first, then /loom ==="
R4="$(new_repo cell4)"
reset_user_scope
write_stub_repo_skills "$R4"
copy_project_hooks "$R4"
ensure_project_hook_wiring "$R4" >/dev/null 2>&1
expect "install /loom after /repo: one engine, the compatible Repo Skills guard" "$(engine_project "$R4")" "canonical 1"
expect "install: check-guards-installed" "$(check_installed "$R4")" "0"
HASH4="$(sha256sum "$R4/.loom/hooks/guard-destructive.sh" | cut -c1-16)"
write_stub_repo_skills "$R4" search
expect "update: Repo Skills updated to a build lacking probe (c): fallback to vendored, still one engine" "$(engine_project "$R4")" "vendored 1"
write_stub_repo_skills "$R4" bodyat
expect "update: build lacking probe (d): fallback to vendored, still one engine" "$(engine_project "$R4")" "vendored 1"
write_stub_repo_skills "$R4"
expect "update: compatible build again: canonical, one engine" "$(engine_project "$R4")" "canonical 1"
remove_stub_repo_skills "$R4"
expect "uninstall /repo: Loom falls back to vendored (rollback), one engine" "$(engine_project "$R4")" "vendored 1"
expect "uninstall /repo: Loom dispatcher untouched" "$(sha256sum "$R4/.loom/hooks/guard-destructive.sh" | cut -c1-16)" "$HASH4"

echo "=== Cell 5: both, /loom installed first, then /repo ==="
R5="$(new_repo cell5)"
reset_user_scope
copy_project_hooks "$R5"
ensure_project_hook_wiring "$R5" >/dev/null 2>&1
HASH5="$(sha256sum "$R5/.claude/settings.json" | cut -c1-16)"
expect "install /loom alone: vendored" "$(engine_project "$R5")" "vendored 1"
write_stub_repo_skills "$R5"
expect "install /repo after /loom: one engine, canonical" "$(engine_project "$R5")" "canonical 1"
expect "install /repo does not rewrite Loom's settings wiring (simulated installer: nothing writes it)" "$(sha256sum "$R5/.claude/settings.json" | cut -c1-16)" "$HASH5"
expect "install: check-guards-installed" "$(check_installed "$R5")" "0"
rm -rf "$R5/.loom/hooks"
copy_project_hooks "$R5"
expect "update /loom with /repo present: one engine" "$(engine_project "$R5")" "canonical 1"
remove_stub_repo_skills "$R5"
expect "uninstall /repo: vendored again, one engine" "$(engine_project "$R5")" "vendored 1"

echo "=== Cell 6: transition (per-repo copy AND user-scope wiring present, #4200 dedup) ==="
R6="$(new_repo cell6)"
reset_user_scope
copy_project_hooks "$R6"
ensure_project_hook_wiring "$R6" >/dev/null 2>&1
provision
expect "user-scope wrapper defers to the project copy (no decision, so no double fire)" "$(engine_user_scope "$R6")" "none 0"
expect "project entry decides alone: vendored, one engine" "$(engine_project "$R6")" "vendored 1"
write_stub_repo_skills "$R6"
expect "with Repo Skills present: user-scope still defers" "$(engine_user_scope "$R6")" "none 0"
expect "with Repo Skills present: project entry runs canonical once" "$(engine_project "$R6")" "canonical 1"
rm -rf "$R6/.loom/hooks"
expect "per-repo copy stripped: user-scope wrapper takes over, one engine (canonical)" "$(engine_user_scope "$R6")" "canonical 1"
remove_stub_repo_skills "$R6"
expect "per-repo copy stripped, no Repo Skills: vendored via machine checkout" "$(engine_user_scope "$R6")" "vendored 1"

echo
echo "Results: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
