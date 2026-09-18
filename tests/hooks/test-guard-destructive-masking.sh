#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — masking.
#
# One slice of the former monolithic tests/hooks/test-guard-destructive.sh,
# split per #7741. Shared fixtures, assertions and catastrophic-phrase payloads
# live in tests/hooks/lib/guard-destructive-harness.sh.
#
# Usage: ./tests/hooks/test-guard-destructive-masking.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

echo -e "${YELLOW}--- ASK-tier heredoc-body masking for force-op / stash-scope (#5779) ---${NC}"
# =========================================================================
#
# COMMAND_ASK_SCAN (which parse_force_ops()'s force-op:detached/force-op:protected
# and the stash-scope:* checks both read) never had heredoc-body masking applied
# to it, unlike the catastrophic-tier gh-api-rawfield-body-literal-at check
# (#5181/#5198, tested above at line ~629). So a SINGLE-QUOTED heredoc body that
# merely QUOTES a force-op/stash phrase as inert prose (e.g. a report destined
# for a file, discussing the anti-pattern) tripped an ask exactly like a live
# invocation would -- an unanswerable stall in a headless run. Fixed by reusing
# the same tested mask_heredoc_bodies_selective() primitive to build
# COMMAND_ASK_SCAN, gated on literal '<<' presence.

# --- False positive fixed: a heredoc body destined for a plain file sink (not
# an interpreter) that merely quotes a force-op phrase stays allowed ----------
assert_allow "#5779: Allow a single-quoted heredoc body that merely QUOTES 'git reset --hard' as inert prose" \
    'cat > /tmp/report-5779-a.md <<'"'"'EOF'"'"'
Documentation example -- do NOT actually run this:
git reset --hard origin/main
EOF
echo done'

assert_allow "#5779: Allow a single-quoted heredoc body that merely QUOTES 'git push --force' (non-main) as inert prose" \
    'cat > /tmp/report-5779-b.md <<'"'"'EOF'"'"'
Documentation example -- do NOT actually run this:
git push --force origin feature/my-branch
EOF
echo done'

# --- stash-scope companion (#5754 follow-up, same root cause) ---------------
ST5779_REPO=$(make_wt_repo_linked)
assert_allow "#5779: Allow a single-quoted heredoc body that merely QUOTES 'git stash pop' as inert prose (main checkout)" \
    'cat > /tmp/report-5779-c.md <<'"'"'EOF'"'"'
Documentation example -- do NOT actually run this:
git stash pop
EOF
echo done' "$ST5779_REPO"

# --- Narrows, never widens: a REAL (non-heredoc) invocation must keep asking,
# both standalone and sitting in the same multi-line command as an unrelated
# heredoc (mirrors the #5181 "narrows, never widens" test at line ~645) ------
assert_ask "#5779: A live (non-heredoc) git reset --hard invocation still asks (regression guard)" \
    "git reset --hard HEAD~1"

assert_ask "#5779: A live (non-heredoc) git stash pop invocation still asks in main checkout (regression guard)" \
    "git stash pop" "$ST5779_REPO"

assert_ask "#5779: A real force-op invocation AFTER an unrelated heredoc in the same command still asks" \
    'cat > /tmp/report-5779-d.md <<'"'"'EOF'"'"'
just some unrelated prose
EOF
git reset --hard HEAD~1'

assert_ask "#5779: A real stash-pop invocation AFTER an unrelated heredoc in the same command still asks" \
    'cat > /tmp/report-5779-e.md <<'"'"'EOF'"'"'
just some unrelated prose
EOF
git stash pop' "$ST5779_REPO"

# --- Interpreter-fed heredoc: a force-op/stash phrase piped into a real
# interpreter is genuinely LIVE code and must still ask (mirrors the #5198
# interpreter-fed-heredoc tests at line ~666) --------------------------------
assert_ask "#5779: A live git reset --hard wrapped in 'bash <<EOF ... EOF' still asks (interpreter-fed heredoc)" \
    'bash <<'"'"'EOF'"'"'
git reset --hard HEAD~1
EOF'

assert_ask "#5779: A live git stash pop wrapped in 'bash <<EOF ... EOF' still asks (interpreter-fed heredoc)" \
    'bash <<'"'"'EOF'"'"'
git stash pop
EOF' "$ST5779_REPO"

# --- Unquoted delimiter + command substitution: the outer shell evaluates
# $(...)/backticks inside an UNQUOTED heredoc body while constructing it --
# genuinely live code -- even when the block's sink is an inert command like
# `cat`. heredoc_delim_at() must distinguish a quoted delimiter (<<'EOF',
# masked, inert) from a bare one (<<EOF / <<-EOF, left visible), or this
# reopens the exact class of bypass #5779 closed, just via $(...) instead of
# prose (security regression found in review of PR #5781, fixed by gating
# mask_heredoc_bodies_selective() on HEREDOC_DELIM_QUOTED).
#
# Both patterns below are the regex/substring ASK_PATTERNS + stash-scope
# scans (which read COMMAND_ASK_SCAN as plain text and so are directly what
# this masking fix protects); force-op patterns like `git reset --hard` are
# deliberately NOT used here -- those are recognized by parse_force_ops'
# command-word SEGMENT tokenizer, which requires the segment's first token to
# be exactly `git` and so never matches a `$(git ...)` prefix regardless of
# masking (a separate, pre-existing tokenizer limitation, out of scope for
# this heredoc-masking fix). ---------------------------------------------
assert_ask "#5781: An unquoted <<EOF heredoc body containing a live \$( git clean -fd) substitution still asks" \
    'cat > /tmp/report-5781-a.md <<EOF
$( git clean -fd)
EOF'

assert_ask "#5781: An unquoted <<-EOF heredoc body containing a live \$( git clean -fd) substitution still asks" \
    'cat > /tmp/report-5781-b.md <<-EOF
$( git clean -fd)
EOF'

assert_ask "#5781: An unquoted <<EOF heredoc body containing a live \$(git stash pop ) substitution still asks (main checkout)" \
    'cat > /tmp/report-5781-c.md <<EOF
$(git stash pop )
EOF' "$ST5779_REPO"

assert_ask "#5781: An unquoted <<-EOF heredoc body containing a live \$(git stash pop ) substitution still asks (main checkout)" \
    'cat > /tmp/report-5781-d.md <<-EOF
$(git stash pop )
EOF' "$ST5779_REPO"

echo ""

# =========================================================================
echo -e "${YELLOW}--- UNQUOTED-delimiter cat-heredoc body masking (#6056) ---${NC}"
# =========================================================================
#
# #5779/#5781 left every UNQUOTED-delimiter heredoc body (`cat <<EOF`) visible
# to COMMAND_ASK_SCAN, because the outer shell expands $(...)/backticks inside
# such a body. Correct as a default, but too strict for the routine Judge idiom
#   gh pr comment N --body "$(cat <<EOF ... EOF)"
# whose prose merely QUOTES a force-op as coaching for a human reviewer: both
# occurrences logged in #6056 were "Changes Requested - Merge Conflict" comments
# that force-op:protected asked on, stalling a headless run with nobody to
# answer. mask_unquoted_cat_heredoc_bodies() masks that body ONLY when the cat
# capture is confined to a text-data flag value AND the body is proven free of
# `$(` / unescaped-backtick expansion (a bare $VAR parameter expansion is text,
# not execution, so it does NOT disqualify -- both real occurrences carried a
# `sha=$VERDICT_SHA` trailer).

# --- False positive fixed (the #6056 reproduction) --------------------------
assert_allow "#6056: Allow gh pr comment --body unquoted-delimiter heredoc quoting a force-op as prose" \
    'gh pr comment 6056 --body "$(cat <<EOF
Changes Requested - Merge Conflict

Please rebase and force-push:
git reset --hard origin/main
EOF
)"'

# Exact real-world shape: markdown fenced code block (escaped backticks) plus a
# $VERDICT_SHA parameter expansion in the trailer. Both logged occurrences
# carried exactly these two features, so a "no $ and no backtick at all" rule
# (as used by the guard-loom-workflow.sh sibling fix) would not have fixed them.
assert_allow "#6056: Allow the real Judge merge-conflict comment shape (escaped fences + \$VAR trailer)" \
    'VERDICT_SHA="aa2c1b0"
gh pr comment 6056 --body "$(cat <<EOF
Please rebase and resolve:
\`\`\`bash
git rebase origin/main
git reset --hard origin/main
\`\`\`

<!-- loom:verdict-sha sha=$VERDICT_SHA verdict=changes-requested -->
EOF
)" && gh pr edit 6056 --add-label "loom:changes-requested"'

# The `<<-` tab-stripping unquoted variant gets the same treatment.
assert_allow "#6056: Allow the unquoted <<- tab-stripping variant of the same shape" \
    'gh pr comment 6056 --body "$(cat <<-EOF
Please avoid running this:
git reset --hard origin/main
EOF
)"'

# `gh api -f body=` field syntax is in the same confinement allowlist.
assert_allow "#6056: Allow gh api -f body= unquoted-delimiter heredoc quoting a force-op as prose" \
    'gh api repos/o/r/issues/1/comments -f body="$(cat <<EOF
Do not run this here:
git reset --hard origin/main
EOF
)"'

# Escaped backticks are literal text and do NOT disqualify the body, so a
# markdown inline-code span quoting an ask-phrase is masked like any prose.
assert_allow "#6056: Allow a body whose only backticks are backslash-ESCAPED (markdown inline code)" \
    'gh pr comment 6056 --body "$(cat <<EOF
Do not run \`git clean -fd\` in prose:
git reset --hard origin/main
EOF
)"'

# --- Narrows, never widens: content-gated, not delimiter-gated --------------
# A body that ACTUALLY contains a live $(...) command substitution stays fully
# visible and still asks, even though it is captured into --body. This is what
# proves the relaxation cannot smuggle a real invocation through a "prose" body.
# (These use ASK_PATTERNS phrases rather than a force-op: parse_force_ops
# requires a segment whose FIRST token is `git`, so it never matches a
# `$(git ...)` prefix regardless of masking -- the same tokenizer limitation
# the #5781 tests above call out.)
assert_ask "#6056: An unquoted --body heredoc whose body contains a live \$( git clean -fd) still asks" \
    'gh pr comment 6056 --body "$(cat <<EOF
prose $( git clean -fd) more
EOF
)"'

assert_ask "#6056: An unquoted --body heredoc whose body contains a live backtick substitution still asks" \
    'gh pr comment 6056 --body "$(cat <<EOF
prose `git clean -fd` more
EOF
)"'

# An ESCAPED backslash does not swallow the backtick that follows it, so this
# backtick is live and the body must stay visible.
assert_ask "#6056: A backtick preceded by an ESCAPED backslash is live and still asks" \
    'gh pr comment 6056 --body "$(cat <<EOF
ends with a backslash \\`git clean -fd` more
EOF
)"'

# --- Confinement proof is required: unconfined unquoted heredocs unchanged --
assert_ask "#6056: An unquoted cat-heredoc piped into bash still asks (no text-data-flag capture)" \
    'cat <<EOF | bash
git reset --hard origin/main
EOF'

assert_ask "#6056: An unquoted cat-heredoc redirected to a file still asks (no text-data-flag capture)" \
    'cat > /tmp/report-6056-a.md <<EOF
git reset --hard origin/main
EOF'

assert_ask "#6056: An unquoted heredoc captured by eval (not a text-data flag) still asks" \
    'eval "$(cat <<EOF
git reset --hard origin/main
EOF
)"'

# --- Regression guards: real invocations keep asking ------------------------
assert_ask "#6056: A live (non-heredoc) git reset --hard origin/main still asks" \
    "git reset --hard origin/main"

assert_ask "#6056: A real force-op AFTER a masked --body heredoc in the same command still asks" \
    'gh pr comment 6056 --body "$(cat <<EOF
just some unrelated prose
EOF
)" && git reset --hard origin/main'

echo ""

# =========================================================================
echo -e "${YELLOW}--- #7355: printenv.*SECRET/TOKEN/KEY ask-tier false positives ---${NC}"
# =========================================================================
#
# STATUS (#7795): the substring BACKSTOP this section was written for
# (PRINTENV_ASK_PATTERNS + its dedicated COMMAND_ASK_SCAN_PRINTENV scan copy)
# was RETIRED by the ask-tier sizing pass — 11 hits in 30 days of
# `.loom/logs/guard-decisions.log`, 11 of them false positives and none a live
# invocation. The precise, segment-parsed `printenv_ask_reason()` check
# (#6245) survives and still asks on every real `printenv <CREDENTIAL>`
# invocation, so the "must still ask" assertions below are unchanged and are
# now the load-bearing coverage for this class. The retired backstop's own
# unique shape (the phrase quoted in a variable that is later read) is
# asserted as an ALLOW below, with the tier rationale inline.
#
# The historical narrative below is kept because it explains WHY the copies it
# describes existed; the copies themselves are gone.
#
# Guard-decision telemetry (#3898) found the printenv.*(SECRET|TOKEN|KEY)
# ASK_PATTERNS entries false-asking on two DISTINCT non-live shapes:
#
#   1. A heredoc captured via a PLAIN VARIABLE ASSIGNMENT (unquoted delimiter,
#      `REPORT=$(cat <<EOF ... EOF)`) was not covered by
#      mask_unquoted_cat_heredoc_bodies()'s capre allowlist -- that allowlist
#      only recognized capture into a fixed set of text-data FLAGS
#      (-m/--body/--title/etc.), never a bare shell variable. Fixed by
#      widening capre to also admit a `NAME=` capture immediately before the
#      `$(`/backtick opener.
#   2. A jq/grep/rg command whose own QUOTED filter/pattern argument merely
#      CONTAINS the word "printenv" (with SECRET/TOKEN/KEY elsewhere in the
#      same argument) false-asked because COMMAND_ASK_SCAN deliberately never
#      gets grep/rg/jq positional-argument masking -- it also feeds
#      SQL_DDL_PATTERN below, which intentionally still scans a
#      `grep '<pattern>' file` argument for a live DDL phrase, so masking
#      COMMAND_ASK_SCAN itself would silently blind that check. Fixed by
#      giving the three printenv patterns their OWN further-masked scan copy
#      (COMMAND_ASK_SCAN_PRINTENV), mirroring how COMMAND_CLOUD_ASK_SCAN is
#      already branched off COMMAND_ASK_SCAN for the toggleable cloud-ask
#      tier (#6002) -- never fed back into COMMAND_ASK_SCAN itself, so
#      SQL_DDL_PATTERN keeps reading the fully unmasked copy.
#
# Note: a BARE `grep '<pattern>' file` / `jq '<filter>' file` (no pipe, no
# other shell metacharacter) is unconditionally admitted by the read-only
# fast path (fastpath_builtin_admits()) BEFORE any ASK_PATTERNS scan ever
# runs, regardless of this fix -- see the #5263 fast-path section above. The
# false positives here reproduce with a REAL jq filter's own internal `|`
# (jq's pipe operator, which fastpath_structural_ok() rejects unconditionally
# and not quote-aware) or a genuinely piped/chained grep/rg, matching the
# actual occurrence shapes logged in #3898 -- not the artificially-bare form
# that was already fast-pathed before this fix and is untouched by it.

# --- False positive #1 fixed: heredoc body via plain variable assignment ---
assert_allow "#7355: Allow unquoted-delimiter heredoc captured by a PLAIN VARIABLE assignment quoting 'printenv SECRET' as past-fix prose" \
    'REPORT=$(cat <<EOF
Past fix: this issue was previously resolved by running printenv SECRET_KEY to check for leaks.
EOF
)
echo "$REPORT"'

assert_allow "#7355: Allow the same plain-variable-assignment heredoc shape mentioning printenv TOKEN" \
    'REPORT=$(cat <<EOF
The regression test replays a printenv TOKEN invocation from the incident log.
EOF
)
echo "$REPORT"'

assert_allow "#7355: Allow the same plain-variable-assignment heredoc shape mentioning printenv KEY" \
    'REPORT=$(cat <<EOF
Root cause: an old script ran printenv KEY_NAME directly in CI.
EOF
)
echo "$REPORT"'

# --- Regression check: quoted-delimiter heredoc (already masked before this
#     fix, via mask_heredoc_bodies_selective()) stays unaffected -----------
assert_allow "#7355 regression: a QUOTED-delimiter heredoc plain-variable-assignment capturing the same prose was already masked before this fix" \
    'REPORT=$(cat <<'"'"'EOF'"'"'
Past fix: this issue was previously resolved by running printenv SECRET_KEY to check for leaks.
EOF
)
echo "$REPORT"'

# --- False positive #2 fixed: jq/grep/rg quoted argument merely mentions
#     "printenv" (plus SECRET/TOKEN/KEY elsewhere in the same argument) ----
assert_allow "#7355: Allow a jq filter (internal pipe disqualifies the read-only fast path) whose quoted string argument mentions 'printenv TOKEN_VALUE' as data, no live printenv call" \
    "jq -c 'select(.title | contains(\"ran printenv TOKEN_VALUE previously\"))' guard-decisions.log"

assert_allow "#7355: Allow a chained (non-fast-pathed) grep whose quoted pattern mentions 'printenv SECRET_KEY' as prose, no live printenv call" \
    "grep -n 'run printenv SECRET_KEY to check' guard-decisions.log | grep foo | head"

assert_allow "#7355: Allow a chained (non-fast-pathed) rg whose quoted pattern mentions 'printenv KEY_NAME' as prose, no live printenv call" \
    "rg 'via printenv KEY_NAME earlier' guard-decisions.log | grep foo | head"

# --- Regression guard: SQL_DDL_PATTERN (also fed by COMMAND_ASK_SCAN) must
#     still fire, UNCHANGED, on a real grep '<DDL phrase>' file -- proving
#     this fix did NOT widen positional masking onto COMMAND_ASK_SCAN itself
#     (which would silently blind this check, the exact regression the
#     COMMAND_ASK_SCAN_PRINTENV branch-off is designed to avoid) -----------
assert_deny "#7355 regression: a real DDL grep (declines the fast path via a second pipe) still denies via SQL_DDL_PATTERN, unaffected by the printenv masking fix" \
    "grep '$_HD_DDL' schema.sql | grep foo | head"
assert_deny_env "#7355 regression: a real DDL grep with the read-only fast path disabled still denies via SQL_DDL_PATTERN" \
    "LOOM_GUARD_READONLY_FASTPATH=0" "grep '$_HD_DDL' schema.sql"

# --- Regression guard: a genuine LIVE printenv invocation still asks -------
assert_ask "#7355 regression: a live (non-heredoc, non-quoted-argument) 'printenv SECRET_KEY' invocation still asks" \
    "printenv SECRET_KEY"
assert_ask "#7355 regression: a live 'printenv' invocation of a TOKEN-named variable still asks" \
    "printenv MY_TOKEN"
assert_ask "#7355 regression: a live 'printenv' invocation of a KEY-named variable still asks" \
    "printenv API_KEY"

# --- TIER CHANGE (#7795): the "phrase quoted in a variable that is read
#     later" shape is no longer gated -------------------------------------
#
# History: #6207 made this shape fail CLOSED. mask_catastrophic_var_assignment()'s
# call site feeding COMMAND_ASK_SCAN_PRINTENV was not passing the true original
# $COMMAND as its second argument, so the "is $NAME read elsewhere in the
# command" branch could never fire for the printenv backstop, and a
# printenv-secret assignment that IS live-read via eval was masked and silently
# allowed. #6207 fixed the argument; #7795 retired the backstop the fix served.
#
# Why that is the right call rather than a hole:
#   * The heuristic cannot distinguish `eval "$NOTE"` (executes) from
#     `printf '%s' "$NOTE"` (does not). Every one of the 11 logged hits in
#     2026-08-18..09-16 was the SECOND shape — Guide/Champion digest builders
#     whose variables quote issue TITLES containing the phrase (#6245's own
#     title is one of them) and print them back out.
#   * The gated operation is a credential READ, not a destruction: the worst
#     case is a secret in a local transcript, recoverable by rotation.
#   * It was never a boundary — `echo $GITHUB_TOKEN`, `env | grep TOKEN` and
#     any interpreter one-liner print the same value and were never scanned.
#   * Headless (this fleet's primary mode) the ask protected nothing; it
#     stalled the role tick that tripped it.
# A LIVE `printenv <CREDENTIAL>` invocation still asks — see the assertions
# immediately above, which are unchanged.
assert_allow "#7795: a NOTE var quoting 'printenv SECRET_KEY' read via eval no longer asks (backstop retired; 11/11 logged hits were inert prose)" \
    'NOTE="ran printenv SECRET_KEY earlier"
eval "$NOTE"'
assert_allow "#7795: a NOTE var quoting a printenv TOKEN phrase read via eval no longer asks (backstop retired)" \
    'NOTE="ran printenv MY_TOKEN earlier"
eval "$NOTE"'

# --- #7795 regression: the exact FALSE-POSITIVE shape that convicted the
#     backstop — a digest builder whose variable quotes an ISSUE TITLE
#     containing the phrase, printed back out (never eval'd) ---------------
assert_allow "#7795: a digest variable quoting an issue title that mentions 'printenv SECRET/TOKEN/KEY', printed back out, no longer asks (the 11/11 logged false-positive shape)" \
    'HELD="- **#6290**: fix: name-allowlist printenv SECRET/TOKEN/KEY ask pattern to stop LOOM_TOKEN_NAME false positive"
printf "%s\n" "$HELD"'

echo ""

# =========================================================================
echo -e "${YELLOW}--- #7970: NAME= heredoc-capture masking needs a \$NAME/\${NAME} read check ---${NC}"
# =========================================================================
#
# #7355 widened mask_unquoted_cat_heredoc_bodies()'s capre allowlist to admit
# a plain shell-variable-assignment capture (`R=$(cat <<EOF ... EOF)`) on the
# reasoning that a capture into a variable is as confined as a capture into a
# flag value. True of the capture itself, but unlike a flag value the
# VARIABLE can be re-read later in the SAME command and RE-PARSED as shell
# code (`eval "$R"`, `sh -c "$R"`, `"$R" | bash`) -- and nothing checked for
# that. This closes exactly that gap: when condition 2 matched via the NAME=
# alternative, _heredoc_var_reparsed() now fails closed against the TRUE
# original command whenever the variable is fed to one of those re-parsing
# consumers. Deliberately narrower than mask_catastrophic_var_assignment()'s
# own blanket "$NAME appears anywhere" check -- see that function's mirror,
# _heredoc_var_reparsed(), for why: a blanket check would re-open the #7355
# false positive the tests directly above this section exist to lock in
# (every one of them reads the captured variable via `echo`/`printf`, never
# via a re-parsing consumer).

ST7970_PHRASE='git reset --hard origin/main'

# --- Genuinely dead capture (never read at all) stays masked/allowed -------
assert_allow "#7970: a NAME= heredoc capture that is NEVER read afterward stays masked (allow)" \
    "R=\$(cat <<EOF
$ST7970_PHRASE
EOF
)
echo done"

# --- Display-only read (the #7355 shape) stays masked/allowed --------------
assert_allow "#7970: a NAME= heredoc capture read only via 'echo \"\$NAME\"' (display, not re-parsed) stays masked (allow) -- must not regress #7355" \
    "R=\$(cat <<EOF
$ST7970_PHRASE
EOF
)
echo \"\$R\""

assert_allow "#7970: same shape read via 'printf' (display, not re-parsed) stays masked (allow)" \
    "R=\$(cat <<EOF
$ST7970_PHRASE
EOF
)
printf '%s\n' \"\$R\""

# --- The gap itself: a capture later RE-PARSED as shell code must NOT be
#     masked -- COMMAND_ASK_SCAN needs to see the phrase and ask ------------
assert_ask "#7970: a NAME= heredoc capture later fed to 'eval \"\$NAME\"' is NOT masked -- still asks (the reported gap)" \
    "R=\$(cat <<EOF
$ST7970_PHRASE
EOF
)
eval \"\$R\""

assert_ask "#7970: a NAME= heredoc capture later fed to 'sh -c \"\$NAME\"' is NOT masked -- still asks" \
    "R=\$(cat <<EOF
$ST7970_PHRASE
EOF
)
sh -c \"\$R\""

assert_ask "#7970: a NAME= heredoc capture later piped to bash ('\$NAME | bash') is NOT masked -- still asks" \
    "R=\$(cat <<EOF
$ST7970_PHRASE
EOF
)
echo \"\$R\" | bash"

assert_ask "#7970: the \${NAME} brace form fed to 'eval' is NOT masked -- still asks" \
    "R=\$(cat <<EOF
$ST7970_PHRASE
EOF
)
eval \"\${R}\""

# --- Catastrophic tier is untouched: mask_unquoted_cat_heredoc_bodies() only
#     feeds COMMAND_ASK_SCAN, never the catastrophic-tier scan, so a
#     catastrophic phrase in the SAME shape denies regardless of this fix ---
assert_deny "#7970: the equivalent catastrophic-tier phrase (aws s3 rb) fed to 'eval \"\$NAME\"' still denies, unaffected by the ask-tier fix" \
    "R=\$(cat <<EOF
${_S3RB_CAT}
EOF
)
eval \"\$R\""

# --- Flag-value branch (#6056's original shape) is untouched: a flag value
#     can never be eval'd, so it needs no read check and stays masked -------
assert_allow "#7970: the sibling flag-value capture ('gh pr comment --body \"\$(cat <<EOF...)\"') is UNCHANGED -- no read check applies, stays masked" \
    "gh pr comment 123 --body \"\$(cat <<EOF
$ST7970_PHRASE
EOF
)\""

echo ""

# =========================================================================
echo -e "${YELLOW}--- #7970 x #8003: unquoted-heredoc expansion x NAME= capture read check ---${NC}"
# =========================================================================
#
# #8003 (merged as 5b62d478) reworked how an UNQUOTED heredoc delimiter is
# handled: the shell expands such a body BEFORE the sink reads a byte of it,
# and a BACKSLASH is the only suppressor that survives inside a heredoc body
# -- quote characters carry no quoting meaning there, which is why
# im_hd_expand() is deliberately quote-blind. This PR's condition-2b read
# check (_heredoc_var_reparsed()) lives in the sibling ask-tier masking path,
# and the two had never been exercised TOGETHER. These assertions pin that
# intersection: a #7355-shaped variable-capture heredoc whose body ALSO
# carries expansion syntax, later re-parsed via `eval`.
#
# Differential re-run against `main` INCLUDING #8003 and #7978: every case
# below is ALLOW->ASK or unchanged. No ASK->ALLOW anywhere -- the change is
# monotone toward fail-closed.

# The one case the intersection actually MOVES: the body's only substitution
# is BACKSLASH-escaped, so it is inert per #8003's backslash rule and
# _heredoc_body_expansion_free() lets condition 4 pass -- the capture IS
# masked, so before this PR the later `eval` was a silent ALLOW.
assert_ask "#7970x#8003: unquoted-delimiter capture whose body's only subst is backslash-escaped (inert per #8003), fed to 'eval', is NOT masked -- asks (ALLOW on main)" \
    "R=\$(cat <<EOF
$ST7970_PHRASE
literal \\\$(date)
EOF
)
eval \"\$R\""

# ...and the SAME body read only for DISPLAY stays masked -- the #7355 ALLOW
# this fix must not reopen, now confirmed against the #8003 expansion path.
assert_allow "#7970x#8003: the same backslash-inert body read only via 'echo \"\$NAME\"' stays masked (allow) -- #7355 not reopened" \
    "R=\$(cat <<EOF
$ST7970_PHRASE
literal \\\$(date)
EOF
)
echo \"\$R\""

# A body carrying a LIVE substitution is not expansion-free, so condition 4
# already refuses to mask it and it asked before this PR too. Pinned so that a
# later widening of _heredoc_body_expansion_free() cannot turn this shape into
# an ALLOW without a test noticing.
assert_ask "#7970x#8003: unquoted-delimiter capture whose body carries a LIVE \$( ) substitution, fed to 'eval', asks" \
    "R=\$(cat <<EOF
$ST7970_PHRASE
generated \$(date)
EOF
)
eval \"\$R\""

# #8003's quote-blindness finding: a substitution wrapped in SINGLE quotes
# inside a heredoc body is still live, so the body is still not inert.
assert_ask "#7970x#8003: same shape with the substitution wrapped in single quotes (quote-blind per #8003) still asks" \
    "R=\$(cat <<EOF
$ST7970_PHRASE
marker '\$(date)' here
EOF
)
eval \"\$R\""

# `<<-EOF` tab-stripping form of the same intersection.
assert_ask "#7970x#8003: the <<-EOF dash form of the capture-plus-substitution shape fed to 'eval' asks" \
    "R=\$(cat <<-EOF
	$ST7970_PHRASE
	generated \$(date)
	EOF
)
eval \"\$R\""

echo ""

# =========================================================================
echo -e "${YELLOW}--- #8156: the QUOTED-delimiter sibling of the #7970 capture read check ---${NC}"
# =========================================================================
#
# #7970/PR #8019 gave mask_unquoted_cat_heredoc_bodies()'s NAME= capture
# branch a `$NAME`/`${NAME}` re-parse check, so an UNQUOTED-delimiter capture
# later fed to `eval`/`sh -c`/a pipe into a shell is no longer masked. Its
# sibling mask_heredoc_bodies_selective() -- which owns the QUOTED-delimiter
# heredocs (`<<'EOF'`, `<<"EOF"`) -- had no equivalent check, so the SAME
# capture-then-re-parse shape stayed a silent ALLOW through the other, and
# more idiomatic, delimiter spelling:
#
#     R=$(cat <<'EOF'
#     git reset --hard origin/main
#     EOF
#     )
#     eval "$R"
#
# A quoted delimiter makes the BODY literal -- which is the entire reason
# that function may mask it -- but says NOTHING about the captured VARIABLE:
# `eval "$R"` re-parses that text as shell code exactly as it does in the
# unquoted case. #8156 gates the quoted-delimiter masking branch on the same
# _heredoc_var_reparsed() primitive, against the same TRUE original command
# buffer (#6068's rule), with the same deliberate narrowness (a DISPLAY-only
# read does not count -- #7355's intended ALLOW is exactly that shape).
#
# These assertions are the quoted-delimiter mirror of the two blocks directly
# above, plus the `<<"EOF"` double-quoted-delimiter form.

ST8156_PHRASE='git reset --hard origin/main'

# --- Genuinely dead capture (never read at all) stays masked/allowed -------
assert_allow "#8156: a quoted-delimiter NAME= capture that is NEVER read afterward stays masked (allow)" \
    "R=\$(cat <<'EOF'
$ST8156_PHRASE
EOF
)
echo done"

# --- Display-only reads (the #7355 shape) stay masked/allowed -------------
assert_allow "#8156: a quoted-delimiter NAME= capture read only via 'echo \"\$NAME\"' (display, not re-parsed) stays masked (allow)" \
    "R=\$(cat <<'EOF'
$ST8156_PHRASE
EOF
)
echo \"\$R\""

assert_allow "#8156: the same quoted-delimiter shape read via 'printf' (display, not re-parsed) stays masked (allow)" \
    "R=\$(cat <<'EOF'
$ST8156_PHRASE
EOF
)
printf '%s\n' \"\$R\""

assert_allow "#8156: a <<\"EOF\" (double-quoted delimiter) capture read only via 'echo' stays masked (allow)" \
    "R=\$(cat <<\"EOF\"
$ST8156_PHRASE
EOF
)
echo \"\$R\""

# --- A DIFFERENT variable being eval'd must not disqualify this capture ----
assert_allow "#8156: a quoted-delimiter capture stays masked when the eval'd variable is a DIFFERENT name (no blanket 'eval appears anywhere' test)" \
    "R=\$(cat <<'EOF'
$ST8156_PHRASE
EOF
)
eval \"\$OTHER\""

# --- The gap itself: a quoted-delimiter capture later RE-PARSED as shell
#     code must NOT be masked -- COMMAND_ASK_SCAN sees the phrase and asks ---
assert_ask "#8156: a <<'EOF' capture later fed to 'eval \"\$NAME\"' is NOT masked -- asks (silent ALLOW before this fix)" \
    "R=\$(cat <<'EOF'
$ST8156_PHRASE
EOF
)
eval \"\$R\""

assert_ask "#8156: the <<\"EOF\" double-quoted-delimiter form fed to 'eval \"\$NAME\"' is NOT masked -- asks (silent ALLOW before this fix)" \
    "R=\$(cat <<\"EOF\"
$ST8156_PHRASE
EOF
)
eval \"\$R\""

assert_ask "#8156: a <<'EOF' capture later fed to 'sh -c \"\$NAME\"' is NOT masked -- asks" \
    "R=\$(cat <<'EOF'
$ST8156_PHRASE
EOF
)
sh -c \"\$R\""

assert_ask "#8156: a <<'EOF' capture later piped to bash ('\$NAME | bash') is NOT masked -- asks" \
    "R=\$(cat <<'EOF'
$ST8156_PHRASE
EOF
)
echo \"\$R\" | bash"

assert_ask "#8156: the \${NAME} brace form of a <<'EOF' capture fed to 'eval' is NOT masked -- asks" \
    "R=\$(cat <<'EOF'
$ST8156_PHRASE
EOF
)
eval \"\${R}\""

assert_ask "#8156: the quoted-delimiter capture fed to 'source \"\$NAME\"' is NOT masked -- asks" \
    "R=\$(cat <<'EOF'
$ST8156_PHRASE
EOF
)
source \"\$R\""

# --- `<<-'EOF'` tab-stripping form of the same shape -----------------------
assert_ask "#8156: the <<-'EOF' dash form of the quoted-delimiter capture fed to 'eval' asks" \
    "R=\$(cat <<-'EOF'
	$ST8156_PHRASE
	EOF
)
eval \"\$R\""

assert_allow "#8156: the <<-'EOF' dash form read only via 'echo' stays masked (allow)" \
    "R=\$(cat <<-'EOF'
	$ST8156_PHRASE
	EOF
)
echo \"\$R\""

# --- A quoted body is inert REGARDLESS of what expansion syntax it spells
#     (the quoted-delimiter mirror of the #7970x#8003 intersection): the body
#     is masked either way, so only the read check decides ask vs allow ------
assert_ask "#8156x#8003: quoted-delimiter capture whose body ALSO spells a \$( ) substitution (inert under a quoted delimiter), fed to 'eval', asks" \
    "R=\$(cat <<'EOF'
$ST8156_PHRASE
generated \$(date)
EOF
)
eval \"\$R\""

assert_allow "#8156x#8003: the same body read only via 'echo \"\$NAME\"' stays masked (allow) -- #7355/#5779 not reopened" \
    "R=\$(cat <<'EOF'
$ST8156_PHRASE
generated \$(date)
EOF
)
echo \"\$R\""

# --- Flag-value branch is untouched: a flag value can never be eval'd, so it
#     needs no read check and keeps masking (the #5181/#6056 fixes) ---------
assert_allow "#8156: the quoted-delimiter flag-value capture ('gh pr comment --body \"\$(cat <<'EOF'...)\"') is UNCHANGED -- stays masked" \
    "gh pr comment 123 --body \"\$(cat <<'EOF'
$ST8156_PHRASE
EOF
)\""

# --- #5779's own plain file-sink shape (no capture at all) is untouched ----
assert_allow "#8156: a quoted heredoc body written to a FILE sink (no capture, no re-parse) stays masked (allow) -- #5779 not reopened" \
    "cat > /tmp/report-8156-a.md <<'EOF'
Never run $ST8156_PHRASE from a worktree.
EOF"

assert_allow "#8156: a quoted heredoc body written to a file sink stays masked even when an UNRELATED eval appears later in the same command" \
    "cat > /tmp/report-8156-b.md <<'EOF'
Never run $ST8156_PHRASE from a worktree.
EOF
eval \"\$SOMETHING_ELSE\""

# --- The interpreter carve-out is a DIFFERENT mechanism and is untouched ---
assert_ask "#8156: the interpreter carve-out is unchanged -- a live phrase inside 'bash <<'EOF' ... EOF' still asks" \
    "bash <<'EOF'
$ST8156_PHRASE
EOF"

# --- Catastrophic tier is unaffected: it calls mask_heredoc_bodies_
#     selective() WITHOUT the true original, so `orig` is "" there and its
#     behavior is bit-for-bit what it was ------------------------------------
assert_deny "#8156: the equivalent catastrophic-tier phrase (aws s3 rb) in a <<'EOF' capture fed to 'eval' still denies" \
    "R=\$(cat <<'EOF'
${_S3RB_CAT}
EOF
)
eval \"\$R\""

echo ""

# =========================================================================
echo -e "${YELLOW}--- #6252: COMMAND_NO_COMMENT quote-awareness (ADR-0016 sed test matrix) ---${NC}"
# =========================================================================
#
# ADR-0016 (docs/adr/0016-write-target-confinement-approach.md, "Sed /
# argument-position false positive") root-caused a live, previously
# unreported unsound false-negative: COMMAND_NO_COMMENT's `#`-comment
# stripper was quote-UNAWARE, so a `#` inside ANY whitespace-preceded quoted
# write-idiom argument (a sed script, a `--body`/`-m` prose string, a PR/
# issue reference like `#958`) truncated COMMAND_ASK_SCAN at that point —
# and COMMAND_ASK_SCAN is also extract_write_targets()'s input for the
# worktree-write-confinement DENY (WRITE_TARGETS). The real write target,
# sitting textually AFTER the quoted `#`, silently vanished from the scan,
# producing a silent ALLOW where #4178/#4921 require a DENY.
#
# Fixture: a DEDICATED linked-worktree fixture (make_wt_repo_linked(), the
# same helper the #4921 section above uses) -- NOT a reuse of WT_LINKED_DIR/
# WT_REPO_LINKED, which are already `rm -rf`'d earlier in this file (see the
# cleanup right after the #4933/#5363 cd-tracking section). A cwd pointed at
# a since-deleted directory makes the guard's own git/worktree detection
# silently no-op, which would make every assertion below pass VACUOUSLY
# (looking like a real DENY check while actually never exercising the
# write-confinement path at all) -- so this section gets its own live
# fixture instead.
WT6252_REPO=$(make_wt_repo_linked)
WT6252_DIR="$WT6252_REPO/.loom/worktrees/issue-1"
#
# Cases 1-2 below are the ADR's own two confirmed repros; case 3 proves the
# fix is not sed-specific (a `#` in an UNRELATED quoted argument, followed by
# a write through a DIFFERENT idiom, still gets scanned); case 4 is the
# "must not over-deny" control; case 5 is the ASK/DDL tier's own pre-existing
# regression floor, unaffected by the quote-awareness fix.

# 1. Exact live repro from ADR-0016 / issue #6252: `$SP` is a same-command
#    unresolved variable (no assignment anywhere in the command), so the
#    correct outcome is the ordinary #4921 fail-closed DENY, naming the real
#    write target ('$SP/file.md') -- NEVER a sed-script fragment like
#    "958/' $SP/file.md" (the pre-fix truncated-scan symptom), and NEVER a
#    silent ALLOW (the pre-fix unsound-bypass symptom).
assert_deny_reason_matches "write-confinement (#6252 case 1): sed -i script with a quoted '#958' no longer truncates the scan before the real \$SP write target" \
    "sed -i '' 's/x/y #958/' \$SP/file.md" \
    '\$SP/file\.md' "$WT6252_DIR"

# 2. The exact originally-reported repro cited in ADR-0016 (a sed script
#    replacing prose that itself contains a `#`-issue-reference).
assert_deny_reason_matches "write-confinement (#6252 case 2): ADR-0016's originally-reported sed repro denies, naming the real \$SP write target" \
    "sed -i '' 's/**Blocked by 3a** (per-block em-export/**Blocked by #958** (3a: per-block em-export/' \$SP/issue-3b.md" \
    '\$SP/issue-3b\.md' "$WT6252_DIR"

# 3. A `#` inside an UNRELATED quoted argument (a gh --body value, not part
#    of the write idiom at all), followed LATER in the same command by a
#    write through a DIFFERENT idiom -- proves the fix is not sed-specific,
#    per ADR-0016's own required case 3.
assert_deny_reason_matches "write-confinement (#6252 case 3): quoted '#123' in an unrelated --body value does not swallow a later '>' write target" \
    'gh pr comment 1 --body "notes #123" && echo hi > $SP/f.md' \
    '\$SP/f\.md' "$WT6252_DIR"

# 3b-3e. The same "unrelated quoted #, write happens through a different
# idiom" shape repeated across every other idiom sharing COMMAND_ASK_SCAN
# (the #6252 audit item) -- each one silently ALLOWed pre-fix (verified
# directly against origin/main @ 06df09c8) and now denies, naming the real
# target, not a fragment of the quoted text preceding the `#`.
assert_deny_reason_matches "write-confinement (#6252 audit): '>' redirect target survives a preceding quoted '#123' argument" \
    "echo 'note #123' > \$SP/file.md" \
    '\$SP/file\.md' "$WT6252_DIR"
assert_deny_reason_matches "write-confinement (#6252 audit): '>>' redirect target survives a preceding quoted '#123' argument" \
    "echo 'note #123' >> \$SP/file.md" \
    '\$SP/file\.md' "$WT6252_DIR"
assert_deny_reason_matches "write-confinement (#6252 audit): 'tee' target survives an unrelated preceding quoted '#123' argument" \
    "printf '%s' 'note #123' | tee \$SP/out.txt" \
    '\$SP/out\.txt' "$WT6252_DIR"
assert_deny_reason_matches "write-confinement (#6252 audit): 'cp' destination survives a '#123'-bearing quoted SOURCE argument" \
    "cp 'notes #123.md' \$SP/dest.md" \
    '\$SP/dest\.md' "$WT6252_DIR"
assert_deny_reason_matches "write-confinement (#6252 audit): 'mv' destination survives a '#123'-bearing quoted SOURCE argument" \
    "mv 'todo #123.md' \$SP/dest.md" \
    '\$SP/dest\.md' "$WT6252_DIR"

# 4. Control (ADR-0016 case 4): a literal, non-main-checkout `#`-containing
#    sed write must still ALLOW -- the fix must not turn every `#`-bearing
#    sed command into a deny.
assert_allow "write-confinement (#6252 case 4): sed -i script with a quoted '#z' on a /tmp target still allows" \
    "sed -i '' 's/x/y #z/' /tmp/loom-test-$$-6252-scratch.md" "$WT6252_DIR"

# 5. Control (ADR-0016 case 5): a genuine end-of-line shell comment with no
#    attached write idiom is unaffected -- regression guard on the ASK/DDL
#    tier's existing, correctly-scoped comment-stripping behavior (mirrors
#    the #3553 coverage above, kept here as an #6252-tagged case for
#    traceability to the ADR's own test matrix).
assert_allow "write-confinement (#6252 case 5): a genuine trailing comment with no write idiom is unaffected" \
    "echo hi # this really is a comment" "$WT6252_DIR"

rm -rf "$WT6252_REPO"

echo ""

# =========================================================================
echo -e "${YELLOW}--- #6394: catastrophic-tier whole-line #-comment masking ---${NC}"
# =========================================================================
#
# Guard-Decision Telemetry Review finding (#3898 standing policy): the raw
# ALWAYS_BLOCK_PATTERNS substring scan hard-denied a plain `#`-prefixed shell
# comment that merely QUOTES a catastrophic-tier phrase for documentation/
# forensic purposes, single-line or (unlike a bare single-command `echo`,
# which the #3687 read-only fast path already admits) multi-line too, since
# comments were never masked before reaching this scan. Distinct from #6068
# (the sibling echo/printf-positional-arg gap, covered in its own PR) — this
# section covers ONLY the `#`-comment case, mask_catastrophic_comment_lines()'s
# own new masking pass.
#
# Case 1-2 are the issue's own two repro cases (now fixed); case 3-5 are the
# safety-floor regression guards proving the fix is WHOLE-LINE-ONLY, quote-
# aware, and heredoc-conservative — a real catastrophic invocation must
# still deny in every one of these adjacent shapes.

# 1. Exact repro: a single-line whole-line `#`-comment quoting a
#    catastrophic-tier phrase now allows.
assert_allow "#6394 case 1: single-line whole-line '#'-comment quoting 'aws s3 rb' allows" \
    "# aws s3 rb mentioned here only, single line comment"

# 2. Exact repro: the SAME comment as one line among several (mixed with
#    real, unrelated read-only lines) now allows — the multi-line shape the
#    #3687 read-only fast path does not reach.
assert_allow "#6394 case 2: multi-line command with a whole-line '#'-comment quoting 'aws s3 rb' among real lines allows" \
    "$(printf 'echo hello\n# aws s3 rb mentioned here only, single line comment\necho world')"

# 2b. Same shape for the sibling 'docker system prune' catastrophic pattern,
#     and for a leading-whitespace-indented comment line.
assert_allow "#6394 case 2b: whole-line '#'-comment quoting 'docker system prune' allows" \
    "$(printf 'echo start\n    # docker system prune mentioned here only\necho end')"

# 3. SAFETY FLOOR (AC2): a real, unwrapped catastrophic invocation on its own
#    line, preceded by an unrelated whole-line comment on the PRIOR line,
#    still denies — masking one line must never blind the scan to a real
#    command on an adjacent line.
assert_deny "#6394 case 3: real 'aws s3 rb' invocation after an unrelated whole-line comment still denies" \
    "$(printf '# unrelated comment, nothing dangerous here\naws s3 rb s3://prod-bucket --force')"

# 4. SAFETY FLOOR (AC1 residual-gap regression guard): a TRAILING comment on
#    a line that ALSO carries a real catastrophic invocation is deliberately
#    NOT masked by this whole-line-only pass (see
#    mask_catastrophic_comment_lines()'s header comment, contract #1, for the
#    documented accepted gap) — the command portion must still deny.
assert_deny "#6394 case 4: real 'aws s3 rb' invocation with a trailing same-line comment still denies" \
    "aws s3 rb s3://prod-bucket --force  # decommissioning this bucket"

# 5. SAFETY FLOOR (AC2 quote-awareness): a line that LOOKS like a whole-line
#    '#'-comment (first non-whitespace char is '#') but is actually still
#    inside an OPEN double-quoted span from a prior line must never be
#    mistaken for a real comment start — stays fully visible to the raw scan,
#    still denies. (Matches this file's existing raw-substring-scan posture:
#    quoted data is only ever exempted via a specific, narrow masking pass,
#    never a blanket "if quoted, allow" rule — see the header comment on
#    ALWAYS_BLOCK_PATTERNS' 'aws s3 rm'/'aws s3 rb' entries above.)
assert_deny "#6394 case 5: '#'-looking line still inside an open quote from a prior line still denies" \
    "$(printf 'echo "line one\n# aws s3 rb looks like a comment but is quoted data\nline three"')"

# 6. SAFETY FLOOR (AC2 heredoc-conservative): a '#'-prefixed line inside a
#    heredoc body must stay visible to the scan and still deny — this pass
#    fails closed (does nothing) for the WHOLE buffer whenever '<<' appears
#    anywhere in it, mirroring mask_heredoc_bodies_selective()'s existing
#    interpreter-fed exclusion by simply never touching heredocs at all.
#    Unchanged from pre-#6394 behavior (verified against origin/main): this
#    case denies with or without the fix, proving no regression.
assert_deny "#6394 case 6: '#'-prefixed line inside a heredoc body still denies (heredoc-conservative)" \
    "$(printf "cat <<'EOF'\n# aws s3 rb mentioned inside a heredoc body\nEOF")"

echo ""

# =========================================================================
echo -e "${YELLOW}--- #7498: escaped vs live \$( / backtick in the masking-eligibility gates ---${NC}"
# =========================================================================
#
# Eight masking-eligibility gates in guard-destructive-generic.sh decided
# "does this quoted span carry a command substitution?" with a byte-presence
# check -- index(inner, "$(") / index(inner, "`") -- which cannot tell a
# backslash-ESCAPED backtick / `\$(` (a literal character inside a
# double-quoted string: the standard way to write a markdown code span, and
# what every automated Champion/Judge/Curator/Doctor comment contains) from a
# genuinely live one. The escaped form has zero execution risk, so vetoing
# masking on it is a false positive: the whole span -- including any inert
# mention of a catastrophic/ask-tier phrase -- stayed visible and denied/asked.
# Same class as #5109, #6464/#6866, #7495 (guard-loom-workflow.sh, fixed by
# PR #7496) and #7558; this section covers the guard-destructive-generic.sh
# half, one sub-block per converted site. Every site gets BOTH directions:
# escaped -> masked (allow), AND a genuinely live/mixed span -> still visible
# (deny/ask) -- the fail-safe floor the conversion must not weaken.

# --- Site 1: qsplit() (shared _QSPLIT_AWK snippet) ---------------------------
# The #3755 reproducer shape (4-way lifecycle alternation, target word not
# adjacent to the closing quote) with an escaped-backtick code span added
# inside the same double-quoted pattern. Pre-fix the backtick byte made the
# span "active", the `|`s were treated as real pipes and the phantom `halt`
# segment hard-denied a read-only grep.
assert_allow "#7498 site 1 (qsplit): grep alternation carrying an escaped-backtick code span stays an inert span (allow)" \
    'grep -E "see \`x\` lifecycle|halt|poweroff|init 0" file'
assert_allow "#7498 site 1 (qsplit): grep alternation carrying an escaped \\\$(...) stays an inert span (allow)" \
    'grep -E "see \$(x) lifecycle|halt|poweroff|init 0" file'
assert_deny "#7498 site 1 (qsplit): a GENUINELY live backtick inside the same alternation keeps its separators active (still denies)" \
    'grep -E "see `x` lifecycle|halt|poweroff|init 0" file'
assert_deny "#7498 site 1 (qsplit): mixed escaped + live backtick inside the same alternation still denies" \
    'grep -E "see \`x\` and `y` lifecycle|halt|poweroff|init 0" file'

# --- Site 2: _heredoc_body_expansion_free() (mask_unquoted_cat_heredoc_bodies,
#     the #6056 --body "$(cat <<EOF ...)" idiom, ask tier) -----------------------
# The function's backtick branch already walked backslash parity (see the
# #6056 escaped-backtick case above); its `$(` branch was presence-only, so a
# body that merely spelled `\$(...)` as literal text (the shell's own rule for
# an unquoted-delimiter heredoc: backslash escapes `$`, backtick, `\`) lost
# masking for the WHOLE body and the force-op line below it asked.
assert_allow "#7498 site 2 (_heredoc_body_expansion_free): unquoted --body heredoc whose only \$( is backslash-ESCAPED is still masked (allow)" \
    'gh pr comment 7498 --body "$(cat <<EOF
Prose that spells \$(date) as literal text, then advice:
git reset --hard origin/main
EOF
)"'
assert_ask "#7498 site 2: the same body with an ESCAPED BACKSLASH before \$( (a LIVE substitution) stays visible and still asks" \
    'gh pr comment 7498 --body "$(cat <<EOF
Prose with \\$(date) then advice:
git reset --hard origin/main
EOF
)"'
assert_ask "#7498 site 2: a body mixing an escaped \\\$( and a live \$(...) on another line stays visible and still asks" \
    'gh pr comment 7498 --body "$(cat <<EOF
Literal \$(date) here, but a live $(date) there:
git reset --hard origin/main
EOF
)"'

# --- Site 3: strip_literal_text() (double-quoted branch; catastrophic tier) ---
assert_allow "#7498 site 3 (strip_literal_text): --body double-quoted value quoting a catastrophic phrase inside an escaped-backtick code span is redacted (allow)" \
    'gh issue comment 7498 --body "Never run \`'"$_S3RB"' s3://prod-bucket --force\` by hand."'
assert_allow "#7498 site 3 (strip_literal_text): --body value quoting the phrase inside an escaped \\\$(...) is redacted (allow)" \
    'gh issue comment 7498 --body "Never run \$('"$_S3RB"' s3://prod-bucket --force) by hand."'
assert_deny "#7498 site 3 (strip_literal_text): --body value with a GENUINELY live backtick substitution stays visible (still denies)" \
    'gh issue comment 7498 --body "Result: `'"$_S3RB"' s3://prod-bucket --force`"'
assert_deny "#7498 site 3 (strip_literal_text): --body value with a live \$(...) substitution stays visible (still denies)" \
    'gh issue comment 7498 --body "Result: $('"$_S3RB"' s3://prod-bucket --force)"'
assert_deny "#7498 site 3 (strip_literal_text): mixed escaped + live backtick in the same --body value still denies" \
    'gh issue comment 7498 --body "Safe: \`echo hi\`. Unsafe: `'"$_S3RB"' s3://prod-bucket --force`"'

# --- Site 4: mask_ask_positional_args() (check-duplicate.sh, ask tier) -------
assert_allow "#7498 site 4 (mask_ask_positional_args): check-duplicate.sh DESCRIPTION quoting an ask-tier phrase inside an escaped-backtick code span is masked (allow)" \
    './.loom/scripts/check-duplicate.sh "Title" "see \`git clean -fd\` as prose"'
assert_allow "#7498 site 4 (mask_ask_positional_args): check-duplicate.sh DESCRIPTION quoting the phrase inside an escaped \\\$(...) is masked (allow)" \
    './.loom/scripts/check-duplicate.sh "Title" "see \$(git clean -fd) as prose"'
assert_ask "#7498 site 4 (mask_ask_positional_args): check-duplicate.sh DESCRIPTION with a GENUINELY live backtick substitution stays visible (still asks)" \
    './.loom/scripts/check-duplicate.sh "Title" "see `git clean -fd` as prose"'
assert_ask "#7498 site 4 (mask_ask_positional_args): check-duplicate.sh DESCRIPTION with a live \$(...) substitution stays visible (still asks)" \
    './.loom/scripts/check-duplicate.sh "Title" "see $(git clean -fd) as prose"'
assert_ask "#7498 site 4 (mask_ask_positional_args): mixed escaped + live backtick in the same DESCRIPTION still asks" \
    './.loom/scripts/check-duplicate.sh "Title" "safe \`x\`, unsafe `git clean -fd`"'

# --- Site 5: mask_catastrophic_positional_args() (grep/echo/printf, catastrophic tier) ---
assert_allow "#7498 site 5 (mask_catastrophic_positional_args): grep pattern quoting a catastrophic phrase inside an escaped-backtick code span is masked (allow)" \
    'grep -n "run \`'"$_S3RB"'\` here" notes.md'
assert_allow "#7498 site 5 (mask_catastrophic_positional_args): echo heading quoting the phrase inside an escaped-backtick code span is masked (allow)" \
    'echo "=== \`'"$_DPRUNE"'\` ==="'
assert_allow "#7498 site 5 (mask_catastrophic_positional_args): printf argument quoting the phrase inside an escaped \\\$(...) is masked (allow)" \
    'printf "see \$('"$_DPRUNE"') here\n"'
assert_deny "#7498 site 5 (mask_catastrophic_positional_args): echo argument with a GENUINELY live backtick substitution stays unmasked (still denies)" \
    'echo "`'"$_DPRUNE"'`"'
assert_deny "#7498 site 5 (mask_catastrophic_positional_args): grep pattern with a live \$(...) substitution stays unmasked (still denies)" \
    'grep -n "$('"$_S3RB"' s3://prod-bucket --force)" notes.md'
assert_deny "#7498 site 5 (mask_catastrophic_positional_args): mixed escaped + live backtick in the same echo argument still denies" \
    'echo "safe \`x\` then `'"$_DPRUNE"'`"'

# --- Site 6: mask_stash_scan_positional_args() (COMMAND_STASH_SCAN, main checkout) ---
# NB: the stash-scope regex's trailing boundary class is `[[:space:]]`/`;&|)`
# /backtick/end-of-string, so `\`git stash pop\`` -- where a BACKSLASH
# directly follows `pop` -- never matched even pre-fix; the code span below
# therefore carries a flag after `pop` so the phrase sits on a real boundary
# and the case genuinely reproduces the pre-fix false ask.
ST7498_REPO=$(make_wt_repo_linked)
assert_allow "#7498 site 6 (mask_stash_scan_positional_args): grep search quoting 'git stash pop --quiet' inside an escaped-backtick code span is masked (main checkout, allow)" \
    'grep -n "see \`git stash pop --quiet\` here" notes.md' "$ST7498_REPO"
assert_allow "#7498 site 6 (mask_stash_scan_positional_args): grep search quoting 'git stash pop' inside an escaped \\\$(...) is masked (main checkout, allow)" \
    'grep -n "see \$(git stash pop) here" notes.md' "$ST7498_REPO"
assert_ask "#7498 site 6 (mask_stash_scan_positional_args): grep search with a GENUINELY live backtick substitution stays visible (main checkout, still asks)" \
    'grep -n "see `git stash pop` here" notes.md' "$ST7498_REPO"
assert_ask "#7498 site 6 (mask_stash_scan_positional_args): grep search with a live \$(...) substitution stays visible (main checkout, still asks)" \
    'grep -n "see $(git stash pop) here" notes.md' "$ST7498_REPO"
assert_ask "#7498 site 6 (mask_stash_scan_positional_args): mixed escaped + live backtick in the same search pattern still asks" \
    'grep -n "safe \`x\`, unsafe `git stash pop`" notes.md' "$ST7498_REPO"

# --- Site 7: mask_catastrophic_var_assignment() -- INVERTED polarity ---------
# This gate is the mirror image of the other seven: presence of `$(`/backtick
# FORCED "never mask". Converted as `if (has_live_subst(inner)) never-mask`,
# so an escaped code span in a dead assignment is now maskable, while a live
# substitution -- or ANY later `$NAME`/`${NAME}` read, escaped or not -- stays
# fail-closed exactly as before.
assert_allow "#7498 site 7 (mask_catastrophic_var_assignment): dead assignment whose value quotes the phrase inside an escaped-backtick code span is masked (allow)" \
    'PATTERN="see \`'"$_S3RB_CAT"'\` here"'
assert_allow "#7498 site 7 (mask_catastrophic_var_assignment): dead assignment whose value quotes the phrase inside an escaped \\\$(...) is masked (allow)" \
    'PATTERN="see \$('"$_S3RB"' s3://prod-bucket --force) here"'
assert_deny "#7498 site 7 (mask_catastrophic_var_assignment): assignment whose value carries a GENUINELY live backtick substitution is never masked (still denies)" \
    'PATTERN="`'"$_S3RB"' s3://prod-bucket --force`"'
assert_deny "#7498 site 7 (mask_catastrophic_var_assignment): mixed escaped + live backtick in the same value still denies" \
    'PATTERN="safe \`x\` then `'"$_S3RB"' s3://prod-bucket --force`"'
assert_deny "#7498 site 7 (mask_catastrophic_var_assignment): escaped-backtick value that IS read via eval later in the same command stays fail-closed (still denies)" \
    'PATTERN="see \`'"$_S3RB_CAT"'\` here"; eval "$PATTERN"'

# --- Site 8: mask_catastrophic_forloop_wordlist() (catastrophic tier) --------
assert_allow "#7498 site 8 (mask_catastrophic_forloop_wordlist): word-list literal quoting the phrase inside an escaped-backtick code span, --search fed the loop var, is masked (allow)" \
    'for q in "sql-ddl" "\`'"$_S3RB_CAT"'\`"; do gh issue list --search "$q" --limit 5; done'
assert_allow "#7498 site 8 (mask_catastrophic_forloop_wordlist): word-list literal quoting the phrase inside an escaped \\\$(...) is masked (allow)" \
    'for q in "sql-ddl" "\$('"$_S3RB"' s3://prod-bucket --force)"; do gh issue list --search "$q" --limit 5; done'
assert_deny "#7498 site 8 (mask_catastrophic_forloop_wordlist): word-list literal carrying a GENUINELY live backtick substitution is never masked (still denies)" \
    'for q in "sql-ddl" "`'"$_S3RB"' s3://prod-bucket --force`"; do gh issue list --search "$q" --limit 5; done'
assert_deny "#7498 site 8 (mask_catastrophic_forloop_wordlist): word-list literal carrying a live \$(...) substitution is never masked (still denies)" \
    'for q in "sql-ddl" "$('"$_S3RB"' s3://prod-bucket --force)"; do gh issue list --search "$q" --limit 5; done'
assert_deny "#7498 site 8 (mask_catastrophic_forloop_wordlist): mixed escaped + live backtick in the same word-list literal still denies" \
    'for q in "sql-ddl" "safe \`x\` then `'"$_S3RB"' s3://prod-bucket --force`"; do gh issue list --search "$q" --limit 5; done'

echo ""

# =========================================================================

print_summary
