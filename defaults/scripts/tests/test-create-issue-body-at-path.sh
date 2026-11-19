#!/usr/bin/env bash
# test-create-issue-body-at-path.sh — create-issue.sh refuses a `--body` whose
# entire value is a lone `@path` / `@-` token (#14161).
#
# `gh issue create --body @path` files the literal string `@path` as the whole
# body (the pitfall documented for `gh pr comment` / `gh api -f body=@path`,
# #9258). The script exits 0 and prints the URL, so nothing downstream can
# tell. create-issue.sh now refuses such a `--body` (exit 2, message names
# --body-file) before any forge call. A body that merely CONTAINS `@` (a
# mention, an email-shaped token, a line starting with `@` inside a longer
# body) is accepted, and `--body-file` content is the author's own text --
# never inspected.
#
# Cases:
#   Refuse (exit 2, message names --body-file, no forge call):
#     --body @notes.md, --body @-, --body @, --body @~/r.md,
#     --body " @notes.md " and --body $'\n@notes.md\n' (surrounding whitespace)
#   Accept (filing attempted — the create call reaches the stub daemon/gh):
#     --body "Closes #14", --body "cc @alice please review",
#     --body "@a b" (whitespace inside the token run),
#     --body "contact ops@example.com about the flake" (email-shaped token),
#     a multi-line body with a line starting `@`,
#     --body-file whose content is `@x`.
#
# Hermetic: create-issue.sh + lib/ are copied into a throwaway dir and a stub
# `gh` / `loom-daemon` / `tea` on PATH records any call (pattern:
# test-create-issue-refuse-labels.sh). LOOM_FORGE_TYPE=github is forced so
# forge_detect never touches git/network. The duplicate backstop is skipped
# by construction (check-duplicate.sh is not copied into the throwaway dir).

set -uo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$TEST_DIR/.." && pwd)"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_FAILED=0

pass() {
    TESTS_RUN=$((TESTS_RUN + 1))
    echo -e "  ${GREEN}PASS${NC}: $1"
}

fail() {
    TESTS_RUN=$((TESTS_RUN + 1))
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: $1"
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$WORK/scripts" "$WORK/bin"
cp "$SCRIPTS_DIR/create-issue.sh" "$WORK/scripts/"
cp -R "$SCRIPTS_DIR/lib" "$WORK/scripts/lib"
CALLS="$WORK/forge-calls.log"
for tool in gh loom-daemon tea; do
    cat > "$WORK/bin/$tool" << STUB
#!/usr/bin/env bash
echo "$tool \$*" >> "$CALLS"
exit 1
STUB
    chmod +x "$WORK/bin/$tool"
done

run() {
    : > "$CALLS"
    OUT="$(cd "$WORK" && LOOM_FORGE_TYPE=github PATH="$WORK/bin:$PATH" bash "$WORK/scripts/create-issue.sh" "$@" 2>&1)"
    RC=$?
}

create_was_attempted() {
    grep -q "forge create-issue" "$CALLS" 2>/dev/null \
        || grep -q "issue create" "$CALLS" 2>/dev/null \
        || grep -q "api" "$CALLS" 2>/dev/null
}

echo "create-issue.sh refuses a lone @path --body (#14161)"

refused() {
    local desc="$1"
    shift
    run --title "t" --body "$1"
    if [[ "$RC" -eq 2 ]]; then pass "$desc: exits 2"; else fail "$desc: exited $RC (want 2)"; fi
    if [[ "$OUT" == *"refusing"* && "$OUT" == *"--body-file"* ]]; then
        pass "$desc: message names --body-file"
    else
        fail "$desc: message missing (got: $OUT)"
    fi
    if [[ "$OUT" == *"Nothing was filed"* ]]; then
        pass "$desc: message says nothing was filed"
    else
        fail "$desc: 'Nothing was filed' missing (got: $OUT)"
    fi
    if [[ ! -s "$CALLS" ]]; then pass "$desc: no forge call"; else fail "$desc: forge called: $(cat "$CALLS")"; fi
}

refused "@notes.md path token" "@notes.md"
refused "@- stdin token" "@-"
refused "bare @" "@"
refused "@~/r.md path with tilde" "@~/r.md"
refused "surrounding spaces" " @notes.md "
refused "surrounding newlines" "$(printf '\n@notes.md\n')"

accepted() {
    local desc="$1"
    shift
    run --title "t" "$@"
    if [[ "$OUT" == *"lone '@' token"* ]]; then
        fail "$desc: refused (rc=$RC, got: $OUT)"
    elif [[ "$RC" -eq 0 ]]; then
        fail "$desc: unexpectedly filed with an all-failing stub (rc=0, got: $OUT)"
    else
        pass "$desc: not refused (rc=$RC, filing attempted and stub refused)"
    fi
    if create_was_attempted; then
        pass "$desc: create call reached (guard passed)"
    else
        fail "$desc: no create call — stopped before filing: $(cat "$CALLS" 2>/dev/null || true) / $OUT"
    fi
}

accepted "Closes #14 body" --body "Closes #14"
accepted "mention body" --body "cc @alice please review"
accepted "whitespace inside token run" --body "@a b"
accepted "email-shaped token" --body "contact ops@example.com about the flake"
accepted "multi-line body with a line starting @" --body "$(printf 'Summary line.\n\n@handles owns this area.')"

# A --body-file whose CONTENT is `@x` is the author's own text: accepted,
# never inspected (the guard applies only to the literal --body form).
printf '@x' > "$WORK/at-x-body.md"
accepted "body-file containing @x" --body-file "$WORK/at-x-body.md"

echo
if [[ "$TESTS_FAILED" -eq 0 ]]; then
    echo -e "${GREEN}All $TESTS_RUN checks passed${NC}"
    exit 0
fi
echo -e "${RED}$TESTS_FAILED of $TESTS_RUN checks failed${NC}"
exit 1
