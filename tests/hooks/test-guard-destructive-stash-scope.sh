#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — stash scope.
#
# One slice of the former monolithic tests/hooks/test-guard-destructive.sh,
# split per #7741. Shared fixtures, assertions and catastrophic-phrase payloads
# live in tests/hooks/lib/guard-destructive-harness.sh.
#
# Usage: ./tests/hooks/test-guard-destructive-stash-scope.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

echo -e "${YELLOW}--- Truth table: config-resolver migration polarity (#4063) ---${NC}"
# =========================================================================
#
# guards.sqlDdl / cloudCli / reversibleGh / decisionLog / rmScope / forceScope
# and worktree.root were migrated from a bespoke per-guard jq read to the
# shared loom_config_get() (defaults/scripts/lib/config-resolver.sh). Each
# reader keeps its EXACT prior polarity in bash rather than trusting
# loom_config_get's null-collapses-to-default behavior blindly (see the
# migration comment at each function). This is the truth table the migration
# issue's acceptance criteria asked for: key absent / explicit true / explicit
# false / explicit null / malformed JSON / non-boolean value, each verified to
# resolve to the SAME decision as pre-migration main. Absent / true / false /
# malformed are already covered by each guard's own section above; this
# section adds the two previously-untested shapes — explicit `null` and a
# non-boolean/out-of-range value — for every migrated reader.

TT_NULL_REPO=$(make_sql_repo '{"guards":{"sqlDdl":null,"cloudCli":null,"reversibleGh":null,"decisionLog":null,"rmScope":null,"forceScope":null},"worktree":{"root":null}}')
TT_NONBOOL_REPO=$(make_sql_repo '{"guards":{"sqlDdl":"yes","cloudCli":"yes","reversibleGh":"yes","decisionLog":"yes","rmScope":"banana","forceScope":"banana"},"worktree":{"root":42}}')

# --- sqlDdl: default-on (true); explicit null and a non-boolean value both
# stay ON (only an explicit boolean `false` disables). ---
assert_deny "truth-table sqlDdl=null: DROP TABLE still denied (default true)" \
    "mysql -e 'DROP TABLE users;'" "$TT_NULL_REPO"
assert_deny "truth-table sqlDdl=\"yes\" (non-boolean): DROP TABLE still denied (default true)" \
    "mysql -e 'DROP TABLE users;'" "$TT_NONBOOL_REPO"

# --- cloudCli: default-on (true); explicit null and a non-boolean value both
# still ask (only an explicit boolean `false` disables). ---
assert_ask "truth-table cloudCli=null: aws ec2 terminate-instances still asks (default true)" \
    "aws ec2 terminate-instances --instance-ids i-1234" "$TT_NULL_REPO"
assert_ask "truth-table cloudCli=\"yes\" (non-boolean): aws ec2 terminate-instances still asks (default true)" \
    "aws ec2 terminate-instances --instance-ids i-1234" "$TT_NONBOOL_REPO"

# --- reversibleGh: default-off (false, INVERSE polarity); explicit null and a
# non-boolean value both stay OFF (only an explicit boolean `true` enables). ---
assert_allow "truth-table reversibleGh=null: gh pr close allowed (default false)" \
    "gh pr close 42" "$TT_NULL_REPO"
assert_allow "truth-table reversibleGh=\"yes\" (non-boolean): gh pr close allowed (default false)" \
    "gh pr close 42" "$TT_NONBOOL_REPO"

# --- rmScope: default "repo" (only "off"/"permissive" opt out); explicit null
# and an unrecognized string both fall through to the safe "repo" default. ---
assert_deny "truth-table rmScope=null: outside-repo path still denied (default repo)" \
    "rm -rf /opt/some-vendor/important" "$TT_NULL_REPO"
assert_deny "truth-table rmScope=\"banana\" (out-of-range): outside-repo path still denied (default repo)" \
    "rm -rf /opt/some-vendor/important" "$TT_NONBOOL_REPO"

# --- forceScope: default "all" (only "protected"/"off" opt out); explicit
# null and an unrecognized string both fall through to "all" (force-push to a
# working branch still asks). ---
assert_ask "truth-table forceScope=null: force-push to working branch still asks (default all)" \
    "git push --force origin feature/x" "$TT_NULL_REPO"
assert_ask "truth-table forceScope=\"banana\" (out-of-range): force-push to working branch still asks (default all)" \
    "git push --force origin feature/x" "$TT_NONBOOL_REPO"

# --- worktree.root: explicit null and a non-string (number) value both fall
# through to the in-repo default worktrees dir — no external root is admitted,
# so an outside-repo rm under the would-be configured path is still denied
# under the default rmScope=repo. ---
assert_deny "truth-table worktree.root=null: no external root admitted, outside path denied" \
    "rm -rf /Volumes/scratch/loom-wt/some-worktree/issue-1/foo" "$TT_NULL_REPO"
assert_deny "truth-table worktree.root=42 (non-string): no external root admitted, outside path denied" \
    "rm -rf /Volumes/scratch/loom-wt/some-worktree/issue-1/foo" "$TT_NONBOOL_REPO"

# --- decisionLog: default-off (false, INVERSE polarity); explicit null and a
# non-boolean value both stay OFF (only an explicit boolean `true` enables). No
# env var is set, so the config value alone drives the decision. ---
TT_DL_LOG="$(mktemp -u)"
rm -f "$TT_DL_LOG"
make_input "rm -rf /" "$TT_NULL_REPO" | \
    env LOOM_GUARD_DECISION_LOG_FILE="$TT_DL_LOG" "$GUARD" >/dev/null 2>&1 || true
TOTAL=$((TOTAL + 1))
if [[ ! -f "$TT_DL_LOG" ]]; then
    PASS=$((PASS + 1))
    echo -e "  ${GREEN}PASS${NC}: truth-table decisionLog=null: deny writes NO decision record (default false)"
else
    FAIL=$((FAIL + 1))
    echo -e "  ${RED}FAIL${NC}: truth-table decisionLog=null: deny writes NO decision record (default false)"
    echo -e "       unexpected: $(cat "$TT_DL_LOG")"
fi

rm -f "$TT_DL_LOG"
make_input "rm -rf /" "$TT_NONBOOL_REPO" | \
    env LOOM_GUARD_DECISION_LOG_FILE="$TT_DL_LOG" "$GUARD" >/dev/null 2>&1 || true
TOTAL=$((TOTAL + 1))
if [[ ! -f "$TT_DL_LOG" ]]; then
    PASS=$((PASS + 1))
    echo -e "  ${GREEN}PASS${NC}: truth-table decisionLog=\"yes\" (non-boolean): deny writes NO decision record (default false)"
else
    FAIL=$((FAIL + 1))
    echo -e "  ${RED}FAIL${NC}: truth-table decisionLog=\"yes\" (non-boolean): deny writes NO decision record (default false)"
    echo -e "       unexpected: $(cat "$TT_DL_LOG")"
fi

rm -rf "$TT_NULL_REPO" "$TT_NONBOOL_REPO"

echo ""

# =========================================================================
echo -e "${YELLOW}--- Stash-stack scope: git stash pop/drop/clear in the main checkout (#4281) ---${NC}"
# =========================================================================
#
# The main checkout's stash stack is operator-owned (preserved diagnostic
# state, deliberately-parked WIP) — a role subagent's ad-hoc integration
# check (test-merge, conflict inspection) must never pop/drop/clear it. Uses
# make_wt_repo_linked (defined above, in the write-confinement section) so the
# ask/allow test exercises the REAL show-toplevel-vs-git-common-dir divergence
# between a main checkout and a linked worktree, exactly like the #4210
# write-confinement regression tests.

ST_REPO=$(make_wt_repo_linked)
ST_WT_DIR="$ST_REPO/.loom/worktrees/issue-1"

assert_ask "stash-scope: git stash pop in main checkout asks (#4281)" \
    "git stash pop" "$ST_REPO"
assert_ask "stash-scope: git stash drop in main checkout asks (#4281)" \
    "git stash drop" "$ST_REPO"
assert_ask "stash-scope: git stash clear in main checkout asks (#4281)" \
    "git stash clear" "$ST_REPO"

# --- #6076: the main-checkout ask must NAME the sanctioned replacement ---
#
# 21 identical `stash-scope:main-checkout` asks fired over 2026-08-09..12 in
# headless runs with nobody to answer them. The tier is correct and stays put;
# what was missing was a replacement command the caller could rerun with — the
# message only offered "disable the guard". #6076 added
# `worktree.sh stash-push main` / `stash-pop main` (a per-target ref, never
# refs/stash), so the ask now names it. These assertions pin the message
# content, not just the verdict — the message IS the fix here.
assert_ask_reason_matches "stash-scope (#6076): main-checkout ask names 'stash-push main' as the replacement" \
    "git stash pop" "worktree\.sh stash-push main" "$ST_REPO"
assert_ask_reason_matches "stash-scope (#6076): main-checkout ask names 'stash-pop main' as the restore half" \
    "git stash pop" "worktree\.sh stash-pop main" "$ST_REPO"
assert_ask_reason_matches "stash-scope (#6076): main-checkout ask still documents the guards.stashScope toggle" \
    "git stash pop" "guards\.stashScope:false" "$ST_REPO"

# The replacement pair itself must be guard-transparent: it never invokes
# `git stash pop|drop|clear`, so it must not trip this (or any other) gate.
assert_allow "stash-scope (#6076): 'worktree.sh stash-push main' is ungated in the main checkout" \
    "./.loom/scripts/worktree.sh stash-push main" "$ST_REPO"
assert_allow "stash-scope (#6076): 'worktree.sh stash-pop main' is ungated in the main checkout" \
    "./.loom/scripts/worktree.sh stash-pop main" "$ST_REPO"
assert_allow "stash-scope (#6076): the full 'stash-push main && <check> && stash-pop main' chain is ungated" \
    "./.loom/scripts/worktree.sh stash-push main && shellcheck install.sh; ./.loom/scripts/worktree.sh stash-pop main" "$ST_REPO"

assert_allow "stash-scope: git stash pop in a linked worktree cwd allows (#4281)" \
    "git stash pop" "$ST_WT_DIR"
assert_allow "stash-scope: git stash drop in a linked worktree cwd allows (#4281)" \
    "git stash drop" "$ST_WT_DIR"
assert_allow "stash-scope: git stash clear in a linked worktree cwd allows (#4281)" \
    "git stash clear" "$ST_WT_DIR"

# Non-destructive stash subcommands never remove an entry from the stack, so
# they stay ungated even in the main checkout — including the bare `git stash`
# form, which defaults to `push`.
assert_allow "stash-scope: git stash push in main checkout stays ungated" \
    "git stash push -m wip" "$ST_REPO"
assert_allow "stash-scope: git stash apply in main checkout stays ungated" \
    "git stash apply" "$ST_REPO"
assert_allow "stash-scope: git stash list in main checkout stays ungated" \
    "git stash list" "$ST_REPO"
assert_allow "stash-scope: bare git stash (defaults to push) in main checkout stays ungated" \
    "git stash" "$ST_REPO"

# Chained form: a stash pop after a read-only prefix must still be caught —
# proves the check runs against the full command, not just a first-token match.
assert_ask "stash-scope: chained 'git status && git stash pop' still asks in main checkout (#4281)" \
    "git status && git stash pop" "$ST_REPO"

# --- #5783: backtick / no-space-$(...) command substitution no longer evades
# the stash-scope pre-check + recovery-subcommand check ---
#
# Both checks' leading boundary used to be `(^|[;&|(]|[[:space:]])` — no
# backtick — so any of these three shapes were entirely invisible to the
# main-checkout stash-scope ask (silently ALLOWED, a real narrowing gap). The
# recovery-subcommand check's trailing boundary was ALSO too narrow
# (`([[:space:]]|$)`, no `)` and no backtick), which independently missed a
# no-space closer even once the leading half was fixed.
assert_ask "#5783: backtick-wrapped git stash pop asks in main checkout" \
    'echo `git stash pop`' "$ST_REPO"
assert_ask "#5783: backtick-wrapped git stash drop asks in main checkout" \
    'echo `git stash drop`' "$ST_REPO"
assert_ask "#5783: backtick-wrapped git stash clear asks in main checkout" \
    'echo `git stash clear`' "$ST_REPO"
assert_ask "#5783: 'VAR=\`git stash pop\`' assignment form asks in main checkout" \
    'X=`git stash pop`' "$ST_REPO"
assert_ask "#5783: no-space \$(git stash pop) asks in main checkout" \
    'echo $(git stash pop)' "$ST_REPO"

# The same backtick/worktree-cwd resolution as the unwrapped form: still
# scoped to the MAIN checkout only, a linked worktree cwd stays ungated.
assert_allow "#5783: backtick-wrapped git stash pop allows from a linked worktree cwd" \
    'echo `git stash pop`' "$ST_WT_DIR"

# Non-destructive stash subcommands wrapped in backticks must stay ungated
# too — the fix widens the boundary class, not the recovery-subcommand set.
assert_allow "#5783: backtick-wrapped git stash list stays ungated in main checkout" \
    'echo `git stash list`' "$ST_REPO"
assert_allow "#5783: backtick-wrapped git stash apply stays ungated in main checkout" \
    'echo `git stash apply`' "$ST_REPO"

# --- #5783: a backtick appearing only as inert, quoted documentation text
# (e.g. a gh issue/pr comment body citing an example command) must NOT become
# a new false ask — narrows, never widens, applies to single-quoted flag
# values exactly like it already does for other ASK-tier phrases (#3679). ---
assert_allow "#5783: single-quoted --body citing a backtick-wrapped 'git stash pop' example stays allowed" \
    "gh issue comment 1 --body 'quoting \`git stash pop\` as an example, not running it'" "$ST_REPO"
assert_allow "#5783: single-quoted -m citing a backtick-wrapped 'git clean -fd' example stays allowed" \
    "git commit -m 'mentions \`git clean -fd\` in the changelog text'" "$ST_REPO"

# --- #6501: the main-checkout ask names safe-stash-pop.sh as the recommended
# path for a POP, mirroring how stash-scope:create-redirect names
# worktree.sh snapshot/stash-push. The hint is printed only when the wrapper
# provably exists under the main checkout (same "never name a replacement that
# isn't there" discipline as the create-side redirect), and only for `pop` --
# `drop`/`clear` destroy an entry outright and have no safe equivalent. The
# verdict stays ASK either way: refs/stash has no sanctioned reader other than
# a pop, so a deny would strand work rather than protect it. ---

# Without the wrapper installed: ask, but no hint naming a nonexistent script.
assert_ask "stash-scope (#6501): pop still asks when safe-stash-pop.sh is absent" \
    "git stash pop" "$ST_REPO"
ST_ASK_NO_WRAPPER="$(run_guard "git stash pop" "$ST_REPO")"
TOTAL=$((TOTAL + 1))
if echo "$ST_ASK_NO_WRAPPER" | grep -q "safe-stash-pop.sh"; then
    FAIL=$((FAIL + 1))
    echo -e "  ${RED}FAIL${NC}: stash-scope (#6501): ask must NOT name safe-stash-pop.sh when it is not installed"
    echo -e "       Got: $ST_ASK_NO_WRAPPER"
else
    PASS=$((PASS + 1))
    echo -e "  ${GREEN}PASS${NC}: stash-scope (#6501): ask does NOT name safe-stash-pop.sh when it is not installed"
fi

# With the wrapper installed under the main checkout: the ask names it.
ST_REPO_WRAPPER=$(make_wt_repo_linked)
mkdir -p "$ST_REPO_WRAPPER/.loom/scripts"
: > "$ST_REPO_WRAPPER/.loom/scripts/safe-stash-pop.sh"
assert_ask_reason_matches "stash-scope (#6501): main-checkout pop ask names safe-stash-pop.sh" \
    "git stash pop" "safe-stash-pop\.sh" "$ST_REPO_WRAPPER"
assert_ask_reason_matches "stash-scope (#6501): the hint explains the rollback-on-conflict contract" \
    "git stash pop" "rolls the tree back" "$ST_REPO_WRAPPER"

# drop/clear have no safe equivalent, so they must NOT be pointed at the
# pop wrapper — they still ask with the original message only.
ST_ASK_DROP="$(run_guard "git stash drop" "$ST_REPO_WRAPPER")"
TOTAL=$((TOTAL + 1))
if echo "$ST_ASK_DROP" | grep -q "safe-stash-pop.sh"; then
    FAIL=$((FAIL + 1))
    echo -e "  ${RED}FAIL${NC}: stash-scope (#6501): 'git stash drop' must not be redirected to the pop wrapper"
    echo -e "       Got: $ST_ASK_DROP"
else
    PASS=$((PASS + 1))
    echo -e "  ${GREEN}PASS${NC}: stash-scope (#6501): 'git stash drop' is not redirected to the pop wrapper"
fi

# Invoking the wrapper itself is not a raw stash command, so it never trips
# this ask — the same property worktree.sh stash-pop already has.
assert_allow "stash-scope (#6501): invoking safe-stash-pop.sh in the main checkout stays ungated" \
    "./.loom/scripts/safe-stash-pop.sh --json" "$ST_REPO_WRAPPER"

rm -rf "$ST_REPO_WRAPPER"

# Toggle opt-out: guards.stashScope:false / LOOM_GUARD_STASH_SCOPE=0 (default on).
ST_REPO_OFF=$(make_wt_repo_linked)
mkdir -p "$ST_REPO_OFF/.loom"
printf '%s' '{"guards":{"stashScope":false}}' > "$ST_REPO_OFF/.loom/config.json"
assert_allow "stash-scope: guards.stashScope:false -> allow in main checkout" \
    "git stash pop" "$ST_REPO_OFF"
assert_allow_env "stash-scope: LOOM_GUARD_STASH_SCOPE=0 -> allow in main checkout" \
    "LOOM_GUARD_STASH_SCOPE=0" "git stash pop" "$ST_REPO"
assert_ask_env "stash-scope: LOOM_GUARD_STASH_SCOPE=1 overrides config-off -> ask" \
    "LOOM_GUARD_STASH_SCOPE=1" "git stash pop" "$ST_REPO_OFF"

# Read-only fast path is unaffected: `git status` alone still fast-paths to
# allow (it never reaches the stash check at all).
assert_allow "stash-scope: git status alone still allowed (read-only fast path unaffected)" \
    "git status" "$ST_REPO"

# --- Ask-tier positional-argument masking false-positive regressions (#5235) ----
#
# COMMAND_ASK_SCAN (which every ASK_PATTERNS entry, including
# stash-scope:main-checkout, matches against) used to have NO positional-
# argument masking at all -- strip_literal_text() is keyed only on a fixed
# set of named flags (--body/-m/--title/--notes/--comment), so a script with
# a purely POSITIONAL signature (no flags) never triggered it. This is the
# same class of bug #5155/#5160 already fixed for guard-loom-workflow.sh's
# gh-pr-merge-redirect scan; mask_ask_positional_args() (issue #5235) closes
# the analogous gap here for check-duplicate.sh. Reuses ST_REPO (main
# checkout cwd) so `git stash pop/drop/clear` quoted as inert prose
# exercises the real stash-scope:main-checkout ask this bug used to
# false-trigger.

assert_allow "ask-tier (#5235): check-duplicate.sh positional TITLE/DESCRIPTION quoting 'git stash pop' as inert prose no longer asks" \
    './.loom/scripts/check-duplicate.sh "Guard false positive: stash-scope redirect" "quotes git stash pop as inert text, not a live invocation"' "$ST_REPO"

# VERDICT CHANGED by #5263. grep/rg are still NOT in the ask-tier positional-arg
# allowlist (COMMAND_ASK_SCAN also feeds the SQL DDL/DML check, so a grep's own
# quoted search pattern is deliberately still scanned once a command reaches the
# full path). This case USED to `cat`-pipe the grep specifically to disqualify
# the #3687 read-only fast path and thereby REACH the ask-tier scan, so it asked.
# #5263 added a narrow search-pipe carve-out: `grep|egrep|fgrep|rg … | (read-only
# sink)` is now fast-pathed to a silent allow, because a read-only search piped to
# a pager/counter is 100% read-only — the quoted phrase is inert search text grep
# never executes. So `grep -n "…git stash pop…" file | cat` now ALLOWS silently.
# This is the same false-positive class #5263 fixes for SQL-DDL, applied to the
# stash-scope phrase, and is correct: no real stash operation runs. The two
# regression guards below still prove a REAL invocation (a `&&`-chained stash pop,
# an `echo … | bash`) is unaffected and still asks — the carve-out only admits the
# search-to-sink shape, not chains or non-search upstreams.
assert_allow_silent "ask-tier (#5235/#5263): grep -n search quoting 'git stash pop' piped to cat now fast-paths (read-only search-pipe carve-out)" \
    'grep -n "this example mentions git stash pop mid-sentence" defaults/hooks/guard-destructive-generic.sh | cat' "$ST_REPO"

# Regression guard: masking a matched positional span must not blind the
# ask-tier scan to a SECOND, REAL invocation elsewhere on the same command
# line -- masking only narrows what THIS check misses inside the matched
# check-duplicate.sh argument, it never widens what it misses outside that
# span.
assert_ask_reason_matches "ask-tier (#5235): still asks on a REAL git stash pop chained after a masked check-duplicate.sh call" \
    './.loom/scripts/check-duplicate.sh "title" "this example mentions git stash pop mid-sentence" && git stash pop' \
    "MAIN checkout" "$ST_REPO"

# Regression guard: a command NOT in the positional-arg allowlist (echo) must
# leave the phrase fully visible -- the allowlist narrows, it never widens.
assert_ask_reason_matches "ask-tier (#5235): still asks when phrase is quoted in an echo argument (echo not allowlisted)" \
    'echo "this example mentions git stash pop mid-sentence" | bash' \
    "MAIN checkout" "$ST_REPO"

# --- #7363: stash-scope-specific grep/awk positional-pattern masking -------
#
# Unlike COMMAND_ASK_SCAN (which stays unmasked for grep/rg above so
# SQL_DDL_PATTERN keeps seeing a grep's own quoted pattern -- see the #5235
# comment just above), the `_stash_is_recover`/`_stash_is_pop`/
# stash_create_invoked() detectors scan a SEPARATE, more-aggressively-masked
# copy (COMMAND_STASH_SCAN) that DOES mask grep/egrep/fgrep/rg/awk's own
# quoted pattern/program argument, because those detectors have no competing
# raw-text consumer to protect. Guard-decision telemetry
# (`.loom/logs/guard-decisions.log`) caught two real false-triggers: a
# read-only `grep`/`awk` search for a TEST-CASE NAME that happens to contain
# the literal substring "git stash pop", misread as a live invocation. The
# first case below is the EXACT repro from #7363 (a double-quoted grep
# pattern containing a backslash-escaped inner `"`, which an escape-UNAWARE
# quote scan would misparse, truncating the "masked" span early and leaving
# the dangerous-looking suffix visible).
assert_allow "stash-scope (#7363): grep search for a test-case name containing a backslash-escaped quote and 'git stash pop' as literal text no longer asks (main checkout)" \
    'grep -n "^assert_ask \"stash-scope: git stash pop in main checkout asks" tests/hooks/test-guard-destructive.sh' "$ST_REPO"

assert_allow "stash-scope (#7363): awk search (single-quoted program) for a phrase containing 'git stash pop' no longer asks (main checkout)" \
    "awk '/stash-scope: git stash pop in main checkout asks/{print NR\": \"\$0}' tests/hooks/test-guard-destructive.sh" "$ST_REPO"

assert_allow "stash-scope (#7363): grep search quoting 'git stash drop' as literal text no longer asks (main checkout)" \
    'grep -n "test name mentions git stash drop mid-sentence" tests/hooks/test-guard-destructive.sh' "$ST_REPO"

assert_allow "stash-scope (#7363): grep search quoting 'git stash clear' as literal text no longer asks (main checkout)" \
    'grep -n "test name mentions git stash clear mid-sentence" tests/hooks/test-guard-destructive.sh' "$ST_REPO"

# Edge case required by #7363's acceptance criteria: a quoted string followed
# by a REAL trailing stash-recovery invocation on the same (multi-line)
# command must still ask -- masking only narrows what THIS check misses
# inside the matched grep/awk argument span, it never widens what remains
# visible outside it.
assert_ask_reason_matches "stash-scope (#7363): still asks on a REAL git stash pop chained (newline-separated) after a masked grep search" \
    'grep -n "^assert_ask \"stash-scope: git stash pop in main checkout asks" tests/hooks/test-guard-destructive.sh
git stash pop' \
    "MAIN checkout" "$ST_REPO"

# --- #7516: mask_ask_positional_args() escape-aware double-quote scanning --
#
# mask_ask_positional_args() builds COMMAND_ASK_SCAN, which every
# ASK_PATTERNS entry (including stash-scope:main-checkout) matches against.
# Unlike mask_stash_scan_positional_args() above (fixed for #7363), this
# function used a plain same-character double-quote scan that closes on the
# FIRST raw `"` regardless of a preceding backslash. The repro below mirrors
# #7363's shape but through check-duplicate.sh's TITLE/DESCRIPTION signature
# (the only allowlisted positional-arg command on this working copy — grep/rg
# stay excluded here since COMMAND_ASK_SCAN also feeds SQL_DDL_PATTERN, see
# mask_ask_positional_args()'s own header comment): a DESCRIPTION containing a
# backslash-escaped inner `"` around the literal phrase "git stash pop". A
# naive same-character scan stops at that escaped quote, leaving "git stash
# pop" fully visible past the truncation point and false-triggering
# stash-scope:main-checkout even though the phrase is only ever inert dedup
# text, never a live invocation.
assert_allow "ask-tier (#7516): check-duplicate.sh DESCRIPTION with a backslash-escaped inner quote around 'git stash pop' no longer asks (main checkout)" \
    './.loom/scripts/check-duplicate.sh "title" "test name mentions \"git stash pop\" mid-sentence"' "$ST_REPO"

# Fail-closed floor case (acceptance criteria): an escaped BACKSLASH
# (`\\`) immediately precedes the real closing quote. The escape-aware scan
# must consume the `\\` as one atomic two-character unit and correctly land
# on the following char as the real closing `"` — not skip past it — so a
# genuine ask-triggering invocation chained after the masked argument is
# still fully visible and still asks.
assert_ask_reason_matches "ask-tier (#7516): still asks on a REAL git stash pop chained after a masked check-duplicate.sh call whose DESCRIPTION ends in an escaped backslash" \
    './.loom/scripts/check-duplicate.sh "title" "description ends with an escaped backslash\\" && git stash pop' \
    "MAIN checkout" "$ST_REPO"

rm -rf "$ST_REPO" "$ST_REPO_OFF"

echo ""

# =========================================================================
echo -e "${YELLOW}--- Stash-stack scope: worktree-to-worktree collision (#4821) ---${NC}"
# =========================================================================
#
# refs/stash is a SINGLE stack shared across every linked worktree of a repo
# (not per-worktree, despite the intuitive naming) -- so two parallel
# Builders each in a DIFFERENT linked worktree (neither one the main
# checkout) can pop/drop each other's WIP. The main-checkout-only branch
# above never asks in this configuration. With >=2 `.loom-managed`
# worktrees active, a pop/drop/clear from ANY linked worktree cwd must ask;
# with only ONE managed worktree (the existing block above), there is no
# other worktree to collide with, so it stays ungated.

# A real `.loom/scripts/worktree.sh` is provisioned here (#5754): the
# create-side redirect only denies when the safe equivalent it names actually
# exists on disk, so without this file the fixture would silently exercise the
# "no alternative available -> allow" path instead of the guarded one. See
# make_wt_repo_two_linked_no_helper below for the deliberate negative control.


ST2_REPO=$(make_wt_repo_two_linked)
ST2_WT1_DIR="$ST2_REPO/.loom/worktrees/issue-1"
ST2_WT2_DIR="$ST2_REPO/.loom/worktrees/issue-2"

assert_ask "stash-scope: git stash pop from worktree-1 asks when >=2 managed worktrees exist (#4821)" \
    "git stash pop" "$ST2_WT1_DIR"
assert_ask "stash-scope: git stash drop from worktree-2 asks when >=2 managed worktrees exist (#4821)" \
    "git stash drop" "$ST2_WT2_DIR"
assert_ask "stash-scope: git stash clear from a linked worktree asks when >=2 managed worktrees exist (#4821)" \
    "git stash clear" "$ST2_WT1_DIR"

# Stack-neutral subcommands remain ungated even with >=2 managed worktrees.
# `push`/`save` USED to sit in this list; they moved to the create-redirect
# deny in #5754 (see the dedicated section further below) because putting an
# entry ON the shared stack is the half of the cycle that creates the
# collision hazard in the first place.
assert_allow "stash-scope: git stash apply from worktree stays ungated even with >=2 managed worktrees (#4821)" \
    "git stash apply" "$ST2_WT1_DIR"
assert_allow "stash-scope: git stash list from worktree stays ungated even with >=2 managed worktrees (#4821)" \
    "git stash list" "$ST2_WT1_DIR"

# The main checkout still asks via the original main-checkout branch,
# independently of the worktree-collision branch (either condition alone
# is sufficient to ask).
assert_ask "stash-scope: git stash pop in main checkout still asks with >=2 managed worktrees (#4821)" \
    "git stash pop" "$ST2_REPO"

# Toggle opt-out also covers the worktree-collision branch. Config is
# resolved from REPO_ROOT = `git rev-parse --show-toplevel` of the command's
# CWD, which for a worktree CWD is the worktree's own root, NOT the main
# checkout -- so the config file must live in the WORKTREE's own (nested)
# `.loom/config.json`, mirroring how a real committed .loom/config.json
# would appear in every checkout of the same tracked path.
ST2_REPO_OFF=$(make_wt_repo_two_linked)
mkdir -p "$ST2_REPO_OFF/.loom/worktrees/issue-1/.loom"
printf '%s' '{"guards":{"stashScope":false}}' > "$ST2_REPO_OFF/.loom/worktrees/issue-1/.loom/config.json"
assert_allow "stash-scope: guards.stashScope:false -> allow from worktree even with >=2 managed worktrees (#4821)" \
    "git stash pop" "$ST2_REPO_OFF/.loom/worktrees/issue-1"

# --- #7363: same grep/awk positional-pattern masking, but from a LINKED
# worktree cwd with >=2 managed worktrees -- exercises the
# stash-scope:worktree-collision ask (the exact one #7363's guard-decision
# telemetry caught false-triggering), not just the main-checkout ask covered
# above.
assert_allow "stash-scope (#7363): grep search from a linked worktree for a test-case name containing 'git stash pop' no longer asks (worktree-collision, >=2 managed worktrees)" \
    'grep -n "^assert_ask \"stash-scope: git stash pop in main checkout asks" tests/hooks/test-guard-destructive.sh' "$ST2_WT1_DIR"

assert_allow "stash-scope (#7363): awk search from a linked worktree for a phrase containing 'git stash drop' no longer asks (worktree-collision, >=2 managed worktrees)" \
    "awk '/test name mentions git stash drop mid-sentence/{print}' tests/hooks/test-guard-destructive.sh" "$ST2_WT2_DIR"

# Edge case: the same trailing-real-invocation guard, from a linked worktree
# this time -- masking a matched grep span must not blind the
# worktree-collision ask to a REAL 'git stash pop' chained after it.
assert_ask_reason_matches "stash-scope (#7363): still asks on a REAL git stash pop chained after a masked grep search from a linked worktree (worktree-collision)" \
    'grep -n "^assert_ask \"stash-scope: git stash pop in main checkout asks" tests/hooks/test-guard-destructive.sh
git stash pop' \
    "ANOTHER builder's WIP" "$ST2_WT1_DIR"

rm -rf "$ST2_REPO" "$ST2_REPO_OFF"

echo ""

# =========================================================================
echo -e "${YELLOW}--- Stash-stack scope: cd-prefix threading (#5173) ---${NC}"
# =========================================================================
#
# Regression: the hook's reported session cwd can still be the MAIN repo root
# while the COMMAND itself first `cd`s into a linked worktree and restores a
# stash entry there — a routine, safe operation per this repo's own CLAUDE.md
# worktree workflow (`cd .loom/worktrees/issue-N && git stash pop`). Before
# the #5173 fix, main-checkout/worktree-collision scope resolution fell back
# to the raw session cwd whenever no `cd` prefix was accounted for, so it
# queried the MAIN checkout (protected) instead of the worktree the command
# actually targets, and incorrectly asked citing the main checkout. Mirrors
# the fixture pattern from #5156/PR #5161's cd-tracking fix for
# parse_force_ops(). A REAL linked `git worktree add` fixture is used (not a
# plain subdirectory) so the worktree genuinely has its own toplevel/common-dir
# divergence, mirroring make_wt_repo_linked above.

CD_ST_REPO=$(make_wt_repo_linked)
CD_ST_WT_DIR="$CD_ST_REPO/.loom/worktrees/issue-1"

# Hook cwd = MAIN repo root; command cd's into the worktree, then restores a
# stash entry there -> must ALLOW (the false-ask this issue fixes). Only ONE
# managed worktree exists, so the worktree-collision branch (#4821) must not
# fire either.
assert_allow "stash-scope (#5173): cd into worktree then stash pop allows (hook cwd=main root)" \
    "cd $CD_ST_WT_DIR && git stash pop" "$CD_ST_REPO"
assert_allow "stash-scope (#5173): cd into worktree then stash drop allows (hook cwd=main root)" \
    "cd $CD_ST_WT_DIR && git stash drop" "$CD_ST_REPO"
assert_allow "stash-scope (#5173): cd into worktree then stash clear allows (hook cwd=main root)" \
    "cd $CD_ST_WT_DIR && git stash clear" "$CD_ST_REPO"
# A read-only prefix ahead of the cd must not break resolution.
assert_allow "stash-scope (#5173): chained 'cd <worktree> && git status && git stash pop' allows (hook cwd=main root)" \
    "cd $CD_ST_WT_DIR && git status && git stash pop" "$CD_ST_REPO"

# Same effective operation with the hook cwd already AT the worktree -> must
# also ALLOW (already correct pre-fix; kept as a matching control, #5161-style).
assert_allow "stash-scope (#5173): cd into worktree (redundant) then stash pop allows (hook cwd=worktree already)" \
    "cd $CD_ST_WT_DIR && git stash pop" "$CD_ST_WT_DIR"

# Control: cd-ing BACK into the main (protected) checkout root must still ASK
# citing the main-checkout reason -- the fix must never widen an allow past a
# genuine main-checkout stash restore.
assert_ask_reason_matches "stash-scope (#5173): cd into main root then stash pop still asks (hook cwd=worktree)" \
    "cd $CD_ST_REPO && git stash pop" "MAIN checkout" "$CD_ST_WT_DIR"

# Control: cd into a directory that does not exist / is not a git checkout
# must stay ambiguous -> ASK, never silently allow ("never widen a deny/ask
# into an allow").
assert_ask_reason_matches "stash-scope (#5173): cd into an unresolvable directory still asks (ambiguous)" \
    "cd /nonexistent-dir-5173-does-not-exist && git stash pop" "could not be resolved" "$CD_ST_REPO"

# #5315: the SAME cd-tracking here now tilde/$HOME-expands its argument via
# expand_cd_arg(). With HOME set to the main repo root, `cd ~/.loom/worktrees/
# issue-1 && git stash pop` must resolve into the worktree exactly like the
# literal-path control above -> ALLOW. Pre-#5315 the literal `~` join produced a
# bogus curcwd whose toplevel could not be resolved -> a spurious ask.
assert_allow_env "stash-scope (#5315): 'cd ~/.loom/worktrees/issue-1 && git stash pop' (HOME=main root) resolves into worktree, allows" \
    "HOME=$CD_ST_REPO" "cd ~/.loom/worktrees/issue-1 && git stash pop" "$CD_ST_REPO"
# Control: cd back into the main checkout via a bare `~` must still ASK -- the
# expansion must never widen an ask into an allow. (assert_ask_env sets HOME;
# the reason-matching variant has no env parameter, so ask-only is asserted.)
assert_ask_env "stash-scope (#5315): 'cd ~ && git stash pop' (HOME=main root) still asks (no widening)" \
    "HOME=$CD_ST_REPO" "cd ~ && git stash pop" "$CD_ST_WT_DIR"
# Control: a QUOTED tilde is not expanded -> bogus literal curcwd -> ambiguous
# -> ASK (fail-closed), never silently allowed.
assert_ask_env "stash-scope (#5315): 'cd '\''~/.loom/worktrees/issue-1'\''' (quoted tilde stays literal) still asks (ambiguous)" \
    "HOME=$CD_ST_REPO" "cd '~/.loom/worktrees/issue-1' && git stash pop" "$CD_ST_REPO"

# #5372: resolve_stash_cwd()'s `cd`-argument classification now reuses
# strip_cd_quoting() (#5363), mirroring extract_write_targets() and
# parse_force_ops() (above). A FULLY quoted absolute `cd` argument
# ('<worktree>' / "<worktree>") starts with a quote character rather than
# `/`, so the pre-#5372 naive `~ /^\//` test misclassified it RELATIVE and
# joined it onto curcwd instead of recognizing it as absolute -- the
# resolved toplevel could not be found and the guard fell back to ASK
# (fail-closed, never a bypass). Post-fix it correctly resolves into the
# worktree -> ALLOW.
for _q5372 in "'" '"'; do
    assert_allow "stash-scope (#5372): cd ${_q5372}-quoted worktree path then stash pop allows (hook cwd=main root)" \
        "cd ${_q5372}$CD_ST_WT_DIR${_q5372} && git stash pop" "$CD_ST_REPO"
done
unset _q5372

# PARTIALLY quoted absolute `cd` argument -- the quote closes MID-TOKEN
# (e.g. '<parent>'/issue-1) -- is also now classified ABSOLUTE (mirrors the
# extract_write_targets() partial-quote fixture, #5363 probe A).
assert_allow "stash-scope (#5372): cd PARTIALLY-quoted worktree path then stash pop allows (hook cwd=main root)" \
    "cd '$CD_ST_REPO/.loom/worktrees'/issue-1 && git stash pop" "$CD_ST_REPO"

# Control: an unbalanced/unterminated quote keeps today's verdict (ASK, with
# the ambiguous-resolution reason) -- strip_cd_quoting()'s fallback contract
# never widens ambiguity into an allow.
assert_ask_reason_matches "stash-scope (#5372): unbalanced leading single-quote in cd argument keeps today's ask" \
    "cd '$CD_ST_WT_DIR && git stash pop" "could not be resolved" "$CD_ST_REPO"

# Control: cd-ing (quoted) BACK into the main (protected) checkout root must
# still ASK citing the main-checkout reason -- the fix must never widen an
# allow past a genuine main-checkout stash restore.
assert_ask_reason_matches "stash-scope (#5372): cd quoted main root then stash pop still asks (hook cwd=worktree)" \
    "cd '$CD_ST_REPO' && git stash pop" "MAIN checkout" "$CD_ST_WT_DIR"

rm -rf "$CD_ST_REPO"

# Worktree-collision (#4821) consistency: the SAME cd-threaded
# _stash_toplevel/_stash_common_parent resolution feeds both checks, so a
# cd-prefixed stash op resolving into a linked worktree (not the main
# checkout) while >=2 managed worktrees are active must ask citing the
# COLLISION reason -- not the main-checkout reason a raw-cwd-only resolution
# would have (incorrectly) produced.
CD_ST2_REPO=$(make_wt_repo_two_linked)
CD_ST2_WT1_DIR="$CD_ST2_REPO/.loom/worktrees/issue-1"
CD_ST2_WT2_DIR="$CD_ST2_REPO/.loom/worktrees/issue-2"

assert_ask_reason_matches "stash-scope (#5173): cd into worktree-1 then stash pop asks with collision reason (hook cwd=main root, >=2 worktrees)" \
    "cd $CD_ST2_WT1_DIR && git stash pop" "ANOTHER builder's WIP" "$CD_ST2_REPO"
assert_ask_reason_matches "stash-scope (#5173): cd from worktree-1 into worktree-2 then stash drop asks with collision reason" \
    "cd $CD_ST2_WT2_DIR && git stash drop" "ANOTHER builder's WIP" "$CD_ST2_WT1_DIR"

# Toggle opt-out also covers the cd-prefixed form (guards.stashScope:false /
# LOOM_GUARD_STASH_SCOPE=0, default on).
mkdir -p "$CD_ST2_REPO/.loom"
printf '%s' '{"guards":{"stashScope":false}}' > "$CD_ST2_REPO/.loom/config.json"
assert_allow "stash-scope (#5173): guards.stashScope:false -> allow for cd-prefixed stash pop into worktree" \
    "cd $CD_ST2_WT1_DIR && git stash pop" "$CD_ST2_REPO"

rm -rf "$CD_ST2_REPO"

# =========================================================================
echo -e "${YELLOW}--- Stash-stack scope: quoted cd argument with an embedded space (#6552) ---${NC}"
# =========================================================================
#
# resolve_stash_cwd()'s per-segment tokenizer used a plain `/[ \t]+/` split,
# which is NOT quote-aware: `cd "<dir with a space>"` truncated at the first
# embedded space, leaving an unterminated-quote fragment (still carrying its
# opening quote) that strip_cd_quoting() correctly declines to unquote
# (#5372's contract), so the fragment was misclassified RELATIVE and joined
# onto the session cwd -- producing a bogus, nonexistent path. With
# _stash_toplevel/_stash_common_parent left empty, the guard fell through to
# the cd-unresolved ASK even though the cd target is a perfectly valid git
# checkout (#6552). Fixed by reusing mask_ws()/unmask_ws() (#4934) -- the
# same technique extract_write_targets() already uses -- to mask whitespace
# INSIDE a quoted span before the split runs, so a quoted argument with an
# embedded space yields exactly ONE token.
#
# Fixture mirrors the issue's own two-repo repro: a REAL linked worktree
# whose full path contains a literal space (a parent directory segment, e.g.
# ".../Real Estate CRM/.loom/worktrees/issue-1"), so the bug reproduces on
# the very first embedded space rather than requiring a specially-crafted
# worktree name.

CD_ST_SPACE_REPO=$(make_wt_repo_linked_spacepath)
CD_ST_SPACE_WT_DIR="$CD_ST_SPACE_REPO/.loom/worktrees/issue-1"

# Hook cwd = MAIN repo root (space-free CONTROL already covered above by
# CD_ST_REPO); command cd's (double-quoted) into the space-containing
# worktree path, then restores a stash entry there -> must ALLOW.
assert_allow "stash-scope (#6552): double-quoted cd into a space-containing worktree path then stash pop allows" \
    "cd \"$CD_ST_SPACE_WT_DIR\" && git stash pop" "$CD_ST_SPACE_REPO"
assert_allow "stash-scope (#6552): double-quoted cd into a space-containing worktree path then stash drop allows" \
    "cd \"$CD_ST_SPACE_WT_DIR\" && git stash drop" "$CD_ST_SPACE_REPO"
assert_allow "stash-scope (#6552): double-quoted cd into a space-containing worktree path then stash clear allows" \
    "cd \"$CD_ST_SPACE_WT_DIR\" && git stash clear" "$CD_ST_SPACE_REPO"

# SINGLE-quoted form must resolve identically.
assert_allow "stash-scope (#6552): single-quoted cd into a space-containing worktree path then stash pop allows" \
    "cd '$CD_ST_SPACE_WT_DIR' && git stash pop" "$CD_ST_SPACE_REPO"

# Control: cd-ing (quoted, space-containing) BACK into the main (protected)
# checkout root must still ASK citing the main-checkout reason -- the fix
# must never widen an allow past a genuine main-checkout stash restore.
assert_ask_reason_matches "stash-scope (#6552): quoted cd into a space-containing main checkout root then stash pop still asks" \
    "cd \"$CD_ST_SPACE_REPO\" && git stash pop" "MAIN checkout" "$CD_ST_SPACE_WT_DIR"

rm -rf "$CD_ST_SPACE_REPO"

echo ""

# =========================================================================
echo -e "${YELLOW}--- Stash-stack scope: worktree-confined baseline stash via worktree.sh stash-push/stash-pop (#5217) ---${NC}"
# =========================================================================
#
# #5217: a legitimate `git stash push && <baseline check> && git stash pop`
# chain — used to diff a clean baseline against WIP (clippy/shellcheck/test
# comparisons) — is correctly gated by stash-scope:worktree-collision
# whenever >=2 managed worktrees are active (nearly always true in this
# repo), producing an unanswerable `ask` in headless mode. The fix is NOT to
# widen the guard's own ask condition (a same-chain push/pop heuristic was
# considered and rejected — see the comment above the worktree-collision ask
# in guard-destructive-generic.sh — because another worktree's concurrent
# `git stash push` can still land on the SHARED stack in the window between
# the two guard-approved Bash calls). Instead, `worktree.sh stash-push` /
# `stash-pop` (added by #5217) never touch `refs/stash` at all — they anchor
# WIP to a PER-ISSUE ref — so invoking them is guard-transparent: the text
# never contains a raw `git stash pop|drop|clear`, so the pattern this block
# scans for never matches. These tests assert BOTH halves of the fix: the
# narrowed-safe path is genuinely available, AND raw git stash usage
# (including a same-chain push/pop, proving the rejected heuristic was NOT
# adopted) is exactly as gated as before.

ST3_REPO=$(make_wt_repo_two_linked)
ST3_WT1_DIR="$ST3_REPO/.loom/worktrees/issue-1"

# The sanctioned replacement commands never literally invoke `git stash
# pop|drop|clear`, so they sail through even with >=2 managed worktrees
# active and cwd inside a linked worktree — the exact configuration that
# asks for raw git stash above.
assert_allow "stash-scope (#5217): worktree.sh stash-push allows from a linked worktree even with >=2 managed worktrees" \
    "./.loom/scripts/worktree.sh stash-push 1" "$ST3_WT1_DIR"
assert_allow "stash-scope (#5217): worktree.sh stash-pop allows from a linked worktree even with >=2 managed worktrees" \
    "./.loom/scripts/worktree.sh stash-pop 1" "$ST3_WT1_DIR"
assert_allow "stash-scope (#5217): chained stash-push, baseline check, stash-pop allows from a linked worktree" \
    "./.loom/scripts/worktree.sh stash-push 1 && cat file.txt && ./.loom/scripts/worktree.sh stash-pop 1" "$ST3_WT1_DIR"
assert_allow "stash-scope (#5217): worktree.sh stash-push --include-untracked allows from a linked worktree" \
    "./.loom/scripts/worktree.sh stash-push 1 --include-untracked" "$ST3_WT1_DIR"

# Control: raw git stash pop/drop/clear from the SAME fixture must still ask,
# unchanged — the new commands are an addition, not a relaxation of the
# existing worktree-collision protection.
assert_ask "stash-scope (#5217): raw git stash pop from a linked worktree still asks with >=2 managed worktrees" \
    "git stash pop" "$ST3_WT1_DIR"
assert_ask "stash-scope (#5217): raw git stash drop from a linked worktree still asks with >=2 managed worktrees" \
    "git stash drop" "$ST3_WT1_DIR"

# Control: the rejected same-chain heuristic must NOT have been adopted — a
# raw `git stash push && <cmd> && git stash pop` chain (the shape the
# original #5217 report described) is still gated, not waved through. Since
# #5754 it is gated at the FRONT of the chain (create-redirect deny) rather
# than at its tail (collision ask): same "not allowed" verdict, but lossless
# and actionable instead of an unanswerable prompt about work already shelved.
assert_deny "stash-scope (#5217/#5754): raw chained 'git stash push && ... && git stash pop' still gated (same-chain heuristic NOT adopted)" \
    "git stash push -u && cat file.txt && git stash pop" "$ST3_WT1_DIR"

# Control: the updated worktree-collision ask message documents BOTH
# sanctioned alternatives (snapshot for ad-hoc WIP, stash-push/stash-pop for
# a baseline-diff comparison) so a headless sweep that hits the ask can see
# the guard-transparent path without a human needing to explain it.
assert_ask_reason_matches "stash-scope (#5217): worktree-collision ask message documents the stash-push/stash-pop alternative" \
    "git stash pop" "stash-push.*stash-pop" "$ST3_WT1_DIR"

rm -rf "$ST3_REPO"

echo ""

# =========================================================================
echo -e "${YELLOW}--- Stash-stack scope: create-side redirect (#5754) ---${NC}"
# =========================================================================
#
# Guard-decision telemetry over 2026-08-04..08 showed 32 stash-scope asks
# (~7.2/day), ALL of them after the role-prompt guidance and the guard's own
# inline suggestion text had already landed. Classifying them by chain shape
# showed the guard was gated on the wrong half of the stash cycle: 15/32
# chained a CREATE and a RECOVERY in one command, so the guard only spoke up
# at the pop — about a decision made at the head of the same chain — while
# 11/32 were RECOVERY-ONLY, i.e. WIP already stranded on the shared stack by
# an earlier, silently-allowed create.
#
# So the CREATE is denied (lossless: the working tree is untouched, the agent
# just reruns with the named per-issue command, and no entry ever reaches the
# shared stack), while pop/drop/clear deliberately stay at ASK — `git stash
# pop` is the only reader of `refs/stash` (worktree.sh's stash-pop reads a
# per-issue ref instead), so denying it would strand work with no recovery
# path rather than protect it.
#
# The deny is narrow by construction: linked worktree only, `.loom-managed`
# sentinel present, `issue-<N>` directory name, a real worktree.sh on disk,
# and >=2 managed worktrees — the same collision predicate as the ask.

ST4_REPO=$(make_wt_repo_two_linked)
ST4_WT1_DIR="$ST4_REPO/.loom/worktrees/issue-1"
ST4_WT2_DIR="$ST4_REPO/.loom/worktrees/issue-2"

# Every create spelling is redirected.
assert_deny "stash-scope (#5754): bare 'git stash' from a linked worktree denies" \
    "git stash" "$ST4_WT1_DIR"
assert_deny "stash-scope (#5754): 'git stash push -m wip' from a linked worktree denies" \
    "git stash push -m wip" "$ST4_WT1_DIR"
assert_deny "stash-scope (#5754): 'git stash push -- <file>' from a linked worktree denies" \
    "git stash push -- defaults/scripts/tests/t.sh" "$ST4_WT1_DIR"
assert_deny "stash-scope (#5754): 'git stash save <msg>' from a linked worktree denies" \
    "git stash save wip" "$ST4_WT1_DIR"
assert_deny "stash-scope (#5754): option-prefixed create 'git stash -u' from a linked worktree denies" \
    "git stash -u" "$ST4_WT1_DIR"
assert_deny "stash-scope (#5754): 'git stash --include-untracked' from a linked worktree denies" \
    "git stash --include-untracked" "$ST4_WT1_DIR"

# #5783: stash_create_invoked()'s own leading/subcommand/trailing boundary
# classes had the identical backtick gap as the pre-check above — a
# backtick-wrapped create was invisible to the outer pre-check (so the whole
# block was skipped) AND, even once that is fixed, the subcommand token
# extraction would swallow a closing backtick into the token itself
# (`push\``, which does not equal `push`) without its own fix.
assert_deny "#5783: backtick-wrapped 'git stash push' from a linked worktree denies" \
    'echo `git stash push`' "$ST4_WT1_DIR"

# The exact shape the telemetry is full of: create at the head of the chain,
# recovery at its tail. The DENY must win, so the agent is stopped before it
# shelves anything rather than prompted afterwards.
assert_deny_reason_matches "stash-scope (#5754): 'git stash && <check>; git stash pop' denies at the create, not asks at the pop" \
    "git stash && bash defaults/scripts/tests/t.sh; git stash pop" "Blocked:" "$ST4_WT1_DIR"

# The message must name the literal per-issue commands — the whole point of
# the change is that the caller does not have to look up or fill in an
# `<issue-number>` placeholder to comply.
assert_deny_reason_matches "stash-scope (#5754): deny message interpolates the real issue number into snapshot" \
    "git stash" "worktree\.sh snapshot 1" "$ST4_WT1_DIR"
assert_deny_reason_matches "stash-scope (#5754): deny message interpolates the real issue number into stash-push/stash-pop" \
    "git stash" "worktree\.sh stash-push 1.*worktree\.sh stash-pop 1" "$ST4_WT1_DIR"
assert_deny_reason_matches "stash-scope (#5754): deny message states nothing was run (the deny is lossless)" \
    "git stash" "working tree is untouched" "$ST4_WT1_DIR"
assert_deny_reason_matches "stash-scope (#5754): deny from worktree-2 names worktree-2's own issue number" \
    "git stash" "worktree\.sh snapshot 2" "$ST4_WT2_DIR"

# Recovery stays an ASK, never a deny — popping is the only way back for WIP
# that is already on the shared stack.
assert_ask "stash-scope (#5754): git stash pop stays an ask, NOT escalated to deny" \
    "git stash pop" "$ST4_WT1_DIR"
assert_ask "stash-scope (#5754): git stash drop stays an ask, NOT escalated to deny" \
    "git stash drop" "$ST4_WT1_DIR"
assert_ask "stash-scope (#5754): git stash clear stays an ask, NOT escalated to deny" \
    "git stash clear" "$ST4_WT1_DIR"

# Stack-neutral and plumbing subcommands are untouched. `git stash create` in
# particular MUST allow: it is exactly what worktree.sh's own stash-push runs,
# so matching it would deny the sanctioned replacement path itself.
assert_allow "stash-scope (#5754): git stash create allows (worktree.sh stash-push uses it internally)" \
    "git stash create" "$ST4_WT1_DIR"
assert_allow "stash-scope (#5754): git stash apply allows" \
    "git stash apply" "$ST4_WT1_DIR"
assert_allow "stash-scope (#5754): git stash list allows" \
    "git stash list" "$ST4_WT1_DIR"
assert_allow "stash-scope (#5754): git stash show allows" \
    "git stash show" "$ST4_WT1_DIR"
assert_allow "stash-scope (#5754): git stash --help allows" \
    "git stash --help" "$ST4_WT1_DIR"
# Token boundary: `stash` must be a whole word, or `git stashx` would be
# misread as a bare create.
assert_allow "stash-scope (#5754): 'git stashx' is not a stash create" \
    "git stashx" "$ST4_WT1_DIR"

# The sanctioned replacements stay guard-transparent — they never mention a
# raw stash verb, so nothing about #5754 makes them harder to call.
assert_allow "stash-scope (#5754): worktree.sh stash-push still allows unaffected" \
    "./.loom/scripts/worktree.sh stash-push 1" "$ST4_WT1_DIR"
assert_allow "stash-scope (#5754): worktree.sh stash-pop still allows unaffected" \
    "./.loom/scripts/worktree.sh stash-pop 1" "$ST4_WT1_DIR"
assert_allow "stash-scope (#5754): worktree.sh snapshot still allows unaffected" \
    "./.loom/scripts/worktree.sh snapshot 1" "$ST4_WT1_DIR"

# MAIN CHECKOUT: no `worktree.sh stash-push` equivalent exists for it, so a
# raw create there has nothing to be redirected to and must stay ALLOWED,
# byte-for-byte as before. Only the recovery half is gated in the main
# checkout — that behaviour is unchanged.
assert_allow "stash-scope (#5754): bare 'git stash' in the MAIN checkout still allows (no per-issue equivalent exists)" \
    "git stash" "$ST4_REPO"
assert_allow "stash-scope (#5754): 'git stash push -m wip' in the MAIN checkout still allows" \
    "git stash push -m wip" "$ST4_REPO"
assert_ask_reason_matches "stash-scope (#5754): main-checkout stash pop still asks (recovery half unchanged)" \
    "git stash pop" "MAIN checkout" "$ST4_REPO"

# cd-prefix threading reaches the create redirect too: the hook's session cwd
# is the main root while the command cd's into a worktree first — the dominant
# shape in the telemetry (`cd .loom/worktrees/issue-N && git stash && ...`).
assert_deny_reason_matches "stash-scope (#5754): cd into worktree then 'git stash' denies with that worktree's issue number (hook cwd=main root)" \
    "cd $ST4_WT2_DIR && git stash && bash t.sh; git stash pop" "worktree\.sh snapshot 2" "$ST4_REPO"

# The toggle covers the new deny, exactly like the asks.
assert_allow_env "stash-scope (#5754): LOOM_GUARD_STASH_SCOPE=0 -> allow for a worktree stash create" \
    "LOOM_GUARD_STASH_SCOPE=0" "git stash" "$ST4_WT1_DIR"

rm -rf "$ST4_REPO"

ST4_REPO_OFF=$(make_wt_repo_two_linked)
mkdir -p "$ST4_REPO_OFF/.loom/worktrees/issue-1/.loom"
printf '%s' '{"guards":{"stashScope":false}}' > "$ST4_REPO_OFF/.loom/worktrees/issue-1/.loom/config.json"
assert_allow "stash-scope (#5754): guards.stashScope:false -> allow for a worktree stash create" \
    "git stash" "$ST4_REPO_OFF/.loom/worktrees/issue-1"
rm -rf "$ST4_REPO_OFF"

# Negative control 1: only ONE managed worktree active. Nothing to collide
# with, so the create stays ungated — the deny fires on exactly the same
# predicate as the collision ask, never wider.
ST4_SOLO=$(make_wt_repo_linked)
mkdir -p "$ST4_SOLO/.loom/scripts"
printf '#!/usr/bin/env bash\n' > "$ST4_SOLO/.loom/scripts/worktree.sh"
assert_allow "stash-scope (#5754): 'git stash' from the ONLY managed worktree stays ungated" \
    "git stash" "$ST4_SOLO/.loom/worktrees/issue-1"
rm -rf "$ST4_SOLO"

# Negative control 2: no `.loom/scripts/worktree.sh` on disk. There is no safe
# equivalent to redirect to, so denying would leave the caller with no path at
# all — behaviour must be unchanged (allow), while the recovery ask, which
# does not depend on the helper, still fires.
ST4_NOHELPER=$(make_wt_repo_two_linked_no_helper)
assert_allow "stash-scope (#5754): 'git stash' allows when worktree.sh is absent (no safe equivalent to name)" \
    "git stash" "$ST4_NOHELPER/.loom/worktrees/issue-1"
assert_ask "stash-scope (#5754): worktree-collision ask is independent of worktree.sh being present" \
    "git stash pop" "$ST4_NOHELPER/.loom/worktrees/issue-1"
rm -rf "$ST4_NOHELPER"

# Negative control 3: a `.loom-managed` worktree whose directory name yields
# no issue number cannot be given a literal replacement command, so it is not
# denied (a message with an unfillable placeholder is the friction #5754 is
# removing, not a fix).
ST4_UNNAMED=$(make_wt_repo_two_linked)
git -C "$ST4_UNNAMED" worktree add -q "$ST4_UNNAMED/.loom/worktrees/scratch" \
    -b feature/scratch >/dev/null 2>&1
: > "$ST4_UNNAMED/.loom/worktrees/scratch/.loom-managed"
assert_allow "stash-scope (#5754): 'git stash' allows from a managed worktree with no issue-<N> name" \
    "git stash" "$ST4_UNNAMED/.loom/worktrees/scratch"
rm -rf "$ST4_UNNAMED"

# Negative control 4: a linked worktree WITHOUT the `.loom-managed` sentinel
# is user-provisioned, not Loom's to redirect.
ST4_UNMANAGED=$(make_wt_repo_two_linked)
git -C "$ST4_UNMANAGED" worktree add -q "$ST4_UNMANAGED/.loom/worktrees/issue-9" \
    -b feature/issue-9 >/dev/null 2>&1
assert_allow "stash-scope (#5754): 'git stash' allows from a linked worktree with no .loom-managed sentinel" \
    "git stash" "$ST4_UNMANAGED/.loom/worktrees/issue-9"
rm -rf "$ST4_UNMANAGED"

echo ""

# =========================================================================

print_summary
