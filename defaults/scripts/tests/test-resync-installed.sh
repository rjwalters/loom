#!/usr/bin/env bash
# test-resync-installed.sh - Smoke tests for resync-installed.sh (#3777, #4239)
#
# Constructs throwaway git repos with synthetic defaults/ and installed .loom/
# trees so it can deterministically exercise the load-bearing cases:
#   (a) already in sync     -> exit 0, "Already in sync", no writes
#   (b) drift (differing)   -> file rewritten to match defaults/, exit 0
#   (c) missing installed   -> file created from defaults/, exit 0
#   (d) --dry-run + drift    -> exit 2, installed file UNCHANGED
#   (e) --dry-run + in sync -> exit 0
#   (f) repo-specific file   -> file present only in .loom/ left untouched
#   (g) .loom/resync-ignore  -> pinned file reported "skipped", not overwritten
#   (h) idempotent rerun     -> second run reports all unchanged
# Widened surfaces (#4239):
#   (i) drift in each new surface (roles/docs/bin/commands) -> updated + exit 2 on dry-run
#   (j) local-only custom role -> survives untouched
#   (k) resync-ignore pins a new-surface file -> reported "skipped"
#   (l) symlinked install target -> skipped, not clobbered
#   (m) recorded loom_source gone -> clear error, exit 1
#   (n) metadata re-stamp -> loom_version/loom_commit/last_resync present after apply
# Plus contract checks:
#   - --help prints usage, exit 0
#   - unknown arg exits 1
#   - not-a-git-repo exits 1
#
# Usage:
#   ./.loom/scripts/tests/test-resync-installed.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPERS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SCRIPT="$HELPERS_DIR/resync-installed.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

pass() {
    TESTS_RUN=$((TESTS_RUN + 1))
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: $1"
}

fail() {
    TESTS_RUN=$((TESTS_RUN + 1))
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: $1"
}

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/test-resync.XXXXXX")"
# shellcheck disable=SC2329  # invoked indirectly via the EXIT trap below
cleanup() { rm -rf "$WORKDIR" 2>/dev/null || true; }
trap cleanup EXIT

export GIT_AUTHOR_NAME="test" GIT_AUTHOR_EMAIL="test@example.com"
export GIT_COMMITTER_NAME="test" GIT_COMMITTER_EMAIL="test@example.com"

# --- fixture builder ---------------------------------------------------------
# Creates a git repo at $WORKDIR/repo with:
#   defaults/hooks/guard.sh          (source of truth, "A")
#   defaults/scripts/foo.sh          (source of truth, "S")
#   defaults/scripts/lib/bar.sh      (source of truth, "L")
#   .loom/hooks/guard.sh             (installed, "OLD" -> drift)
#   .loom/scripts/foo.sh             (installed, "S"   -> in sync)
#   (.loom/scripts/lib/bar.sh MISSING -> to be created)
#   .loom/scripts/custom-only.sh     (repo-specific, no defaults/ counterpart)
# Widened surfaces (#4239):
#   defaults/roles/builder.md            -> .loom/roles/builder.md (drift)
#   defaults/docs/troubleshooting.md     -> .loom/docs/troubleshooting.md (drift)
#   defaults/.loom/bin/loom              -> .loom/bin/loom (drift)
#   defaults/.claude/commands/loom/x.md  -> .claude/commands/loom/x.md (drift)
#   .loom/roles/custom-role.md           (repo-specific, no defaults/ counterpart)
#   package.json ("version": "9.9.9")    (loom_version source for re-stamp)
#   .loom/install-metadata.json          (re-stamp target; loom_source -> $repo)
make_fixture() {
    local repo="$WORKDIR/repo"
    rm -rf "$repo"
    mkdir -p "$repo/defaults/hooks" "$repo/defaults/scripts/lib" \
             "$repo/defaults/roles" "$repo/defaults/docs" \
             "$repo/defaults/.loom/bin" "$repo/defaults/.claude/commands/loom" \
             "$repo/.loom/hooks" "$repo/.loom/scripts/lib" \
             "$repo/.loom/roles" "$repo/.loom/docs" \
             "$repo/.loom/bin" "$repo/.claude/commands/loom"
    git -C "$repo" init -q

    printf 'A\n' > "$repo/defaults/hooks/guard.sh"
    printf 'S\n' > "$repo/defaults/scripts/foo.sh"
    printf 'L\n' > "$repo/defaults/scripts/lib/bar.sh"
    chmod +x "$repo/defaults/hooks/guard.sh" "$repo/defaults/scripts/foo.sh" \
             "$repo/defaults/scripts/lib/bar.sh"

    printf 'OLD\n' > "$repo/.loom/hooks/guard.sh"
    printf 'S\n'   > "$repo/.loom/scripts/foo.sh"
    printf 'REPO-SPECIFIC\n' > "$repo/.loom/scripts/custom-only.sh"

    # Widened pure-copy surfaces (#4239): each drifts (installed differs from
    # defaults) so a single apply exercises all four new surfaces at once.
    printf 'ROLE-NEW\n' > "$repo/defaults/roles/builder.md"
    printf 'ROLE-OLD\n' > "$repo/.loom/roles/builder.md"
    printf 'CUSTOM-ROLE\n' > "$repo/.loom/roles/custom-role.md"   # local-only

    printf 'DOC-NEW\n' > "$repo/defaults/docs/troubleshooting.md"
    printf 'DOC-OLD\n' > "$repo/.loom/docs/troubleshooting.md"

    printf 'BIN-NEW\n' > "$repo/defaults/.loom/bin/loom"
    printf 'BIN-OLD\n' > "$repo/.loom/bin/loom"
    chmod +x "$repo/defaults/.loom/bin/loom"

    printf 'CMD-NEW\n' > "$repo/defaults/.claude/commands/loom/builder.md"
    printf 'CMD-OLD\n' > "$repo/.claude/commands/loom/builder.md"

    # Version source + metadata re-stamp target.
    printf '{\n  "version": "9.9.9"\n}\n' > "$repo/package.json"
    printf '{\n  "loom_version": "0.0.0",\n  "loom_commit": "old",\n  "install_date": "2020-01-01",\n  "loom_source": "%s",\n  "installed_files": []\n}\n' \
        "$repo" > "$repo/.loom/install-metadata.json"

    # A real commit so loom_commit re-stamps to an actual short sha.
    git -C "$repo" add -A >/dev/null 2>&1
    git -C "$repo" commit -qm "fixture" >/dev/null 2>&1

    echo "$repo"
}

# --- (a) in-sync / (b) drift / (c) missing: a single apply run --------------
echo "Test group 1: apply resyncs drift + creates missing, leaves the rest"
REPO="$(make_fixture)"
OUT="$(cd "$REPO" && bash "$SCRIPT" 2>&1)"
RC=$?
if [[ $RC -eq 0 ]]; then pass "apply exits 0"; else fail "apply exits 0 (got $RC)"; fi
if [[ "$(cat "$REPO/.loom/hooks/guard.sh")" == "A" ]]; then
    pass "(b) drifted hooks/guard.sh rewritten to match defaults"
else
    fail "(b) drifted hooks/guard.sh not updated"
fi
if [[ -f "$REPO/.loom/scripts/lib/bar.sh" && "$(cat "$REPO/.loom/scripts/lib/bar.sh")" == "L" ]]; then
    pass "(c) missing scripts/lib/bar.sh created from defaults"
else
    fail "(c) missing scripts/lib/bar.sh not created"
fi
if [[ "$(cat "$REPO/.loom/scripts/custom-only.sh")" == "REPO-SPECIFIC" ]]; then
    pass "(f) repo-specific custom-only.sh left untouched"
else
    fail "(f) repo-specific custom-only.sh was modified/removed"
fi
if grep -q "updated" <<<"$OUT" && grep -q "created" <<<"$OUT"; then
    pass "reports both 'updated' and 'created'"
else
    fail "did not report both updated and created"
fi

# --- (h) idempotent rerun ----------------------------------------------------
echo "Test group 2: idempotent rerun is a no-op"
OUT="$(cd "$REPO" && bash "$SCRIPT" 2>&1)"
RC=$?
if [[ $RC -eq 0 ]] && grep -q "Already in sync" <<<"$OUT"; then
    pass "(h) second run reports already in sync, exit 0"
else
    fail "(h) second run not a clean no-op (rc=$RC)"
fi

# --- (d) dry-run + drift: exit 2, no writes ----------------------------------
echo "Test group 3: --dry-run previews without writing"
REPO="$(make_fixture)"
OUT="$(cd "$REPO" && bash "$SCRIPT" --dry-run 2>&1)"
RC=$?
if [[ $RC -eq 2 ]]; then
    pass "(d) --dry-run with drift exits 2"
else
    fail "(d) --dry-run with drift exits 2 (got $RC)"
fi
if [[ "$(cat "$REPO/.loom/hooks/guard.sh")" == "OLD" ]]; then
    pass "(d) --dry-run left installed file UNCHANGED"
else
    fail "(d) --dry-run modified the installed file"
fi
if [[ ! -f "$REPO/.loom/scripts/lib/bar.sh" ]]; then
    pass "(d) --dry-run did not create the missing file"
else
    fail "(d) --dry-run created a file it should only have previewed"
fi

# --- (e) dry-run + in sync: exit 0 -------------------------------------------
echo "Test group 4: --dry-run when already in sync exits 0"
REPO="$(make_fixture)"
(cd "$REPO" && bash "$SCRIPT" >/dev/null 2>&1)   # apply first
OUT="$(cd "$REPO" && bash "$SCRIPT" --dry-run 2>&1)"
RC=$?
if [[ $RC -eq 0 ]] && grep -q "already in sync" <<<"$OUT"; then
    pass "(e) --dry-run in sync exits 0"
else
    fail "(e) --dry-run in sync exits 0 (rc=$RC)"
fi

# --- (g) resync-ignore pins a local override ---------------------------------
echo "Test group 5: .loom/resync-ignore preserves a pinned local override"
REPO="$(make_fixture)"
printf 'PINNED-LOCAL\n' > "$REPO/.loom/hooks/guard.sh"
printf 'hooks/guard.sh  # keep my local tweak\n' > "$REPO/.loom/resync-ignore"
OUT="$(cd "$REPO" && bash "$SCRIPT" 2>&1)"
RC=$?
if [[ $RC -eq 0 ]] && grep -q "skipped" <<<"$OUT"; then
    pass "(g) pinned file reported as skipped"
else
    fail "(g) pinned file not reported skipped (rc=$RC)"
fi
if [[ "$(cat "$REPO/.loom/hooks/guard.sh")" == "PINNED-LOCAL" ]]; then
    pass "(g) pinned file NOT overwritten"
else
    fail "(g) pinned file was overwritten despite resync-ignore"
fi

# --- (i) widened surfaces: drift detected + fixed ----------------------------
echo "Test group 7: widened surfaces (roles/docs/bin/commands) resync"
REPO="$(make_fixture)"
# dry-run first: drift across the new surfaces must be detected (exit 2)
OUT="$(cd "$REPO" && bash "$SCRIPT" --dry-run 2>&1)"
RC=$?
if [[ $RC -eq 2 ]]; then
    pass "(i) --dry-run detects drift across widened surfaces (exit 2)"
else
    fail "(i) --dry-run did not exit 2 across widened surfaces (got $RC)"
fi
for surf in "roles/builder.md" "docs/troubleshooting.md" "bin/loom" "commands/loom/builder.md"; do
    if grep -q "$surf" <<<"$OUT"; then
        pass "(i) --dry-run reports drift for $surf"
    else
        fail "(i) --dry-run did not report drift for $surf"
    fi
done
# apply, then verify each installed surface now matches defaults
OUT="$(cd "$REPO" && bash "$SCRIPT" 2>&1)"
RC=$?
if [[ $RC -eq 0 ]]; then pass "(i) apply exits 0"; else fail "(i) apply exits 0 (got $RC)"; fi
if [[ "$(cat "$REPO/.loom/roles/builder.md")" == "ROLE-NEW" ]]; then
    pass "(i) roles/builder.md resynced from defaults"
else
    fail "(i) roles/builder.md not resynced"
fi
if [[ "$(cat "$REPO/.loom/docs/troubleshooting.md")" == "DOC-NEW" ]]; then
    pass "(i) docs/troubleshooting.md resynced from defaults"
else
    fail "(i) docs/troubleshooting.md not resynced"
fi
if [[ "$(cat "$REPO/.loom/bin/loom")" == "BIN-NEW" && -x "$REPO/.loom/bin/loom" ]]; then
    pass "(i) bin/loom resynced (and executable bit preserved)"
else
    fail "(i) bin/loom not resynced or not executable"
fi
if [[ "$(cat "$REPO/.claude/commands/loom/builder.md")" == "CMD-NEW" ]]; then
    pass "(i) commands/loom/builder.md resynced from defaults"
else
    fail "(i) commands/loom/builder.md not resynced"
fi
# second run is a clean no-op across the widened surfaces too
OUT="$(cd "$REPO" && bash "$SCRIPT" 2>&1)"
RC=$?
if [[ $RC -eq 0 ]] && grep -q "Already in sync" <<<"$OUT"; then
    pass "(i) widened surfaces idempotent (second run already in sync)"
else
    fail "(i) widened surfaces not idempotent (rc=$RC)"
fi

# --- (j) local-only custom role survives -------------------------------------
echo "Test group 8: local-only custom role is never touched"
if [[ "$(cat "$REPO/.loom/roles/custom-role.md")" == "CUSTOM-ROLE" ]]; then
    pass "(j) local-only custom-role.md left untouched"
else
    fail "(j) local-only custom-role.md was modified/removed"
fi

# --- (k) resync-ignore pins a new-surface file -------------------------------
echo "Test group 9: .loom/resync-ignore pins a widened-surface file"
REPO="$(make_fixture)"
printf 'PINNED-ROLE\n' > "$REPO/.loom/roles/builder.md"
printf 'roles/builder.md  # keep my local role tweak\n' > "$REPO/.loom/resync-ignore"
OUT="$(cd "$REPO" && bash "$SCRIPT" 2>&1)"
RC=$?
if [[ $RC -eq 0 ]] && grep -q "skipped" <<<"$OUT" && grep -q "roles/builder.md" <<<"$OUT"; then
    pass "(k) pinned roles/builder.md reported as skipped"
else
    fail "(k) pinned roles/builder.md not reported skipped (rc=$RC)"
fi
if [[ "$(cat "$REPO/.loom/roles/builder.md")" == "PINNED-ROLE" ]]; then
    pass "(k) pinned roles/builder.md NOT overwritten"
else
    fail "(k) pinned roles/builder.md was overwritten despite resync-ignore"
fi

# --- (l) symlinked install target is skipped, not clobbered ------------------
echo "Test group 10: symlinked install target is skipped (dogfood safety)"
REPO="$(make_fixture)"
# Replace the installed docs file with a symlink pointing back at defaults/
# (mirrors this repo's dogfood .loom/docs/*.md layout).
rm -f "$REPO/.loom/docs/troubleshooting.md"
ln -s "../../defaults/docs/troubleshooting.md" "$REPO/.loom/docs/troubleshooting.md"
OUT="$(cd "$REPO" && bash "$SCRIPT" 2>&1)"
RC=$?
if [[ $RC -eq 0 ]] && grep -qi "symlink" <<<"$OUT"; then
    pass "(l) symlinked docs entry reported as skipped"
else
    fail "(l) symlinked docs entry not reported skipped (rc=$RC)"
fi
if [[ -L "$REPO/.loom/docs/troubleshooting.md" ]]; then
    pass "(l) symlink left intact (not clobbered into a regular file)"
else
    fail "(l) symlink was clobbered"
fi

# --- (m) recorded loom_source gone -> clear error, exit 1 --------------------
echo "Test group 11: recorded loom_source moved/deleted errors clearly"
REPO="$(make_fixture)"
rm -rf "$REPO/defaults"                       # no dogfood defaults/ tree
rm -f "$REPO/.loom/loom-source-path"           # no source sidecar
printf '{\n  "loom_source": "%s/gone"\n}\n' "$REPO" > "$REPO/.loom/install-metadata.json"
RC=0; OUT="$(cd "$REPO" && bash "$SCRIPT" 2>&1)" || RC=$?
if [[ $RC -eq 1 ]]; then
    pass "(m) missing source tree exits 1"
else
    fail "(m) missing source tree did not exit 1 (got $RC)"
fi
if grep -qi "could not locate" <<<"$OUT"; then
    pass "(m) missing source tree prints a clear error"
else
    fail "(m) missing source tree error message unclear"
fi

# --- (n) metadata re-stamp ---------------------------------------------------
echo "Test group 12: successful apply re-stamps install-metadata.json"
REPO="$(make_fixture)"
(cd "$REPO" && bash "$SCRIPT" >/dev/null 2>&1)
META="$REPO/.loom/install-metadata.json"
if grep -q '"loom_version": *"9.9.9"' "$META"; then
    pass "(n) loom_version re-stamped from source package.json"
else
    fail "(n) loom_version not re-stamped"
fi
if grep -q "\"last_resync\": *\"$(date +%Y-%m-%d)\"" "$META"; then
    pass "(n) last_resync stamped with today's date"
else
    fail "(n) last_resync not stamped"
fi
if grep -q '"loom_commit"' "$META" && ! grep -q '"loom_commit": *"old"' "$META"; then
    pass "(n) loom_commit re-stamped (no longer the stale value)"
else
    fail "(n) loom_commit not re-stamped"
fi
# out-of-scope metadata fields must be preserved
if grep -q '"install_date": *"2020-01-01"' "$META"; then
    pass "(n) install_date preserved (installer-owned, out of scope)"
else
    fail "(n) install_date was altered"
fi
# --dry-run must NOT re-stamp
REPO="$(make_fixture)"
(cd "$REPO" && bash "$SCRIPT" --dry-run >/dev/null 2>&1)
if grep -q '"loom_version": *"0.0.0"' "$REPO/.loom/install-metadata.json"; then
    pass "(n) --dry-run leaves install-metadata.json unstamped"
else
    fail "(n) --dry-run re-stamped metadata (should be preview-only)"
fi

# --- contract checks ---------------------------------------------------------
echo "Test group 13: flag/contract checks"
if bash "$SCRIPT" --help 2>&1 | grep -q "resync-installed.sh"; then
    pass "--help prints usage"
else
    fail "--help did not print usage"
fi
if bash "$SCRIPT" --help >/dev/null 2>&1; then pass "--help exits 0"; else fail "--help did not exit 0"; fi

REPO="$(make_fixture)"
RC=0; (cd "$REPO" && bash "$SCRIPT" --bogus >/dev/null 2>&1) || RC=$?
if [[ $RC -eq 1 ]]; then pass "unknown arg exits 1"; else fail "unknown arg did not exit 1 (got $RC)"; fi

NON_REPO="$WORKDIR/not-a-repo"
mkdir -p "$NON_REPO"
RC=0; (cd "$NON_REPO" && bash "$SCRIPT" >/dev/null 2>&1) || RC=$?
if [[ $RC -eq 1 ]]; then pass "outside a git repo exits 1"; else fail "outside a git repo did not exit 1 (got $RC)"; fi

# --- summary -----------------------------------------------------------------
echo ""
echo "========================================"
echo "Results: $TESTS_PASSED/$TESTS_RUN passed"
echo "========================================"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "${RED}$TESTS_FAILED test(s) failed${NC}"
    exit 1
fi
echo -e "${GREEN}All tests passed${NC}"
exit 0
