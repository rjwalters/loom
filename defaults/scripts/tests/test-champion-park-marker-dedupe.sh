#!/usr/bin/env bash
# test-champion-park-marker-dedupe.sh - #13880: the Capped-PR Recovery Pass
# keep-parked guard suppresses per rejection EPISODE, not per marker-key
# spelling, and also suppresses on zero-rejection PRs (the `:none` key).
#
# Extract-and-execute (#7979): the SHIPPED bash block that follows the
# "### Step 4b:" heading in champion-pr-merge.md is run with `gh` and
# `post-comment.sh` stubbed (`jq` stays real). Scenarios:
#   (a) park comments with DIFFERENT keys, all newer than the one rejection
#       => no post (the gf180-parasynth#647 duplicate-verdict incident);
#   (b) park comment older than a newer rejection => one post (fresh episode);
#   (c) zero rejections + an existing `:none`-keyed park comment => no post;
#   (d) zero rejections + no comments => one post;
#   (e) a comment QUOTING the marker mid-body => still posts (startswith, #5371);
#   (f) a rejection EDITED after the park comment => one post (edit opens an
#       episode; REST updated_at, not created_at).
#
# Hermetic: no network, no forge, no tokens.
#
# Usage:
#   ./.loom/scripts/tests/test-champion-park-marker-dedupe.sh

# SC2034: PR_NUMBER/GH_READ are read by the eval'd shipped block.
# SC2016: backticks in the asserted literals are Markdown, not expansions.
# shellcheck disable=SC2034,SC2016

set -uo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$TEST_DIR/.." && pwd)"
if [[ -d "$SCRIPTS_DIR/../../.claude/commands/loom" ]]; then
    PROMPT_DIR="$(cd "$SCRIPTS_DIR/../../.claude/commands/loom" && pwd)"
else
    PROMPT_DIR="$(cd "$SCRIPTS_DIR/../.claude/commands/loom" && pwd)"
fi
CHAMPION_MD="$PROMPT_DIR/champion-pr-merge.md"

TESTS_RUN=0
TESTS_FAILED=0
pass() { TESTS_RUN=$((TESTS_RUN + 1)); echo "  PASS: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo "  FAIL: $1"; }
assert_contains() {
    if grep -qF -- "$2" <<<"$1"; then pass "$3"; else fail "$3 (missing '$2')"; fi
}
assert_lacks() {
    if grep -qF -- "$2" <<<"$1"; then fail "$3 (unexpected '$2')"; else pass "$3"; fi
}

# The Step 4b block: first ```bash fence after the "### Step 4b:" heading.
# The `PR_NUMBER=<number>` placeholder line is dropped — the harness sets a
# real PR_NUMBER before eval'ing the block.
BLOCK=$(awk '
    /^### Step 4b:/ { armed = 1; next }
    armed && /^```bash/ { inside = 1; next }
    inside && /^```/ { exit }
    inside { print }
' "$CHAMPION_MD" | sed '/^PR_NUMBER=<number>$/d')

echo "Test 1: the shipped block exists and keeps its structural guards"
if [[ -n "$BLOCK" ]]; then pass "block extracted"; else fail "block not found in $CHAMPION_MD"; fi
assert_contains "$BLOCK" 'champion:capped-pr-parked' "still writes a park marker"
assert_contains "$BLOCK" 'startswith("<!-- champion:capped-pr-parked:' "suppression matches marker bodies via startswith (#5371)"
assert_contains "$BLOCK" 'contains("champion:capped-pr")) | not' "Champion comments excluded from rejection selection"
assert_contains "$BLOCK" '.updated_at] | max) as $rejected' "episode boundary is the newest rejection edit time"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/bin" "$WORK/.loom/scripts"
cat >"$WORK/bin/gh" <<'EOF'
#!/usr/bin/env bash
echo "gh $*" >>"$CALLS"
if [[ "${1:-}" == "api" ]]; then
    cat "$COMMENTS_FILE"
else
    printf '%s\n' "$REJECTION_ID"
fi
EOF
cat >"$WORK/.loom/scripts/post-comment.sh" <<'EOF'
#!/usr/bin/env bash
echo "post-comment $1 $2" >>"$CALLS"
printf '%s\n' "$4" >"$BODY_FILE"
EOF
chmod +x "$WORK/bin/gh" "$WORK/.loom/scripts/post-comment.sh"

# run_block <comments-fixture> — evals the shipped block with the stubs.
run_block() {
    export CALLS="$WORK/calls.log" BODY_FILE="$WORK/body.txt"
    export COMMENTS_FILE="$WORK/comments.jsonl" REJECTION_ID="IC_test_rejection_1"
    cp "$1" "$COMMENTS_FILE"
    rm -f "$CALLS" "$BODY_FILE"
    (
        cd "$WORK" || exit 1
        PATH="$WORK/bin:$PATH"
        PR_NUMBER=42
        eval "$BLOCK"
    )
    cat "$CALLS" 2>/dev/null || true
}

post_count() { grep -c "post-comment 42 --pr" "$WORK/calls.log" 2>/dev/null || true; }

echo "Test 2: differently-keyed park comments newer than the one rejection => no post"
cat >"$WORK/f-a.jsonl" <<'EOF'
{"body":"❌ **Changes Requested**\nstill broken","created_at":"2026-10-11T14:33:16Z","updated_at":"2026-10-11T14:33:16Z"}
{"body":"<!-- champion:capped-pr-parked:IC_kwDOUfc03s8AAAABbDCtZQ -->\n**Champion: Keeping This PR Parked**","created_at":"2026-10-16T01:47:22Z","updated_at":"2026-10-16T01:47:22Z"}
{"body":"<!-- champion:capped-pr-parked:head-d61ddca0 -->\n**Champion: Keeping This PR Parked**","created_at":"2026-10-17T20:47:06Z","updated_at":"2026-10-17T20:47:06Z"}
{"body":"<!-- champion:capped-pr-parked:d61ddca08e75 -->\n**Champion: Keeping This PR Parked**","created_at":"2026-11-02T18:24:42Z","updated_at":"2026-11-02T18:24:42Z"}
EOF
OUT=$(run_block "$WORK/f-a.jsonl")
if [[ "$(post_count)" -eq 0 ]]; then pass "no duplicate verdict posted"; else fail "posted $(post_count) duplicate verdict(s)"; fi
assert_contains "$OUT" "already posted" "says the verdict already stands"

echo "Test 3: park comment older than a newer rejection => one fresh post"
cat >"$WORK/f-b.jsonl" <<'EOF'
{"body":"<!-- champion:capped-pr-parked:IC_kwDOUfc03s8AAAABbDCtZQ -->\n**Champion: Keeping This PR Parked**","created_at":"2026-10-16T01:47:22Z","updated_at":"2026-10-16T01:47:22Z"}
{"body":"❌ **Changes Requested**\nfresh defects","created_at":"2026-10-18T00:00:00Z","updated_at":"2026-10-18T00:00:00Z"}
EOF
OUT=$(run_block "$WORK/f-b.jsonl")
if [[ "$(post_count)" -eq 1 ]]; then pass "one fresh verdict posted"; else fail "expected exactly 1 post, got $(post_count)"; fi
BODY=$(cat "$WORK/body.txt" 2>/dev/null || true)
if [[ "$BODY" == "<!-- champion:capped-pr-parked:"* ]]; then
    pass "the comment opens with the park marker"
else
    fail "the comment must open with the marker: $BODY"
fi

echo "Test 4: zero rejections + existing :none park comment => no post"
cat >"$WORK/f-c.jsonl" <<'EOF'
{"body":"<!-- champion:capped-pr-parked:none -->\n**Champion: Keeping This PR Parked**","created_at":"2026-10-01T00:00:00Z","updated_at":"2026-10-01T00:00:00Z"}
EOF
OUT=$(run_block "$WORK/f-c.jsonl")
if [[ "$(post_count)" -eq 0 ]]; then pass "no duplicate on the :none episode"; else fail "posted $(post_count) duplicate(s) on a zero-rejection PR (would re-post every tick)"; fi

echo "Test 5: zero rejections + no comments => one post"
: >"$WORK/f-d.jsonl"
OUT=$(run_block "$WORK/f-d.jsonl")
if [[ "$(post_count)" -eq 1 ]]; then pass "first verdict on an empty thread posts once"; else fail "expected exactly 1 post, got $(post_count)"; fi

echo "Test 6: a comment merely QUOTING the marker mid-body => still posts"
cat >"$WORK/f-e.jsonl" <<'EOF'
{"body":"prose discussing the <!-- champion:capped-pr-parked:none --> marker without posting a verdict","created_at":"2026-10-01T00:00:00Z","updated_at":"2026-10-01T00:00:00Z"}
EOF
OUT=$(run_block "$WORK/f-e.jsonl")
if [[ "$(post_count)" -eq 1 ]]; then pass "quoted marker does not suppress (#5371)"; else fail "quoted marker wrongly suppressed the post"; fi

echo "Test 7: rejection EDITED after the park comment => one fresh post"
cat >"$WORK/f-f.jsonl" <<'EOF'
{"body":"<!-- champion:capped-pr-parked:IC_kwDOUfc03s8AAAABbDCtZQ -->\n**Champion: Keeping This PR Parked**","created_at":"2026-10-16T00:00:00Z","updated_at":"2026-10-16T00:00:00Z"}
{"body":"❌ **Changes Requested**\nrejection re-edited with new findings","created_at":"2026-10-11T14:33:16Z","updated_at":"2026-10-20T00:00:00Z"}
EOF
OUT=$(run_block "$WORK/f-f.jsonl")
if [[ "$(post_count)" -eq 1 ]]; then pass "edited rejection opens a fresh episode"; else fail "expected exactly 1 post, got $(post_count)"; fi

echo "Test 8: reference doc describes the zero-rejection arm"
REF="$PROMPT_DIR/champion-reference.md"
if grep -qF 'thread holds no Judge rejection' "$REF" 2>/dev/null; then
    pass "champion-reference.md names the no-rejection suppression"
else
    fail "champion-reference.md must name the no-rejection suppression ($REF)"
fi

echo
echo "$TESTS_RUN tests, $TESTS_FAILED failed"
[[ "$TESTS_FAILED" -eq 0 ]]
