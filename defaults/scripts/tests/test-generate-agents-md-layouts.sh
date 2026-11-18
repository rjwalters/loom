#!/usr/bin/env bash
# test-generate-agents-md-layouts.sh - generate-agents-md.sh works in both the
# installed and the source layout (#13910)
#
# generate-agents-md.sh used to hard-code SRC="$SCRIPT_DIR/../.loom/CLAUDE.md",
# which is right only in the source layout (defaults/scripts/ beside
# defaults/.loom/). Installed repos flatten to X/.loom/scripts/ beside
# X/.loom/CLAUDE.md, so the generator exited 1 with "source not found" in
# every consumer repo. The fix resolves the source from the CLAUDE.md
# candidate that exists: the installed candidate (../CLAUDE.md) first, then
# the source-layout candidate (../.loom/CLAUDE.md), and names both paths in
# the error when neither does.
#
# Usage:
#   ./.loom/scripts/tests/test-generate-agents-md-layouts.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GENERATOR="$SCRIPT_DIR/../generate-agents-md.sh"
# defaults/.loom/AGENTS.md — the checked-in artifact in this repo's source
# layout (the defaults/ tree is the root of what ships; there is no repo root
# above it to resolve against).
REAL_CHECKED_IN="$SCRIPT_DIR/../../.loom/AGENTS.md"

source "$SCRIPT_DIR/lib/assert.sh"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/test-gen-agents-md.XXXXXX")"
# shellcheck disable=SC2329,SC2317  # invoked indirectly via the EXIT trap below
cleanup() { rm -rf "$WORKDIR" 2>/dev/null || true; }
trap cleanup EXIT

# A minimal marker-bearing source, enough to exercise path resolution and
# extraction without depending on the real template's prose.
make_claude_md() {
    local dest="$1"
    cat > "$dest" <<'EOF'
# Guide
intro text
<!-- agents-md:include:start -->
shared content line
<!-- agents-md:include:end -->
outro text
EOF
}

# make_installed_layout <dir> [with-claude-md yes|no]
#   dir/.loom/scripts/generate-agents-md.sh beside dir/.loom/CLAUDE.md
make_installed_layout() {
    local dir="$1" with_src="${2:-yes}"
    mkdir -p "$dir/.loom/scripts"
    cp "$GENERATOR" "$dir/.loom/scripts/generate-agents-md.sh"
    [[ "$with_src" == yes ]] && make_claude_md "$dir/.loom/CLAUDE.md"
    return 0
}

# make_source_layout <dir> [with-claude-md yes|no]
#   dir/defaults/scripts/generate-agents-md.sh beside dir/defaults/.loom/CLAUDE.md
make_source_layout() {
    local dir="$1" with_src="${2:-yes}"
    mkdir -p "$dir/defaults/scripts" "$dir/defaults/.loom"
    cp "$GENERATOR" "$dir/defaults/scripts/generate-agents-md.sh"
    [[ "$with_src" == yes ]] && make_claude_md "$dir/defaults/.loom/CLAUDE.md"
    return 0
}

echo ""
echo "Test group 1: installed layout — --stdout succeeds and extracts (#13910)"
INSTALLED="$WORKDIR/installed"
make_installed_layout "$INSTALLED"
if OUT="$(cd "$INSTALLED" && bash .loom/scripts/generate-agents-md.sh --stdout)"; then
    pass "(#13910) installed --stdout exits 0"
else
    fail "(#13910) installed --stdout exited non-zero"
fi
if [[ "$OUT" == *"shared content line"* ]]; then
    pass "(#13910) installed --stdout extracts the marker range"
else
    fail "(#13910) installed --stdout missing extracted content"
fi

echo ""
echo "Test group 2: installed layout — default write target is X/.loom/AGENTS.md (#13910)"
INSTALLED2="$WORKDIR/installed-default"
make_installed_layout "$INSTALLED2"
(cd "$INSTALLED2" && bash .loom/scripts/generate-agents-md.sh >/dev/null 2>&1)
assert_doc_contains "$INSTALLED2/.loom/AGENTS.md" "shared content line" \
    "(#13910) installed default write lands beside the source CLAUDE.md"

echo ""
echo "Test group 3: installed layout — explicit <path> argument honoured (#13910)"
INSTALLED3="$WORKDIR/installed-explicit"
make_installed_layout "$INSTALLED3"
(cd "$INSTALLED3" && bash .loom/scripts/generate-agents-md.sh custom-agents.md >/dev/null 2>&1)
assert_doc_contains "$INSTALLED3/custom-agents.md" "shared content line" \
    "(#13910) installed explicit <path> honoured"
if [[ -e "$INSTALLED3/.loom/AGENTS.md" ]]; then
    fail "(#13910) explicit <path> also touched the default target"
else
    pass "(#13910) explicit <path> leaves the default target untouched"
fi

echo ""
echo "Test group 4: source layout — default write target is defaults/.loom/AGENTS.md (#13910)"
SOURCE="$WORKDIR/source"
make_source_layout "$SOURCE"
(cd "$SOURCE" && bash defaults/scripts/generate-agents-md.sh >/dev/null 2>&1)
assert_doc_contains "$SOURCE/defaults/.loom/AGENTS.md" "shared content line" \
    "(#13910) source layout default write target unchanged"

echo ""
echo "Test group 5: source layout — real repo --stdout output unchanged (#13910)"
if [[ ! -f "$REAL_CHECKED_IN" ]]; then
    pass "(#13910) real source layout not present here — skipped"
else
    if [[ "$(bash "$GENERATOR" --stdout)" == "$(cat "$REAL_CHECKED_IN")" ]]; then
        pass "(#13910) real source --stdout matches the checked-in defaults/.loom/AGENTS.md"
    else
        fail "(#13910) real source --stdout drifted from defaults/.loom/AGENTS.md"
    fi
fi

echo ""
echo "Test group 6: both candidates missing — exit 1 naming both paths (#13910)"
NONE="$WORKDIR/none"
make_installed_layout "$NONE" no
ERR="$(cd "$NONE" && bash .loom/scripts/generate-agents-md.sh --stdout 2>&1 >/dev/null)"
RC=$?
if [[ $RC -eq 1 ]]; then
    pass "(#13910) both candidates missing exits 1"
else
    fail "(#13910) both candidates missing exited $RC, expected 1"
fi
# The error names the paths as constructed (unnormalized ../ segments).
INSTALLED_CANDIDATE="$NONE/.loom/scripts/../CLAUDE.md"
SOURCE_CANDIDATE="$NONE/.loom/scripts/../.loom/CLAUDE.md"
assert_contains "$ERR" "$INSTALLED_CANDIDATE" "(#13910) error names the installed candidate"
assert_contains "$ERR" "$SOURCE_CANDIDATE" "(#13910) error names the source-layout candidate"

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
