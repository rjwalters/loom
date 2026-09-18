#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — qsplit()
# unquoted backslash-newline line-continuation joining (#7945).
#
# Ported from `2AMLogic/sky130-modexp` commit `fdced41` (PR #123, closes
# #121), adapted to the shared tests/hooks/lib/guard-destructive-harness.sh
# fixtures/assertions used by every sibling test-guard-destructive-*.sh suite
# here, rather than sky130-modexp's standalone copy-the-hook TMPROOT harness.
#
# Background: an Auditor guard-decision telemetry review on sky130-modexp
# found the base `worktree-write-confinement` pattern firing a false-positive
# `deny` on a multi-source `cp` whose destination (the last argument, on its
# own `\`-continued line) was correctly and fully confined inside the acting
# worktree, while every *source* argument (reads, not writes) resolved
# outside it.
#
# The `cp`/`mv` branch in extract_write_targets() already only ever treats
# the LAST non-flag argument as the write target when it sees >= 2 non-flag
# arguments in one segment -- that part is correct and unchanged by this fix.
# The actual root cause is upstream, in the shared `qsplit()` command-
# segmentation helper: it copied an embedded `\n` through to the segment
# splitter (`n = split($0, segs, "\n")`) for EVERY reason except the ;/&/|
# separators -- including the newline half of a real shell `\`-continuation,
# which is not a statement boundary at all. That stranded each physical line
# of a `\`-continued `cp`/`mv` invocation into its own bogus one-line
# "segment": the real destination argument never reached the cp/mv branch's
# token list (it sat alone on its own line, with no `cp`/`mv` command word),
# while an EARLIER line ending in `cp <source-path> \` was read as a
# two-non-flag-argument cp invocation (the source path, and the literal
# trailing `\` as a second "argument") -- so the cp/mv branch picked the
# bogus trailing `\` as "the last argument", resolved it relative to curcwd,
# and denied it as a worktree-isolation bypass.
#
# The fix makes qsplit() elide an UNQUOTED `\` immediately followed by `\n`
# (matching real shell line-continuation semantics) -- see that function's
# header comment in guard-destructive-generic.sh for the full contract,
# including why this can only ever narrow a false positive/negative, never
# widen a deny into an allow of something that was never proven safe.
#
# Usage: ./tests/hooks/test-guard-destructive-cp-mv-continuation.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

echo -e "${YELLOW}--- qsplit() backslash-newline line-continuation joining (#7945) ---${NC}"
# =========================================================================

WT_REPO=$(make_wt_repo)
WT_DIR="$WT_REPO/.loom/worktrees/issue-1"

# --- (a) the exact reported false-positive shape: multi-source cp, sources
# outside the worktree (reads), destination (last arg, own continuation
# line) inside the worktree -> must ALLOW.
CMD_A="mkdir -p /tmp/loom-test-$$-continuation
cp /tmp/loom-test-$$-continuation/run-console.txt \\
   /tmp/loom-test-$$-continuation/klt-functional-verification.json \\
   /tmp/loom-test-$$-continuation/gate-level-width-16.jsonl \\
   /tmp/loom-test-$$-continuation/cross-check-console.txt \\
   /tmp/loom-test-$$-continuation/modexp_post_route.v \\
   $WT_DIR/src/"
assert_allow "cp-mv-continuation (a): multi-source continuation-line cp into in-worktree dest -> allow" \
    "$CMD_A" "$WT_REPO"

# --- (b) same shape via `mv` -----------------------------------------------
CMD_B="mv /tmp/loom-test-$$-continuation/run-console.txt \\
   /tmp/loom-test-$$-continuation/klt-functional-verification.json \\
   $WT_DIR/src/"
assert_allow "cp-mv-continuation (b): same shape via mv -> allow" \
    "$CMD_B" "$WT_REPO"

# --- (c) a SINGLE-source continuation-line cp (still 2 real arguments once
# joined: one source, one destination) into the worktree -> allow. This is
# the minimal case that isolates the mechanism: before the fix, the
# continuation newline stranded the destination on its own line (no `cp`
# command word) while the source-only line was misread as a 1-argument cp
# whose sole path became the bogus "last argument" write target.
CMD_C="cp /tmp/loom-test-$$-continuation/run-console.txt \\
   $WT_DIR/src/"
assert_allow "cp-mv-continuation (c): single-source continuation-line cp into in-worktree dest -> allow" \
    "$CMD_C" "$WT_REPO"

# --- SAFETY (d): multi-source continuation-line cp whose ACTUAL destination
# (still the last argument, still on its own continuation line) resolves
# OUTSIDE the worktree -- into the main checkout -- must still DENY. Proves
# the fix only removes the phantom mid-invocation split; it does not weaken
# detection of a genuine out-of-worktree cp/mv destination.
CMD_D="cp /tmp/a.txt \\
   /tmp/b.txt \\
   $WT_REPO/evil-dest/"
assert_deny "cp-mv-continuation (d) SAFETY: continuation-line cp, real destination outside worktree -> still deny" \
    "$CMD_D" "$WT_REPO"

# --- SAFETY (e): same as (d) via mv -----------------------------------------
CMD_E="mv /tmp/a.txt \\
   /tmp/b.txt \\
   $WT_REPO/evil-dest/"
assert_deny "cp-mv-continuation (e) SAFETY: continuation-line mv, real destination outside worktree -> still deny" \
    "$CMD_E" "$WT_REPO"

# --- SAFETY (f): a single-line (non-continued) cp/mv whose destination is
# outside the worktree must still deny, completely unaffected by this fix
# (baseline sanity -- no backslash-newline involved at all).
assert_deny "cp-mv-continuation (f) SAFETY: single-line cp, real destination outside worktree -> still deny (baseline, unaffected)" \
    "cp /tmp/a.txt /tmp/b.txt $WT_REPO/evil-dest/" "$WT_REPO"

# --- SAFETY (g): a `\` that is NOT immediately followed by a newline (e.g. a
# real escaped space in a path) must be left completely untouched -- this
# proves the join is narrowly `\` + `\n` only, never a generic backslash
# strip.
assert_allow "cp-mv-continuation (g) SAFETY: escaped-space path (backslash not followed by newline) -> allow, untouched by the join" \
    "cp /tmp/a\\ b.txt $WT_DIR/src/" "$WT_REPO"

# --- SAFETY (h): a `\` + newline embedded INSIDE a plain (no command
# substitution) single-quoted span must be preserved verbatim -- the join
# only ever applies OUTSIDE a quoted span. Uses a multi-line single-quoted
# sed script (unrelated to any write target) ahead of a same-line cp whose
# own destination is outside the worktree, so a DENY here proves the quoted
# span's embedded "\\\n" was never touched (if it had been misjoined into the
# surrounding text, the sed script's own trailing text could spill out and be
# misread, changing the verdict away from a clean deny on the real cp
# destination).
CMD_H="sed 's/a\\
b/c/' /tmp/x.txt; cp /tmp/a.txt $WT_REPO/evil-dest/"
assert_deny "cp-mv-continuation (h) SAFETY: backslash-newline inside a quoted span is untouched; real cp dest outside worktree -> still deny" \
    "$CMD_H" "$WT_REPO"

# --- (i) REGRESSION GUARD (not this issue's scope): a `sed -i` write to a
# bare /tmp path with no relation to any worktree, and no `\`-continuation at
# all -- qsplit()'s new branch (`c == "\\" && next == "\n"`) cannot ever
# match inside it, so this fix must not alter its verdict either way (still
# allow: /tmp is outside the main checkout entirely).
CMD_I="sed -i '' 's/from foo import bar/from foo_v2 import bar/' /tmp/run_test.py
head -3 /tmp/run_test.py"
assert_allow "cp-mv-continuation (i) regression guard: unrelated sed -i /tmp write (no continuation) is unaffected by this fix -> allow" \
    "$CMD_I" "$WT_REPO"

echo ""
echo -e "${YELLOW}--- qsplit() BACKSLASH PARITY: \\\\ + newline is NOT a continuation (#7978) ---${NC}"
# =========================================================================
#
# The join above must fire on a backslash the SHELL treats as an escape
# character -- i.e. one with EVEN backslash parity behind it -- and on no
# other. `foo\\` + a real newline is an escaped literal backslash followed
# by an ORDINARY, UNESCAPED line end: two shell statements, not one. Joining
# them deletes a real statement boundary, which hides the SECOND statement's
# command word from toks[1] -- and the cp / mv / sed -i / mkdir branches of
# extract_write_targets() are all keyed on toks[1]. The first port of this
# fix had no parity tracking and turned three existing DENY verdicts into
# ALLOW that way; the suite shipped green because no case below existed.
#
# Spelled with explicit named variables rather than inline escapes: the
# whole point of these cases is the exact NUMBER of backslashes, and a
# miscounted `\\\\` inside a double-quoted bash string is precisely the
# reading error they exist to catch.
BS1='\'    # ONE literal backslash   -> a real shell line continuation
BS2='\\'   # TWO literal backslashes -> escaped literal backslash, then a real line end
NL=$'\n'

# CWD is the worktree itself here (not $WT_REPO as in (d)-(h) above): these
# model a Builder acting from inside its own managed worktree and writing
# out into the main checkout, the exact shape of the reported bypass.

# --- SAFETY (j): cp across a `\\` line end -> the cp is a SECOND statement
# and its destination escapes the worktree -> must DENY.
assert_deny "cp-mv-continuation (j) SAFETY: \\\\ + newline is a real statement boundary; escaping cp on the next line -> deny" \
    "echo foo${BS2}${NL}cp /tmp/src.txt \"$WT_REPO/pwned-j.txt\"" "$WT_DIR"

# --- SAFETY (k): same via mv ------------------------------------------------
assert_deny "cp-mv-continuation (k) SAFETY: \\\\ + newline, escaping mv on the next line -> deny" \
    "echo foo${BS2}${NL}mv /tmp/src.txt \"$WT_REPO/pwned-k.txt\"" "$WT_DIR"

# --- SAFETY (l): same via sed -i --------------------------------------------
assert_deny "cp-mv-continuation (l) SAFETY: \\\\ + newline, escaping sed -i on the next line -> deny" \
    "echo foo${BS2}${NL}sed -i '' 's/a/b/' \"$WT_REPO/pwned-l.txt\"" "$WT_DIR"

# --- SAFETY (m): FOUR backslashes -- two escaped literal backslashes, then
# an ordinary line end. Proves the parity rule is a rule, not a special case
# hard-coded for exactly two.
assert_deny "cp-mv-continuation (m) SAFETY: \\\\\\\\ (two escaped pairs) + newline is still a statement boundary -> deny" \
    "echo foo${BS2}${BS2}${NL}cp /tmp/src.txt \"$WT_REPO/pwned-m.txt\"" "$WT_DIR"

# --- PARITY (n): THREE backslashes -- an escaped literal backslash, THEN a
# genuine continuation. The real shell joins these two physical lines into
# ONE `echo` command, so no cp is ever invoked and there is no write to
# confine: allow is the CORRECT verdict, and asserting it is what proves the
# guard discriminates on parity rather than just denying every backslash run.
# Verified against a real shell: with 1 or 3 trailing backslashes bash joins
# the lines (one statement); with 2 or 4 it runs both (two statements).
assert_allow "cp-mv-continuation (n) PARITY: \\\\\\ (escaped pair + real continuation) still joins -- one echo, no cp invoked -> allow" \
    "echo foo${BS2}${BS1}${NL}cp /tmp/src.txt \"$WT_REPO/pwned-n.txt\"" "$WT_DIR"

# --- PARITY (o): ONE backslash -- the ordinary continuation this fix exists
# to honour, on the same text as (j). Completes the 1/2/3/4 ladder and shows
# (j) denies because of parity, not because of the `cp` token.
assert_allow "cp-mv-continuation (o) PARITY: single \\ + newline joins as before -- one echo, no cp invoked -> allow" \
    "echo foo${BS1}${NL}cp /tmp/src.txt \"$WT_REPO/pwned-o.txt\"" "$WT_DIR"

echo ""

# =========================================================================

print_summary
