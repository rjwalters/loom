#!/usr/bin/env bash
# test-sync-labels-repo-flag.sh - Unit tests for sync-labels.sh's --repo /
# --dry-run flags (issue #4498).
#
# Why --repo exists: bringing a new repo online in a daemon fleet needs that
# repo's Loom labels created on the forge. Every GitHub label operation in
# sync-labels.sh is already a `gh label` API call, so the only thing that
# forced a local checkout of the target was target *resolution* — the script
# inferred the NWO from the current directory's git remote via
# forge_detect/forge_get_repo_nwo. `--repo OWNER/NAME` names the target
# instead, so one workspace can sync labels onto klayout-tools and every
# gf180-*/sky130-* canary without cloning any of them.
#
# This is a black-box test: sync-labels.sh is a full CLI script, so we stub
# `gh` on PATH, run the real script as a subprocess from a scratch directory,
# and assert on exit codes, stderr, and the recorded `gh` argv log.
#
# The load-bearing assertions:
#   1. --repo resolves the target WITHOUT consulting the forge/git for the NWO
#      (no `gh repo view` in the log) and works from a directory that is not a
#      git repo at all — the short-circuit, proven rather than asserted.
#   2. Every mutating gh call carries `-R <override>`, never the invoking repo.
#   3. labels.yml is still read from WORKTREE_PATH (the source stays local;
#      only the target moves).
#   4. --dry-run is completely forge-free (zero gh invocations) — it is the
#      preview an operator runs before pointing deletions at a repo they are
#      not standing in.
#   5. The no-flag path is byte-for-byte the old behavior (additive flag):
#      NWO still comes from `gh repo view`, and a directory with no resolvable
#      remote still hard-errors.
#   6. Argument-parsing rejections: missing/invalid NWO, unknown option, extra
#      positional, and --repo combined with a Gitea forge.
#
# Usage:
#   ./defaults/scripts/tests/test-sync-labels-repo-flag.sh

set -uo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$TEST_DIR/.." && pwd)"
REPO_ROOT="$(cd "$SCRIPTS_DIR/../.." && pwd)"
SLS="$SCRIPTS_DIR/sync-labels.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

assert_eq() {
    local expected="$1" actual="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$expected" == "$actual" ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg"
        echo "    Expected: '$expected'"
        echo "    Actual:   '$actual'"
    fi
}

assert_contains() {
    local haystack="$1" needle="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if printf '%s' "$haystack" | grep -qF -- "$needle"; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg"
        echo "    Expected substring: '$needle'"
        echo "    In: '$haystack'"
    fi
}

assert_not_contains() {
    local haystack="$1" needle="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if ! printf '%s' "$haystack" | grep -qF -- "$needle"; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg"
        echo "    Unexpected substring: '$needle'"
        echo "    In: '$haystack'"
    fi
}

if [[ ! -x "$SLS" ]]; then
    echo -e "${RED}FATAL${NC}: $SLS not found or not executable" >&2
    exit 2
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP" 2>/dev/null || true' EXIT

STUB_DIR="$TMP/stub"
mkdir -p "$STUB_DIR"

# --- Stub gh on PATH ---------------------------------------------------------
# Appends every invocation's argv to $LOOM_TEST_GH_LOG (one line per call), so a
# test can assert both what WAS called and what was NOT.
#
#   gh repo view ...   -> echoes $LOOM_TEST_GH_NWO, or exits 1 when it is empty
#                         (simulating "no resolvable remote here")
#   gh label list ...  -> echoes nothing (script takes the `create` branch)
#   gh label create|edit|delete ... -> exit 0
cat > "$STUB_DIR/gh" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "${LOOM_TEST_GH_LOG:?stub gh: LOOM_TEST_GH_LOG not set}"

case "$1" in
  repo)
    if [[ -z "${LOOM_TEST_GH_NWO:-}" ]]; then
      echo "stub gh: no remote configured" >&2
      exit 1
    fi
    printf '%s\n' "$LOOM_TEST_GH_NWO"
    exit 0
    ;;
  label)
    case "$2" in
      list)   exit 0 ;;
      create|edit|delete) exit 0 ;;
    esac
    echo "stub gh: unhandled label args: $*" >&2
    exit 3
    ;;
esac
echo "stub gh: unhandled args: $*" >&2
exit 3
STUB
chmod +x "$STUB_DIR/gh"

# --- Scratch source tree -----------------------------------------------------
# Deliberately NOT a git repo: `git remote get-url origin` and `git rev-parse`
# find nothing here, which is exactly the condition --repo must tolerate. It
# holds only the labels.yml the sync reads (the source stays local; --repo
# moves the target).
SRC="$TMP/src"
mkdir -p "$SRC/.github"
cat > "$SRC/.github/labels.yml" <<'EOF'
# BEGIN LOOM LABELS
- name: loom:issue
  description: "Approved and ready for a Builder"
  color: "3B82F6"
- name: loom:pr
  description: "Approved pull request"
  color: "10B981"
# END LOOM LABELS
EOF

GH_LOG="$TMP/gh.log"

# Run the real script with the stub gh first on PATH, from $SRC.
#   run_sls [--nwo NWO] -- <script args...>
# Sets: RC, OUT (merged stdout+stderr), LOG (recorded gh argv lines).
RC=0
OUT=""
LOG=""
run_sls() {
    local nwo=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --nwo) nwo="$2"; shift 2 ;;
            --) shift; break ;;
            *) break ;;
        esac
    done
    : > "$GH_LOG"
    OUT="$(
        cd "$SRC" || exit 99
        PATH="$STUB_DIR:$PATH" \
        LOOM_TEST_GH_LOG="$GH_LOG" \
        LOOM_TEST_GH_NWO="$nwo" \
        LOOM_FORGE_TYPE="${LOOM_FORGE_TYPE_OVERRIDE:-}" \
        bash "$SLS" "$@" 2>&1
    )"
    RC=$?
    LOG="$(cat "$GH_LOG")"
}

echo ""
echo "=== --repo short-circuits NWO resolution (no git remote needed) ==="

# The scratch dir has no git remote and the stub `gh repo view` fails, so the
# legacy resolution path cannot produce an NWO. --repo must still succeed.
run_sls -- --repo octocat/hello-world
assert_eq "0" "$RC" "--repo succeeds in a directory with no resolvable remote"
assert_contains "$OUT" "Target repository: octocat/hello-world (github)" \
    "--repo resolves REPO to the flag value"
assert_not_contains "$LOG" "repo view" \
    "--repo never calls 'gh repo view' (forge_get_repo_nwo short-circuited)"
assert_contains "$LOG" "label delete bug -R octocat/hello-world" \
    "default-label deletion targets the override NWO"
assert_contains "$LOG" "label create loom:issue -R octocat/hello-world" \
    "label create targets the override NWO"
assert_contains "$OUT" "Synced 2 labels" \
    "labels.yml is still read from WORKTREE_PATH (both labels synced)"
# Every recorded gh call must name exactly one target, and it must be the
# override: a stray fallback to remote-based resolution would surface here as a
# second distinct -R value (or as a call with no -R at all).
assert_eq "-R octocat/hello-world" \
    "$(printf '%s\n' "$LOG" | grep -o -- '-R [^ ]*' | sort -u | tr '\n' ' ' | sed 's/ *$//')" \
    "the override NWO is the ONLY -R target in the whole gh log"
assert_eq "" \
    "$(printf '%s\n' "$LOG" | grep -v -- '-R octocat/hello-world' || true)" \
    "no gh call was made without the override target"

# --repo=OWNER/NAME is the same flag.
run_sls -- --repo=octocat/hello-world
assert_eq "0" "$RC" "--repo=OWNER/NAME form accepted"
assert_contains "$OUT" "Target repository: octocat/hello-world (github)" \
    "--repo= form resolves the same target"

# The flag order relative to the positional path must not matter.
run_sls -- --repo octocat/hello-world "$SRC"
assert_eq "0" "$RC" "--repo composes with an explicit WORKTREE_PATH positional"
assert_contains "$OUT" "Target repository: octocat/hello-world (github)" \
    "--repo plus positional still targets the override"

echo ""
echo "=== --dry-run is forge-free ==="

run_sls -- --repo octocat/hello-world --dry-run
assert_eq "0" "$RC" "--repo --dry-run exits 0"
assert_eq "" "$LOG" "--dry-run makes ZERO gh calls"
assert_contains "$OUT" "[dry-run] would delete default label: bug" \
    "--dry-run reports the default-label deletions it would make"
assert_contains "$OUT" "[dry-run] would create or update label: loom:issue" \
    "--dry-run reports the labels it would sync"
assert_contains "$OUT" "Dry run complete: 2 labels would be synced to octocat/hello-world" \
    "--dry-run names the resolved target in its summary"

echo ""
echo "=== No-flag path is unchanged (the flag is additive) ==="

# With a resolvable NWO, the legacy path still resolves via `gh repo view` and
# targets whatever that returned — untouched by this change.
run_sls --nwo owner/from-remote --
assert_eq "0" "$RC" "no-flag run succeeds when the NWO resolves"
assert_contains "$LOG" "repo view" \
    "no-flag run still calls 'gh repo view' (legacy resolution intact)"
assert_contains "$OUT" "Target repository: owner/from-remote (github)" \
    "no-flag run resolves the NWO from the forge, not from a flag"
assert_contains "$LOG" "label create loom:issue -R owner/from-remote" \
    "no-flag run targets the resolved repo"
assert_not_contains "$OUT" "dry-run" \
    "no-flag run emits no dry-run output"

# ...and with nothing resolvable it must still hard-error, as before.
run_sls --
assert_eq "1" "$RC" "no-flag run in a non-repo still fails"
assert_contains "$OUT" "Could not determine repository from git remote" \
    "no-flag failure keeps its original error message"

# --dry-run alone (no --repo) previews against the detected repo.
run_sls --nwo owner/from-remote -- --dry-run
assert_eq "0" "$RC" "--dry-run without --repo exits 0"
assert_contains "$OUT" "Target repository: owner/from-remote (github)" \
    "--dry-run without --repo still detects the local repo"
assert_not_contains "$LOG" "label create" \
    "--dry-run without --repo performs no label mutations"

echo ""
echo "=== Argument-parsing rejections (exit 2, no forge contact) ==="

run_sls -- --repo
assert_eq "2" "$RC" "--repo with no value exits 2"
assert_contains "$OUT" "Option --repo requires an OWNER/NAME argument" \
    "--repo with no value explains itself"
assert_eq "" "$LOG" "--repo with no value contacts no forge"

run_sls -- --repo=
assert_eq "2" "$RC" "--repo= with an empty value exits 2"

for bad in bad-nwo owner/repo/extra "own er/repo" /repo owner/; do
    run_sls -- --repo "$bad"
    assert_eq "2" "$RC" "invalid --repo value '$bad' exits 2"
    assert_contains "$OUT" "Invalid --repo value" \
        "invalid --repo value '$bad' is reported as invalid"
done

run_sls -- --bogus
assert_eq "2" "$RC" "unknown option exits 2"
assert_contains "$OUT" "Unknown option: --bogus" "unknown option is named"

run_sls -- "$SRC" extra
assert_eq "2" "$RC" "a second positional argument exits 2"
assert_contains "$OUT" "Unexpected extra argument: extra" \
    "the extra positional is named (previously silently ignored)"

echo ""
echo "=== --repo is GitHub-only ==="

LOOM_FORGE_TYPE_OVERRIDE="gitea" run_sls -- --repo octocat/hello-world
assert_eq "2" "$RC" "--repo with LOOM_FORGE_TYPE=gitea exits 2"
assert_contains "$OUT" "--repo is GitHub-only" \
    "the Gitea combination fails loudly instead of silently syncing GitHub"
assert_eq "" "$LOG" "the Gitea rejection contacts no forge"

echo ""
echo "=== --help documents the new flags ==="

run_sls -- --help
assert_eq "0" "$RC" "--help exits 0"
assert_contains "$OUT" "--repo OWNER/NAME" "--help documents --repo"
assert_contains "$OUT" "--dry-run" "--help documents --dry-run"

echo ""
echo "=== Installed-tree parity (.loom/scripts is a symlinked dir) ==="

# AC: ".loom/scripts/sync-labels.sh reflects the change automatically". In this
# repo .loom/scripts is a whole-directory symlink to defaults/scripts, so the
# two paths must resolve to the same file — one edit covers both surfaces.
INSTALLED="$REPO_ROOT/.loom/scripts/sync-labels.sh"
if [[ -e "$INSTALLED" ]]; then
    LINKED_REAL="$(cd "$(dirname "$INSTALLED")" && pwd -P)/$(basename "$INSTALLED")"
    SOURCE_REAL="$(cd "$(dirname "$SLS")" && pwd -P)/$(basename "$SLS")"
    assert_eq "$SOURCE_REAL" "$LINKED_REAL" \
        ".loom/scripts/sync-labels.sh resolves to defaults/scripts/sync-labels.sh"
else
    echo "  SKIP: $INSTALLED not present (source-only checkout)"
fi

echo ""
echo "────────────────────────────────"
echo "Results: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"

if [[ $TESTS_FAILED -gt 0 ]]; then
    exit 1
fi
exit 0
