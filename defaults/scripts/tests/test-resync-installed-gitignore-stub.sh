#!/usr/bin/env bash
# test-resync-installed-gitignore-stub.sh - the workspace-stub predicate behind
# resync-installed.sh's .gitignore stale-check (#13898, #14586).
#
# _gitignore_workspace_is_stub() decides whether the managed .gitignore block
# must carry STUB_WORKSPACE_PATTERNS by reading $WRITE_ROOT/package.json, and
# _gitignore_warn_if_stale() stands the whole stale-check down when it answers
# "unknown". This suite pins #14586 item (1):
#
#   jq on PATH:  nameless / consumer / stub / unparseable / absent package.json
#                -> no / no / yes / no / no -- the unparseable case matches the
#                Rust side's workspace_uses_package_stub() (unwrap_or(false)),
#                NOT a stand-down.
#   jq absent:   a present package.json (stub or unparseable) -> unknown; an
#                absent file is still "no" (answerable without jq).
#
# Plus the stand-down note itself: it names what is actually unavailable (jq
# not on PATH) and does NOT fire for an unparseable package.json when jq is
# present, and the #14586 item (2) invariant comment on the predicate.
#
# Extraction pattern: resync-installed.sh runs top-level installer logic when
# sourced, so the functions under test are eval'd in isolation (same pattern
# as tests/install/test-pnpm-runnable-check.sh).
#
# Usage:
#   bash defaults/scripts/tests/test-resync-installed-gitignore-stub.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPERS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SCRIPT="$HELPERS_DIR/resync-installed.sh"
REPO_ROOT="$(cd "$HELPERS_DIR/../.." && pwd)"

source "$SCRIPT_DIR/lib/assert.sh"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/test-resync-gitignore-stub.XXXXXX")"
# shellcheck disable=SC2317  # invoked indirectly via the EXIT trap below
cleanup() { rm -rf "$WORKDIR" 2>/dev/null || true; }
trap cleanup EXIT

# --- Extract the units under test -------------------------------------------

# Grab a top-level function definition (header at column 0 through the first
# closing brace at column 0) by exact string match on the header line.
extract_fn() {
    local name="$1" src
    src="$(awk -v start="${name}() {" '$0 == start {f=1} f {print} f && $0 == "}" {exit}' "$SCRIPT")"
    if [[ -z "$src" ]]; then
        echo "FATAL: could not extract ${name}() from $SCRIPT" >&2
        exit 1
    fi
    printf '%s\n' "$src"
}

eval "$(extract_fn _gitignore_workspace_is_stub)"
eval "$(extract_fn _gitignore_source_ephemeral_patterns)"
eval "$(extract_fn _gitignore_source_stub_patterns)"
eval "$(extract_fn _gitignore_restore_managed_block)"
eval "$(extract_fn _gitignore_warn_if_stale)"

# Consumed by the extracted functions at runtime (invisible to shellcheck):
# $QUIET by note()-style checks, the colors by the restore path's
# interpolations. Emptied to keep captured text grep-stable.
# shellcheck disable=SC2034
QUIET=0
# shellcheck disable=SC2034
GREEN='' RED='' YELLOW='' BOLD='' NC=''
NOTE_LOG="$WORKDIR/notes.log"
# shellcheck disable=SC2317  # invoked by the extracted _gitignore_warn_if_stale
note() { printf '%s\n' "$*" >>"$NOTE_LOG"; }
# shellcheck disable=SC2317  # same
warn() { printf 'WARN: %s\n' "$*" >>"$NOTE_LOG"; }

# The source-pattern extractors read the REAL post_init.rs (ground truth,
# #5294) -- point SOURCE_ROOT at this checkout.
SOURCE_ROOT="$REPO_ROOT"

# new_write_root <name>: create (or reset) a fixture dir, echo its path.
new_write_root() {
    local dir="$WORKDIR/root-$1"
    rm -rf "$dir"
    mkdir -p "$dir"
    printf '%s\n' "$dir"
}

echo ""
echo "Test group 1: predicate answers with jq on PATH (#14586 item 1)"
if command -v jq >/dev/null 2>&1; then
    ROOT="$(new_write_root nameless)"
    printf '{"version": "1.0.0"}\n' >"$ROOT/package.json"
    assert_eq no "$(WRITE_ROOT="$ROOT" _gitignore_workspace_is_stub)" "nameless package.json -> no"

    ROOT="$(new_write_root consumer)"
    printf '{"name": "consumer"}\n' >"$ROOT/package.json"
    assert_eq no "$(WRITE_ROOT="$ROOT" _gitignore_workspace_is_stub)" "consumer package.json -> no"

    ROOT="$(new_write_root stub)"
    printf '{"name": "loom-workspace"}\n' >"$ROOT/package.json"
    assert_eq yes "$(WRITE_ROOT="$ROOT" _gitignore_workspace_is_stub)" "stub package.json -> yes"

    ROOT="$(new_write_root unparseable)"
    printf '{"name":\n' >"$ROOT/package.json"
    assert_eq no "$(WRITE_ROOT="$ROOT" _gitignore_workspace_is_stub)" \
        "unparseable package.json -> no (matches Rust unwrap_or(false), NOT unknown)"

    ROOT="$(new_write_root absent)"
    assert_eq no "$(WRITE_ROOT="$ROOT" _gitignore_workspace_is_stub)" "absent package.json -> no"
else
    echo "  SKIP: jq not on PATH; the jq-present shapes are not runnable here"
fi

echo ""
echo "Test group 2: \"unknown\" is jq-absent only, and jq-absent dominates the file's shape"
NOJQ_BIN="$WORKDIR/nojq-bin"
mkdir -p "$NOJQ_BIN"
for tool in bash sed awk grep git cat head tail mktemp mv rm; do
    if command -v "$tool" >/dev/null 2>&1; then
        ln -sf "$(command -v "$tool")" "$NOJQ_BIN/$tool"
    fi
done

ROOT="$(new_write_root stub)"
printf '{"name": "loom-workspace"}\n' >"$ROOT/package.json"
assert_eq unknown "$(PATH="$NOJQ_BIN" WRITE_ROOT="$ROOT" _gitignore_workspace_is_stub)" \
    "stub package.json with jq absent -> unknown (stand-down)"

ROOT="$(new_write_root unparseable)"
printf '{"name":\n' >"$ROOT/package.json"
assert_eq unknown "$(PATH="$NOJQ_BIN" WRITE_ROOT="$ROOT" _gitignore_workspace_is_stub)" \
    "unparseable package.json with jq absent -> unknown (the tool is the missing piece)"

ROOT="$(new_write_root absent)"
assert_eq no "$(PATH="$NOJQ_BIN" WRITE_ROOT="$ROOT" _gitignore_workspace_is_stub)" \
    "absent package.json with jq absent -> no (answerable without jq)"

echo ""
echo "Test group 3: the stand-down note names what is actually unavailable"
# (a) jq absent: the note fires and names jq not being on PATH, not a vague
#     "needs jq" that also covered the unparseable-file case pre-#14586.
ROOT="$(new_write_root stub)"
printf '{"name": "loom-workspace"}\n' >"$ROOT/package.json"
printf 'unrelated\n' >"$ROOT/.gitignore"
: >"$NOTE_LOG"
( PATH="$NOJQ_BIN" SOURCE_ROOT="$SOURCE_ROOT" WRITE_ROOT="$ROOT" _gitignore_warn_if_stale "$WORKDIR/fake-bin" )
NOTES="$(cat "$NOTE_LOG")"
assert_contains "$NOTES" "jq not on PATH" "stand-down note names jq absent"
assert_not_contains "$NOTES" "stub detection needs jq" "pre-#14586 ambiguous note wording is gone"

# Fixture sanity: the real post_init.rs must yield at least one base pattern,
# or (b)'s exact-match block below would be vacuous.
[[ -n "$(_gitignore_source_ephemeral_patterns)" ]]
pass "fixture sanity: post_init.rs yields base patterns"

# (b) jq present + unparseable package.json: NO stand-down. The managed block
#     carries exactly the base list (stub_state is "no"), so the check runs to
#     its equal-content early return and prints nothing at all.
ROOT="$(new_write_root unparseable)"
printf '{"name":\n' >"$ROOT/package.json"
{
    printf '# >>> loom-managed (do not edit) >>>\n'
    printf '# Loom runtime state (do not commit these)\n'
    while IFS= read -r pattern; do [[ -n "$pattern" ]] && printf '%s\n' "$pattern"; done \
        < <(_gitignore_source_ephemeral_patterns)
    printf '# <<< loom-managed <<<\n'
} >"$ROOT/.gitignore"
: >"$NOTE_LOG"
( SOURCE_ROOT="$SOURCE_ROOT" WRITE_ROOT="$ROOT" _gitignore_warn_if_stale "$WORKDIR/fake-bin" )
NOTES="$(cat "$NOTE_LOG")"
assert_not_contains "$NOTES" "stale-check skipped" \
    "unparseable package.json with jq present does NOT stand the check down"
assert_eq "" "$NOTES" "exact-match base block with stub_state=no runs silent"

echo ""
echo "Test group 4: #14586 item (2) invariant comment on the predicate"
assert_grep "must be TRACKED" "$SCRIPT" \
    "_gitignore_workspace_is_stub's comment states the tracked-marker invariant (#14586 item 2)"

echo ""
if [[ "$TESTS_FAILED" -eq 0 ]]; then
    echo "All $TESTS_RUN tests passed"
    exit 0
else
    echo "$TESTS_FAILED of $TESTS_RUN tests failed"
    exit 1
fi
