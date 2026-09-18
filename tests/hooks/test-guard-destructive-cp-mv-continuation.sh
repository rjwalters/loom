#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — qsplit()
# unquoted-backslash ESCAPE handling: backslash-newline line-continuation
# joining (#7945/#7978) and the backslash-escaped QUOTE that must not open a
# quoted span (#8025).
#
# Both halves are the same helper (`qsplit()`), the same class of bug (an
# unquoted backslash whose escaping effect the segmenter did not model) and
# the same fail-OPEN direction (a deleted statement boundary hides the next
# statement's command word from toks[1], which is what gates every
# cp / mv / sed -i / mkdir branch of extract_write_targets()), so they share
# one suite rather than drifting apart in two.
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
echo -e "${YELLOW}--- qsplit() ESCAPED QUOTE: an unquoted \\\" / \\' opens no span (#8025) ---${NC}"
# =========================================================================
#
# An unquoted `\"` / `\'` is a LITERAL quote character to the real shell -- it
# opens nothing. Before #8025, qsplit()'s `c == DQ || c == SQ` branch had no
# lookback for a preceding backslash, so such a character entered the
# quoted-span branch anyway: the branch scanned forward to the next same-type
# quote ANYWHERE later in the command and copied everything in between
# verbatim as inert quoted data. Every `;`/`&`/`|` in that stretch stopped
# being a separator, so a genuine statement boundary was deleted and the
# following statement's command word never reached toks[1] -- the exact gate
# the cp / mv / sed -i / mkdir write idioms are keyed on. Fail-OPEN: a
# worktree-write-confinement DENY silently became an ALLOW, with no ask and
# no telemetry. Same shape as (j)-(o) above, independent cause (that one was
# the continuation branch, this one the quote branch).
#
# Spelled with explicit named variables for the same reason the parity ladder
# above is: the whole point is the exact number of backslashes in front of
# the quote character, and a miscounted inline escape is precisely the
# reading error these cases exist to catch.
DQC='"'    # one literal double-quote character
SQC="'"    # one literal single-quote character

# CWD is the worktree itself (as in (j)-(o)): a Builder acting from inside
# its own managed worktree and writing out into the main checkout.

# --- SAFETY (p): the reported shape. The escaped `"` must not swallow the
# `;`, so the `cp` is still its own statement and its out-of-worktree
# destination is still seen -> must DENY.
assert_deny "cp-mv-continuation (p) SAFETY: escaped \\\" does not open a span; escaping cp after the ; -> deny" \
    "echo foo${BS1}${DQC}bar; cp /tmp/src.txt ${DQC}$WT_REPO/pwned-p.txt${DQC}" "$WT_DIR"

# --- SAFETY (q): same via an escaped single quote -------------------------
assert_deny "cp-mv-continuation (q) SAFETY: escaped \\' does not open a span; escaping cp after the ; -> deny" \
    "echo foo${BS1}${SQC}bar; cp /tmp/src.txt ${DQC}$WT_REPO/pwned-q.txt${DQC}" "$WT_DIR"

# --- SAFETY (r): same via mv ----------------------------------------------
assert_deny "cp-mv-continuation (r) SAFETY: escaped \\\" then an escaping mv -> deny" \
    "echo foo${BS1}${DQC}bar; mv /tmp/src.txt ${DQC}$WT_REPO/pwned-r.txt${DQC}" "$WT_DIR"

# --- SAFETY (s): same via sed -i -------------------------------------------
assert_deny "cp-mv-continuation (s) SAFETY: escaped \\\" then an escaping sed -i -> deny" \
    "echo foo${BS1}${DQC}bar; sed -i ${SQC}${SQC} ${SQC}s/a/b/${SQC} ${DQC}$WT_REPO/pwned-s.txt${DQC}" "$WT_DIR"

# --- SAFETY (t): same via mkdir --------------------------------------------
assert_deny "cp-mv-continuation (t) SAFETY: escaped \\\" then an escaping mkdir -> deny" \
    "echo foo${BS1}${DQC}bar; mkdir -p ${DQC}$WT_REPO/pwned-dir${DQC}" "$WT_DIR"

# --- CONTROL (u): the identical command with NO escaped quote. It denied
# before #8025 and must keep denying after -- this is the row that proves
# (p)-(t) are about the escaped quote and nothing else.
assert_deny "cp-mv-continuation (u) CONTROL: no escaped quote at all -> deny (unchanged baseline)" \
    "echo foobar; cp /tmp/src.txt ${DQC}$WT_REPO/pwned-u.txt${DQC}" "$WT_DIR"

# --- PARITY (v): TWO backslashes then a quote. The first backslash escapes
# the second (a literal backslash), so the quote has EVEN parity behind it
# and really DOES open a span -- the real shell reads
# `echo foo\` + `"; cp ... pwned-v.txt"` as ONE echo with a quoted argument,
# no cp invoked, nothing written. ALLOW is the correct verdict, and asserting
# it is what proves #8025 discriminates on parity instead of blanket-ignoring
# every quote preceded by a backslash.
assert_allow "cp-mv-continuation (v) PARITY: \\\\\" (escaped backslash, then a REAL opening quote) still opens a span -> allow" \
    "echo foo${BS2}${DQC}; cp /tmp/src.txt $WT_REPO/pwned-v.txt${DQC}" "$WT_DIR"

# --- PARITY (w): THREE backslashes then a quote -- escaped literal
# backslash, then an escaped literal quote. Odd parity again, so the span
# does NOT open and the `;` is live: the cp is a second statement writing
# outside the worktree -> deny. Completes the 1/2/3 ladder.
assert_deny "cp-mv-continuation (w) PARITY: \\\\\\\" (escaped backslash + escaped quote) opens no span -> deny" \
    "echo foo${BS2}${BS1}${DQC}bar; cp /tmp/src.txt ${DQC}$WT_REPO/pwned-w.txt${DQC}" "$WT_DIR"

# --- SAFETY (x): a genuine, fully-quoted span is still inert. The `;` and
# the `cp` here are DATA inside one double-quoted echo argument, no statement
# boundary and no write at all -> allow. Proves #8025 narrowed only the
# PHANTOM span, leaving real quoted-span suppression intact.
assert_allow "cp-mv-continuation (x) SAFETY: a real quoted span keeps its separators inert -> allow" \
    "echo ${DQC}a; cp /tmp/src.txt $WT_REPO/pwned-x.txt${DQC}" "$WT_DIR"

# --- SAFETY (y): an escaped quote inside a path argument of a write whose
# destination is legitimately INSIDE the worktree must not be denied -- the
# new branch emits both bytes verbatim, so the token text downstream is
# unchanged and no phantom write target is manufactured.
assert_allow "cp-mv-continuation (y) SAFETY: escaped quote in a source path, in-worktree destination -> allow" \
    "cp /tmp/a${BS1}${DQC}b.txt $WT_DIR/src/" "$WT_DIR"

echo ""
echo -e "${YELLOW}--- qsplit() ESCAPED CLOSING QUOTE: \\\" INSIDE a \"…\" span (#8166) ---${NC}"
# =========================================================================
#
# Sibling of the (p)-(y) block above, SAME helper, DIFFERENT branch, and NOT
# fixed by #8025. #8025 taught the top-level loop that an UNQUOTED `\"` opens
# no span. This is the other half: the forward scan that looks for the
# CLOSING quote once a span is already open. That scan matched on the quote
# CHARACTER alone, with no backslash-parity question, so `"foo\"bar"` ended
# its span at the ESCAPED quote instead of the real one. The leftover `bar"`
# re-entered the top-level loop as unquoted text and that stray quote opened
# a span of ITS own, running to the next same-type quote anywhere later in
# the command -- swallowing every separator in between. Same fail-OPEN
# outcome as (p)-(t): a worktree-write-confinement DENY became a silent
# ALLOW, no ask, no telemetry.
#
# DIRECTION (why these cases are not optional): unlike #8025's branch, which
# could only ever ADD segment boundaries, this fix EXTENDS a span to its real
# closer -- which makes MORE text inert, the fail-OPEN direction. It is the
# correct parse (real bash reads `"foo\"bar"` as one word), but "correct
# parse" is a claim about the corpus, not a licence: (ac)-(ak) below pin both
# the flips it is allowed to make and the verdicts it must NOT touch.

# CWD is the worktree itself, as in (j)-(y): a Builder acting from inside its
# own managed worktree and writing out into the main checkout.

# --- SAFETY (aa): the reported shape. The escaped `"` is DATA inside the
# span; the span's real closer is the quote after `bar`, so the `;` after it
# is a LIVE separator and the `cp` is its own statement writing outside the
# worktree -> must DENY. Allowed before #8166.
assert_deny "cp-mv-continuation (aa) SAFETY: \\\" inside a \"…\" span does not end it; escaping cp after the ; -> deny" \
    "echo ${DQC}foo${BS1}${DQC}bar${DQC}; cp /tmp/src.txt ${DQC}$WT_REPO/pwned-aa.txt${DQC}" "$WT_DIR"

# --- SAFETY (ab): the mkdir variant from the issue's evidence table --------
assert_deny "cp-mv-continuation (ab) SAFETY: \\\" inside a \"…\" span, escaping mkdir after the ; -> deny" \
    "echo ${DQC}foo${BS1}${DQC}bar${DQC}; mkdir -p ${DQC}$WT_REPO/pwned-ab-dir${DQC}" "$WT_DIR"

# --- SAFETY (ac): same via mv ----------------------------------------------
assert_deny "cp-mv-continuation (ac) SAFETY: \\\" inside a \"…\" span, escaping mv after the ; -> deny" \
    "echo ${DQC}foo${BS1}${DQC}bar${DQC}; mv /tmp/src.txt ${DQC}$WT_REPO/pwned-ac.txt${DQC}" "$WT_DIR"

# --- SAFETY (ad): same via sed -i ------------------------------------------
assert_deny "cp-mv-continuation (ad) SAFETY: \\\" inside a \"…\" span, escaping sed -i after the ; -> deny" \
    "echo ${DQC}foo${BS1}${DQC}bar${DQC}; sed -i ${SQC}${SQC} ${SQC}s/a/b/${SQC} ${DQC}$WT_REPO/pwned-ad.txt${DQC}" "$WT_DIR"

# --- CONTROL (ae): the identical command with NO escaped quote anywhere. It
# denied before #8166 and must keep denying after -- the row that proves
# (aa)-(ad) are about the escaped CLOSING quote and nothing else. (This is
# the issue's own control row.)
assert_deny "cp-mv-continuation (ae) CONTROL: no escaped quote at all -> deny (unchanged baseline)" \
    "echo foobar; cp /tmp/src.txt ${DQC}$WT_REPO/pwned-ae.txt${DQC}" "$WT_DIR"

# --- SAFETY (af): a plain, escape-free double-quoted span must still be
# INERT. The `;` and the `cp` here are data inside one echo argument -- real
# bash runs no cp at all -> allow. Proves the parity scan did not turn every
# quoted span into live text.
assert_allow "cp-mv-continuation (af) SAFETY: an ordinary escape-free \"…\" span stays inert -> allow" \
    "echo ${DQC}a; cp /tmp/src.txt $WT_REPO/pwned-af.txt${DQC}" "$WT_DIR"

# --- PARITY (ag): TWO backslashes before the candidate closer. The first
# escapes the second (a literal backslash), so the quote has EVEN parity
# behind it and really IS the closer: real bash reads `echo "foo\\"` as one
# complete word and then runs the `cp` as a second statement, writing outside
# the worktree -> deny. The ladder partner of (aa); asserting it is what
# proves the scan discriminates on parity rather than skipping every quote
# that happens to follow a backslash.
assert_deny "cp-mv-continuation (ag) PARITY: \\\\\" (escaped backslash, then the REAL closer) ends the span -> deny" \
    "echo ${DQC}foo${BS2}${DQC}; cp /tmp/src.txt ${DQC}$WT_REPO/pwned-ag.txt${DQC}" "$WT_DIR"

# --- PARITY (ah): THREE backslashes -- escaped literal backslash, then an
# escaped literal quote. Odd parity again, so the span does NOT close there
# and runs on to the quote after `bar`; the `;` after THAT is live -> deny.
# Completes the 1/2/3 ladder.
assert_deny "cp-mv-continuation (ah) PARITY: \\\\\\\" inside the span is still escaped data -> deny" \
    "echo ${DQC}foo${BS2}${BS1}${DQC}bar${DQC}; cp /tmp/src.txt ${DQC}$WT_REPO/pwned-ah.txt${DQC}" "$WT_DIR"

# --- SAFETY (ai): SINGLE quotes are deliberately NOT given parity treatment
# -- a backslash has no escaping power inside '…' in real bash, so the quote
# after it genuinely closes the span. Real bash runs
# `echo 'foo\'` then `bar; cp … pwned-ai.txt` -- the cp IS a live statement
# writing outside the worktree -> must still deny. If the parity rule were
# (wrongly) applied to SQ too, the span would run past its real end and
# swallow this `;`.
assert_deny "cp-mv-continuation (ai) SAFETY: \\' inside a '…' span DOES close it (no parity for SQ) -> deny" \
    "echo ${SQC}foo${BS1}${SQC}bar; cp /tmp/src.txt ${DQC}$WT_REPO/pwned-ai.txt${DQC}" "$WT_DIR"

# --- SAFETY (aj): a span with an escaped quote in it whose FOLLOWING
# statement writes legitimately INSIDE the worktree must not be denied. The
# extended span changes where the boundary is, not whether in-worktree writes
# are allowed -- guards against the fix manufacturing a phantom write target.
assert_allow "cp-mv-continuation (aj) SAFETY: \\\" span followed by an IN-worktree cp -> allow" \
    "echo ${DQC}foo${BS1}${DQC}bar${DQC}; cp /tmp/src.txt $WT_DIR/src/" "$WT_DIR"

# --- (ak) UNCHANGED-VERDICT PIN: a double-quoted span in which the ONLY
# remaining same-type quote is escaped. The parity scan finds no unescaped
# closer, so it newly routes this input into qsplit()'s pre-existing `ci == 0`
# fallback (emit the stray opening quote, keep the separators live) instead of
# closing the span at the escaped quote. That re-route must not change the
# end-to-end verdict, and it does not: allow before #8166, allow after.
#
# ALLOW is the correct answer here and is NOT a suppressed deny: the input is
# unterminated to the real shell too -- `bash -n` reports "unexpected EOF
# while looking for matching \"" and runs NOTHING, so there is no write to
# confine. (The allow is produced downstream of qsplit anyway: mask_ws() sees
# the unterminated span and masks every following space, so the `cp` never
# splits into tokens. That is mask_ws()'s own unterminated-quote behaviour,
# untouched by this fix -- qsplit() itself does split this input correctly,
# verified directly on the helper.)
#
# Note there is deliberately no quoting on the cp destination: a quote there
# would be an unescaped closer and this would stop being the unterminated
# case it exists to pin.
assert_allow "cp-mv-continuation (ak) PIN: \"…\\\" with no unescaped closer -- unrunnable by bash, verdict unchanged -> allow" \
    "echo ${DQC}foo${BS1}${DQC}bar; cp /tmp/src.txt $WT_REPO/pwned-ak.txt" "$WT_DIR"

echo ""
echo -e "${YELLOW}--- mask_ws()/mask_gt() BACKTICK SPAN: a quote inside \`…\` is data (#8211) ---${NC}"
# =========================================================================
#
# Third sibling of the (p)-(y) / (aa)-(ak) blocks, and the one that is NOT in
# qsplit() at all. qsplit() segments every input below correctly -- its
# has_live_subst() check sees the backtick and keeps the separators live -- so
# the statement boundary reaches mask_ws() intact. mask_ws()/mask_gt() then
# lost it: they modelled three quote states (unquoted / single / double) and
# had NO branch for a backtick, so the `"` inside an unquoted `…` span opened
# a span of its own running to the next `"` in the command -- the opening
# quote of the `cp` destination -- and every space between them was masked.
# The `cp` segment then split into ONE token, `toks[1]` was never `cp`, and
# every cp / mv / sed -i / mkdir branch of extract_write_targets() was
# skipped. Fail-OPEN, measured: a worktree-write-confinement DENY became a
# silent ALLOW while real bash ran the write (the substitution itself errors,
# but the `;` after it is a live separator and the second statement still
# executes).
#
# DIRECTION (why these cases are not optional): suppressing a quote toggle
# changes WHICH whitespace is masked, and it is reachable in BOTH directions
# -- masking less splits more tokens (fail-closed), masking more merges them
# and can hide a command word (fail-open). (as)-(aw) pin both edges.
BTC='`'    # one literal backtick character

# CWD is the worktree itself, as in (j)-(ak): a Builder acting from inside its
# own managed worktree and writing out into the main checkout.

# --- SAFETY (al): the issue's first fixture. The `"` is DATA inside the
# backtick span, so the `;` is live and the `cp` is its own statement writing
# outside the worktree -> must DENY. Allowed before #8211.
assert_deny "cp-mv-continuation (al) SAFETY: \" inside a \`…\` span does not open one; escaping cp after the ; -> deny" \
    "echo ${BTC}${DQC}${BTC}; cp /tmp/src.txt ${DQC}$WT_REPO/pwned-al.txt${DQC}" "$WT_DIR"

# --- SAFETY (am): the issue's second fixture, single quote inside the span --
assert_deny "cp-mv-continuation (am) SAFETY: ' inside a \`…\` span does not open one -> deny" \
    "echo ${BTC}${SQC}${BTC}; cp /tmp/src.txt ${DQC}$WT_REPO/pwned-am.txt${DQC}" "$WT_DIR"

# --- SAFETY (an): the issue's third fixture -- the span does not have to
# start the word for the mask to go wrong.
assert_deny "cp-mv-continuation (an) SAFETY: a\`\"\` (span mid-word) -> deny" \
    "echo a${BTC}${DQC}${BTC}; cp /tmp/src.txt ${DQC}$WT_REPO/pwned-an.txt${DQC}" "$WT_DIR"

# --- CONTROL (ao): the identical command with no backtick anywhere. It denied
# before #8211 and must keep denying after -- the row that proves (al)-(an)
# are about the backtick span and nothing else. (The issue's own control row.)
assert_deny "cp-mv-continuation (ao) CONTROL: no backtick at all -> deny (unchanged baseline)" \
    "echo foobar; cp /tmp/src.txt ${DQC}$WT_REPO/pwned-ao.txt${DQC}" "$WT_DIR"

# --- SAFETY (ap)/(aq)/(ar): the same span ahead of the other three toks[1]-
# keyed write idioms, so the fix is proven at the gate they all share.
assert_deny "cp-mv-continuation (ap) SAFETY: \`\"\` span, escaping mv after the ; -> deny" \
    "echo ${BTC}${DQC}${BTC}; mv /tmp/src.txt ${DQC}$WT_REPO/pwned-ap.txt${DQC}" "$WT_DIR"

assert_deny "cp-mv-continuation (aq) SAFETY: \`\"\` span, escaping mkdir after the ; -> deny" \
    "echo ${BTC}${DQC}${BTC}; mkdir -p ${DQC}$WT_REPO/pwned-aq-dir${DQC}" "$WT_DIR"

assert_deny "cp-mv-continuation (ar) SAFETY: \`\"\` span, escaping sed -i after the ; -> deny" \
    "echo ${BTC}${DQC}${BTC}; sed -i ${SQC}${SQC} ${SQC}s/a/b/${SQC} ${DQC}$WT_REPO/pwned-ar.txt${DQC}" "$WT_DIR"

# --- PARITY (as): an ESCAPED backtick opens no span. mask_ws()'s pre-existing
# `esc` flag (#8025) consumes it before the new branch is reached, which is the
# same even/odd parity rule has_live_subst() (#7498) applies to the same
# question. The destination is a QUOTED path containing a literal space on
# purpose: that is the #4934 contract the escape has to preserve -- if the
# escaped backtick wrongly opened a span, the destination's `"` would not open
# one either, the space would split the token, and the surviving fragment
# (`as.txt"`) would resolve as a RELATIVE path inside the acting worktree and
# silently ALLOW. Real bash prints a literal backtick and runs the cp -> deny.
assert_deny "cp-mv-continuation (as) PARITY: \\\` opens no span; quoted dest with a space still masked -> deny" \
    "echo ${BS1}${BTC}x; cp /tmp/src.txt ${DQC}$WT_REPO/pwned as.txt${DQC}" "$WT_DIR"

# --- PARITY (at): the ladder partner of (as) -- the SAME text with an
# UNESCAPED backtick. The span is unterminated, so `btick` stays set to the end
# of the buffer, no later quote opens a span, and LESS whitespace is masked:
# the fail-CLOSED direction. ALLOW is the correct answer and is not a
# suppressed deny -- `bash -n` rejects this input outright ("unexpected EOF
# while looking for matching \`"), so nothing runs and there is no write to
# confine. It DENIED before #8211; this is a false positive the fix removes,
# and asserting it is what proves the branch discriminates on the escape.
assert_allow "cp-mv-continuation (at) PARITY: unterminated \`…\` span -- unrunnable by bash -> allow" \
    "echo ${BTC}x; cp /tmp/src.txt ${DQC}$WT_REPO/pwned at.txt${DQC}" "$WT_DIR"

# --- SAFETY (au): the same backtick span followed by a write that is
# legitimately INSIDE the worktree must not be denied. The fix changes which
# whitespace is masked, not whether in-worktree writes are allowed -- guards
# against it manufacturing a phantom write target.
assert_allow "cp-mv-continuation (au) SAFETY: \`\"\` span followed by an IN-worktree cp -> allow" \
    "echo ${BTC}${DQC}${BTC}; cp /tmp/src.txt $WT_DIR/src/" "$WT_DIR"

# --- PIN (av): mask_ws() must still mask whitespace inside an ORDINARY quoted
# span -- the #4934 contract this whole helper exists for. Without it the
# target splits at the space, the fragment `file.sh"` resolves relative to the
# worktree and the write into the main checkout is allowed. No backtick is
# involved, so the new branch must leave it completely alone -> deny.
assert_deny "cp-mv-continuation (av) PIN: quoted write target containing a space is still masked (#4934) -> deny" \
    "echo x > ${DQC}$WT_REPO/evil file.sh${DQC}" "$WT_DIR"

# --- PIN (aw): mask_gt() gets the same span state, and a `>` INSIDE the span
# stays LIVE -- it really is a redirection for the substituted command, and
# real bash writes the file. Masking it would drop a genuine write target, so
# this pins the one thing the mask_gt() half must NOT do -> deny.
assert_deny "cp-mv-continuation (aw) PIN: a > inside a \`…\` span is still a live redirect -> deny" \
    "echo ${BTC}cat /tmp/a > $WT_REPO/pwned-aw.txt${BTC}" "$WT_DIR"

echo ""

# =========================================================================

print_summary
