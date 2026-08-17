#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-loom-workflow.sh
#
# Usage: ./tests/hooks/test-guard-loom-workflow.sh
#
# Tests the extracted Loom-workflow PreToolUse guard (issue #3604): the
# 'gh pr merge' -> merge-pr.sh redirect and the 'pip install -e' worktree block.
# Exit code 0 = all tests pass, 1 = failures detected.
#
# The guard under test is the canonical source at defaults/hooks/ (the
# version-controlled source of truth), NOT the gitignored .loom/hooks/ install
# artifact — so the suite validates exactly what ships.

set -euo pipefail

# Hermetic baseline: ambient guard-behavior overrides must not leak into tests
# (#4325). Tests that exercise env-driven behavior deliberately inject their
# vars explicitly per invocation, or via run_guard_in_worktree, so they are
# unaffected by this unset.
unset LOOM_FORCE_SCOPE LOOM_DEFAULT_BRANCH LOOM_GUARD_SQL LOOM_GUARD_CLOUD \
      LOOM_GUARD_REVERSIBLE_GH LOOM_RM_SCOPE LOOM_GUARD_READONLY_FASTPATH \
      LOOM_GUARD_WORKTREE_ISOLATION LOOM_WORKTREE_PATH LOOM_WORKTREE_ROOT \
      LOOM_GUARD_DECISION_LOG LOOM_GUARD_DECISION_LOG_FILE

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
GUARD="$REPO_ROOT/defaults/hooks/guard-loom-workflow.sh"

PASS=0
FAIL=0
TOTAL=0

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

# Build a JSON input blob for the guard script
make_input() {
    local cmd="$1"
    local cwd="${2:-$REPO_ROOT}"
    jq -n --arg cmd "$cmd" --arg cwd "$cwd" '{
        tool_name: "Bash",
        tool_input: { command: $cmd },
        cwd: $cwd
    }'
}

# Run the guard and capture output + exit code
run_guard() {
    local cmd="$1"
    local cwd="${2:-$REPO_ROOT}"
    local output
    local exit_code
    output=$(make_input "$cmd" "$cwd" | "$GUARD" 2>&1) || exit_code=$?
    exit_code=${exit_code:-0}
    echo "$output"
    return $exit_code
}

# Run the guard with LOOM_WORKTREE_PATH set (simulates worktree context)
run_guard_in_worktree() {
    local cmd="$1"
    local cwd="${2:-$REPO_ROOT}"
    local output
    local exit_code
    output=$(LOOM_WORKTREE_PATH="$cwd" make_input "$cmd" "$cwd" | LOOM_WORKTREE_PATH="$cwd" "$GUARD" 2>&1) || exit_code=$?
    exit_code=${exit_code:-0}
    echo "$output"
    return $exit_code
}

# Assert the guard denies a command
assert_deny() {
    local description="$1"
    local cmd="$2"
    local cwd="${3:-$REPO_ROOT}"
    TOTAL=$((TOTAL + 1))
    local output
    output=$(run_guard "$cmd" "$cwd") || true
    if echo "$output" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: deny"
        echo -e "       Got: $output"
    fi
}

# Assert the guard denies a command AND the reason matches an ERE.
assert_deny_reason_matches() {
    local description="$1"
    local cmd="$2"
    local pattern="$3"
    local cwd="${4:-$REPO_ROOT}"
    TOTAL=$((TOTAL + 1))
    local output reason
    output=$(run_guard "$cmd" "$cwd") || true
    reason=$(echo "$output" | jq -r '.hookSpecificOutput.permissionDecisionReason // empty' 2>/dev/null)
    if echo "$output" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 && \
       echo "$reason" | grep -qE "$pattern"; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: deny with reason matching /$pattern/"
        echo -e "       Got: $output"
    fi
}

# Assert the guard allows a command (exit 0, no decision)
assert_allow() {
    local description="$1"
    local cmd="$2"
    local cwd="${3:-$REPO_ROOT}"
    TOTAL=$((TOTAL + 1))
    local output
    local exit_code=0
    output=$(run_guard "$cmd" "$cwd") || exit_code=$?
    if [[ $exit_code -eq 0 ]] && \
       ! echo "$output" | jq -e '.hookSpecificOutput.permissionDecision' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: allow (exit 0, no decision)"
        echo -e "       Exit code: $exit_code"
        echo -e "       Got: $output"
    fi
}

# Assert the guard denies a command when inside a worktree
assert_deny_in_worktree() {
    local description="$1"
    local cmd="$2"
    local cwd="${3:-$REPO_ROOT}"
    TOTAL=$((TOTAL + 1))
    local output
    output=$(run_guard_in_worktree "$cmd" "$cwd") || true
    if echo "$output" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: deny"
        echo -e "       Got: $output"
    fi
}

# Assert the guard allows a command when inside a worktree
assert_allow_in_worktree() {
    local description="$1"
    local cmd="$2"
    local cwd="${3:-$REPO_ROOT}"
    TOTAL=$((TOTAL + 1))
    local output
    local exit_code=0
    output=$(run_guard_in_worktree "$cmd" "$cwd") || exit_code=$?
    if [[ $exit_code -eq 0 ]] && \
       ! echo "$output" | jq -e '.hookSpecificOutput.permissionDecision' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: allow (exit 0, no decision)"
        echo -e "       Exit code: $exit_code"
        echo -e "       Got: $output"
    fi
}

# =========================================================================
echo ""
echo -e "${YELLOW}=== Testing guard-loom-workflow.sh ===${NC}"
echo ""

# =========================================================================
echo -e "${YELLOW}--- gh pr merge redirect ---${NC}"
# =========================================================================

assert_deny "Block gh pr merge" \
    "gh pr merge 123"

assert_deny "Block gh pr merge --squash" \
    "gh pr merge 123 --squash"

# The deny message must name merge-pr.sh so the agent learns the correct tool.
assert_deny_reason_matches "gh pr merge deny reason names merge-pr.sh" \
    "gh pr merge 123" "merge-pr\.sh"

# --- False-positive regression tests (issue #5109) -----------------------
# The phrase "gh pr merge" appearing as INERT TEXT (a heredoc-quoted commit
# message, a --search query value) must not deny -- only an actual invocation
# should. Both reproduce the exact occurrences reported in #5109.

PHRASE_CMD="gh pr merge"

# Occurrence 1: a commit message built via the CLAUDE.md-documented
# `-m "$(cat <<'EOF' ... EOF)"` heredoc idiom, quoting the phrase as prose
# documenting the very rule this guard enforces.
GH_5109_COMMIT_CMD='git add foo.md && git commit -m "$(cat <<'"'"'EOF'"'"'
Document the rule: never `'"$PHRASE_CMD"'` directly, use merge-pr.sh instead.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)" && git push'
assert_allow "Allow heredoc commit message that quotes the phrase as prose" \
    "$GH_5109_COMMIT_CMD"

# Occurrence 2: a read-only search query whose --search value happens to
# contain the phrase as text to search FOR, not a command to run.
assert_allow "Allow gh issue list --search containing the phrase as query text" \
    "gh issue list --state open --search \"$PHRASE_CMD redirect guard false positive\" --limit 20 --json number,title,url"

# Regression guard: masking must NOT weaken the guard against an actual
# invocation wrapped in a shell -c string -- '-c' is deliberately not in the
# masked-flag whitelist, so this must still deny.
assert_deny "Still block gh pr merge wrapped in sh -c (no masking regression)" \
    "sh -c \"$PHRASE_CMD 123\""

# Regression guard: a heredoc that feeds an INTERPRETER (not `cat`) is live
# code, not inert data, and must stay visible to the check.
GH_5109_BASH_HEREDOC_CMD='bash <<'"'"'EOF'"'"'
'"$PHRASE_CMD"' 123
EOF'
assert_deny "Still block gh pr merge inside a bash-fed (non-cat) heredoc" \
    "$GH_5109_BASH_HEREDOC_CMD"

# Regression guard (PR #5115 review): `cat` never executes its own body, but
# piping its stdout into a shell on the SAME line -- `cat <<'EOF' | bash` --
# makes the heredoc body live, executed code despite `cat` being the literal
# consumer. The cat-heredoc masking must NOT neutralize such a body (it is not
# confined to inert text: it reaches `bash`), so a real invocation shaped this
# way must still deny. This is the exact bypass reported in the #5115 review.
GH_5115_CAT_PIPE_BASH_CMD='cat <<'"'"'EOF'"'"' | bash
'"$PHRASE_CMD"' 123 --admin
EOF'
assert_deny "Still block gh pr merge in a cat-heredoc piped into bash" \
    "$GH_5115_CAT_PIPE_BASH_CMD"

# And its `| sh` cousin -- same reasoning, different interpreter.
GH_5115_CAT_PIPE_SH_CMD='cat <<'"'"'EOF'"'"' | sh
'"$PHRASE_CMD"' 123
EOF'
assert_deny "Still block gh pr merge in a cat-heredoc piped into sh" \
    "$GH_5115_CAT_PIPE_SH_CMD"

# And a cat-heredoc captured then eval-executed: captured by $() but the
# consumer is `eval`, NOT a text-data flag, so the body is not inert and must
# stay visible.
GH_5115_CAT_EVAL_CMD='eval "$(cat <<'"'"'EOF'"'"'
'"$PHRASE_CMD"' 123
EOF
)"'
assert_deny "Still block gh pr merge in a cat-heredoc captured then eval'd" \
    "$GH_5115_CAT_EVAL_CMD"

# Regression guard (issue #5122 -- capre-gate bypass reported after #5115
# merged): the `capre` prefix gate (added by #5115 to allow the documented
# `-m "$(cat <<'EOF' ... EOF)"` idiom) only inspected the text BEFORE the
# `cat` token -- never what follows the heredoc opener line. So wrapping the
# SAME bypass shapes above in a flag-captured prefix (`-m "$(cat <<'EOF' |
# bash ...)"`) slipped straight past the gate and was masked as inert data,
# even though `cat`'s stdout is still piped into a live shell. The opener
# line must END immediately after the quoted delimiter to be masked; any
# suffix -- a pipe, a redirect, anything -- must keep the body visible to
# the merge-redirect grep.
GH_5122_FLAG_CAPTURED_PIPE_BASH_CMD='git commit -m "$(cat <<'"'"'EOF'"'"' | bash
'"$PHRASE_CMD"' 123 --admin
EOF
)"'
assert_deny "Still block gh pr merge in a flag-captured cat-heredoc piped into bash (#5122)" \
    "$GH_5122_FLAG_CAPTURED_PIPE_BASH_CMD"

# Same class, but the redirect parks the body in a file instead of piping it
# directly -- a later command on the same line then executes that file. The
# pipe is not the only live vector; any redirect on the opener line is.
GH_5122_FLAG_CAPTURED_REDIRECT_FILE_CMD='git commit -m "$(cat <<'"'"'EOF'"'"' 1> /tmp/loom-test-5122.sh
'"$PHRASE_CMD"' 123 --admin
EOF
)" ; bash /tmp/loom-test-5122.sh'
assert_deny "Still block gh pr merge in a flag-captured cat-heredoc redirected to a file then bash'd (#5122)" \
    "$GH_5122_FLAG_CAPTURED_REDIRECT_FILE_CMD"

# The `<<-` (dash) heredoc variant (strips leading tabs from the body) must
# get the same treatment -- the opener-line-suffix check does not special-
# case the `-` after `<<`.
GH_5122_FLAG_CAPTURED_DASH_PIPE_BASH_CMD='git commit -m "$(cat <<-'"'"'EOF'"'"' | bash
'"$PHRASE_CMD"' 123 --admin
EOF
)"'
assert_deny "Still block gh pr merge in a flag-captured cat <<- heredoc piped into bash (#5122)" \
    "$GH_5122_FLAG_CAPTURED_DASH_PIPE_BASH_CMD"

# --- False-positive regression tests (issue #5328) -----------------------
# `mask_cat_heredoc_bodies()` masked a commit-message heredoc body for the
# `git commit -m "$(cat <<'EOF' ... EOF)"` form (#5109/#5155) but NOT for
# `git commit -F - <<'EOF' ... EOF` -- a commit whose MESSAGE quotes the
# phrase as prose was denied as though it were a real invocation.

# Repro (a): `git commit -F - <<'EOF' ... EOF` quoting the phrase as prose ->
# allow.
GH_5328_COMMIT_F_DASH_CMD='git commit -F - <<'"'"'EOF'"'"'
Document the rule: never `'"$PHRASE_CMD"'` directly, use merge-pr.sh instead.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF'
assert_allow "Allow git commit -F - heredoc commit message that quotes the phrase as prose (#5328)" \
    "$GH_5328_COMMIT_F_DASH_CMD"

# Repro variant: `git commit --file=- <<'EOF' ... EOF` (the "=" long-option
# form) must be recognized too.
GH_5328_COMMIT_FILE_EQ_CMD='git commit --file=- <<'"'"'EOF'"'"'
Document the rule: never `'"$PHRASE_CMD"'` directly, use merge-pr.sh instead.
EOF'
assert_allow "Allow git commit --file=- heredoc commit message that quotes the phrase as prose (#5328)" \
    "$GH_5328_COMMIT_FILE_EQ_CMD"

# The `<<-` (dash) tab-stripping heredoc variant must get the same treatment.
GH_5328_COMMIT_F_DASH_TABSTRIP_CMD='git commit -F - <<-'"'"'EOF'"'"'
	Document the rule: never `'"$PHRASE_CMD"'` directly, use merge-pr.sh instead.
	EOF'
assert_allow "Allow git commit -F - <<- (tab-stripping) heredoc quoting the phrase as prose (#5328)" \
    "$GH_5328_COMMIT_F_DASH_TABSTRIP_CMD"

# Other flags between `commit` and `-F -` (e.g. `-a`) must not defeat
# recognition -- the gate allows any whitespace-separated tokens in between.
GH_5328_COMMIT_OTHER_FLAGS_CMD='git commit -a -F - <<'"'"'EOF'"'"'
Document the rule: never `'"$PHRASE_CMD"'` directly, use merge-pr.sh instead.
EOF'
assert_allow "Allow git commit -a -F - heredoc quoting the phrase as prose (#5328)" \
    "$GH_5328_COMMIT_OTHER_FLAGS_CMD"

# Regression guard: an UNQUOTED delimiter (`git commit -F - <<EOF`) still
# masks nothing -- $()/backtick/$VAR expansion is live there, matching the
# existing quoted-delimiter requirement for the `cat` case.
GH_5328_UNQUOTED_DELIM_CMD='git commit -F - <<EOF
'"$PHRASE_CMD"' 123
EOF'
assert_deny "Still block git commit -F - <<EOF with an UNQUOTED delimiter (#5328)" \
    "$GH_5328_UNQUOTED_DELIM_CMD"

# Regression guard: a heredoc feeding an INTERPRETER, not `git commit -F -`,
# must remain fully visible -- `sh <<EOF` alongside the new commit-stdin gate.
GH_5328_SH_HEREDOC_CMD='sh <<'"'"'EOF'"'"'
'"$PHRASE_CMD"' 123
EOF'
assert_deny "Still block gh pr merge inside a sh-fed (non-commit-stdin) heredoc (#5328)" \
    "$GH_5328_SH_HEREDOC_CMD"

# Regression guard: `cat <<'EOF' | bash` must still deny -- the new
# `git commit -F -`/`--file=-` allowlist entry must not weaken the existing
# `cat | bash` hazard detection.
GH_5328_CAT_PIPE_BASH_CMD='cat <<'"'"'EOF'"'"' | bash
'"$PHRASE_CMD"' 123
EOF'
assert_deny "Still block cat <<'EOF' | bash after the #5328 fix (no regression)" \
    "$GH_5328_CAT_PIPE_BASH_CMD"

# Regression guard: a REAL gh pr merge invocation must still deny.
assert_deny "Still block a real gh pr merge invocation after the #5328 fix" \
    "gh pr merge 887 --squash"

# Regression guard (#5087 provably-closed rule): an UNTERMINATED
# `git commit -F - <<'EOF'` opener (no closing delimiter line found in the
# buffer) masks NOTHING and fails safe -- the raw phrase stays visible and
# the command is denied/scanned exactly as before.
GH_5328_UNTERMINATED_CMD='git commit -F - <<'"'"'EOF'"'"'
'"$PHRASE_CMD"' 123 mentioned but the heredoc never closes'
assert_deny "Still block an UNTERMINATED git commit -F - heredoc opener (#5087 rule, #5328)" \
    "$GH_5328_UNTERMINATED_CMD"

# Security regression (#5333, Judge finding): the #5328 `commit_stdin_re`
# anchored its start to `(^|[^A-Za-z0-9_])`, which only proves the substring
# `git commit -F -` appears immediately before `<<` -- NOT that `git` is the
# first word of the command actually consuming the heredoc. `bash -s --`
# executes the heredoc body as a live script and treats the trailing tokens
# (`git commit -F -`) as mere positional parameters ($1..$4), never passed to
# git. Masking that body hid the real `gh pr merge` invocation. The anchor is
# now a genuine command boundary, so this must still DENY.
GH_5333_BASH_S_DECOY_CMD='bash -s -- git commit -F - <<'"'"'EOF'"'"'
'"$PHRASE_CMD"' 887 --admin
EOF'
assert_deny "Still block gh pr merge in a bash -s heredoc with a git commit -F - decoy suffix (#5333)" \
    "$GH_5333_BASH_S_DECOY_CMD"

# Lower-severity variant of the same looseness: a decoy command (`echo`)
# preceding `git commit -F -` on the line must no longer trigger masking.
# `echo` ignores stdin so this is not itself dangerous, but it proves the
# old any-non-word-char anchor was wrong (#5333).
GH_5333_ECHO_DECOY_CMD='echo git commit -F - <<'"'"'EOF'"'"'
'"$PHRASE_CMD"' 887 --admin
EOF'
assert_deny "Still block gh pr merge behind an echo git commit -F - decoy prefix (#5333)" \
    "$GH_5333_ECHO_DECOY_CMD"

# Positive control (#5333): a genuine `git commit -F -` chained after a real
# command separator (`;`) IS a real git-commit consumer of the heredoc, so the
# command-boundary anchor must still MASK/ALLOW it -- the operator legitimately
# starts a new simple command.
GH_5333_SEMICOLON_OK_CMD='echo staged ; git commit -F - <<'"'"'EOF'"'"'
Document the rule: never `'"$PHRASE_CMD"'` directly, use merge-pr.sh instead.
EOF'
assert_allow "Allow git commit -F - after a ; command separator quoting the phrase as prose (#5333)" \
    "$GH_5333_SEMICOLON_OK_CMD"

# Security regression (#5333, SECOND Judge finding + adversarial follow-up):
# the command-boundary anchor above was evaluated against PHYSICAL-line text,
# but bash's real statement boundaries do not align with physical newlines.
# Every shape below is LIVE CODE (verified by executing each with a harmless
# `echo` payload: the heredoc body really is run by an interpreter), yet each
# was masked -- and therefore ALLOWED -- by the previous anchor. All must DENY.

# (a) Backslash-newline line continuation: `git commit -F -` starts physical
# line 2 at column 1, so the `^` alternative matched -- but the continuation
# joins the two physical lines into ONE logical command, `bash -s -- git commit
# -F - <<EOF`, whose heredoc body `bash -s` executes as a script ($1..$4 bind
# to git/commit/-F/-).
GH_5333_CONTINUATION_DECOY_CMD='bash -s -- \
git commit -F - <<'"'"'EOF'"'"'
harmless-looking prose line
'"$PHRASE_CMD"' 887 --admin
EOF'
assert_deny "Still block a backslash-newline-continued bash -s decoy before git commit -F - (#5333)" \
    "$GH_5333_CONTINUATION_DECOY_CMD"

# (b) ESCAPED control operator: `\;` is a literal semicolon ARGUMENT to
# `bash -s`, not a command separator, so the heredoc is still bash's stdin.
GH_5333_ESCAPED_SEMI_CMD='bash -s -- \; git commit -F - <<'"'"'EOF'"'"'
'"$PHRASE_CMD"' 887 --admin
EOF'
assert_deny "Still block an ESCAPED semicolon faking a command boundary (#5333)" \
    "$GH_5333_ESCAPED_SEMI_CMD"

# (b2) Same class with `\&`, and combined with a continuation, to prove the
# fix is not a special case for one operator or one line shape.
GH_5333_ESCAPED_AMP_CMD='bash -s -- \& git commit -F - <<'"'"'EOF'"'"'
'"$PHRASE_CMD"' 887 --admin
EOF'
assert_deny "Still block an ESCAPED ampersand faking a command boundary (#5333)" \
    "$GH_5333_ESCAPED_AMP_CMD"

GH_5333_ESCAPED_SEMI_CONT_CMD='bash -s -- \;\
git commit -F - <<'"'"'EOF'"'"'
'"$PHRASE_CMD"' 887 --admin
EOF'
assert_deny "Still block an escaped semicolon PLUS a line continuation (#5333)" \
    "$GH_5333_ESCAPED_SEMI_CONT_CMD"

# (c) QUOTED newline / quoted operator: an unterminated double quote on an
# earlier physical line means the newline before the opener is inside a string,
# so what looks like a fresh `git commit` statement is really string content
# and the phrase line executes at top level once the string closes.
GH_5333_MULTILINE_QUOTE_CMD='bash -s -- "x
git commit -F - <<'"'"'EOF'"'"'
"
'"$PHRASE_CMD"' 887 --admin
EOF'
assert_deny "Still block an opener sitting inside a multi-line quoted string (#5333)" \
    "$GH_5333_MULTILINE_QUOTE_CMD"

GH_5333_QUOTED_SEMI_CMD='bash -s -- "x ; git commit -F - <<'"'"'EOF'"'"'
"
'"$PHRASE_CMD"' 887 --admin
EOF'
assert_deny "Still block a QUOTED semicolon faking a command boundary (#5333)" \
    "$GH_5333_QUOTED_SEMI_CMD"

# (d) Metacharacter smuggled inside a token between `commit` and `-F -`: the
# old `[^ \t]+` token class swallowed `;`, so `git commit -a` + a second,
# interpreter command read as one git invocation.
GH_5333_METACHAR_TOKEN_CMD='git commit -a;bash -s -- -F - <<'"'"'EOF'"'"'
'"$PHRASE_CMD"' 887 --admin
EOF'
assert_deny "Still block a metacharacter smuggled into a git commit flag token (#5333)" \
    "$GH_5333_METACHAR_TOKEN_CMD"

# (e) The opener sits inside an OUTER heredoc whose delimiter is UNQUOTED, so
# the outer shell expands -- and therefore executes -- a $(...) living in what
# looks like an inner commit-message body.
GH_5333_OUTER_EXPANDING_HEREDOC_CMD='bash <<OUTER
git commit -F - <<'"'"'EOF'"'"'
$('"$PHRASE_CMD"' 887 --admin)
EOF
OUTER'
assert_deny "Still block an opener nested in an outer EXPANDING heredoc body (#5333)" \
    "$GH_5333_OUTER_EXPANDING_HEREDOC_CMD"

# Positive controls (#5333): multi-line-formatted, genuinely anchored
# `git commit -F -` usages must still mask/allow.
GH_5333_AND_CHAIN_OK_CMD='git add -A && git commit -F - <<'"'"'EOF'"'"'
Document the rule: never `'"$PHRASE_CMD"'` directly, use merge-pr.sh instead.
EOF'
assert_allow "Allow git add -A && git commit -F - quoting the phrase as prose (#5333)" \
    "$GH_5333_AND_CHAIN_OK_CMD"

GH_5333_PRIOR_LINE_OK_CMD='git add -A
git commit -F - <<'"'"'EOF'"'"'
Document the rule: never `'"$PHRASE_CMD"'` directly, use merge-pr.sh instead.
EOF'
assert_allow "Allow git commit -F - on its own line after a prior statement line (#5333)" \
    "$GH_5333_PRIOR_LINE_OK_CMD"

# Deliberate FAIL-SAFE NARROWING (#5333): a prefix containing a quote (or a
# backslash) cannot be proven to be outside a string, so the body is left
# visible and the command denies even though this particular shape is benign.
# A false deny here is recoverable (rephrase or drop the quotes); a false allow
# on this catastrophic-tier guard is not. Locked in by a test so a future
# re-widening of the anchor is a deliberate, visible decision.
GH_5333_QUOTED_PREFIX_NARROWING_CMD='echo "staged" ; git commit -F - <<'"'"'EOF'"'"'
Document the rule: never `'"$PHRASE_CMD"'` directly, use merge-pr.sh instead.
EOF'
assert_deny "Fail-safe narrowing: a QUOTED token in the prefix is unprovable, so deny (#5333)" \
    "$GH_5333_QUOTED_PREFIX_NARROWING_CMD"

# --- False-positive regression tests (issue #5155) -----------------------
# The phrase appearing as INERT TEXT inside a POSITIONAL (no preceding flag
# name) argument to a known non-executing command must not deny -- only an
# actual invocation should. Both reproduce the exact occurrences reported in
# #5155 (a fresh shape not covered by #5109/#5115, which only masked
# NAMED-flag values).

# Reproduction 1: ./.loom/scripts/check-duplicate.sh's signature is
# `check-duplicate.sh TITLE DESCRIPTION` (purely positional, no flags). Both
# the title and description quote the phrase as prose describing this very
# guard bug.
GH_5155_CHECK_DUPLICATE_CMD="./.loom/scripts/check-duplicate.sh \"Guard false positive: $PHRASE_CMD redirect\" \"quotes the phrase '$PHRASE_CMD' as inert text, not a live invocation\""
assert_allow "Allow check-duplicate.sh positional TITLE/DESCRIPTION quoting the phrase as prose" \
    "$GH_5155_CHECK_DUPLICATE_CMD"

# Reproduction 2: a read-only `grep -n` source-code search whose pattern
# argument (positional, after the `-n` flag) happens to contain the phrase as
# search text. `grep` cannot execute anything it searches for.
assert_allow "Allow grep -n search of the guard's own source for the phrase" \
    "grep -n \"gh-pr-merge-redirect\\|$PHRASE_CMD\" defaults/hooks/guard-loom-workflow.sh"

# ripgrep cousin of the same shape.
assert_allow "Allow rg search containing the phrase as a search pattern" \
    "rg -n \"$PHRASE_CMD\" defaults/hooks/guard-loom-workflow.sh"

# Regression guard: masking must NOT weaken the guard against a command that
# is NOT in the new positional-arg allowlist -- `echo` piped into `bash` is a
# genuine (if contrived) execution vector, and `echo` is deliberately absent
# from the allowlist, so the phrase must remain visible and still deny.
assert_deny "Still block phrase piped through echo | bash (echo not allowlisted, #5155)" \
    "echo \"$PHRASE_CMD 123\" | bash"

# Regression guard: masking a matched positional span must not blind the
# check to a SECOND, real invocation elsewhere on the same command line --
# masking only narrows what THIS ONE check misses inside the matched
# grep/rg/check-duplicate.sh argument, it never widens what it misses
# outside that span.
assert_deny "Still block a real gh pr merge invocation chained after a masked grep search (#5155)" \
    "grep -n \"$PHRASE_CMD\" defaults/hooks/guard-loom-workflow.sh && $PHRASE_CMD 123"

echo ""

# --- False-positive regression tests (issue #6400) -----------------------
# mask_command_positional_args()'s cmdre allowlist covered grep/rg/
# check-duplicate.sh (#5155) but not echo/printf: a bare `echo "..."` or
# `printf "..."` line that merely NARRATES/quotes the phrase as prose reached
# the raw GH_PR_MERGE_SCAN_TEXT unmasked and denied even though nothing
# EXECUTED the flagged invocation. Unlike grep/rg/check-duplicate.sh, though,
# echo/printf's own quoted text CAN become a real execution vector when piped
# into an interpreter or fed through a command substitution consumed by one
# -- so the fix additionally withholds masking whenever an echo/printf
# invocation's quoted-argument run is immediately followed by a pipe, or the
# invocation itself sits inside a `$(...)`/backtick command substitution.

# Reproduction (exact #6400 repro, phrase spelled out): a standalone echo
# narration line quoting the phrase as prose must ALLOW.
assert_allow "Allow standalone echo narration quoting the phrase as prose (#6400)" \
    "echo \"---generic $PHRASE_CMD redirect---\""

# printf cousin of the same shape.
assert_allow "Allow standalone printf narration quoting the phrase as prose (#6400)" \
    "printf '%s\\n' \"---generic $PHRASE_CMD redirect---\""

# Exact original multi-line repro from the issue: two benign gh issue list
# calls separated by an echo narration line.
GH_6400_REPRO_CMD='gh issue list --repo rjwalters/loom --state open --search "pr-merge-redirect" --limit 20 --json number,title
echo "---generic '"$PHRASE_CMD"' redirect---"
gh issue list --repo rjwalters/loom --state open --search "redirect" --limit 20 --json number,title'
assert_allow "Allow the exact multi-line repro: two gh issue list calls separated by echo narration (#6400)" \
    "$GH_6400_REPRO_CMD"

# Regression guard: a real gh pr merge invocation, unchanged, still denies.
assert_deny "Still block a real gh pr merge invocation after the #6400 fix" \
    "$PHRASE_CMD 123"

# Regression guard (already true pre-fix, must remain true): echo piped
# directly into an interpreter is a genuine execution vector -- the phrase
# must stay visible and still deny.
assert_deny "Still block phrase piped through echo | bash after the #6400 fix" \
    "echo \"$PHRASE_CMD 123\" | bash"

# printf cousin of the same pipe-to-interpreter shape.
assert_deny "Still block phrase piped through printf | bash (#6400)" \
    "printf '%s\\n' \"$PHRASE_CMD 123\" | bash"

# NOTE ON SHAPE: every wrapped-form case below quotes the phrase as an
# argument (`echo "<phrase> 123"`, not `echo <phrase> 123`). Masking only ever
# touches QUOTED positional arguments, so an unquoted phrase is left visible by
# construction and denies no matter what the nesting logic decides -- such a
# case cannot regress and therefore cannot guard anything. Keep the quotes.

# Regression guard: eval "$(echo ...)" -- the echo's own output nested inside
# a command substitution consumed by eval -- must stay visible and still
# deny, per the Test Plan's explicit wrapped-form requirement.
GH_6400_EVAL_ECHO_CMD='eval "$(echo "'"$PHRASE_CMD"' 123")"'
assert_deny "Still block gh pr merge wrapped in eval \"\$(echo ...)\" (#6400)" \
    "$GH_6400_EVAL_ECHO_CMD"

# Regression guards for the WHITESPACE-TOLERANT forms of the same wrapped
# invocation. The first cut of the #6400 fix decided "is this echo/printf
# nested in a command substitution?" from the single character immediately
# preceding the matched token, so it only recognized the exact `$(echo`
# adjacency above. Bash allows arbitrary whitespace -- including newlines --
# between `$(` (or a backtick) and the command, so every form below is an
# equally real `eval`-consumed execution vector that MUST stay visible and
# deny. These are the cases the adjacency-only check masked (i.e. wrongly
# ALLOWED); the nesting-depth map that replaced it catches all of them.

# 1. A single space between `$(` and `echo`.
GH_6400_EVAL_ECHO_SPACE_CMD='eval "$( echo "'"$PHRASE_CMD"' 123" )"'
assert_deny "Still block gh pr merge in eval \"\$( echo ... )\" with a space after the paren (#6400)" \
    "$GH_6400_EVAL_ECHO_SPACE_CMD"

# 2. A newline between `$(` and `echo` (the multi-line command-substitution
#    form). The masking pass joins input lines into one buffer, so the
#    nesting test has to survive a newline delimiter, not just a space.
GH_6400_EVAL_ECHO_NEWLINE_CMD='eval "$(
  echo "'"$PHRASE_CMD"' 123"
)"'
assert_deny "Still block gh pr merge in a newline-separated eval \"\$(\\n echo ... \\n)\" (#6400)" \
    "$GH_6400_EVAL_ECHO_NEWLINE_CMD"

# 3. The backtick cousin, also whitespace-separated.
GH_6400_EVAL_BACKTICK_SPACE_CMD='eval "` echo "'"$PHRASE_CMD"' 123" `"'
assert_deny "Still block gh pr merge in eval with a whitespace-separated backtick substitution (#6400)" \
    "$GH_6400_EVAL_BACKTICK_SPACE_CMD"

# 4. A SECOND echo later inside the same still-open command substitution. Its
#    own preceding delimiter is a harmless `;`, so an adjacency-only check
#    saw it as a bare statement start even though the whole statement list is
#    inside `$(...)` and consumed by the caller.
GH_6400_SUBST_SECOND_ECHO_CMD='eval "$(echo setup; echo "'"$PHRASE_CMD"' 123")"'
assert_deny "Still block a gh pr merge echo that is the SECOND statement inside one \$(...) (#6400)" \
    "$GH_6400_SUBST_SECOND_ECHO_CMD"

# 5. printf inside `bash -c "$( ... )"` -- same nesting, different interpreter
#    and different allowlisted command.
GH_6400_BASH_C_PRINTF_CMD='bash -c "$(  printf %s "'"$PHRASE_CMD"' 123" )"'
assert_deny "Still block printf wrapped in bash -c \"\$( printf ... )\" (#6400)" \
    "$GH_6400_BASH_C_PRINTF_CMD"

# Regression guards for the COUNTER-UNDERFLOW forms (#6400 re-review). The
# nesting-depth map that replaced the adjacency check was first built from a
# single scalar counter that `$(` incremented and ANY `)` decremented. That
# conflated "a `)` closing the real enclosing `$(...)`" with "a `)` closing an
# unrelated bare `(...)` grouping that appears as an earlier sibling statement
# inside the same still-open `$(...)`" -- so a throwaway `(true);` silently
# un-nested every echo/printf after it and the phrase was masked (i.e.
# ALLOWED) even though `eval` really does execute it. The map now uses an
# opener-type-aware STACK: a `)` pops whichever level is on top and only
# decrements substitution depth when the level it popped was opened by `$(`.

# 6. The exact re-review repro: one bare-paren subshell sibling ahead of the
#    echo, all inside a single still-open `$( ... )` consumed by eval.
GH_6400_BARE_PAREN_SIBLING_CMD='eval "$( (true); echo "'"$PHRASE_CMD"' 123" )"'
assert_deny "Still block gh pr merge after a bare-paren sibling inside one \$(...) (#6400)" \
    "$GH_6400_BARE_PAREN_SIBLING_CMD"

# 7. Two bare-paren siblings -- a scalar counter would underflow twice.
GH_6400_TWO_BARE_PARENS_CMD='eval "$( (true); (:); echo "'"$PHRASE_CMD"' 123" )"'
assert_deny "Still block gh pr merge after TWO bare-paren siblings inside one \$(...) (#6400)" \
    "$GH_6400_TWO_BARE_PARENS_CMD"

# 8. Bare parens nested within bare parens -- the stack has to unwind the
#    GROUP levels without ever touching the SUB level underneath them.
GH_6400_NESTED_BARE_PARENS_CMD='eval "$( ( (true) ); echo "'"$PHRASE_CMD"' 123" )"'
assert_deny "Still block gh pr merge after nested bare-paren siblings inside one \$(...) (#6400)" \
    "$GH_6400_NESTED_BARE_PARENS_CMD"

# 9. The bare-paren subshell AFTER the echo rather than before it -- the echo
#    is still inside the substitution, so ordering must not matter.
GH_6400_BARE_PAREN_AFTER_CMD='eval "$( echo "'"$PHRASE_CMD"' 123"; (true) )"'
assert_deny "Still block gh pr merge with the bare-paren sibling AFTER the echo (#6400)" \
    "$GH_6400_BARE_PAREN_AFTER_CMD"

# 10. `$(( ... ))` arithmetic expansion as the sibling: its inner `(` / `)`
#     pair hit the same underflow, since only the outer `$(` was counted.
GH_6400_ARITH_SIBLING_CMD='eval "$( n=$((1+2)); echo "'"$PHRASE_CMD"' $n" )"'
assert_deny "Still block gh pr merge after a \$((...)) arithmetic sibling inside one \$(...) (#6400)" \
    "$GH_6400_ARITH_SIBLING_CMD"

# 11. A `)` that is merely part of a QUOTED string inside the substitution --
#     not shell syntax at all, so it must not close anything either.
GH_6400_QUOTED_PAREN_CMD='eval "$( echo "a)b" ; echo "'"$PHRASE_CMD"' 123" )"'
assert_deny "Still block gh pr merge after a quoted \")\" inside one \$(...) (#6400)" \
    "$GH_6400_QUOTED_PAREN_CMD"

# 12. Same, with the stray `)` inside SINGLE quotes.
GH_6400_SQ_PAREN_CMD='eval "$( echo '"'"'a)b'"'"'; echo "'"$PHRASE_CMD"' 123" )"'
assert_deny "Still block gh pr merge after a single-quoted \")\" inside one \$(...) (#6400)" \
    "$GH_6400_SQ_PAREN_CMD"

# 13. An unpaired apostrophe earlier in the buffer (a `#` comment line, where
#     it is not a quote at all) must not silence substitution tracking for
#     everything after it -- the quote-blind half of the depth map is the
#     floor that keeps this denying.
GH_6400_COMMENT_APOSTROPHE_CMD="# it's fine
"'eval "$(echo "'"$PHRASE_CMD"' 123")"'
assert_deny "Still block gh pr merge in \$(echo ...) after an unpaired apostrophe in a comment (#6400)" \
    "$GH_6400_COMMENT_APOSTROPHE_CMD"

# A `case` PATTERN terminator is the one common shell construct that writes a
# `)` with no opener at all, so it is the remaining way an unrelated `)` could
# pop a `$(...)` level it never opened. The depth map tracks `case`/`esac` per
# stack frame and treats a `)` arriving inside an open case as a pattern
# terminator that pops nothing.

# 14. The plain case-pattern sibling.
GH_6400_CASE_PATTERN_CMD='eval "$( case x in x) :;; esac; echo "'"$PHRASE_CMD"' 123" )"'
assert_deny "Still block gh pr merge after a case-pattern \")\" inside one \$(...) (#6400)" \
    "$GH_6400_CASE_PATTERN_CMD"

# 15. Nested case constructs -- both levels have to be counted and unwound.
GH_6400_CASE_NESTED_CMD='eval "$( case x in a) case y in b) :;; esac;; esac; echo "'"$PHRASE_CMD"' 123" )"'
assert_deny "Still block gh pr merge after NESTED case-pattern \")\"s inside one \$(...) (#6400)" \
    "$GH_6400_CASE_NESTED_CMD"

# 16. A case construct inside a bare-paren group inside the substitution --
#     the case is tracked against the group frame, so the group still closes
#     normally and the substitution stays open.
GH_6400_CASE_IN_GROUP_CMD='eval "$( ( case x in x) :;; esac ); echo "'"$PHRASE_CMD"' 123" )"'
assert_deny "Still block gh pr merge after a case inside a bare-paren group inside \$(...) (#6400)" \
    "$GH_6400_CASE_IN_GROUP_CMD"

# 17. A real command substitution inside a case BODY must still open and close
#     normally -- case tracking is per frame, so it does not swallow that `)`.
GH_6400_CASE_BODY_SUBST_CMD='eval "$( case x in x) d=$(date);; esac; echo "'"$PHRASE_CMD"' 123" )"'
assert_deny "Still block gh pr merge after a case body containing its own \$(...) (#6400)" \
    "$GH_6400_CASE_BODY_SUBST_CMD"

# False-positive guards for case tracking: the literal word `case` only opens
# the construct at a COMMAND position, so ordinary text mentioning it must not
# freeze the depth map and start denying narration.
assert_allow "Allow echo narration after \$(grep case ...) -- 'case' as an argument, not a keyword (#6400)" \
    "X=\$(grep case /etc/hosts); echo \"note about $PHRASE_CMD\""

assert_allow "Allow echo narration after a top-level case ... esac statement (#6400)" \
    "case x in x) :;; esac; echo \"note about $PHRASE_CMD\""

# Counterpart false-positive guards for the stack: once a substitution has
# genuinely CLOSED, depth is back to zero and later narration still masks.

# A bare-paren group nested INSIDE a substitution that then closes properly.
assert_allow "Allow echo narration after a \$( (...) ) that closed properly (#6400)" \
    "TS=\$( (date -u +%FT%TZ) ); echo \"[\$TS] note about $PHRASE_CMD redirect\""

# A bare-paren group as a standalone statement ahead of pure narration.
assert_allow "Allow echo narration after a standalone bare-paren subshell statement (#6400)" \
    "(true); echo \"note about $PHRASE_CMD here\""

# A plain subshell (no \$) does not capture or execute its own output, so
# narration wrapped in one is still narration -- masked and allowed, and now
# uniformly so regardless of spacing (behavior delta accepted in #6404 review).
assert_allow "Allow echo narration wrapped in a plain (no-\$) subshell (#6400)" \
    "(echo \"note about $PHRASE_CMD here\")"

# A `)` inside quoted narration AFTER an already-closed substitution must not
# be read as re-opening or poisoning anything.
assert_allow "Allow echo narration containing a quoted \")\" after a closed \$(...) (#6400)" \
    "eval \"\$(true)\"; echo \"note :) about $PHRASE_CMD\""

# Counterpart false-positive guard: a command substitution that has already
# CLOSED before the narration line must not poison it -- depth is back to
# zero, so the narration still masks and allows.
assert_allow "Allow echo narration after an already-closed command substitution (#6400)" \
    "TS=\$(date -u +%FT%TZ); echo \"[\$TS] note about $PHRASE_CMD redirect\""

# Regression guard: an echo narration line immediately followed by a real
# gh pr merge chained on the same logical command must still deny -- masking
# already stops at the first non-quoted token, so this should already hold.
assert_deny "Still block echo narration chained with a real gh pr merge via && (#6400)" \
    "echo \"just a note\" && $PHRASE_CMD 123"

echo ""

# --- False-positive regression tests (issue #5172) -----------------------
# `gh api`'s `-f <field>=<value>` syntax is a DIFFERENT shape from the
# `--body <value>` flags #5109/#5115 masked, and a heredoc ASSIGNED TO A
# SHELL VARIABLE earlier in the command (then only referenced later via that
# variable) is a two-hop indirection neither #5109 nor #5155 covered. Both
# shapes are reproduced here from the exact occurrence reported in #5172.

# Reproduction (exact #5172 shape): a heredoc assigned to $BODY, whose prose
# quotes the disallowed phrase describing test cases, referenced later via
# `gh api -f body="$BODY"`.
GH_5172_VAR_HEREDOC_CMD='BODY="$(cat <<'"'"'EOF2'"'"'
Tested: raw '"$PHRASE_CMD"' 123 denied; echo "'"$PHRASE_CMD"' 123" | bash denied.
EOF2
)"
gh api "repos/rjwalters/loom/issues/5172/comments" -f body="$BODY"'
assert_allow "Allow gh api -f body=\$VAR where \$VAR is a heredoc quoting the phrase as prose (#5172)" \
    "$GH_5172_VAR_HEREDOC_CMD"

# A heredoc directly captured (no variable indirection) by `-f body=` must
# also be recognized -- the `gh api` field-syntax analog of the #5109 `-m`/
# `--body` case.
GH_5172_DIRECT_FIELD_HEREDOC_CMD='gh api "repos/o/r/issues/1/comments" -f body="$(cat <<'"'"'EOF5'"'"'
'"$PHRASE_CMD"' 123 as prose
EOF5
)"'
assert_allow "Allow gh api -f body=\"\$(cat <<EOF ...)\" heredoc captured directly (#5172)" \
    "$GH_5172_DIRECT_FIELD_HEREDOC_CMD"

# A literal (non-heredoc) `-f body="..."` value quoting the phrase directly.
assert_allow "Allow gh api -f body=\"...\" literal value quoting the phrase as prose (#5172)" \
    "gh api \"repos/o/r/issues/1/comments\" -f body=\"Tested: $PHRASE_CMD 123 denied\""

# The variable-indirection fix must generalize beyond `body` to the other
# known text-bearing `gh api` fields (message here).
GH_5172_VAR_HEREDOC_MESSAGE_CMD='MSG="$(cat <<'"'"'EOF6'"'"'
'"$PHRASE_CMD"' 123 as prose in message field
EOF6
)"
gh api "repos/o/r/issues/1/comments" -f message="$MSG"'
assert_allow "Allow gh api -f message=\$VAR where \$VAR is a heredoc quoting the phrase as prose (#5172)" \
    "$GH_5172_VAR_HEREDOC_MESSAGE_CMD"

# Regression guard (control, #5172): the two-hop indirection fix must NOT
# widen the guard into a bypass. A heredoc assigned to a variable that is
# THEN eval'd is a genuine live invocation and must still deny -- masking a
# variable-assigned heredoc's body is gated on EVERY later reference to that
# variable being confined to a known-safe flag/field value; `eval "$CMD"` is
# not in that allowlist.
GH_5172_EVAL_BYPASS_CMD='CMD="$(cat <<'"'"'EOF3'"'"'
'"$PHRASE_CMD"' 123 --admin
EOF3
)"
eval "$CMD"'
assert_deny "Still block a heredoc-assigned variable later eval'd (two-hop bypass attempt, #5172)" \
    "$GH_5172_EVAL_BYPASS_CMD"

# Regression guard (control, #5172): a heredoc-assigned variable referenced
# bare (as a command, not a safe flag/field value) must also still deny.
GH_5172_BARE_REF_CMD='X="$(cat <<'"'"'EOF4'"'"'
'"$PHRASE_CMD"' 123
EOF4
)"
$X'
assert_deny "Still block a heredoc-assigned variable referenced bare as a command (#5172)" \
    "$GH_5172_BARE_REF_CMD"

# Regression guard (control, #5172): a heredoc-assigned variable referenced
# TWICE -- once safely (-f body=), once unsafely (piped into bash) -- must
# still deny. One confined reference does not excuse an unconfined one.
GH_5172_MIXED_REF_CMD='X="$(cat <<'"'"'EOF7'"'"'
'"$PHRASE_CMD"' 123
EOF7
)"
gh api "repos/o/r/issues/1/comments" -f body="$X"
echo "$X" | bash'
assert_deny "Still block a heredoc-assigned variable with one safe and one unsafe later reference (#5172)" \
    "$GH_5172_MIXED_REF_CMD"

# --- Broaden confinement-reference detection to ALL param-expansion forms (#5297)
# The #5172 masking scanner recognized ONLY the exact "$VAR" / closed "${VAR}"
# literals as a later reference. Any OTHER bash parameter-expansion of the same
# heredoc-assigned variable (${VAR:0:100}, ${VAR#}, ${VAR:-}) was invisible to
# the scanner, and an undetected reference defaulted to "confined" -- so the
# heredoc body was masked and the guard never saw the real invocation, even
# though the variable is genuinely dereferenced and executes at runtime. Each
# case below assigns a REAL `gh pr merge 123` invocation to the variable via a
# heredoc, then dereferences it through one of these forms + `eval`: all must
# still DENY (the guard's deny-on-real-bypass invariant).

# Substring/offset expansion: `eval "${BODY:0:100}"` -- the exact shape Judge
# reproduced. Pre-fix this ALLOWED (bypass); it must DENY.
GH_5297_SUBSTR_EVAL_CMD='BODY="$(cat <<'"'"'EOF8'"'"'
'"$PHRASE_CMD"' 123
EOF8
)"
eval "${BODY:0:100}"'
assert_deny "Still block heredoc-assigned var eval-d via substring expansion \${VAR:0:100} (#5297)" \
    "$GH_5297_SUBSTR_EVAL_CMD"

# Prefix-removal expansion: `eval "${BODY#}"`.
GH_5297_STRIP_EVAL_CMD='BODY="$(cat <<'"'"'EOF9'"'"'
'"$PHRASE_CMD"' 123
EOF9
)"
eval "${BODY#}"'
assert_deny "Still block heredoc-assigned var eval-d via prefix-removal expansion \${VAR#} (#5297)" \
    "$GH_5297_STRIP_EVAL_CMD"

# Default-value expansion: `eval "${BODY:-}"`.
GH_5297_DEFAULT_EVAL_CMD='BODY="$(cat <<'"'"'EOF10'"'"'
'"$PHRASE_CMD"' 123
EOF10
)"
eval "${BODY:-}"'
assert_deny "Still block heredoc-assigned var eval-d via default-value expansion \${VAR:-} (#5297)" \
    "$GH_5297_DEFAULT_EVAL_CMD"

# Indirect expansion: `REF=BODY; eval "${!REF}"` -- the variable name never
# appears literally as "$BODY"/"${BODY", so the scanner sees ZERO references.
# Zero references is NOT proof of safety, so the body must be left UNMASKED and
# the invocation must still DENY.
GH_5297_INDIRECT_EVAL_CMD='BODY="$(cat <<'"'"'EOF11'"'"'
'"$PHRASE_CMD"' 123
EOF11
)"
REF=BODY; eval "${!REF}"'
assert_deny "Still block heredoc-assigned var eval-d via indirect expansion \${!REF} (#5297)" \
    "$GH_5297_INDIRECT_EVAL_CMD"

# Control (#5297): the broadened detection must NOT re-introduce the #5172
# false positive. A heredoc-assigned var whose body only quotes the phrase as
# prose, referenced through a param-expansion form INSIDE a confined field
# value (`-f body="${BODY:0:200}"`), stays confined -> masked -> ALLOWED.
GH_5297_CONFINED_EXPANSION_CMD='BODY="$(cat <<'"'"'EOF12'"'"'
Tested: '"$PHRASE_CMD"' 123 denied as prose
EOF12
)"
gh api "repos/o/r/issues/1/comments" -f body="${BODY:0:200}"'
assert_allow "Allow gh api -f body=\${VAR:0:200} where \$VAR is a heredoc quoting the phrase as prose (#5297)" \
    "$GH_5297_CONFINED_EXPANSION_CMD"

echo ""

# --- False-positive regression tests (issue #5672) -----------------------
# `mask_cat_heredoc_bodies()` only masked a cat-heredoc captured into a
# known text-data flag when its delimiter was QUOTED (`cat <<'EOF'`). The
# exact real-world occurrence reported in #5672 -- a Judge review comment
# posted via `gh pr comment N --body "$(cat <<EOF ... EOF)"` with an
# UNQUOTED delimiter, whose body prose merely quotes "gh pr merge" as an
# example -- was left completely unmasked and denied as though it were a
# real invocation.

# Reproduction (exact #5672 shape): unquoted-delimiter cat-heredoc captured
# directly into `gh pr comment --body`, body is pure prose quoting the
# phrase, no '$'/backtick anywhere in the body -> must ALLOW.
GH_5672_UNQUOTED_BODY_CMD='gh pr comment 5333 --body "$(cat <<EOF2
Please avoid: '"$PHRASE_CMD"' 123 --squash
EOF2
)"'
assert_allow "Allow gh pr comment --body unquoted-delimiter heredoc quoting the phrase as prose (#5672)" \
    "$GH_5672_UNQUOTED_BODY_CMD"

# The `<<-` (dash) tab-stripping unquoted-delimiter variant must get the same
# treatment.
GH_5672_UNQUOTED_BODY_TABSTRIP_CMD='gh pr comment 5333 --body "$(cat <<-EOF3
	Please avoid: '"$PHRASE_CMD"' 123 --squash
	EOF3
)"'
assert_allow "Allow gh pr comment --body unquoted <<- heredoc quoting the phrase as prose (#5672)" \
    "$GH_5672_UNQUOTED_BODY_TABSTRIP_CMD"

# Regression guard: a REAL gh pr merge invocation must still deny (no change
# in behavior for genuine misuse).
assert_deny "Still block a real gh pr merge invocation after the #5672 fix" \
    "gh pr merge 5333 --squash"

# Regression guard: the relaxation is content-gated, not delimiter-gated --
# an unquoted-delimiter body that ACTUALLY contains a live '$(...)' command
# substitution must stay fully visible and still deny, even though it is
# captured into --body. This is what proves the fix cannot be used to smuggle
# a real invocation through a forged "prose" body.
GH_5672_LIVE_EXPANSION_CMD='gh pr comment 5333 --body "$(cat <<EOF4
Please avoid this: $('"$PHRASE_CMD"' 999)
EOF4
)"'
assert_deny "Still block an unquoted-delimiter --body heredoc whose body contains a live \$(...) (#5672)" \
    "$GH_5672_LIVE_EXPANSION_CMD"

# Regression guard: a backtick-based live command substitution in the same
# shape must also still deny.
GH_5672_LIVE_BACKTICK_CMD='gh pr comment 5333 --body "$(cat <<EOF5
Please avoid this: `'"$PHRASE_CMD"' 999`
EOF5
)"'
assert_deny "Still block an unquoted-delimiter --body heredoc whose body contains a live backtick substitution (#5672)" \
    "$GH_5672_LIVE_BACKTICK_CMD"

# Regression guard (control, #5328): the relaxation is scoped to the
# `is_cat_word` (flag-captured `cat`) branch only -- `git commit -F -`/
# `--file=-` with an UNQUOTED delimiter must keep denying exactly as before,
# even though this body is equally inert prose. That branch has no
# capre-style capture proof to fall back on, so it is deliberately excluded.
GH_5672_COMMIT_STDIN_UNQUOTED_STILL_DENIES_CMD='git commit -F - <<EOF6
Document the rule: never '"$PHRASE_CMD"' directly, use merge-pr.sh instead.
EOF6'
assert_deny "Still block git commit -F - with an UNQUOTED delimiter, unaffected by the #5672 cat-only relaxation" \
    "$GH_5672_COMMIT_STDIN_UNQUOTED_STILL_DENIES_CMD"

# Regression guard: an unquoted-delimiter cat-heredoc piped into a shell must
# still deny -- the capre confinement check (already required by #5109/#5122)
# still gates this branch first, so this shape was never reachable by the
# new relaxation in the first place.
GH_5672_PIPE_BASH_CMD='cat <<EOF7 | bash
'"$PHRASE_CMD"' 123
EOF7'
assert_deny "Still block an unquoted-delimiter cat-heredoc piped into bash (#5672)" \
    "$GH_5672_PIPE_BASH_CMD"

echo ""

# =========================================================================
echo -e "${YELLOW}--- pip install -e WORKTREE GUARD (issue #2495) ---${NC}"
# =========================================================================

# Should DENY editable installs when LOOM_WORKTREE_PATH is set
assert_deny_in_worktree "Block pip install -e in worktree" \
    "pip install -e ."

assert_deny_in_worktree "Block pip install -e ./loom-tools in worktree" \
    "pip install -e ./loom-tools"

assert_deny_in_worktree "Block pip3 install -e in worktree" \
    "pip3 install -e ."

assert_deny_in_worktree "Block pip install --editable in worktree" \
    "pip install --editable ."

assert_deny_in_worktree "Block uv pip install -e in worktree" \
    "uv pip install -e ./loom-tools"

assert_deny_in_worktree "Block pip install -e with absolute path in worktree" \
    "pip install -e /Users/dev/project/loom-tools"

# The deny message must reference issue #2495.
TOTAL=$((TOTAL + 1))
_wt_out=$(run_guard_in_worktree "pip install -e ." "$REPO_ROOT") || true
if echo "$_wt_out" | jq -r '.hookSpecificOutput.permissionDecisionReason // empty' 2>/dev/null | grep -q "2495"; then
    PASS=$((PASS + 1))
    echo -e "  ${GREEN}PASS${NC}: pip install -e deny reason references issue #2495"
else
    FAIL=$((FAIL + 1))
    echo -e "  ${RED}FAIL${NC}: pip install -e deny reason references issue #2495"
    echo -e "       Got: $_wt_out"
fi

# Should ALLOW non-editable pip installs in worktree
assert_allow_in_worktree "Allow pip install (non-editable) in worktree" \
    "pip install pytest"

assert_allow_in_worktree "Allow pip install -r requirements.txt in worktree" \
    "pip install -r requirements.txt"

# Should ALLOW editable installs OUTSIDE worktrees (no LOOM_WORKTREE_PATH)
assert_allow "Allow pip install -e outside worktree" \
    "pip install -e ."

assert_allow "Allow pip3 install -e ./loom-tools outside worktree" \
    "pip3 install -e ./loom-tools"

assert_allow "Allow uv pip install -e outside worktree" \
    "uv pip install -e ."

assert_allow "Allow pip install --editable outside worktree" \
    "pip install --editable ."

echo ""

# =========================================================================
echo -e "${YELLOW}--- Unrelated commands pass through (allow) ---${NC}"
# =========================================================================

assert_allow "Allow git status" \
    "git status"

assert_allow "Allow gh pr create" \
    "gh pr create --title 'My PR' --body 'Description'"

assert_allow "Allow gh pr list" \
    "gh pr list"

# Catastrophic/generic patterns are NOT this hook's job (guard-destructive.sh
# owns them); this hook must allow them through.
assert_allow "Allow rm -rf / (not this hook's responsibility)" \
    "rm -rf /"

echo ""

# =========================================================================
echo -e "${YELLOW}--- Hook schema contract ---${NC}"
# =========================================================================

# Deny decisions must carry hookEventName: PreToolUse (#3550).
TOTAL=$((TOTAL + 1))
_schema_out=$(run_guard "gh pr merge 123" "$REPO_ROOT") || true
if echo "$_schema_out" | jq -e '.hookSpecificOutput.hookEventName == "PreToolUse"' >/dev/null 2>&1; then
    PASS=$((PASS + 1))
    echo -e "  ${GREEN}PASS${NC}: deny decision carries hookEventName == PreToolUse"
else
    FAIL=$((FAIL + 1))
    echo -e "  ${RED}FAIL${NC}: deny decision carries hookEventName == PreToolUse"
    echo -e "       Got: $_schema_out"
fi

# Never exits non-zero, even on empty command input.
TOTAL=$((TOTAL + 1))
_empty_exit=0
echo '{"tool_input":{"command":""},"cwd":"'"$REPO_ROOT"'"}' | "$GUARD" >/dev/null 2>&1 || _empty_exit=$?
if [[ $_empty_exit -eq 0 ]]; then
    PASS=$((PASS + 1))
    echo -e "  ${GREEN}PASS${NC}: empty command exits 0 (allow)"
else
    FAIL=$((FAIL + 1))
    echo -e "  ${RED}FAIL${NC}: empty command exits 0 (allow), got exit $_empty_exit"
fi

echo ""

# =========================================================================
echo -e "${YELLOW}--- Decision telemetry log (#3898) ---${NC}"
# =========================================================================
#
# guard-loom-workflow.sh appends one JSONL record per DENY to the SAME decision
# log guard-destructive.sh writes (.loom/logs/guard-decisions.log by default),
# gated by guards.decisionLog / LOOM_GUARD_DECISION_LOG (default OFF). The record
# schema is the STABLE contract shared with guard-destructive.sh:
#   {"ts","decision":"deny","pattern":"<tag>","tier":"catastrophic","command"}.
# The LOOM_GUARD_DECISION_LOG_FILE test seam overrides the write path.

DLW_DIR="$(mktemp -d)"
DLW_LOG="$DLW_DIR/guard-decisions.log"

dlw_assert() {
    TOTAL=$((TOTAL + 1))
    if [[ "$2" -eq 0 ]]; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $1"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $1"
        [[ -n "${3:-}" ]] && echo -e "       ${3}"
    fi
}

# (a) gh pr merge deny writes decision=deny, tier=catastrophic, stable tag.
rm -f "$DLW_LOG"
make_input "gh pr merge 123" "$REPO_ROOT" | \
    env LOOM_GUARD_DECISION_LOG=1 LOOM_GUARD_DECISION_LOG_FILE="$DLW_LOG" "$GUARD" >/dev/null 2>&1 || true
_dlw_rec="$(tail -1 "$DLW_LOG" 2>/dev/null)"
if [[ -f "$DLW_LOG" ]] && \
   [[ "$(printf '%s' "$_dlw_rec" | jq -r '.decision' 2>/dev/null)" == "deny" ]] && \
   [[ "$(printf '%s' "$_dlw_rec" | jq -r '.tier' 2>/dev/null)" == "catastrophic" ]] && \
   [[ "$(printf '%s' "$_dlw_rec" | jq -r '.pattern' 2>/dev/null)" == "loom:gh-pr-merge-redirect" ]] && \
   [[ -n "$(printf '%s' "$_dlw_rec" | jq -r '.ts' 2>/dev/null)" ]] && \
   [[ -n "$(printf '%s' "$_dlw_rec" | jq -r '.command' 2>/dev/null)" ]]; then
    dlw_assert "gh pr merge deny logs a JSONL record (tag=loom:gh-pr-merge-redirect)" 0
else
    dlw_assert "gh pr merge deny logs a JSONL record (tag=loom:gh-pr-merge-redirect)" 1 "record: ${_dlw_rec:-<none>}"
fi

# (b) pip install -e worktree deny writes its own stable tag.
rm -f "$DLW_LOG"
LOOM_WORKTREE_PATH="$REPO_ROOT" make_input "pip install -e ." "$REPO_ROOT" | \
    env LOOM_WORKTREE_PATH="$REPO_ROOT" LOOM_GUARD_DECISION_LOG=1 LOOM_GUARD_DECISION_LOG_FILE="$DLW_LOG" "$GUARD" >/dev/null 2>&1 || true
_dlw_rec="$(tail -1 "$DLW_LOG" 2>/dev/null)"
if [[ -f "$DLW_LOG" ]] && \
   [[ "$(printf '%s' "$_dlw_rec" | jq -r '.pattern' 2>/dev/null)" == "loom:pip-install-editable-worktree" ]]; then
    dlw_assert "pip install -e worktree deny logs tag=loom:pip-install-editable-worktree" 0
else
    dlw_assert "pip install -e worktree deny logs tag=loom:pip-install-editable-worktree" 1 "record: ${_dlw_rec:-<none>}"
fi

# (c) Toggle default OFF (no env, non-repo cwd so config can't flip it on):
# no decision record is written even though the command denies.
_dlw_norepo="$(mktemp -d)"
rm -f "$DLW_LOG"
make_input "gh pr merge 123" "$_dlw_norepo" | \
    env LOOM_GUARD_DECISION_LOG_FILE="$DLW_LOG" "$GUARD" >/dev/null 2>&1 || true
if [[ ! -f "$DLW_LOG" ]]; then
    dlw_assert "toggle default OFF: deny writes NO decision record" 0
else
    dlw_assert "toggle default OFF: deny writes NO decision record" 1 "unexpected: $(cat "$DLW_LOG")"
fi
rm -rf "$_dlw_norepo"

# (d) An allow-only command writes NO record even with the toggle on.
rm -f "$DLW_LOG"
make_input "git status" "$REPO_ROOT" | \
    env LOOM_GUARD_DECISION_LOG=1 LOOM_GUARD_DECISION_LOG_FILE="$DLW_LOG" "$GUARD" >/dev/null 2>&1 || true
if [[ ! -f "$DLW_LOG" ]] || [[ "$(wc -l < "$DLW_LOG" 2>/dev/null || echo 0)" -eq 0 ]]; then
    dlw_assert "allow-only command writes NO decision record (toggle on)" 0
else
    dlw_assert "allow-only command writes NO decision record (toggle on)" 1 "unexpected: $(cat "$DLW_LOG")"
fi

# (e) Fail-open: an unwritable decision-log path never changes the deny and never
# causes a non-zero exit.
_dlw_out=""; _dlw_rc=0
_dlw_out="$(make_input "gh pr merge 123" "$REPO_ROOT" | \
    env LOOM_GUARD_DECISION_LOG=1 LOOM_GUARD_DECISION_LOG_FILE="/nonexistent-dir-3898/a/b/decisions.log" "$GUARD" 2>/dev/null)" || _dlw_rc=$?
if [[ "$_dlw_rc" -eq 0 ]] && \
   [[ "$(printf '%s' "$_dlw_out" | jq -r '.hookSpecificOutput.permissionDecision' 2>/dev/null)" == "deny" ]]; then
    dlw_assert "fail-open: unwritable decision log still denies and exits 0" 0
else
    dlw_assert "fail-open: unwritable decision log still denies and exits 0" 1 "rc=$_dlw_rc out=$_dlw_out"
fi

[[ -n "$DLW_DIR" && "$DLW_DIR" != "/" && -d "$DLW_DIR" ]] && rm -rf "$DLW_DIR"

echo ""

# =========================================================================
# Summary
# =========================================================================

echo "========================================="
echo -e "  Total:  $TOTAL"
echo -e "  ${GREEN}Passed${NC}: $PASS"
echo -e "  ${RED}Failed${NC}: $FAIL"
echo "========================================="

if [[ $FAIL -gt 0 ]]; then
    echo -e "\n${RED}TESTS FAILED${NC}"
    exit 1
else
    echo -e "\n${GREEN}ALL TESTS PASSED${NC}"
    exit 0
fi
