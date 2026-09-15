#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — write confinement.
#
# One slice of the former monolithic tests/hooks/test-guard-destructive.sh,
# split per #7741. Shared fixtures, assertions and catastrophic-phrase payloads
# live in tests/hooks/lib/guard-destructive-harness.sh.
#
# Usage: ./tests/hooks/test-guard-destructive-write-confinement.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

echo -e "${YELLOW}--- Bash-tool write confinement (guards.worktreeIsolation / LOOM_GUARD_WORKTREE_ISOLATION, #4178) ---${NC}"
# =========================================================================
#
# guard-worktree-paths.sh confines Edit/Write tool calls to a builder's issue
# worktree, but the Bash tool had no equivalent -- `>`/`>>` redirection, `tee`,
# `sed -i`, `cp`/`mv` all write files without going through Edit/Write. Sweep
# #4063 used exactly this escape to edit live guard hooks in the main checkout
# while its own worktree stayed clean (see issue #4178's root-cause writeup:
# the guard denied 10x on the Edit/Write path, then the escaped edits landed
# in the silent window that followed via a Bash write instead).
#
# Fixture: an isolated throwaway git repo (its own REPO_ROOT / MAIN_ROOT, so
# these tests never touch the real Loom checkout) with a managed worktree at
# <repo>/.loom/worktrees/issue-1 (the `.loom-managed` sentinel worktree.sh
# writes at every worktree root).

# Create a throwaway git repo with a fixture managed worktree
# (<repo>/.loom/worktrees/issue-1/.loom-managed) and an optional
# .loom/config.json. Echoes the repo path.

# Create a throwaway git repo with a REAL linked git worktree (via `git
# worktree add`) at <repo>/.loom/worktrees/issue-1, carrying a `.loom-managed`
# sentinel. Unlike make_wt_repo (a plain subdirectory), a linked worktree
# exercises the show-toplevel vs. git-common-dir divergence: from inside the
# worktree, `git rev-parse --show-toplevel` returns the *worktree* root while
# `--git-common-dir/..` returns the *main* checkout. Echoes the repo path.

WT_REPO=$(make_wt_repo)
WT_DIR="$WT_REPO/.loom/worktrees/issue-1"

assert_deny "write-confinement: echo > main-checkout path denies" \
    "echo x > $WT_REPO/defaults/hooks/f.sh" "$WT_REPO"
assert_deny "write-confinement: echo >> (append) main-checkout path denies" \
    "echo x >> $WT_REPO/defaults/hooks/f.sh" "$WT_REPO"
assert_deny "write-confinement: tee main-checkout path denies" \
    "echo x | tee $WT_REPO/f" "$WT_REPO"
assert_deny "write-confinement: sed -i on main-checkout path denies" \
    "sed -i 's/a/b/' $WT_REPO/f" "$WT_REPO"
assert_deny "write-confinement: cp destination in main checkout denies" \
    "cp /tmp/a.sh $WT_REPO/defaults/hooks/f.sh" "$WT_REPO"
assert_deny "write-confinement: mv destination in main checkout denies" \
    "mv /tmp/a.sh $WT_REPO/defaults/hooks/f.sh" "$WT_REPO"
assert_deny "write-confinement: heredoc cat > main-checkout path denies" \
    "cat > $WT_REPO/defaults/hooks/f.sh <<EOF
hello
EOF" "$WT_REPO"
assert_deny "write-confinement: relative target + cwd at main root denies" \
    "echo x > defaults/hooks/f.sh" "$WT_REPO"

# --- #6110: the deny message must point at the sanctioned escape hatch, and
# steer toward the RELIABLE .loom/config.json route rather than an inline env
# prefix (which does not reach this hook -- it runs as a separate process and
# reads its own env, the same trap as LOOM_GUARD_STASH_SCOPE).
assert_deny_reason_matches "write-confinement (#6110): deny reason names the guards.worktreeIsolation escape hatch" \
    "echo x > $WT_REPO/defaults/hooks/f.sh" \
    'guards\.worktreeIsolation:false in \.loom/config\.json' "$WT_REPO"
assert_deny_reason_matches "write-confinement (#6110): deny reason warns the inline LOOM_GUARD_WORKTREE_ISOLATION=0 prefix does NOT work" \
    "echo x > $WT_REPO/defaults/hooks/f.sh" \
    'LOOM_GUARD_WORKTREE_ISOLATION=0.*does NOT work' "$WT_REPO"

assert_allow "write-confinement: echo > target inside the managed worktree allows" \
    "echo x > $WT_DIR/src/f.sh" "$WT_REPO"
assert_allow "write-confinement: tee target inside the managed worktree allows" \
    "echo x | tee $WT_DIR/src/f.sh" "$WT_REPO"
assert_allow "write-confinement: echo > target in /tmp allows" \
    "echo x > /tmp/loom-test-$$-f.sh" "$WT_REPO"
assert_allow "write-confinement: cd <worktree> && echo > relative target allows" \
    "cd $WT_DIR && echo x > f.sh" "$WT_REPO"

# --- #7415: a git worktree NESTED under the main checkout, created by a plain
# `git worktree add` (so carrying no `.loom-managed` sentinel), was
# indistinguishable from the main checkout: the sentinel walk-up found nothing
# and the main-root prefix test matched, so `cp`/`mv`/`tee`/redirection into a
# `<main>/.claude/worktrees/<name>` worktree -- a layout some repos document --
# was denied with a message pointing at `.loom/worktrees/issue-<N>`, which does
# not even exist for that workflow. git itself treats such a directory as a
# separate working tree sharing nothing with the main checkout's index or
# tracked files, so `git worktree list --porcelain` is now consulted and any
# registered worktree OTHER than the main one is treated as "not the main
# checkout". The main checkout stays denied -- that is what #4178 protects.

WT_NESTED_REPO=$(make_wt_repo_nested_unmanaged)
WT_NESTED_DIR="$WT_NESTED_REPO/.claude/worktrees/x"

assert_allow "write-confinement (#7415): cp into a registered-but-unmanaged worktree nested under the main checkout allows" \
    "cp /tmp/a.pdf $WT_NESTED_DIR/src/paper.pdf" "$WT_NESTED_DIR"
assert_allow "write-confinement (#7415): the reported repro -- relative cp destination from the nested worktree's own cwd -- allows" \
    "cp /tmp/ns.pdf src/paper.pdf" "$WT_NESTED_DIR"
assert_allow "write-confinement (#7415): cd <nested worktree> && cp relative destination allows (cwd at main root)" \
    "cd $WT_NESTED_DIR && cp /tmp/ns.pdf src/paper.pdf" "$WT_NESTED_REPO"
assert_allow "write-confinement (#7415): mv into the nested unmanaged worktree allows" \
    "mv /tmp/a.sh $WT_NESTED_DIR/src/f.sh" "$WT_NESTED_DIR"
assert_allow "write-confinement (#7415): echo > redirect into the nested unmanaged worktree allows" \
    "echo x > $WT_NESTED_DIR/src/f.txt" "$WT_NESTED_DIR"
assert_allow "write-confinement (#7415): tee into the nested unmanaged worktree allows" \
    "echo x | tee $WT_NESTED_DIR/src/f2.txt" "$WT_NESTED_DIR"

# ...and the widening must NOT leak into the main checkout itself, from either
# cwd, nor onto a plain directory that is not a registered worktree.
assert_deny "write-confinement (#7415): main-checkout write from the nested worktree's cwd still denies" \
    "cp /tmp/a.sh $WT_NESTED_REPO/defaults/hooks/f.sh" "$WT_NESTED_DIR"
assert_deny "write-confinement (#7415): main-checkout write from the main cwd still denies while a nested worktree is registered" \
    "echo x > $WT_NESTED_REPO/defaults/hooks/g.sh" "$WT_NESTED_REPO"
assert_deny "write-confinement (#7415): a plain directory under .claude/worktrees that is NOT a registered worktree still denies" \
    "cp /tmp/a.sh $WT_NESTED_REPO/.claude/worktrees/not-a-worktree/f.sh" "$WT_NESTED_REPO"
assert_deny "write-confinement (#7415): the nested worktree's PARENT directory (not itself a worktree) still denies" \
    "echo x > $WT_NESTED_REPO/.claude/worktrees/stray.txt" "$WT_NESTED_REPO"

rm -rf "$WT_NESTED_REPO"

# --- #7415 (env fast path): guard-worktree-paths.sh honors LOOM_WORKTREE_PATH
# as "this session is pinned to exactly this worktree" and allows any path
# under it; this block had no equivalent, so the SAME target could be accepted
# through Edit/Write and denied through cp/mv/tee/redirection. The allow half
# is now mirrored here. It is allow-only (this block never confined writes
# outside the repo at all), and a pin AT the main checkout root is ignored so
# one inherited env var cannot switch the whole #4178 confinement off.
WT_PIN_DIR="$WT_REPO/pinned-session"
mkdir -p "$WT_PIN_DIR"
assert_deny "write-confinement (#7415 control): write into a plain main-checkout dir denies with no pin" \
    "cp /tmp/a.sh $WT_PIN_DIR/f.sh" "$WT_REPO"
assert_allow_env "write-confinement (#7415): LOOM_WORKTREE_PATH pin allows a write under the pinned path (parity with guard-worktree-paths.sh)" \
    "LOOM_WORKTREE_PATH=$WT_PIN_DIR" "cp /tmp/a.sh $WT_PIN_DIR/f.sh" "$WT_REPO"
assert_deny_env "write-confinement (#7415): a LOOM_WORKTREE_PATH pin does not widen to the rest of the main checkout" \
    "LOOM_WORKTREE_PATH=$WT_PIN_DIR" "cp /tmp/a.sh $WT_REPO/defaults/hooks/f.sh" "$WT_REPO"
assert_deny_env "write-confinement (#7415): a LOOM_WORKTREE_PATH pinned AT the main checkout root is ignored (cannot switch the guard off)" \
    "LOOM_WORKTREE_PATH=$WT_REPO" "cp /tmp/a.sh $WT_REPO/defaults/hooks/f.sh" "$WT_REPO"

# --- #7415 (message): the deny hint must name the ACTUALLY configured worktree
# root rather than the hardcoded relative literal `.loom/worktrees/issue-<N>`,
# which is wrong for any repo that relocates its worktree root.
assert_deny_reason_matches "write-confinement (#7415): deny reason names the resolved worktree root, not a bare relative hint" \
    "echo x > $WT_REPO/defaults/hooks/f.sh" \
    "$(printf '%s' "$WT_REPO/.loom/worktrees/issue-<N>" | sed 's/[][\.*^$+?(){}|/]/\\&/g')" "$WT_REPO"

# --- #5232: a heredoc redirection operator/delimiter trailing a real tee/cp/mv
# (or sed -i) write target must never be misread as an ADDITIONAL write
# target. Unlike the #5226/#5181 tee-heredoc assertions above (which run
# against the default REPO_ROOT cwd and so only exercise this precondition
# when the ambient primary checkout happens to have a sibling managed
# worktree -- the normal but not guaranteed state of this repo's own primary
# clone), these use the hermetic make_wt_repo() fixture so the "a managed
# worktree exists elsewhere" precondition is deterministic in ANY checkout,
# including a fresh clone or CI runner with zero sibling worktrees. Before the
# #5232 fix, the phantom "<repo>/<<EOF" (or "<repo>/<<" + "<repo>/EOF" for the
# bare space-separated form) target resolved into $WT_REPO -- the protected
# main checkout -- and triggered a false DENY even though the real target
# (under /tmp, unprotected) was entirely fine on its own.
assert_allow "write-confinement (#5232): tee to /tmp with a trailing quoted heredoc delimiter <<'EOF' is not misread as a second write target" \
    "tee /tmp/loom-test-$$-report1.md <<'EOF'
some text
EOF
echo done" "$WT_REPO"
assert_allow "write-confinement (#5232): sudo tee to /tmp with a trailing quoted heredoc delimiter <<'EOF' is not misread as a second write target" \
    "sudo tee /tmp/loom-test-$$-report2.md <<'EOF'
some text
EOF
echo done" "$WT_REPO"
assert_allow "write-confinement (#5232): tee to /tmp with a bare space-separated '<< EOF' heredoc operator is not misread as two extra write targets" \
    "tee /tmp/loom-test-$$-report3.md << EOF
some text
EOF
echo done" "$WT_REPO"
assert_allow "write-confinement (#5232): cp with a trailing bare '<< EOF' heredoc operator+delimiter is not misread as the destination" \
    "cp /tmp/a.sh /tmp/loom-test-$$-copy.sh << EOF
irrelevant
EOF" "$WT_REPO"
assert_allow "write-confinement (#5232): sed -i on a /tmp path with a trailing <<EOF heredoc is not misread as an extra file operand" \
    "sed -i 's/a/b/' /tmp/loom-test-$$-sed.sh <<EOF
ignored
EOF" "$WT_REPO"
# The real-target-in-main-checkout DENY must still fire when a heredoc is
# ALSO present -- the exclusion must narrow false positives, not weaken the
# genuine confinement check.
assert_deny "write-confinement (#5232): tee into the main checkout with a trailing heredoc still denies (real target, not the heredoc token)" \
    "tee $WT_REPO/defaults/hooks/f2.sh <<'EOF'
some text
EOF
echo done" "$WT_REPO"

# --- #5232 (herestring half): a `<<<` HERESTRING is the same defect class as
# the heredoc forms above, but it fails one step later. `<<<` is excluded from
# the target list by the same operator test, so the OPERATOR itself no longer
# becomes a phantom target -- but a BARE `<<<` is followed by its content WORD
# (real data, e.g. `tee f <<< hello` / `tee f <<< "some text"`), and unless that
# word is consumed too it falls through and is misread as an extra write
# target, resolving into $WT_REPO exactly like the heredoc delimiter did.
# Consuming exactly ONE following word is shell-accurate: bash's herestring
# takes a single word, so in `tee f <<< some text` the `text` really IS a tee
# operand (and is deliberately still scanned as such below).
assert_allow "write-confinement (#5232): tee to /tmp with a bare '<<< word' herestring is not misread as a second write target" \
    "tee /tmp/loom-test-$$-hs1.md <<< hello" "$WT_REPO"
assert_allow "write-confinement (#5232): tee to /tmp with a bare '<<< \"quoted content\"' herestring is not misread as a second write target" \
    "tee /tmp/loom-test-$$-hs2.md <<< \"some text\"" "$WT_REPO"
assert_allow "write-confinement (#5232): tee to /tmp with an ATTACHED '<<<word' herestring is not misread as a second write target" \
    "tee /tmp/loom-test-$$-hs3.md <<<hello" "$WT_REPO"
assert_allow "write-confinement (#5232): cp with a trailing bare '<<< word' herestring is not misread as the destination" \
    "cp /tmp/a.sh /tmp/loom-test-$$-hs4.sh <<< hello" "$WT_REPO"
assert_allow "write-confinement (#5232): sed -i on a /tmp path with a trailing '<<< word' herestring is not misread as an extra file operand" \
    "sed -i 's/a/b/' /tmp/loom-test-$$-hs5.sh <<< hello" "$WT_REPO"
# The genuine confinement DENY must still fire with a herestring present, and
# consuming the herestring word must not swallow a REAL trailing operand.
assert_deny "write-confinement (#5232): tee into the main checkout with a trailing herestring still denies (real target, not the herestring word)" \
    "tee $WT_REPO/defaults/hooks/f3.sh <<< hello" "$WT_REPO"
assert_deny "write-confinement (#5232): a real tee operand AFTER a bare '<<< word' herestring is still scanned (only ONE word is consumed)" \
    "tee /tmp/loom-test-$$-hs6.md <<< hello $WT_REPO/defaults/hooks/f4.sh" "$WT_REPO"

# --- #5232 x #4914 composition: the heredoc/herestring exclusion above and
# main's same-command `$VAR` redirect resolution (#4914) touch the SAME three
# loops and were developed in parallel, so neither PR's suite covers them
# TOGETHER. These assertions pin the composed behavior: the exclusion must not
# shadow resolve_var() (a `$VAR` target that resolves INTO the main checkout
# must still DENY even when a heredoc/herestring shares the command), and
# resolve_var() must not resurrect the phantom target the exclusion removes (a
# `$VAR` target resolving to an unprotected path must now ALLOW -- pre-#5232 it
# false-DENIED on the heredoc token, not on the variable). Also pins that
# consuming the ONE herestring content word never swallows a real `$VAR`
# operand that follows it, and that #4914's cat-heredoc BODY exemption still
# holds with the new exclusion in place.
assert_deny "write-confinement (#5232 x #4914): \$VAR tee target resolving into the main checkout still denies with a trailing herestring" \
    "F=$WT_REPO/defaults/hooks/x1.sh; tee \$F <<< hello" "$WT_REPO"
assert_deny "write-confinement (#5232 x #4914): \$VAR tee target resolving into the main checkout still denies with a trailing heredoc" \
    "F=$WT_REPO/defaults/hooks/x2.sh; tee \$F <<EOF
text
EOF" "$WT_REPO"
assert_allow "write-confinement (#5232 x #4914): \$VAR tee target resolving to /tmp allows with a trailing herestring (phantom heredoc target gone)" \
    "F=/tmp/loom-test-$$-var1.sh; tee \$F <<< hello" "$WT_REPO"
assert_deny "write-confinement (#5232 x #4914): a \$VAR operand AFTER a bare '<<< word' herestring is still resolved and scanned" \
    "F=$WT_REPO/defaults/hooks/x3.sh; tee /tmp/loom-test-$$-var2.md <<< hello \$F" "$WT_REPO"
assert_allow "write-confinement (#5232 x #4914): a cat-heredoc BODY naming a main-checkout tee target stays exempt with the new exclusion in place" \
    "cat > /tmp/loom-test-$$-body.md <<'EOF'
tee $WT_REPO/defaults/hooks/x4.sh
EOF" "$WT_REPO"

# No managed worktree anywhere -> fail open (allow).
WT_REPO_NOWT=$(make_wt_repo)
rm -rf "$WT_REPO_NOWT/.loom/worktrees"
assert_allow "write-confinement: no managed worktree anywhere -> allow (fail-open)" \
    "echo x > $WT_REPO_NOWT/defaults/hooks/f.sh" "$WT_REPO_NOWT"

# Toggle opt-out: guards.worktreeIsolation:false / LOOM_GUARD_WORKTREE_ISOLATION=0.
WT_REPO_OFF=$(make_wt_repo '{"guards":{"worktreeIsolation":false}}')
assert_allow "write-confinement: guards.worktreeIsolation:false -> allow at main root" \
    "echo x > $WT_REPO_OFF/defaults/hooks/f.sh" "$WT_REPO_OFF"
assert_allow_env "write-confinement: LOOM_GUARD_WORKTREE_ISOLATION=0 -> allow at main root" \
    "LOOM_GUARD_WORKTREE_ISOLATION=0" "echo x > $WT_REPO/defaults/hooks/f.sh" "$WT_REPO"
assert_deny_env "write-confinement: LOOM_GUARD_WORKTREE_ISOLATION=1 overrides config-off -> deny" \
    "LOOM_GUARD_WORKTREE_ISOLATION=1" "echo x > $WT_REPO_OFF/defaults/hooks/f.sh" "$WT_REPO_OFF"

# config_resolver migration (#4241): a `guards.worktreeIsolation:false` set
# ONLY in the .loom-project/project.json tier (no legacy .loom/config.json)
# must be honored the same as the legacy-tier test above -- proves
# worktree_isolation_guard_enabled() actually resolves through
# loom_config_get()/config-resolver.sh rather than reading .loom/config.json
# directly.
WT_REPO_PROJECT_OFF=$(make_wt_repo)
mkdir -p "$WT_REPO_PROJECT_OFF/.loom-project"
printf '%s' '{"guards":{"worktreeIsolation":false}}' > "$WT_REPO_PROJECT_OFF/.loom-project/project.json"
assert_allow "write-confinement: guards.worktreeIsolation:false in .loom-project/ tier only -> allow at main root" \
    "echo x > $WT_REPO_PROJECT_OFF/defaults/hooks/f.sh" "$WT_REPO_PROJECT_OFF"

# --- #6021: read-only-by-role `dist/` scratch carve-out ---
#
# A role with NO Write/Edit tool at all (e.g. Auditor, whose
# defaults/.claude/agents/loom-auditor.md `tools:` frontmatter grants only
# Read/Glob/Grep/Bash) has no issue worktree to redirect to and was never the
# threat this guard defends against (a Builder/Doctor bypassing Edit/Write
# confinement via Bash). LOOM_ROLE identifies the acting role (set by
# role_runner/daemon dispatch); the carve-out only fires for a role on the
# read-only allowlist AND only for the well-known, already-`.gitignore`d
# `dist/` scratch directory at the main-checkout root -- never anywhere else,
# and never for Builder/Doctor/an unset or unrecognized role.
assert_deny "write-confinement (#6021): cp into dist/ scratch path denies with no LOOM_ROLE set (unaffected by the carve-out)" \
    "cp /tmp/a.sh $WT_REPO/dist/loom-daemon-x86_64-unknown-linux-gnu" "$WT_REPO"
assert_allow_env "write-confinement (#6021): LOOM_ROLE=auditor allows cp into the well-known dist/ scratch path" \
    "LOOM_ROLE=auditor" "cp /tmp/a.sh $WT_REPO/dist/loom-daemon-x86_64-unknown-linux-gnu" "$WT_REPO"
assert_allow_env "write-confinement (#6021): LOOM_ROLE=AUDITOR (uppercase) allows cp into dist/ (case-insensitive role match)" \
    "LOOM_ROLE=AUDITOR" "cp /tmp/a.sh $WT_REPO/dist/loom-daemon-x86_64-unknown-linux-gnu" "$WT_REPO"
assert_allow_env "write-confinement (#6021): LOOM_ROLE=auditor allows a relative dist/ target when cwd is the main root" \
    "LOOM_ROLE=auditor" "cp /tmp/a.sh dist/loom-daemon-x86_64-unknown-linux-gnu" "$WT_REPO"
assert_deny_env "write-confinement (#6021): LOOM_ROLE=builder still denies dist/ scratch path (Builder unaffected — has Write/Edit)" \
    "LOOM_ROLE=builder" "cp /tmp/a.sh $WT_REPO/dist/loom-daemon-x86_64-unknown-linux-gnu" "$WT_REPO"
assert_deny_env "write-confinement (#6021): LOOM_ROLE=doctor still denies dist/ scratch path (Doctor unaffected — has Write/Edit)" \
    "LOOM_ROLE=doctor" "cp /tmp/a.sh $WT_REPO/dist/loom-daemon-x86_64-unknown-linux-gnu" "$WT_REPO"
assert_deny_env "write-confinement (#6021): LOOM_ROLE=sweep-lifecycle still denies dist/ scratch path (not on the read-only allowlist)" \
    "LOOM_ROLE=sweep-lifecycle" "cp /tmp/a.sh $WT_REPO/dist/loom-daemon-x86_64-unknown-linux-gnu" "$WT_REPO"
assert_deny_env "write-confinement (#6021): an unrecognized LOOM_ROLE value still denies dist/ scratch path (fails closed)" \
    "LOOM_ROLE=some-unknown-role" "cp /tmp/a.sh $WT_REPO/dist/loom-daemon-x86_64-unknown-linux-gnu" "$WT_REPO"
assert_deny_env "write-confinement (#6021): LOOM_ROLE=auditor still denies a NON-dist main-checkout path (scoped to dist/ only)" \
    "LOOM_ROLE=auditor" "cp /tmp/a.sh $WT_REPO/defaults/hooks/f.sh" "$WT_REPO"

# False-positive guard: a `>` quoted inside a -m/--body value must NOT be
# read as a redirection target (COMMAND_ASK_SCAN redaction, mirrors #3679).
assert_allow "write-confinement: '>' inside a quoted -m value is not a target" \
    "git commit -m \"if (a > b) then something\"" "$WT_REPO"
# fd-dup (2>&1) is not a file write and must not manufacture a phantom target.
assert_allow "write-confinement: fd-dup 2>&1 is not treated as a file write" \
    "echo x 2>&1 | tee /tmp/loom-test-$$-log" "$WT_REPO"

# -------------------------------------------------------------------------
# Quote-aware `>` masking (#4245) -- mask_gt() in extract_write_targets().
#
# A `>` that is only DATA inside a quoted --body/--title/... value (e.g. prose
# describing the env > config > default precedence order) must never be
# misread as a shell redirection operator, no matter how many such quoted `>`
# characters the value contains. This is the exact false positive reported in
# #4245: `gh issue create --body "... env > config > default ..."` was denied
# as a "worktree-isolation bypass" even though `gh issue create` writes
# nothing to the filesystem.
assert_allow "write-confinement (#4245): gh issue create --body with a quoted '>' allows" \
    "gh issue create --title \"Test\" --label \"loom:triage\" --body \"a > b\"" "$WT_REPO"
assert_allow "write-confinement (#4245): multiple quoted '>' in one --body value allows" \
    "gh issue create --title \"Test\" --body \"... following the env > config > default precedence ...\"" "$WT_REPO"
assert_allow "write-confinement (#4245): quoted '>' inside a single-quoted value allows" \
    "echo 'a > b'" "$WT_REPO"

# Regression: a quote-aware mask must only NARROW detection, never widen it --
# a REAL (unquoted) redirection into the main checkout must still deny even
# when the same command also contains a quoted `>` elsewhere.
assert_deny "write-confinement (#4245): quoted '>' alongside a real unquoted '>' still denies" \
    "echo \"a > b\" > $WT_REPO/defaults/hooks/f.sh" "$WT_REPO"
assert_deny "write-confinement (#4245): bare '>' redirection still denies (regression)" \
    "echo x > $WT_REPO/defaults/hooks/g.sh" "$WT_REPO"

# -------------------------------------------------------------------------
# Arithmetic/test-context comparison operators (#5515) -- mask_gt() in
# extract_write_targets() no longer misreads an unquoted `>`/`>=`/`<`/`<=`
# used as a comparison inside `(( ... ))` or `[[ ... ]]` as a redirection
# operator. Before the fix, `(( x > 0 ))` manufactured a phantom write target
# of the literal token following the bare `>` (e.g. "0"), and `(( x >= y ))`
# matched the ATTACHED-form redirection branch, stripped the leading `>`, and
# manufactured a phantom target of the literal "=" -- both resolving inside
# the main checkout cwd and false-DENYing a command that writes nothing. Both
# are the exact reproductions from the issue.
assert_allow "write-confinement (#5515): bare arithmetic '>' comparison is not a redirection (Example A shape)" \
    "if (( \${#MISSING[@]} > 0 )); then echo \"MISSING\"; else echo \"All present\"; fi" "$WT_REPO"
assert_allow "write-confinement (#5515): arithmetic '>=' comparison is not a redirection (Example B shape)" \
    "NOW_EPOCH=100
STALE_EPOCH=50
if (( NOW_EPOCH >= STALE_EPOCH )); then echo \"stale\"; fi" "$WT_REPO"
assert_allow "write-confinement (#5515): simple arithmetic '>' comparison allows" \
    "x=5; if (( x > 0 )); then echo hi; fi" "$WT_REPO"
assert_allow "write-confinement (#5515): arithmetic '<' and '<=' comparisons allow" \
    "x=5; y=10; if (( x < y )); then echo hi; fi; if (( x <= y )); then echo yo; fi" "$WT_REPO"
assert_allow "write-confinement (#5515): '[[ ... ]]' string comparison '>' allows" \
    "a=foo; b=bar; if [[ \"\$a\" > \"\$b\" ]]; then echo hi; fi" "$WT_REPO"
assert_allow "write-confinement (#5515): arithmetic expansion form \$(( x > 0 )) allows" \
    "x=5; echo \$(( x > 0 ))" "$WT_REPO"

# Narrows, never widens: a REAL unquoted redirection sharing the SAME
# segment as a closed arithmetic/test span must still be scanned and denied.
assert_deny "write-confinement (#5515): real '>' redirection AFTER a closed arithmetic span on the same line still denies" \
    "echo \$(( 1 > 0 )) > $WT_REPO/defaults/hooks/f.sh" "$WT_REPO"
assert_deny "write-confinement (#5515): real '>' redirection into the main checkout still denies alongside an arithmetic comparison elsewhere" \
    "x=5; if (( x > 0 )); then echo hi > $WT_REPO/defaults/hooks/g.sh; fi" "$WT_REPO"
assert_deny "write-confinement (#5515): bare '>' redirection (no arithmetic context at all) still denies (regression)" \
    "echo x > $WT_REPO/defaults/hooks/h.sh" "$WT_REPO"
assert_deny "write-confinement (#5515): tee into the main checkout still denies with an unrelated arithmetic comparison present" \
    "x=5; (( x > 0 )); echo x | tee $WT_REPO/f5515.sh" "$WT_REPO"

# -------------------------------------------------------------------------
# Heredoc-body masking (#5000) -- extract_write_targets()/mask_gt() no longer
# misreads a `>` (or other write-idiom syntax) sitting on a heredoc BODY line
# as a real redirection target. Distinct from #4245 above: #4245 covers a `>`
# quoted on the SAME line as the opening quote; this covers a `>` several
# PHYSICAL LINES later, inside a heredoc-wrapped `--body "$(cat <<'EOF' ...
# EOF)"` value -- the idiom this repo's own conventions recommend for any
# multi-line/special-character --body/-m/--title/--notes/--comment value, and
# whose `$(` trips strip_literal_text()'s own command-substitution safety
# floor (#3679), so the raw multi-line text (never X-redacted) flows
# unmodified into extract_write_targets(). Distinct from #4881 (unexpanded
# $VAR redirect *targets*) -- this is misparsed heredoc-body *content*, not
# an unresolvable target.
#
# The literal confirmed repro from #5000 (a `>` several lines into a
# heredoc-wrapped --body value, with a real semicolon elsewhere in the same
# body line):
assert_allow "write-confinement (#5000): heredoc-wrapped --body with '>' in the body allows" \
    'gh issue comment 253 --repo 2AMLogic/klayout-tools --body "$(cat <<'"'"'EOF'"'"'
... observed >240s; later boots ~19s ...
EOF
)"' "$WT_REPO"

# The plain single-line form of the same prose (no heredoc) was already
# allowed pre-#5000 but was never covered by an explicitly named regression
# test -- add one now per the #5000 acceptance criteria.
assert_allow "write-confinement (#5000): plain single-line --body with '>240s' prose allows" \
    'gh issue comment 253 --repo 2AMLogic/klayout-tools --body "... observed >240s; later boots ~19s ..."' "$WT_REPO"

# Narrows, never widens: a REAL unquoted '>' target OUTSIDE the heredoc body
# (in the same multi-line command) must still deny -- proves the heredoc-body
# masking does not blanket-disable write-target detection for the rest of the
# command.
assert_deny "write-confinement (#5000): real unquoted '>' outside a heredoc body still denies" \
    'gh issue comment 253 --body "$(cat <<'"'"'EOF'"'"'
prose with >240s inside, harmless
EOF
)" && echo pwned > '"$WT_REPO"'/defaults/hooks/h.sh' "$WT_REPO"

# -------------------------------------------------------------------------
# Fail-open regression (#5087) -- mask_heredoc_bodies() must NEVER mask a
# heredoc body whose closing delimiter line does not actually exist in the
# buffer.
#
# The first cut of the #5000 fix flipped a sticky `inbody` flag the moment it
# saw the literal substring `<<` anywhere on a line, and only ever cleared it
# on a line that was exactly the bare delimiter. With no such line following,
# `inbody` never reset, so EVERYTHING from that (false) opener to the end of
# the command was replaced with inert placeholders -- including a genuine
# `>`/`tee`/`cp`/`mv` target on a later line, which then never reached
# qsplit()/mask_gt() and so could not be denied. That silently defeated the
# whole write-confinement guard (#4178) on ordinary multi-line Bash-tool
# input containing no heredoc at all.
#
# Both shapes below DENY on pre-#5000 `main` and must keep denying: masking is
# a NARROWING pass, so an unterminated/false opener has to mask nothing rather
# than swallow the rest of the command. These are the two confirmed repros
# from #5087; the three #5000 tests above all use a properly CLOSED
# `<<'EOF' ... EOF` block, which is exactly why none of them caught this.

# 1. A quoted string that merely CONTAINS `<<TOKEN` (no heredoc anywhere),
#    followed on the next line by a real out-of-worktree write.
assert_deny "write-confinement (#5087): quoted '<<TOKEN' then a real '>' write still denies" \
    "echo \"test <<TOKEN\"
echo \"malicious content\" > $WT_REPO/pwned.txt" "$WT_REPO"

# 2. An ordinary arithmetic bitshift (`1 << 3` -- zero heredoc intent),
#    followed on the next line by the same real write. Also pins the
#    opener-detection tightening: a BARE delimiter starting with a digit is a
#    shift operand, not a heredoc delimiter.
assert_deny "write-confinement (#5087): arithmetic '<<' bitshift then a real '>' write still denies" \
    "x=\$((1 << 3))
echo pwned > $WT_REPO/evil.txt" "$WT_REPO"

# Same fail-open shape reached through the other write idioms the masking pass
# feeds -- proves the fix is not `>`-specific.
assert_deny "write-confinement (#5087): unterminated heredoc opener then a real 'tee' still denies" \
    "cat <<UNTERMINATED
some body text that never closes
echo x | tee $WT_REPO/defaults/hooks/teed.sh" "$WT_REPO"
assert_deny "write-confinement (#5087): unterminated heredoc opener then a real 'cp' still denies" \
    "echo \"prose mentioning <<EOF in passing\"
cp /tmp/a.sh $WT_REPO/defaults/hooks/copied.sh" "$WT_REPO"

# A `<<<` herestring is not a heredoc opener either, so a later real write
# must still be seen.
assert_deny "write-confinement (#5087): '<<<' herestring then a real '>' write still denies" \
    "cat <<<\"some string\"
echo pwned > $WT_REPO/defaults/hooks/hs.sh" "$WT_REPO"

# Narrows-not-widens, the other direction: the #5000 false positive must stay
# fixed even when an unterminated/false opener appears EARLIER in the same
# command -- a rejected candidate opener must not prevent a genuinely CLOSED
# heredoc later in the buffer from being masked.
assert_allow "write-confinement (#5087): false opener before a CLOSED heredoc body with '>' still allows" \
    'echo "mentions <<NOPE in prose"
gh issue comment 253 --body "$(cat <<'"'"'EOF'"'"'
... observed >240s; later boots ~19s ...
EOF
)"' "$WT_REPO"

# A quoted delimiter that starts with a digit IS unambiguous heredoc intent
# (unlike a bare `<< 3` shift operand), so a properly closed `<<'3'` block
# still gets its body masked.
assert_allow "write-confinement (#5087): quoted digit delimiter <<'3' masks a closed body" \
    'gh issue comment 253 --body "$(cat <<'"'"'3'"'"'
... observed >240s in the body ...
3
)"' "$WT_REPO"

# -------------------------------------------------------------------------
# Regression (#4210): CWD is the builder's own LINKED worktree, and the write
# targets the MAIN checkout by absolute path (or via `cd $MAIN`). This is the
# canonical builder setup (`cd .loom/worktrees/issue-N`). The guard must key
# its "inside the main checkout" test on the true main root
# (--git-common-dir/..), NOT on `git rev-parse --show-toplevel` (which returns
# the worktree root from a linked worktree) — otherwise a main-checkout write
# from a worktree CWD slips through as ALLOW, leaving the headline #4178
# protection open in exactly the configuration it is meant to cover.
WT_REPO_LINKED=$(make_wt_repo_linked)
WT_LINKED_DIR="$WT_REPO_LINKED/.loom/worktrees/issue-1"
assert_deny "write-confinement: CWD=linked worktree, abs main-checkout write denies (#4210)" \
    "echo x > $WT_REPO_LINKED/defaults/hooks/f.sh" "$WT_LINKED_DIR"
assert_deny "write-confinement: CWD=linked worktree, cd \$MAIN && relative main write denies (#4210)" \
    "cd $WT_REPO_LINKED && echo x > defaults/hooks/f.sh" "$WT_LINKED_DIR"
assert_deny "write-confinement: CWD=linked worktree, sed -i on abs main-checkout path denies (#4210)" \
    "sed -i 's/a/b/' $WT_REPO_LINKED/defaults/hooks/f.sh" "$WT_LINKED_DIR"
# Sibling-allow checks from the same worktree CWD: writing inside the worktree
# and to /tmp must still be permitted (no over-blocking from the new main root).
assert_allow "write-confinement: CWD=linked worktree, write inside the worktree allows (#4210)" \
    "echo x > $WT_LINKED_DIR/src/f.sh" "$WT_LINKED_DIR"
assert_allow "write-confinement: CWD=linked worktree, write to /tmp allows (#4210)" \
    "echo x > /tmp/loom-test-$$-linked.sh" "$WT_LINKED_DIR"

# -------------------------------------------------------------------------
# Unresolvable `$…` write targets fail CLOSED from a LINKED-WORKTREE cwd
# (#4921).
#
# extract_write_targets() emits a target it cannot resolve as the RAW token
# (`$A/evil`), which the resolution then cwd-prefixes as if it were a relative
# path. From a MAIN-CHECKOUT cwd that fabricated path landed inside the main
# checkout, so the containment test denied it and the "unresolvable -> fail
# closed" backstop appeared to hold. From a LINKED-WORKTREE cwd — the
# canonical builder setup, and the only mode #4178 actually protects — the
# very same fabricated path walked straight back up into the acting worktree's
# own `.loom-managed` sentinel and was ALLOWED before the main-root
# containment test ever ran, whatever the variable would expand to at runtime.
#
# Every fixture below therefore uses cwd == WT_LINKED_DIR (a genuine `git
# worktree add` linked worktree), which is exactly what the pre-#4921 suite
# never exercised for these shapes — all of its `$`-target coverage ran with
# cwd == the main checkout, where the bug is invisible.
UNRESOLVED_MAIN_TARGET="$WT_REPO_LINKED/defaults/hooks"

# Headline repro: a variable that is never assigned anywhere in the command.
assert_deny "write-confinement (#4921): CWD=linked worktree, unresolvable \$VAR target denies" \
    "echo x > \$SNEAK_NOT_ASSIGNED_ANYWHERE/evil" "$WT_LINKED_DIR"
# #6110: the unresolved-var deny (a distinct call site from the plain
# main-checkout deny above) must ALSO name the escape hatch.
assert_deny_reason_matches "write-confinement (#4921 x #6110): unresolvable \$VAR deny reason names the guards.worktreeIsolation escape hatch" \
    "echo x > \$SNEAK_NOT_ASSIGNED_ANYWHERE/evil" \
    'guards\.worktreeIsolation:false in \.loom/config\.json' "$WT_LINKED_DIR"
# Same-command CONFLICTING assignment (the shape #4914's record_assign()
# poisons to unresolvable on purpose) must reach the same fail-closed answer.
assert_deny "write-confinement (#4921): CWD=linked worktree, conflicting same-command assignment denies" \
    "A=/tmp/outside
A=$UNRESOLVED_MAIN_TARGET
echo x > \$A/evil" "$WT_LINKED_DIR"
# Shape variants: a leading `./`, surrounding quotes, or `${}` braces must not
# buy an allow the bare form does not get.
assert_deny "write-confinement (#4921): CWD=linked worktree, './\$VAR/…' target denies" \
    "echo x > ./\$SNEAK/evil" "$WT_LINKED_DIR"
assert_deny "write-confinement (#4921): CWD=linked worktree, double-quoted \"\$VAR\"/… target denies" \
    "echo x > \"\$SNEAK\"/evil" "$WT_LINKED_DIR"
assert_deny "write-confinement (#4921): CWD=linked worktree, \${BRACED} target denies" \
    "echo x > \${SNEAK}/evil" "$WT_LINKED_DIR"
# A bare `$VAR` with no path separator at all: the variable may itself hold an
# absolute path into the main checkout, so the root is unknown -> fail closed.
assert_deny "write-confinement (#4921): CWD=linked worktree, bare '\$VAR' target (no slash) denies" \
    "cat > \$DEST" "$WT_LINKED_DIR"
# `$(...)` command substitution is unresolvable in the same way.
assert_deny "write-confinement (#4921): CWD=linked worktree, \$(...) command-substitution target denies" \
    "cat > \$(mktemp)" "$WT_LINKED_DIR"
# Every write idiom, not just `>` redirection.
assert_deny "write-confinement (#4921): CWD=linked worktree, tee with an unresolvable \$VAR denies" \
    "echo x | tee \$OUT/f" "$WT_LINKED_DIR"
assert_deny "write-confinement (#4921): CWD=linked worktree, cp destination \$VAR denies" \
    "cp /tmp/a.sh \$DEST" "$WT_LINKED_DIR"
assert_deny "write-confinement (#4921): CWD=linked worktree, sed -i on an unresolvable \$VAR denies" \
    "sed -i 's/a/b/' \$DEST" "$WT_LINKED_DIR"
# The unexpanded `$` can arrive through the CWD channel instead of the target
# (`cd $A` threads an unresolved curcwd into extract_write_targets()).
assert_deny "write-confinement (#4921): CWD=linked worktree, 'cd \$VAR && relative write' denies" \
    "cd \$A && echo y > f.sh" "$WT_LINKED_DIR"
# An absolute prefix INSIDE the worktree plus an unknown directory component:
# the variable can hold `../..`, so the sentinel walk-up proves nothing.
assert_deny "write-confinement (#4921): CWD=linked worktree, worktree-absolute path with a \$VAR directory component denies" \
    "echo x > $WT_LINKED_DIR/\$X/f.sh" "$WT_LINKED_DIR"
# `/$A/evil` looks absolute but its FIRST component is the variable, so there
# is no known prefix to judge — the runtime value picks the top-level
# directory, the main checkout's own included.
assert_deny "write-confinement (#4921): CWD=linked worktree, '/\$VAR/…' (variable as first component) denies" \
    "echo x > /\$SNEAK/evil" "$WT_LINKED_DIR"
# `/$A` with NO further slash is the same shape — a shell variable's value can
# contain `/`, so "the final component" is a fiction when that component is
# everything below the root.
assert_deny "write-confinement (#4921): CWD=linked worktree, '/\$VAR' (whole path below root is the variable) denies" \
    "echo x > /\$SNEAK" "$WT_LINKED_DIR"
# A `..` traversal inside the KNOWN prefix must be normalized before the
# prefix is judged, or `/tmp/../\$A/evil` hands the test a prefix (`/tmp`) that
# is not where the write actually starts — it collapses to `/`, i.e. the first
# real component is the variable again.
assert_deny "write-confinement (#4921): CWD=linked worktree, known prefix that collapses to '/' via '..' denies" \
    "echo x > /tmp/../\$SNEAK/evil" "$WT_LINKED_DIR"

# --- No new false positives (all from the same linked-worktree cwd) ---
# A `$` only in the FINAL path component leaves the directory fully known and
# genuinely cwd-relative -> the ordinary worktree/main-root logic still applies.
assert_allow "write-confinement (#4921): CWD=linked worktree, \$VAR only in the filename allows" \
    "echo x > out-\$STAMP.log" "$WT_LINKED_DIR"
assert_allow "write-confinement (#4921): CWD=linked worktree, known worktree subdir + \$VAR filename allows" \
    "echo x > src/\$f.txt" "$WT_LINKED_DIR"
# A known prefix OUTSIDE the protected area (e.g. /tmp) protects nothing.
assert_allow "write-confinement (#4921): CWD=linked worktree, /tmp prefix with a \$VAR directory component allows" \
    "echo x > /tmp/loom-test-\$STAMP/f.log" "$WT_LINKED_DIR"
# A `$` a real shell would NEVER expand is literal data, not an unknown path:
# single-quoted and backslash-escaped forms keep their existing treatment
# (mirrors the quoted-tilde rule of #4382).
assert_allow "write-confinement (#4921): CWD=linked worktree, single-quoted '\$A/…' is literal (shell never expands it) and allows" \
    "echo x > '\$A/evil'" "$WT_LINKED_DIR"
assert_allow "write-confinement (#4921): CWD=linked worktree, backslash-escaped \\\$A/… is literal and allows" \
    "echo x > \\\$A/evil" "$WT_LINKED_DIR"
# The filename-only exemption must NOT become a hole into the main checkout:
# the directory is fully known there, so the ordinary containment test still
# runs and still denies.
assert_deny "write-confinement (#4921): CWD=linked worktree, \$VAR filename under a MAIN-checkout dir still denies" \
    "echo x > $UNRESOLVED_MAIN_TARGET/out-\$STAMP.log" "$WT_LINKED_DIR"
# A `$` that is only quoted DATA in a message argument (never a write target)
# must not manufacture a deny — the quote-aware `>` scan of #4245/#4289 and the
# literal-text redaction still decide that, unchanged.
assert_allow "write-confinement (#4921): CWD=linked worktree, quoted '>' and '\$' inside a commit message allows" \
    "git commit -m \"price > \$5 total\"" "$WT_LINKED_DIR"
# Regression: ordinary in-worktree and /tmp writes are untouched.
assert_allow "write-confinement (#4921): CWD=linked worktree, plain in-worktree write still allows" \
    "echo x > $WT_LINKED_DIR/src/plain.sh" "$WT_LINKED_DIR"

# The pre-existing MAIN-checkout-cwd behaviour for the same command must not
# change (it was already fail-closed there -- #4921 makes the two cwds agree,
# it does not relax either one).
assert_deny "write-confinement (#4921): CWD=main checkout, unresolvable \$VAR target still denies" \
    "echo x > \$SNEAK_NOT_ASSIGNED_ANYWHERE/evil" "$WT_REPO_LINKED"

# Fail-open contract: with no managed worktree anywhere, an unresolvable
# target is allowed exactly like every other write in that repo.
assert_allow "write-confinement (#4921): no managed worktree anywhere -> unresolvable \$VAR allows (fail-open)" \
    "echo x > \$SNEAK/evil" "$WT_REPO_NOWT"
# And the category toggle still switches the whole check off.
assert_allow_env "write-confinement (#4921): LOOM_GUARD_WORKTREE_ISOLATION=0 -> unresolvable \$VAR allows" \
    "LOOM_GUARD_WORKTREE_ISOLATION=0" "echo x > \$SNEAK/evil" "$WT_LINKED_DIR"

# -------------------------------------------------------------------------
# Tilde expansion for write targets (#4382, same fix family as #4245/#4289's
# quote-aware `>` scanning). Reported incident: `cp <built-binary>
# ~/.local/bin/loom-daemon` from a main-checkout cwd was denied because the
# raw `~/.local/bin/loom-daemon` token was resolved as REPO-relative -- the
# real shell expands the leading `~` to $HOME first, landing the write far
# outside the checkout entirely.
#
# HOME_FIXTURE_OUTSIDE is a throwaway dir with no relation to WT_REPO, used to
# make the "expands outside the repo -> allow" cases deterministic regardless
# of the operator's real $HOME.
HOME_FIXTURE_OUTSIDE=$(mktemp -d)
CURRENT_UNIX_USER=$(id -un 2>/dev/null || whoami)

assert_allow_env "write-confinement (#4382): unquoted leading '~/' expands to \$HOME, landing outside the checkout allows" \
    "HOME=$HOME_FIXTURE_OUTSIDE" \
    "cp /tmp/a.sh ~/.local/bin/loom-daemon" "$WT_REPO"
assert_allow_env "write-confinement (#4382): bare unquoted '~' (whole word) expands to \$HOME, outside the checkout allows" \
    "HOME=$HOME_FIXTURE_OUTSIDE" \
    "cp /tmp/a.sh ~" "$WT_REPO"
assert_allow "write-confinement (#4382): unquoted '~user/' (current user) resolves via the passwd db, outside the checkout allows" \
    "cp /tmp/a.sh ~${CURRENT_UNIX_USER}/.local/bin/loom-daemon" "$WT_REPO"

# Expansion must not become a blanket allow -- if $HOME itself resolves inside
# the main checkout, the expanded (now-absolute) target still denies exactly
# like any other absolute main-checkout write. This also proves the guard
# expands using its OWN process $HOME (set once, before the command is ever
# parsed) rather than scanning the command text for a `HOME=...` token -- an
# inline `HOME=<repo> cmd ~/x` game in the analyzed command string cannot
# redefine what "$HOME" means to the guard, mirroring real bash: a same-line
# `VAR=value command` prefix only changes the CHILD command's environment, it
# never affects tilde expansion of that same command line (word expansion
# runs against the invoking shell's own $HOME, not the prefix assignment).
assert_deny_env "write-confinement (#4382): expanded '~/' landing INSIDE the main checkout still denies (no blanket ~ allow)" \
    "HOME=$WT_REPO" \
    "cp /tmp/a.sh ~/defaults/hooks/f.sh" "$WT_REPO"

# Quoted / escaped tildes are NOT expanded by a real shell -- must keep the
# existing literal repo-relative treatment (no regression).
assert_deny "write-confinement (#4382): single-quoted leading tilde stays literal (shell never expands it), still denies" \
    "cp /tmp/a.sh '~/defaults/hooks/f.sh'" "$WT_REPO"
assert_deny "write-confinement (#4382): backslash-escaped leading tilde stays literal (shell never expands it), still denies" \
    "cp /tmp/a.sh \~/defaults/hooks/f.sh" "$WT_REPO"

# A tilde that is not the FIRST character of the token is not an expansion
# position at all (e.g. `foo~/bar`) -- must stay untouched/literal.
assert_deny "write-confinement (#4382): non-leading tilde ('backup~/f.sh') is not an expansion case, still resolves repo-relative" \
    "cp /tmp/a.sh defaults/hooks/backup~/f.sh" "$WT_REPO"

# An unresolvable ~user (no matching account) is left untouched rather than
# guessed -- falls back to the existing (safe) repo-relative/deny treatment.
assert_deny "write-confinement (#4382): unresolvable '~nonexistentuser/' falls back to literal repo-relative path, still denies" \
    "cp /tmp/a.sh ~nonexistentloomuser999/defaults/hooks/f.sh" "$WT_REPO"

# -------------------------------------------------------------------------
# Same-command $VAR/${VAR} resolution for write targets (#4881). Reported
# incident: `SCRATCH=/private/tmp/.../scratchpad` assigned on one line, then
# `gh pr view ... >> $SCRATCH/wave1-merged-files.txt` on the next, was denied
# as a worktree-isolation bypass -- the tokenizer treated the literal string
# "$SCRATCH/wave1-merged-files.txt" as a REPO-RELATIVE path (cwd-prefixed)
# instead of resolving it via the SCRATCH assignment two lines earlier, even
# though the real target resolves far outside the repo.
OUTSIDE_SCRATCH=$(mktemp -d)

assert_allow "write-confinement (#4881): \$VAR assigned earlier in the same command, redirect resolves outside the repo -> allow" \
    "SCRATCH=$OUTSIDE_SCRATCH
echo x >> \$SCRATCH/out.txt" "$WT_REPO"
assert_allow "write-confinement (#4881): \${VAR} (braced) form resolves outside the repo -> allow" \
    "SCRATCH=$OUTSIDE_SCRATCH
echo x >> \${SCRATCH}/out.txt" "$WT_REPO"
assert_allow "write-confinement (#4881): tee target resolved via same-command \$VAR outside the repo -> allow" \
    "SCRATCH=$OUTSIDE_SCRATCH
echo x | tee \$SCRATCH/out.txt" "$WT_REPO"
assert_allow "write-confinement (#4881): cp destination resolved via same-command \$VAR outside the repo -> allow" \
    "SCRATCH=$OUTSIDE_SCRATCH
cp /tmp/a.sh \$SCRATCH/out.txt" "$WT_REPO"

# The resolved target STILL denies when it lands inside the main checkout --
# variable resolution must only narrow the false positive, never weaken the
# #4178 protection.
assert_deny "write-confinement (#4881): \$VAR assigned earlier in the same command, redirect resolves INSIDE the repo -> still denies" \
    "SCRATCH=$WT_REPO
echo x >> \$SCRATCH/defaults/hooks/f.sh" "$WT_REPO"

# Other assignment SHAPES resolve too (#4914 review). Before this, only a
# segment that was EXACTLY one bare `NAME=value` populated the resolver, so
# every other (extremely common) assignment shape stayed unresolvable.
assert_allow "write-confinement (#4881): 'export'-prefixed assignment resolves outside the repo -> allow" \
    "export SCRATCH=$OUTSIDE_SCRATCH
echo x >> \$SCRATCH/out.txt" "$WT_REPO"
assert_allow "write-confinement (#4881): 'readonly'-prefixed assignment resolves outside the repo -> allow" \
    "readonly SCRATCH=$OUTSIDE_SCRATCH
cp /tmp/a.sh \$SCRATCH/out.txt" "$WT_REPO"
assert_allow "write-confinement (#4881): 'declare -x' assignment (keyword + flag) resolves outside the repo -> allow" \
    "declare -x SCRATCH=$OUTSIDE_SCRATCH
echo x >> \$SCRATCH/out.txt" "$WT_REPO"
assert_allow "write-confinement (#4881): 'local' assignment inside a function body resolves outside the repo -> allow" \
    "f() {
  local SCRATCH=$OUTSIDE_SCRATCH
  cp /tmp/a.sh \$SCRATCH/out.txt
}" "$WT_REPO"
assert_allow "write-confinement (#4881): several assignments in one segment resolve outside the repo -> allow" \
    "A=1 SCRATCH=$OUTSIDE_SCRATCH
mv /tmp/a.sh \$SCRATCH/out.txt" "$WT_REPO"
assert_allow "write-confinement (#4881): env-var prefix on the writing command itself resolves outside the repo -> allow" \
    "SCRATCH=$OUTSIDE_SCRATCH
LC_ALL=C cp /tmp/a.sh \$SCRATCH/out.txt" "$WT_REPO"

# ...and each of those shapes STILL denies when the resolved value lands
# inside the main checkout -- widening the assignment scan must not weaken the
# #4178 protection for the shapes it newly understands.
assert_deny "write-confinement (#4881): 'export'-prefixed assignment resolving INSIDE the repo -> still denies" \
    "export SNEAK=$WT_REPO/defaults/hooks
echo pwned > \$SNEAK/evil.sh" "$WT_REPO"
assert_deny "write-confinement (#4881): 'readonly'-prefixed assignment resolving INSIDE the repo -> still denies" \
    "readonly SNEAK=$WT_REPO/defaults/hooks
cp /tmp/a.sh \$SNEAK/evil.sh" "$WT_REPO"
assert_deny "write-confinement (#4881): 'declare'-prefixed assignment resolving INSIDE the repo -> still denies" \
    "declare SNEAK=$WT_REPO/defaults/hooks
cp /tmp/a.sh \$SNEAK/evil.sh" "$WT_REPO"
assert_deny "write-confinement (#4881): 'local' assignment in a function resolving INSIDE the repo -> still denies" \
    "f() {
  local SNEAK=$WT_REPO/defaults/hooks
  cp /tmp/a.sh \$SNEAK/evil.sh
}" "$WT_REPO"
assert_deny "write-confinement (#4881): multi-assignment segment resolving INSIDE the repo -> still denies" \
    "A=1 SNEAK=$WT_REPO/defaults/hooks
mv /tmp/a.sh \$SNEAK/evil.sh" "$WT_REPO"
assert_deny "write-confinement (#4881): env-var prefix on the writing command itself, target INSIDE the repo -> still denies" \
    "SNEAK=$WT_REPO/defaults/hooks
LC_ALL=C cp /tmp/a.sh \$SNEAK/evil.sh" "$WT_REPO"
# An env-var prefix must not hide the command it prefixes from the scan at all.
assert_deny "write-confinement (#4881): env-var-prefixed cp to a literal in-repo path -> still denies" \
    "LC_ALL=C cp /tmp/a.sh $WT_REPO/defaults/hooks/evil.sh" "$WT_REPO"

# FAIL-CLOSED (#4914 review): an UNRESOLVABLE $VAR is NOT skipped. It keeps
# the pre-#4881 literal (repo-relative) treatment, so an unparsed assignment
# shape can never become a free worktree-isolation bypass. The narrow #4881
# fix only relaxes targets it can actually PROVE resolve outside the repo.
assert_deny "write-confinement (#4881): unresolvable \$VAR (no matching assignment) stays fail-closed -> denies" \
    "echo x >> \$NOSUCHVARFORLOOMTEST4881/out.txt" "$WT_REPO"
assert_deny "write-confinement (#4881): \$VAR whose value is itself an unresolved \$VAR (chained) stays fail-closed -> denies" \
    "SNEAK=\$SOMETHINGUNKNOWN4881/defaults/hooks
cp /tmp/a.sh \$SNEAK/evil.sh" "$WT_REPO"
assert_deny "write-confinement (#4881): \$(...) command-substitution target stays fail-closed -> denies" \
    "cp /tmp/a.sh \$(echo defaults)/hooks/evil.sh" "$WT_REPO"
assert_deny "write-confinement (#4881): \${VAR:-default} (non-bare reference) stays fail-closed -> denies" \
    "cp /tmp/a.sh \${NOSUCHVAR4881:-defaults}/hooks/evil.sh" "$WT_REPO"
# An assignment appearing only AFTER the write must not resolve it backwards.
assert_deny "write-confinement (#4881): assignment AFTER the write does not resolve it retroactively -> denies" \
    "cp /tmp/a.sh \$LATER4881/evil.sh
LATER4881=$OUTSIDE_SCRATCH" "$WT_REPO"

# -------------------------------------------------------------------------
# #6444: DOUBLE-QUOTED reference to a same-command literal assignment
# (`"$VAR/path"`) must resolve exactly like the unquoted form above. qsplit()
# preserves quote characters verbatim in each token, so every one of the
# five write-target print sites inside extract_write_targets() previously
# called resolve_var() on the RAW, still-quoted token -- resolve_var()'s own
# `substr(tok, 1, 1) != "$"` guard saw a leading `"` (not `$`) and bailed out
# immediately, leaving an otherwise fully-known target unresolved and
# denying "worktree-write-confinement-unresolved-var" for the extremely
# common, safe double-quoted idiom. Covers all 5 call sites: bare `>`,
# attached `>file`, tee, sed -i, and cp/mv.
assert_allow "write-confinement (#6444): double-quoted \"\$VAR/path\" bare > redirect resolves outside the repo -> allow" \
    "SCRATCH=$OUTSIDE_SCRATCH
echo x > \"\$SCRATCH/out.txt\"" "$WT_REPO"
assert_allow "write-confinement (#6444): double-quoted \"\$VAR/path\" attached >file redirect resolves outside the repo -> allow" \
    "SCRATCH=$OUTSIDE_SCRATCH
echo x >\"\$SCRATCH/out.txt\"" "$WT_REPO"
assert_allow "write-confinement (#6444): double-quoted \"\$VAR/path\" tee target resolves outside the repo -> allow" \
    "SCRATCH=$OUTSIDE_SCRATCH
echo x | tee \"\$SCRATCH/out.txt\"" "$WT_REPO"
assert_allow "write-confinement (#6444): double-quoted \"\$VAR/path\" cp destination resolves outside the repo -> allow" \
    "SCRATCH=$OUTSIDE_SCRATCH
cp /tmp/a.sh \"\$SCRATCH/out.txt\"" "$WT_REPO"
assert_allow "write-confinement (#6444): double-quoted \"\$VAR/path\" sed -i target resolves outside the repo -> allow" \
    "SCRATCH=$OUTSIDE_SCRATCH
sed -i 's/a/b/' \"\$SCRATCH/out.txt\"" "$WT_REPO"

# The issue's own exact single-line repro shape: literal double-quoted
# assignment, double-quoted usage, all on one physical line (no newlines at
# all -- confirms this was never actually a multi-line-scoping bug).
assert_allow "write-confinement (#6444): single-line double-quoted-assignment + double-quoted-usage repro resolves outside the repo -> allow" \
    "VAR=\"$OUTSIDE_SCRATCH\"; echo hi > \"\$VAR/f.txt\"" "$WT_REPO"

# Still denies when the double-quoted resolved target lands INSIDE the main
# checkout -- quote-aware resolution must only narrow the false positive,
# never weaken the #4178 protection.
assert_deny "write-confinement (#6444): double-quoted \"\$VAR/path\" resolving INSIDE the repo -> still denies" \
    "SNEAK=$WT_REPO/defaults/hooks
echo pwned > \"\$SNEAK/evil.sh\"" "$WT_REPO"

# A literal SINGLE-quoted reference (a file whose name literally contains the
# characters '\$SCRATCH' -- the shell never expands a single-quoted `\$`)
# must stay UNAFFECTED: never substituted with varmap's value. The literal
# path here is cwd-relative and lands inside the repo, so it denies on ITS
# OWN literal-path semantics -- if this had been incorrectly substituted
# with SCRATCH's value it would instead ALLOW, which is the false-ALLOW
# regression this test guards against.
assert_deny "write-confinement (#6444): single-quoted '\$SCRATCH/f' is a shell literal, NOT substituted with varmap's value -> still denies on its own literal path" \
    "SCRATCH=$OUTSIDE_SCRATCH
echo hi > '\$SCRATCH/f'" "$WT_REPO"

# A genuinely unresolvable double-quoted target still denies as unresolved --
# the fail-closed floor (#4921/#6172) is unaffected by quote-aware
# resolution.
assert_deny "write-confinement (#6444): double-quoted \$(mktemp -d) command-substitution target stays fail-closed -> denies" \
    "cp /tmp/a.sh \"\$(mktemp -d)/evil.sh\"" "$WT_REPO"
assert_deny "write-confinement (#6444): double-quoted unresolvable \$VAR (no matching assignment) stays fail-closed -> denies" \
    "echo x >> \"\$NOSUCHVARFORLOOMTEST6444/out.txt\"" "$WT_REPO"

# -------------------------------------------------------------------------
# #6940: a same-command literal assignment consumed by a redirect NESTED
# inside a `$(...)` command substitution. Reported by the Auditor's guard-
# decision telemetry review (#3898): ~19 of the 116
# `worktree-write-confinement-unresolved-var` denials on one host were the
# extremely common capture-stderr idiom
#
#     ERR_FILE=/tmp/champion_ci_err_6212.txt
#     out=$(gh pr checks "$PR" --json bucket 2>"$ERR_FILE")
#
# extract_write_targets() is a tokenizer with no notion of substitution
# nesting, so the redirect target reached resolve_var_q() as the token
# `"$ERR_FILE")` -- the ENCLOSING substitution's closing paren still glued on
# -- which missed both the double-quote-pair test (#6444) and resolve_var()'s
# leading-`$` test, denying a target the resolver had already recorded. The
# identical command WITHOUT the `$(...)` wrapper always resolved fine, which
# is what proved this a tokenization gap rather than a deliberate "a
# substitution is a fresh unresolvable scope" rule. strip_subst_close_parens()
# now peels only UNBALANCED trailing `)` characters before resolution.
assert_allow "write-confinement (#6940): literal \$VAR assignment used by a 2> redirect nested in \$(...) resolves outside the repo -> allow" \
    "ERR_FILE=$OUTSIDE_SCRATCH/champion_ci_err.txt
out=\$(gh pr checks 6212 --json bucket,name 2>\"\$ERR_FILE\")" "$WT_REPO"
assert_allow "write-confinement (#6940): the same nested-\$(...) shape written as a ';'-separated SINGLE line -> allow" \
    "ERR_FILE=$OUTSIDE_SCRATCH/champion_ci_err.txt; out=\$(gh pr checks 6212 2>\"\$ERR_FILE\")" "$WT_REPO"
assert_allow "write-confinement (#6940): UNQUOTED \$VAR redirect target nested in \$(...) (stray ')' stripped) -> allow" \
    "ERR_FILE=$OUTSIDE_SCRATCH
out=\$(gh pr checks 6212 2>\$ERR_FILE/err.txt)" "$WT_REPO"
assert_allow "write-confinement (#6940): SPACED bare '>' redirect target nested in \$(...) resolves outside the repo -> allow" \
    "SCRATCH=$OUTSIDE_SCRATCH
out=\$(echo x > \"\$SCRATCH/out.txt\")" "$WT_REPO"
assert_allow "write-confinement (#6940): tee target inside a pipeline nested in \$(...) resolves outside the repo -> allow" \
    "SCRATCH=$OUTSIDE_SCRATCH
out=\$(echo x | tee \"\$SCRATCH/out.txt\")" "$WT_REPO"
assert_allow "write-confinement (#6940): DOUBLY-nested \$( ... \$( ... 2>\"\$VAR\")) resolves outside the repo -> allow" \
    "ERR_FILE=$OUTSIDE_SCRATCH/err.txt
out=\$(printf '%s' \$(gh pr checks 6212 2>\"\$ERR_FILE\"))" "$WT_REPO"

# ...and every fail-closed guarantee is unchanged for the nested shape. Peeling
# an unbalanced trailing `)` can only ever SHORTEN a path within the same
# parent directory, so a target that resolved INSIDE the main checkout still
# does; and a target the resolver cannot prove is still refused, not guessed.
assert_deny "write-confinement (#6940): nested-\$(...) redirect whose \$VAR resolves INSIDE the repo -> still denies" \
    "SNEAK=$WT_REPO/defaults/hooks
out=\$(gh pr checks 6212 2>\"\$SNEAK/evil.sh\")" "$WT_REPO"
assert_deny "write-confinement (#6940): nested-\$(...) tee target resolving INSIDE the repo -> still denies" \
    "SNEAK=$WT_REPO/defaults/hooks
out=\$(echo pwned | tee \"\$SNEAK/evil.sh\")" "$WT_REPO"
assert_deny "write-confinement (#6940): CONFLICTING same-command reassignment + nested-\$(...) usage stays AMBIG -> denies" \
    "ERR_FILE=$OUTSIDE_SCRATCH/a
ERR_FILE=$OUTSIDE_SCRATCH/b
out=\$(gh pr checks 6212 2>\"\$ERR_FILE\")" "$WT_REPO"
assert_deny "write-confinement (#6940): dynamic \$(mktemp -d) target inside a nested-\$(...) redirect stays fail-closed -> denies" \
    "out=\$(gh pr checks 6212 2>\"\$(mktemp -d)/err.txt\")" "$WT_REPO"
# UPDATED BY #6949: this target used to fail closed here (record_assign()/
# resolve_var() cannot resolve a command-substitution RHS like `$(mktemp -d)`
# at all), but wt_write_mktemp_same_command_safe() (#6949) now proves TMPD
# is a same-command mktemp -d scratch dir regardless of the enclosing
# nested-$(...) redirect (strip_subst_close_parens()/resolve_var_q() still
# strip the stray trailing paren before the mktemp check runs) -- so this now
# correctly allows, matching the identical non-nested case elsewhere in the
# #6949 section below.
assert_allow "write-confinement (#6940/#6949): \$VAR whose value is itself \$(mktemp -d), used in a nested-\$(...) redirect -> allow" \
    "TMPD=\$(mktemp -d)
out=\$(gh pr checks 6212 2>\"\$TMPD/err.txt\")" "$WT_REPO"
assert_deny "write-confinement (#6940): unresolvable \$VAR (no matching assignment) in a nested-\$(...) redirect -> denies" \
    "out=\$(gh pr checks 6212 2>\"\$NOSUCHVARFORLOOMTEST6940/err.txt\")" "$WT_REPO"
# A BALANCED `$(...)` target is the token's OWN paren, never an enclosing
# substitution's -- it must stay untouched and unresolvable (the #6444
# fail-closed case above, restated here as the direct boundary of the #6940
# strip).
assert_deny "write-confinement (#6940): balanced \"\$(mktemp)\" target (no enclosing substitution) is not paren-stripped -> denies" \
    "echo x > \"\$(mktemp)\"" "$WT_REPO"

# -------------------------------------------------------------------------
# #6953: a DOUBLE-QUOTED RHS same-command assignment wrapping a `$(...)`
# command substitution (`NAME="$(cmd)"`) previously corrupted a LATER,
# unrelated write-target token instead of either resolving it or leaving it
# an intact, unresolved literal.
#
# Root cause #1 (assignment-word tokenization): the assignment-scanning loop
# matched a leading `NAME=value` word with the plain, whitespace-based
# `/^[A-Za-z_][A-Za-z0-9_]*=[^ \t]*([ \t]+|$)/` regex -- not quote-aware --
# so a quoted value containing an embedded space (`"$(mktemp -d)"`,
# `"$(echo /tmp/foo)"`, both from `cmd`'s own arguments) truncated the match
# mid-quote and fed record_assign() a mangled fragment (`tmp="$(mktemp`),
# poisoning varmap with an equally mangled value.
#
# Root cause #2 (qsplit() quote-state loss): qsplit()'s own "span carries a
# command substitution, keep separators ACTIVE" branch emitted only the
# OPENING quote character and fell through the main loop with no memory of
# being inside a quote, so the span's REAL, already-located closing quote
# was mistaken for a brand-new quote-open on the next iteration -- silently
# swallowing any live `;`/`&`/`|` separator between it and the NEXT quoted
# span in the buffer (e.g. a later `"$NAME/path"` write-target usage) into a
# bogus "inert" verbatim copy, corrupting the `;`-joined single-line form
# even once root cause #1 was fixed.
#
# Both defects are fixed together: match_assignword() tokenizes the
# assignment word quote-aware (fixing #1), and qsplit() now recurses on the
# already-located inner span instead of losing its closing-quote position
# (fixing #2). The correct behavior mirrors the existing "value chains to
# another unresolved $-reference" rule: `$(cmd)` is not a bare variable
# reference, so record_assign() stores a value that itself starts with `$`,
# and resolve_var() already refuses to guess through THAT -- the usage-line
# target token comes back completely unchanged (intact), not spliced.
#
# #6949 interaction: a same-command `NAME=$(mktemp -d)` assignment -- INCLUDING
# the double-quoted RHS form, since wt_write_mktemp_same_command_safe() lists
# `rhs == "\"$(mktemp -d)\""` as one of its exact-match safe shapes -- is
# proven-safe scratch-dir usage and ALLOWED by #6949. Once this PR's
# match_assignword()/qsplit() fix resolves `tmp` to the clean, intact
# `$(mktemp -d)` value instead of a corrupted fragment, #6949's own
# same-command mktemp check recognizes it and legitimately allows the write --
# so the two MKTEMP-valued cases below assert ALLOW, mirroring #6949's own
# unquoted-form coverage, while the two PLAIN-LITERAL cases (`$(echo
# /tmp/foo)`, which #6949 never treats as safe) still assert the INTACT deny
# reason.
assert_allow "write-confinement (#6953/#6949): mktemp-valued double-quoted-RHS \$(...) assignment, SEPARATE lines -> allow (proven-safe scratch dir per #6949)" \
    "tmp=\"\$(mktemp -d)\"
echo hi > \"\$tmp/out.txt\"" "$WT_REPO"
assert_allow "write-confinement (#6953/#6949): mktemp-valued double-quoted-RHS \$(...) assignment, ';'-JOINED single line -> allow (proven-safe scratch dir per #6949)" \
    "tmp=\"\$(mktemp -d)\"; echo hi > \"\$tmp/out.txt\"" "$WT_REPO"
assert_deny_reason_matches "write-confinement (#6953): plain-literal-valued double-quoted-RHS \$(...) assignment (non-mktemp), SEPARATE lines -> denies with the INTACT usage token (not corrupted)" \
    "tmp=\"\$(echo /tmp/foo)\"
echo hi > \"\$tmp/out.txt\"" \
    'write target '"'"'\$tmp/out\.txt'"'"'' "$WT_REPO"
assert_deny_reason_matches "write-confinement (#6953): plain-literal-valued double-quoted-RHS \$(...) assignment (non-mktemp), ';'-JOINED single line -> denies with the INTACT usage token (not corrupted)" \
    "tmp=\"\$(echo /tmp/foo)\"; echo hi > \"\$tmp/out.txt\"" \
    'write target '"'"'\$tmp/out\.txt'"'"'' "$WT_REPO"

# The two PLAIN-LITERAL assert_deny_reason_matches calls above only PASS if
# the deny reason contains the exact, intact `write target '$tmp/out.txt'`
# substring -- the reported corrupted shapes (`'"$(mktemp/out.txt'`,
# `'"$(echo/out.txt'`) cannot satisfy that pattern, so a regression back to
# either root cause fails these tests directly. The two MKTEMP-valued
# assert_allow calls above exercise the same tokenizer fix from the opposite
# angle: a regression back to either root cause would corrupt `tmp` to an
# unresolved/mangled value, #6949's same-command mktemp check would no longer
# recognize it as safe, and ALLOW would flip back to a fail-closed DENY --
# failing these tests too. No separate negative assertion is needed.

# -------------------------------------------------------------------------
# #7356: Curator-revised diagnosis for the highest-volume guard-decision
# telemetry pattern -- a real log line (`.loom/logs/guard-decisions.log`,
# ts=2026-09-08T00:50:21Z) denied `worktree-write-confinement-unresolved-var`
# on a write target of `"$D/comments.json";` -- a trailing `;` glued onto an
# otherwise-resolvable same-command mktemp write target. The Curator's
# 2026-09-08T05:16Z hypothesis: qsplit()'s "span carries a command
# substitution, keep separators active" branch loses track of its own
# already-located closing quote once a double-quoted span CONTAINING that
# span's own inner `$((` (bash arithmetic expansion, e.g.
# `"@$((NOW-120))"` inside `F1=$(date -u -d "@$((NOW-120))" +"...")`)
# is scanned earlier in the same command -- reprocessing the real closing
# quote as a fresh quote-open and desyncing quote-parity forward, corrupting
# a LATER write target.
#
# Traced and CONFIRMED as a real (now historical) defect: this is the exact
# root cause already fixed by #6956 (closing #6953, merged 2026-09-08
# 17:51 -- about 12.5 hours AFTER the Curator's revision, but before this
# issue was picked up) -- see that commit's own header, which independently
# describes the identical mechanism ("qsplit()'s...branch emitted only the
# OPENING quote character and fell through the main loop with no memory of
# being inside a quote, so the span's REAL, already-located closing quote
# was mistaken for a brand-new quote-open on the next iteration"). #6472
# (merged earlier) fixed the same qsplit() defect for a differently-shaped
# trigger (a `$((...))`-carrying `sed -n` script piped to a later segment).
#
# Verified directly against the CURRENT tokenizer (`bash
# defaults/hooks/guard-destructive-generic.sh` fed the log line's exact
# command, standalone `awk -f`'d `_QSPLIT_AWK` snippet, and this suite's own
# `$WT_REPO` fixture): the exact reproduced command no longer false-denies
# `worktree-write-confinement-unresolved-var`. Replaying the FULL,
# unmodified log-line command (including its `rm -f "$D"/post-count
# "$D"/post-*.body`) now denies for a DIFFERENT, correct reason instead --
# `rm-scope-unresolved-var` -- because rm-scope's OWN same-command mktemp
# resolver (`rm_scope_mktemp_same_command_safe()`, #6520) deliberately
# excludes a SUFFIXED rm target (`$NAME/sub`, not a bare `$NAME`/`${NAME}`)
# from its fast path (see that function's own header doc) -- a documented,
# pre-existing, unrelated scope limitation, not a bug this issue tracks. No
# hook-logic changes are needed; these are pin/regression tests only.
D_MKTEMP_ARITH_SEGMENTS='NOW=$(date -u +%s) && F1=$(date -u -d "@$((NOW-120))" +"%Y-%m-%dT%H:%M:%SZ") && F2=$(date -u -d "@$((NOW-60))" +"%Y-%m-%dT%H:%M:%SZ")'

assert_allow "write-confinement (#7356): mktemp-resolved write target, preceded by TWO \$((...))-containing double-quoted assignment spans in the same command, ';'-terminated -> allows (the exact false-positive shape from the cited log line, now fixed by #6953/#6472)" \
    "D=\$(mktemp -d) && echo first > \"\$D/gh\" && $D_MKTEMP_ARITH_SEGMENTS && echo second > \"\$D/comments.json\"; echo done" \
    "$WT_REPO"
assert_deny_reason_matches "write-confinement (#7356): literal in-repo \$D write target, preceded by the same \$((...))-containing spans -> still denies with the INTACT resolved target (not the '\"\$D/comments.json\";' corrupted splice)" \
    "D=$WT_REPO/defaults/hooks && $D_MKTEMP_ARITH_SEGMENTS && echo second > \"\$D/comments.json\"; echo done" \
    "defaults/hooks/comments\.json" "$WT_REPO"
assert_deny_reason_matches "write-confinement (#7356 fail-open check): a genuinely UNRESOLVABLE \$VAR write target, preceded by the same \$((...))-containing spans, still fails closed with the INTACT target (not corrupted, not silently allowed)" \
    "$D_MKTEMP_ARITH_SEGMENTS && echo pwned > \"\$NEVER_ASSIGNED_7356/evil.sh\"; echo done" \
    'write target '"'"'\$NEVER_ASSIGNED_7356/evil\.sh'"'"'' "$WT_REPO"

# Pin the FULL, byte-for-byte log-line command (ts=2026-09-08T00:50:21Z) as a
# regression fixture: it must deny (never silently allow), and the reason
# must be the rm-scope check -- not a reversion to the write-confinement
# false positive this issue was filed against.
GUARD_LOG_LINE_CMD_7356='D=$(mktemp -d) && sed -n '"'"'118,195p'"'"' defaults/scripts/tests/test-sweep-lease-publish.sh > "$D/gh" && chmod +x "$D/gh" && export LOOM_TEST_STUB_DIR="$D" PATH="$D:$PATH" LOOM_HOST_ID="studio-host" && NOW=$(date -u +%s) && F1=$(date -u -d "@$((NOW-120))" +"%Y-%m-%dT%H:%M:%SZ") && F2=$(date -u -d "@$((NOW-60))" +"%Y-%m-%dT%H:%M:%SZ") && jq -n --arg p "XXX" --arg o "XXX" '"'"'[{updated_at:$p, body:"<!-- loom:lease host=peer-host sweep=sweep-peer-1 -->\nprose"},{updated_at:$o, body:"<!-- loom:lease host=host-471642b3 sweep=sweep-old-local -->\nprose"}]'"'"' > "$D/comments.json" && ./defaults/scripts/sweep-lease-publish.sh publish 6320 --sweep-id sweep-run-NEW; echo "RC=$?"; echo "posts=$(cat "$D/post-count" 2>/dev/null || echo 0)"; echo "--- now malformed-freshest case ---"; rm -f "$D"/post-count "$D"/post-*.body; jq -n --arg p "XXX" --arg o "XXX" '"'"'[{updated_at:$p, body:"<!-- loom:lease host=peer-host sweep=sweep-peer-1 -->\nprose"},{updated_at:$o, body:"<!-- loom:lease host=broken-no-close\nprose"}]'"'"' > "$D/comments.json"; ./defaults/scripts/sweep-lease-publish.sh publish 6320 --sweep-id sweep-run-NEW; echo "RC=$?"; echo "posts=$(cat "$D/post-count" 2>/dev/null || echo 0)"; rm -rf "$D"'
assert_deny_reason_matches "write-confinement (#7356): FULL cited log-line command (ts=2026-09-08T00:50:21Z) denies for a DIFFERENT, correct reason (rm-scope, #6520's documented suffixed-target exclusion) -- not the write-confinement false positive this issue tracked" \
    "$GUARD_LOG_LINE_CMD_7356" \
    '^BLOCKED: rm target' "$WT_REPO"

# CONFLICTING ASSIGNMENTS POISON THE VARIABLE (#4914 review). The assignment
# scan is not control-flow aware -- qsplit() flattens `||`/`&&`/`;` into plain
# segments -- so `A=<in-repo> || A=/tmp/outside` reaches record_assign() as two
# assignments to one name. Last-write-wins would resolve `$A` to the LAST value
# in the token stream, but a real bash short-circuits `||` and never takes that
# branch: the write actually lands INSIDE the main checkout. Poisoning the name
# to the unresolvable sentinel routes it back to the literal (cwd-prefixed)
# fail-closed path, so it denies either way round.
assert_deny "write-confinement (#4914): 'A=<in-repo> || A=<outside>' must not resolve to the un-taken branch -> denies" \
    "SNEAK=$WT_REPO/defaults/hooks || SNEAK=$OUTSIDE_SCRATCH
echo pwned > \$SNEAK/evil.sh" "$WT_REPO"
assert_deny "write-confinement (#4914): '&&' + '||' combined branch assignment does not resolve to the un-taken branch -> denies" \
    "SNEAK=$WT_REPO/defaults/hooks && echo ok || SNEAK=$OUTSIDE_SCRATCH
echo pwned > \$SNEAK/evil.sh" "$WT_REPO"
assert_deny "write-confinement (#4914): conflicting assignment in the OTHER order is poisoned too (fail-closed) -> denies" \
    "SNEAK=$OUTSIDE_SCRATCH || SNEAK=$WT_REPO/defaults/hooks
echo pwned > \$SNEAK/evil.sh" "$WT_REPO"
assert_deny "write-confinement (#4914): sequential 'A=<in-repo>; A=<outside>' reassignment is poisoned (fail-closed) -> denies" \
    "SNEAK=$WT_REPO/defaults/hooks; SNEAK=$OUTSIDE_SCRATCH; echo pwned > \$SNEAK/evil.sh" "$WT_REPO"

# #6444: the same AMBIG poisoning applies unchanged when the write-target
# reference is DOUBLE-quoted -- quote-aware resolution must not weaken the
# conflicting-assignment rule.
assert_deny "write-confinement (#6444): conflicting same-command assignment + double-quoted usage still denies unresolved (AMBIG unaffected)" \
    "VAR=$OUTSIDE_SCRATCH/a
VAR=$OUTSIDE_SCRATCH/b
echo pwned > \"\$VAR/f\"" "$WT_REPO"

# ...but poisoning must not OVERCORRECT. Only a genuinely CONFLICTING value
# poisons: re-stating the SAME value (quotes are stripped before the
# comparison) is unambiguous and must still resolve, and one name being
# re-assigned must never contaminate a DIFFERENT name.
assert_allow "write-confinement (#4914): same value assigned twice in one command is NOT poisoned -> allow" \
    "SCRATCH=$OUTSIDE_SCRATCH || SCRATCH=$OUTSIDE_SCRATCH
echo x > \$SCRATCH/out.txt" "$WT_REPO"
assert_allow "write-confinement (#4914): same value re-stated with quotes is NOT poisoned -> allow" \
    "SCRATCH=$OUTSIDE_SCRATCH || SCRATCH='$OUTSIDE_SCRATCH'
echo x > \$SCRATCH/out.txt" "$WT_REPO"
assert_allow "write-confinement (#4914): poisoning one name does not contaminate a different name -> allow" \
    "SNEAK=$WT_REPO/defaults/hooks || SNEAK=$OUTSIDE_SCRATCH
SCRATCH=$OUTSIDE_SCRATCH
echo x > \$SCRATCH/out.txt" "$WT_REPO"
assert_allow "write-confinement (#4914): a write BEFORE the conflicting reassignment still resolves normally -> allow" \
    "SCRATCH=$OUTSIDE_SCRATCH
echo x > \$SCRATCH/out.txt
SCRATCH=$OUTSIDE_SCRATCH/other" "$WT_REPO"

rm -rf "$OUTSIDE_SCRATCH"

# -------------------------------------------------------------------------
# #6949: SAME-COMMAND mktemp SCRATCH-WRITE RESOLUTION. record_assign()/
# resolve_var() (#4881, above) only ever substitute the LITERAL text
# following `=`, so the extremely common scratch-write idiom
#   tmp=$(mktemp -d) && ... > "$tmp/sub/out"
# left the same-command mktemp value unresolved and denied it as
# worktree-write-confinement-unresolved-var, even though mktemp's own
# contract guarantees a fresh /tmp-or-$TMPDIR-rooted path that can never
# coincide with a worktree or the main checkout. wt_write_mktemp_same_command_
# safe() (mirrors the sibling rm-scope fix, rm_scope_mktemp_same_command_safe(),
# #6520) recognizes a same-command exact-string `NAME=$(mktemp -d)` /
# `NAME=$(mktemp)` assignment and allows a subsequent write under `$NAME`
# (bare, or with a `/`-suffix carrying no `..` traversal) without denying.
# Covers the five write-target call sites #6444/#6940 already touch: bare
# `>`, attached `>file`, tee, sed -i, cp/mv.
assert_allow "write-confinement (#6949): mktemp -d scratch dir with suffix + heredoc body -- the issue's own repro -> allow" \
    "tmp=\$(mktemp -d) && mkdir -p \"\$tmp/.loom/logs\" && cat > \"\$tmp/.loom/logs/sweep-outcome-telemetry.jsonl\" <<'INNER_EOF'
{\"schema_version\":1,\"x\":\"y\"}
INNER_EOF" "$WT_REPO"
assert_allow "write-confinement (#6949): bare mktemp (file, no -d) used directly as a bare > redirect target -> allow" \
    "TMPFILE=\$(mktemp)
echo hi > \"\$TMPFILE\"" "$WT_REPO"
assert_allow "write-confinement (#6949): attached >file redirect under a same-command mktemp -d scratch dir -> allow" \
    "tmp=\$(mktemp -d)
echo x >\"\$tmp/out.txt\"" "$WT_REPO"
assert_allow "write-confinement (#6949): tee target under a same-command mktemp -d scratch dir -> allow" \
    "tmp=\$(mktemp -d)
echo x | tee \"\$tmp/out.txt\"" "$WT_REPO"
assert_allow "write-confinement (#6949): sed -i target under a same-command mktemp -d scratch dir -> allow" \
    "tmp=\$(mktemp -d)
sed -i 's/a/b/' \"\$tmp/out.txt\"" "$WT_REPO"
assert_allow "write-confinement (#6949): cp destination under a same-command mktemp -d scratch dir -> allow" \
    "tmp=\$(mktemp -d)
cp /tmp/a.sh \"\$tmp/out.txt\"" "$WT_REPO"
assert_allow "write-confinement (#6949): mv destination under a same-command mktemp -d scratch dir -> allow" \
    "tmp=\$(mktemp -d)
mv /tmp/a.sh \"\$tmp/out.txt\"" "$WT_REPO"
assert_allow "write-confinement (#6949): TMPDIR= alias assigned via mktemp -d, suffix write -> allow" \
    "TMPDIR=\$(mktemp -d)
cp /tmp/a.sh \"\$TMPDIR/out.sh\"" "$WT_REPO"

# Custom-template / custom-prefix mktemp invocations never match the
# exact-string test (mirrors rm_scope_mktemp_same_command_safe()'s own
# narrowness, #6520) -- still deny as unresolved. The issue's own
# `TMPGUARD=$(mktemp /tmp/guard-XXXX.sh)` example is exactly this shape.
assert_deny "write-confinement (#6949): custom-TEMPLATE mktemp (mktemp /tmp/guard-XXXX.sh) is NOT trusted -> still denies" \
    "TMPGUARD=\$(mktemp /tmp/guard-XXXX.sh)
cat /dev/null > \"\$TMPGUARD\"" "$WT_REPO"
assert_deny "write-confinement (#6949): custom --tmpdir= mktemp is NOT trusted -> still denies" \
    "tmp=\$(mktemp -d --tmpdir=/other/dir)
echo x > \"\$tmp/out.txt\"" "$WT_REPO"

# An ambiguous same-command re-assignment (the mktemp-assigned variable
# reassigned to something else in the same command, in EITHER order) still
# fails closed -- mirrors the rm-scope original's own ambiguity rule.
assert_deny "write-confinement (#6949): mktemp-assigned var reassigned in the same command stays AMBIG -> denies" \
    "tmp=\$(mktemp -d)
tmp=/some/other/path
echo x > \"\$tmp/out.txt\"" "$WT_REPO"
assert_deny "write-confinement (#6949): a plain literal re-assigned to a mktemp value AFTER stays AMBIG -> denies" \
    "tmp=/some/other/path
tmp=\$(mktemp -d)
echo x > \"\$tmp/out.txt\"" "$WT_REPO"

# A '..' traversal in the suffix after a proven-safe mktemp var fails closed
# -- mktemp's own OUTPUT PATH is never known to this static scanner, so a
# '..' component could walk back out of the (unknown) scratch dir to an
# unknown depth, potentially back into a protected worktree/checkout.
assert_deny "write-confinement (#6949): '..' traversal in the suffix after a mktemp -d var fails closed -> denies" \
    "tmp=\$(mktemp -d)
cp /tmp/a.sh \"\$tmp/../../evil.sh\"" "$WT_REPO"

# A write target that genuinely resolves inside the repo/worktree scope must
# still deny -- this is a false-positive refinement only, never a relaxation
# of the confinement invariant (#4178). An unrelated same-command mktemp
# assignment must not accidentally lend its safety to a DIFFERENT,
# genuinely-unresolvable variable.
assert_deny "write-confinement (#6949): unrelated unresolved \$VAR is unaffected by an unrelated same-command mktemp assignment -> denies" \
    "tmp=\$(mktemp -d)
echo pwned > \"\$OTHERVARFORLOOMTEST6949/evil.sh\"" "$WT_REPO"
assert_deny "write-confinement (#6949): a target resolving INSIDE the repo/worktree via a literal same-command assignment still denies -> denies" \
    "SNEAK=$WT_REPO/defaults/hooks
echo pwned > \"\$SNEAK/evil.sh\"" "$WT_REPO"

# Sub-case A regression test (#6445/b7fc163a): confirms the already-fixed
# same-command literal $VAR write into the operator's own worktree (this
# issue's own Sub-case A example) stays fixed. Not a NEW behavior -- a guard
# against silent regression, per this issue's own "Revised Acceptance
# Criteria" (Sub-case A gets a regression test alongside the Sub-case B ones).
assert_allow "write-confinement (#6949 Sub-case A regression): same-command literal WORKTREE_ABS write into the operator's own worktree -> allow" \
    "WORKTREE_ABS=\"$WT_DIR\"
cp \"\$WORKTREE_ABS/src/a.sh\" \"\$WORKTREE_ABS/src/b.sh\"" "$WT_REPO"

# -------------------------------------------------------------------------
# #7294: SAME-COMMAND cd-THEN-RELATIVE-WRITE CWD PROPAGATION. Before this
# fix, extract_write_targets()'s own `cd`-tracking (curcwd) never consulted
# resolve_var() on the `cd` argument itself -- unlike parse_force_ops' `-C`/
# `cd` capture points, which #6152 already fixed. A same-command `cd
# "$VAR/sub"` (VAR assigned earlier via a PLAIN LITERAL, not a $(...)
# command substitution) left curcwd carrying the literal, unexpanded `$VAR`
# all the way into the write-confinement check, so a later RELATIVE write in
# the same command denied as worktree-write-confinement-unresolved-var even
# though the identical `$VAR/...` shape already resolves fine on the
# DIRECT-write-target path (#4881/#6444). This is a distinct bug from
# #6949/#6520 (the mktemp-vs-literal distinction for DIRECT write targets)
# and from #6953 (a corrupted-token bug on the direct-write-target path with
# a double-quoted `$(...)` RHS) -- this one is specifically the cd-then-
# relative-write cwd-propagation gap, reproducible with a plain literal RHS
# and no command substitution at all.
LOOM_ISSUE7294_OUTSIDE=$(mktemp -d)
LOOM_ISSUE7294_OUTSIDE=$(cd "$LOOM_ISSUE7294_OUTSIDE" && pwd -P)

# The issue's own exact repro shape: a literal (non-mktemp) same-command
# assignment, `cd` into a subdirectory of it, then a bare RELATIVE write --
# must now ALLOW once the resolved cwd (outside the repo) is known.
assert_allow "write-confinement (#7294): literal same-command \$VAR, cd into \"\$VAR/sub\", then a later RELATIVE write -> allow" \
    "TMP=$LOOM_ISSUE7294_OUTSIDE
cd \"\$TMP/repo\"
echo hi > README.md" "$WT_REPO"

# The direct-write-target counterpart (already fixed by #6949/#6444) must
# keep working unchanged -- confirms this is genuinely the cd-propagation
# path, not a re-fix of the direct-target resolver.
assert_allow "write-confinement (#7294 regression): the SAME literal \$VAR used directly (no cd) still allows unchanged -> allow" \
    "TMP=$LOOM_ISSUE7294_OUTSIDE
echo hi > \"\$TMP/out.txt\"" "$WT_REPO"

# A same-command MKTEMP-resolved cd argument must propagate too. Unlike the
# literal case above, resolve_var() CANNOT substitute a `$(mktemp -d)` RHS
# (a command-substitution value always stays unresolved, same rule
# wt_write_mktemp_same_command_safe() already relies on for the DIRECT-target
# #6949 fix) -- so this exercises a SEPARATE mechanism: a still-unresolved,
# bare `$NAME`/`${NAME}`(/suffix)? cd argument is now classified as a FRESH
# ROOT (like an absolute path) instead of being joined onto the prior cwd,
# and the write-confinement check tries the identical same-command
# exact-string mktemp proof against the COMBINED cd-suffix + write-target
# path before falling back to the pre-#7294 deny.
assert_allow "write-confinement (#7294): \$(mktemp -d)-resolved \$VAR, cd into \"\$VAR/sub\", then a later RELATIVE write -> allow" \
    "tmp=\$(mktemp -d)
cd \"\$tmp/repo\"
echo hi > README.md" "$WT_REPO"

# A `..` traversal ANYWHERE in the combined cd-suffix + write-target path
# must still fail closed -- mktemp's own output path is never known to this
# static scanner, so a `..` could walk back out of that unknown directory to
# an unknown number of levels (mirrors the #6949 direct-target suffix rule
# exactly; wt_write_mktemp_same_command_safe() runs the identical
# `*/../*|*/..` check on the combined string here).
assert_deny "write-confinement (#7294): \$(mktemp -d)-resolved \$VAR cd, then a '..'-traversing RELATIVE write stays fail-closed -> denies" \
    "tmp=\$(mktemp -d)
cd \"\$tmp/repo\"
echo hi > ../../../etc/passwd" "$WT_REPO"

# Still denies when the cd-resolved cwd lands INSIDE the main checkout --
# same-command resolution must only narrow the false positive, never widen
# an allow beyond what writing that literal path outright would already
# grant (#6172).
assert_deny "write-confinement (#7294): literal same-command \$VAR resolving INSIDE the repo, cd + relative write -> still denies" \
    "SNEAK=$WT_REPO/defaults
cd \"\$SNEAK/hooks\"
echo pwned > evil.sh" "$WT_REPO"

# A genuinely UNRESOLVABLE cd argument (a live \$(...) command substitution
# resolve_var() cannot prove a value for) must stay fail-closed -- the #4921
# floor is unaffected by this fix.
assert_deny "write-confinement (#7294): unresolvable \$(...) cd argument + later relative write stays fail-closed -> denies" \
    "cd \"\$(some_dynamic_command_7294)/repo\"
echo hi > README.md" "$WT_REPO"

rm -rf "$LOOM_ISSUE7294_OUTSIDE"

# -------------------------------------------------------------------------
# ADR-0016 / #6253 (Epic #6172 Phase 2): formalized, citable ambiguity
# contract for the same-command literal-assignment resolver
# (record_assign()/resolve_var(), #4881, ~lines 1608-1686). This resolver is
# the ONE sanctioned mechanism (ADR-0016 "Decision") for converting an
# otherwise-unresolvable `$VAR`-rooted write target into a known one — no
# shell AST/general parser, and (per the "Explicitly does NOT do" section)
# NO control-flow-scoped inference (loops, conditionals, case statements,
# function bodies) of any kind. The behavior pinned below already existed
# before this section was added (verified directly against `record_assign`/
# `resolve_var`'s own code) — this section makes it an EXPLICIT, named
# contract per the ADR's "Ambiguity behavior" table, rather than leaving it
# implicit. A future change that makes any of these DENY assertions start
# failing is reintroducing exactly the ambiguity-resolution risk this ADR
# argues against; it is not simply "more coverage."
AMBIG_OUTSIDE=$(mktemp -d)

# (a) CONFLICTING same-name assignment -> record_assign() poisons the name to
# its AMBIG sentinel, which resolve_var() then treats as unresolved (the
# sentinel itself starts with "$", routing into the same refusal as any other
# unresolved chain). Named explicitly here per ADR-0016's own worked example
# (`p=/tmp/a; p=/tmp/b; echo pwned > $p/f.txt` -> DENY).
assert_deny "ambiguity contract (a) AMBIG: conflicting same-name assignment denies (record_assign() poisons to AMBIG)" \
    "p=$AMBIG_OUTSIDE/a
p=$AMBIG_OUTSIDE/b
echo pwned > \$p/f.txt" "$WT_REPO"

# (b) UNRESOLVABLE RHS: resolve_var() only trusts a plain literal value; any
# RHS shape it cannot statically reduce to a literal string leaves the
# mapped value unchanged (still starting with "$"), so the reference stays
# unresolved. Four named sub-shapes per ADR-0016's ambiguity table row
# ("command substitution ($(...), backticks), read, a chained unresolved
# $OTHER"):
assert_deny "ambiguity contract (b.1) unresolvable RHS: \$(...) command substitution denies" \
    "p=\$(cat /tmp/loom-test-6253-nonexistent)
echo pwned > \$p/f.txt" "$WT_REPO"
assert_deny "ambiguity contract (b.2) unresolvable RHS: backtick command substitution denies" \
    "p=\`cat /tmp/loom-test-6253-nonexistent\`
echo pwned > \$p/f.txt" "$WT_REPO"
assert_deny "ambiguity contract (b.3) unresolvable RHS: chained unresolved \$OTHER denies" \
    "p=\$OTHER_6253_UNRESOLVED
echo pwned > \$p/f.txt" "$WT_REPO"
assert_deny "ambiguity contract (b.4) unresolvable RHS: 'read' produces no NAME=value token at all, so a later \$VAR use stays unresolved and denies" \
    "read p < /tmp/loom-test-6253-nonexistent
echo pwned > \$p/f.txt" "$WT_REPO"

# (c) NO ASSIGNMENT FOUND for the referenced name at all -> baseline #4921
# behavior, unchanged by the #4881 resolver's addition.
assert_deny "ambiguity contract (c) no assignment found: bare unresolved \$VAR with no same-command assignment anywhere denies" \
    "echo pwned > \$P_NEVER_ASSIGNED_6253/f.txt" "$WT_REPO"

rm -rf "$AMBIG_OUTSIDE"

# -------------------------------------------------------------------------
# Permanent regression coverage for PR #5397's three Judge-confirmed
# bypasses (#6253, ADR-0016 Phase 2 follow-on item 6). #5397 attempted a
# narrow carve-out (`_wt_scan_forloop_binding()`) that inferred a `$VAR`'s
# bound value set from an enclosing `for VAR in tok1 tok2; do` construct --
# categorically different from the same-command LITERAL-ASSIGNMENT
# resolution pinned above, because it tried to infer a value from
# control-flow MEMBERSHIP rather than from an unconditional assignment.
# Judge found three independently-confirmed bypasses in three review
# rounds, each a distinct defect class in the same ad-hoc text-scanning
# helper; the PR was closed "not viable" and never merged. `main` today
# (and per this issue's own AC #2, re-verified at Phase 2 start) has NO
# `_wt_scan_forloop_binding()` or lookalike -- these are standing DENY
# assertions for all three repro shapes, so that if any future change
# (in this guard, or in a lookalike added elsewhere) reintroduces ANY form
# of control-flow-scoped binding inference, these tests catch the exact
# bypass class Judge already found rather than requiring it to be
# rediscovered from scratch.
#
# (1) Position/reassignment-unawareness (PR #5397, first Judge review): the
# original carve-out only checked that a `for VARNAME in ...; do` construct
# appeared ANYWHERE in the raw command text, with no check that the write's
# own occurrence was textually inside that loop's body, and no check for an
# intervening reassignment. A throwaway, fully-literal, outside-checkout
# loop earlier in the command "bound" an unrelated variable later
# reassigned via an unresolvable command substitution.
assert_deny "PR #5397 repro 1 (position/reassignment-unawareness): throwaway outside-checkout for-loop + later unresolvable reassignment still denies" \
    "for p in /tmp/outside/a /tmp/outside/b; do :; done
p=\$(cat /tmp/loom-test-6253-nonexistent)
echo pwned > \$p/exploit.txt" "$WT_REPO"

# (2) Decoy-reference (PR #5397, second Judge review): after (1) was
# patched to require SOME reference to \$VAR inside the loop body, a single
# unrelated mention (an \`echo\` of the loop variable, unconnected to the
# real write) satisfied that check while the real write -- using a value
# reassigned via an unresolvable expression -- sailed through unverified.
assert_deny "PR #5397 repro 2 (decoy-reference): unrelated echo of \$p inside the loop body + later unresolvable reassignment still denies" \
    "for p in /tmp/outside/a /tmp/outside/b; do echo \"seen \$p\"; done
p=\$(cat /tmp/loom-test-6253-nonexistent)
echo pwned > \$p/exploit.txt" "$WT_REPO"

# (3) Literal-substring 'done'-match (PR #5397, third Judge review): the
# loop-body span was computed with a plain substring split on the four
# characters d-o-n-e (\`\${after_do%%done*}\`/\`\${after_do#*done}\`), not a
# keyword-boundaried match. An identifier merely CONTAINING "done" (e.g.
# \`is_done=1\`) truncated the body early, letting a same-body reassignment
# escape the (already-present) reassignment check entirely.
assert_deny "PR #5397 repro 3 (substring 'done'-match): an 'is_done=1' decoy identifier inside the loop body must not smuggle a same-body reassignment past a keyword-unaware body-boundary scan -> still denies" \
    "for p in /tmp/outside/a /tmp/outside/b; do echo pwned > \$p/exploit.txt; is_done=1; p=\$(cat /tmp/loom-test-6253-nonexistent); done" "$WT_REPO"

# -------------------------------------------------------------------------
# Heredoc bodies opened with a QUOTED delimiter are DATA, never
# redirect/write-idiom syntax (#4881). Reported incident: filing THIS issue
# via `gh issue create --body "$(cat <<'EOF' ... EOF)"` embedded the original
# bug repro (a redirect-plus-$VAR example) in the heredoc BODY, and the guard
# scanned that quoted example text as if it were real command syntax, denying
# the (read-only-plus-API) `gh issue create` call itself.
HEREDOC_BODY_CMD=$(cat <<'BASH_EOF'
gh issue create --title "Test" --body "$(cat <<'EOF'
SCRATCH=/private/tmp/example/scratchpad
gh pr view 1 --json files -q '.files[].path' >> $SCRATCH/out.txt
EOF
)"
BASH_EOF
)
assert_allow "write-confinement (#4881): redirect-looking text inside a single-quoted heredoc body is DATA, not code -> allow" \
    "$HEREDOC_BODY_CMD" "$WT_REPO"

# Curator-widened repro (#4881): the same false positive is NOT limited to
# the `>`/`>>` scan -- extract_write_targets()'s cp/mv/tee/sed -i matching is
# just as un-heredoc-aware, so a heredoc body quoting one of THOSE shapes
# (e.g. citing a `cp '$src' '$dst'` line from a commit message) manufactured
# the same phantom target on a plain `gh issue comment`.
HEREDOC_BODY_CPMV_CMD=$(cat <<'BASH_EOF'
gh issue comment 1 --body "$(cat <<'EOF'
See the fix in that commit: cp '$src' '$dst' and tee /some/other/path
EOF
)"
BASH_EOF
)
assert_allow "write-confinement (#4881): cp/mv/tee-looking text inside a single-quoted heredoc body is DATA, not code -> allow" \
    "$HEREDOC_BODY_CPMV_CMD" "$WT_REPO"

# Regression: a REAL (unquoted, outside any heredoc body) redirect on the
# heredoc's own START line must still deny -- heredoc-body stripping only
# blanks lines INSIDE the body, never the opening line carrying the actual
# `>` operator, even when the heredoc's own delimiter is quoted.
assert_deny "write-confinement (#4881): real redirect on a quoted-delimiter heredoc START line still denies" \
    "cat > $WT_REPO/defaults/hooks/f.sh <<'EOF'
hello
EOF" "$WT_REPO"

# Phantom-heredoc-opener bypass (#4914 Judge review). The original #4881
# implementation shipped its own line-based `strip_heredoc_bodies()` whose
# opener regex was a plain substring match: a `cat <<'EOF'` sequence appearing
# INSIDE a quoted string (pure DATA -- e.g. grepping for the idiom) opened a
# PHANTOM heredoc that blanked every following line, swallowing a genuine
# write-idiom line and silently ALLOWing a write into the main checkout that
# `origin/main` denied. That function is gone: heredoc-body masking is now
# `mask_heredoc_bodies()` (#5000/#5087), which masks ONLY a block whose
# terminating bare-delimiter line is actually present in the buffer, so a
# phantom opener with no terminator masks NOTHING (fail closed). These three
# cases pin that behavior down for the write-confinement tier.
#
# (a) The exact Judge repro: the idiom quoted as data, followed by a real
#     write into the main checkout on the next line -> must DENY.
assert_deny "write-confinement (#4914): quoted 'cat <<EOF' text is NOT a heredoc opener -- a following real write into the main checkout still denies" \
    "grep -rn \"the cat <<'EOF' idiom\" defaults/
echo x > $WT_REPO/defaults/hooks/f.sh" "$WT_REPO"

# (b) The legitimate `"\$(cat <<'EOF' ... EOF)"` exemption this issue exists
#     to add must NOT regress: a main-checkout path quoted inside a properly
#     TERMINATED cat-heredoc body is inert data and still allows.
assert_allow "write-confinement (#4914): main-checkout write path quoted inside a TERMINATED cat-heredoc body is data -> allow" \
    "gh issue create --title t --body \"\$(cat <<'EOF'
echo x > $WT_REPO/defaults/hooks/f.sh
EOF
)\"" "$WT_REPO"

# (c) Unterminated heredoc (delimiter line never arrives) -> fail closed:
#     nothing is masked, so the real write on the following line still denies.
assert_deny "write-confinement (#4914): UNTERMINATED heredoc masks nothing (fail closed) -- following real write still denies" \
    "cat <<'EOF'
some prose that never terminates
echo x > $WT_REPO/defaults/hooks/f.sh" "$WT_REPO"

# -------------------------------------------------------------------------
# Interpreter-fed heredoc bodies in the write-confinement tier (#5351).
#
# HISTORY: #5117 recorded (KNOWN LIMITATION 1) that the ASK-tier
# write-confinement scan called the PLAIN mask_heredoc_bodies(), which masks an
# INTERPRETER-fed body (`bash <<'EOF' ... EOF`, `sh -s <<'EOF'`,
# `cat <<'EOF' | bash`) exactly like an inert `cat`-body -- so a write into the
# main checkout expressed inside such a body was masked out before the
# confinement check ever saw it, silently ALLOWing a write `origin/main`'s
# single-pass scan would have caught. #4881's earlier assertion that such a
# body still denied was a property of the deleted `cat`-only
# `strip_heredoc_bodies()` and did not survive the move to mask_heredoc_bodies().
#
# #5351 closes that gap: extract_write_targets() now calls the SAME
# mask_heredoc_bodies_selective() variant the CATASTROPHIC tier already used
# (#5198/#5205), which leaves an interpreter-fed body VISIBLE to the scan while
# still masking every inert (non-interpreter) heredoc. A write inside an
# interpreter-fed heredoc body targeting the main checkout therefore now DENYs
# from a managed worktree, and the inert-`cat`-body exemption (#4914/#5000/#5181)
# is unchanged. (The BROADER interpreter-mediated write class -- `bash -c
# '... > f'`, `printf … | bash`, `dd of=f` -- remains a separate follow-up, as
# KNOWN LIMITATIONS #1 records.)

# (a) A live write into the main checkout inside a `bash <<'EOF' ... EOF`
#     interpreter-fed body is genuinely executable code, not inert data -- must
#     DENY (the exact gap #5117 recorded; masked-to-ALLOW on pre-#5351).
assert_deny "write-confinement (#5351): write inside a 'bash <<EOF ... EOF' interpreter-fed heredoc body targeting the main checkout denies" \
    "bash <<'EOF'
echo x > $WT_REPO/defaults/hooks/f.sh
EOF" "$WT_REPO"

# (b) Same evasion via `sh -s <<'EOF' ... EOF` -- another interpreter opener.
assert_deny "write-confinement (#5351): write inside a 'sh -s <<EOF ... EOF' interpreter-fed heredoc body denies" \
    "sh -s <<'EOF'
echo x > $WT_REPO/defaults/hooks/f.sh
EOF" "$WT_REPO"

# (c) Same evasion piped into an interpreter (`cat <<'EOF' ... EOF | bash`).
assert_deny "write-confinement (#5351): write inside a body piped to bash ('cat <<EOF | bash') denies" \
    "cat <<'EOF' | bash
echo x > $WT_REPO/defaults/hooks/f.sh
EOF" "$WT_REPO"

# (d) NO REGRESSION: the SAME write-idiom line inside an INERT (non-interpreter)
#     `cat <<'EOF' ... EOF` sink body is still masked as inert data and stays
#     ALLOWed -- _selective() only un-masks INTERPRETER-fed openers, so the
#     #4914/#5000/#5181 false-positive fix is preserved. This is the crisp
#     contrast with (a): identical body line, interpreter vs. plain sink.
assert_allow "write-confinement (#5351): identical write line inside an inert 'cat <<EOF ... EOF' body stays data -> allow (no #4914/#5181 regression)" \
    "cat <<'EOF' > /tmp/loom-5351-note.txt
echo x > $WT_REPO/defaults/hooks/f.sh
EOF" "$WT_REPO"

# (e) NO REGRESSION: the canonical `--body "\$(cat <<'EOF' ... EOF)"` idiom that
#     merely QUOTES a main-checkout write path as inert prose still allows.
assert_allow "write-confinement (#5351): main-checkout write path quoted inside a '--body \$(cat <<EOF ... EOF)' sink body stays data -> allow" \
    "gh issue create --title t --body \"\$(cat <<'EOF'
echo x > $WT_REPO/defaults/hooks/f.sh
EOF
)\"" "$WT_REPO"

# -------------------------------------------------------------------------
# Non-shell interpreter heredoc bodies must NOT be scanned for shell write
# idioms in the write-confinement tier (#6353).
#
# HISTORY: is_interpreter_opener() (#5351, refined here) put
# python[0-9.]*/perl/ruby/node/nodejs in the SAME "leave heredoc body visible
# to the write-idiom scan" bucket as real shell interpreters
# (bash/sh/zsh/dash/ksh). `>`/`>=`/`<`/`<=` is a live write/read redirect
# ONLY in real shell syntax -- in Python/Perl/Ruby/JS source those same bytes
# are ordinary comparison operators. Leaving those languages' heredoc bodies
# unmasked bought no real protection (their actual writes go through
# language-level APIs this command-word scanner never parses regardless)
# while manufacturing a false worktree-write-confinement DENY on completely
# ordinary code such as `if len(affected) > 20:` -- exactly the production
# repro this issue was filed from (a read-only klayout/Python DRC sanity
# script, denied on a computed write target of "20:").
#
# #6353 narrows extract_write_targets()'s OWN call into
# mask_heredoc_bodies_selective() to shell_only=1, so a quoted-delimiter
# heredoc fed to python/perl/ruby/node is masked exactly like an inert `cat`
# body for THIS scan -- while bash/sh/zsh/dash/ksh keep the #5351 behavior
# (their heredoc bodies stay visible, since a `>` there IS a live redirect).

# (a) Python: the exact repro shape -- a `>` comparison inside an `if` guard,
#     no write idiom of any kind in the body -- must ALLOW, not manufacture a
#     phantom "20:" write target.
assert_allow "write-confinement (#6353): python heredoc body with a '>' comparison (not a redirect) allows" \
    "python3 - <<'EOF'
affected = []
if len(affected) > 20:
    print(\"many\")
EOF" "$WT_REPO"

# (b) Python: '>=' comparison, same class.
assert_allow "write-confinement (#6353): python heredoc body with a '>=' comparison allows" \
    "python - <<'EOF'
count = 5
if count >= 20:
    print(\"big\")
EOF" "$WT_REPO"

# (c) Perl: '<' comparison.
assert_allow "write-confinement (#6353): perl heredoc body with a '<' comparison allows" \
    "perl <<'EOF'
my \$n = 5;
if (\$n < 20) { print \"small\n\"; }
EOF" "$WT_REPO"

# (d) Ruby: '<=' comparison.
assert_allow "write-confinement (#6353): ruby heredoc body with a '<=' comparison allows" \
    "ruby <<'EOF'
n = 5
if n <= 20
  puts \"small\"
end
EOF" "$WT_REPO"

# (e) Node/nodejs: '>' comparison.
assert_allow "write-confinement (#6353): node heredoc body with a '>' comparison allows" \
    "node <<'EOF'
const n = 5;
if (n > 20) { console.log(\"big\"); }
EOF" "$WT_REPO"
assert_allow "write-confinement (#6353): nodejs heredoc body with a '>' comparison allows" \
    "nodejs <<'EOF'
const n = 5;
if (n > 20) { console.log(\"big\"); }
EOF" "$WT_REPO"

# (f) REGRESSION CONTROL (#5351 must not regress): a genuine shell write
#     idiom inside a bash/sh-interpreter-fed heredoc body targeting the main
#     checkout must still DENY -- shell_only=1 only removes the non-shell
#     interpreters from the "leave visible" bucket, bash/sh/zsh/dash/ksh keep
#     the exact #5351 behavior.
assert_deny "write-confinement (#6353 control, #5351 no-regression): write inside a 'bash <<EOF ... EOF' interpreter-fed heredoc body still denies" \
    "bash <<'EOF'
echo x > $WT_REPO/defaults/hooks/f.sh
EOF" "$WT_REPO"
assert_deny "write-confinement (#6353 control, #5351 no-regression): write inside a 'sh <<EOF ... EOF' interpreter-fed heredoc body still denies" \
    "sh <<'EOF'
echo x > $WT_REPO/defaults/hooks/f.sh
EOF" "$WT_REPO"

# Safety-floor regression (issue's own AC): a genuinely smuggled dangerous
# command inside REAL command substitution (not a quoted heredoc at all)
# must still hard-deny -- this file's #3679/#4178 catastrophic-tier
# protection (strip_literal_text()'s `$(`-aware redaction floor,
# ~line 1315-1318) is completely untouched by this issue's fix; neither
# mask_heredoc_bodies() nor resolve_var() ever run on the ALWAYS_BLOCK scan.
assert_deny "write-confinement (#4881 regression): smuggled force-push via \$(...) command substitution (not a quoted heredoc) still hard-denies" \
    "git commit -m \"\$(git push --force origin main)\""

# -------------------------------------------------------------------------
# UNQUOTED-delimiter heredoc BODY prose containing a literal '>' must not be
# misread as a shell redirect operator by the write-confinement scan (#7247).
#
# HISTORY: mask_heredoc_bodies_selective() masked a quoted-delimiter body
# unconditionally (#5351/#6353) but left EVERY unquoted-delimiter body fully
# visible, because the outer shell expands $(...)/backticks while building it
# (#5779/#5781). That default is correct, but a plain `cat <<EOF ... EOF`
# heredoc consumed by a non-interpreter sink (no `$(`/backtick capture at
# all -- the shape mask_unquoted_cat_heredoc_bodies()/#6056 does not reach,
# since it requires a text-data-flag capture) left ordinary markdown/prose
# lines containing a bare '>' (e.g. "rotting >=3d, clean" -- a "greater than"
# comparison, not shell syntax) fully exposed to the `>`/`>>` write-idiom
# scan. That manufactured a SECOND, bogus redirect: `>` followed by target
# "=3d,", which was then cwd-joined and false-denied as a
# worktree-write-confinement bypass -- even though the command performs no
# write outside its one, already-literal target (or no write at all).

# 1. Exact minimal repro from the issue: no external redirect at all, body
#    prose alone manufactures the phantom target pre-fix.
assert_allow "write-confinement (#7247): unquoted cat-heredoc body containing '>=' prose, no external redirect, allows" \
    "cat <<EOF
rotting >=3d, clean
EOF" "$WT_REPO"

# 2. The real Champion idiom cited in the issue: a genuine, already-literal
#    /tmp write target PLUS body prose containing '>='. Must allow -- the one
#    real target is outside the repo, and the body must not manufacture a
#    second, in-repo one.
assert_allow "write-confinement (#7247): 'cat > /tmp/... <<EOF ... EOF' with '>=' body prose allows (Champion digest idiom)" \
    "cat > /tmp/loom-test-$$-7247-digest.md <<EOF
rotting >=3d, clean
| col1 | col2 |
|---|---|
EOF" "$WT_REPO"

# 3. Not cat-specific: the same shape through a different non-interpreter
#    sink (tee) also allows -- unlike mask_unquoted_cat_heredoc_bodies()
#    (#6056), this narrowing is not gated on the consuming command being a
#    literal `cat` captured into a text-data flag.
assert_allow "write-confinement (#7247): 'tee /tmp/... <<EOF ... EOF' with '>=' body prose allows (non-cat sink)" \
    "tee /tmp/loom-test-$$-7247-tee.md <<EOF
rotting >=3d, clean
EOF" "$WT_REPO"

# 4. Quoted-delimiter control: already allowed pre-fix (mask_heredoc_bodies_
#    selective()'s original branch) -- regression lock, must still allow.
assert_allow "write-confinement (#7247 control): quoted-delimiter cat heredoc with '>=' body prose still allows" \
    "cat > /tmp/loom-test-$$-7247-quoted.md <<'EOF'
rotting >=3d, clean
EOF" "$WT_REPO"

# 5. Non-shell-interpreter, UNQUOTED delimiter: a python heredoc body with an
#    ordinary '>' comparison, unquoted delimiter (the #6353 tests above only
#    cover the QUOTED-delimiter variant) -- must also allow, since python is
#    not a shell interpreter for shell_only=1 purposes.
assert_allow "write-confinement (#7247): python heredoc body with unquoted delimiter and a '>' comparison allows" \
    "python3 - <<EOF
affected = []
if len(affected) > 20:
    print(\"many\")
EOF" "$WT_REPO"

# --- CRITICAL SAFETY REGRESSION: a genuine embedded write must still deny ---
# 6. An unquoted heredoc body containing a REAL \$(...) command substitution
#    that itself performs a write into the main checkout must stay fully
#    visible and still deny -- _heredoc_body_expansion_free() disqualifies
#    the body from masking the instant a live \$( is present, so this can
#    only ever NARROW the scan, never blind it to an actual embedded write.
assert_deny "write-confinement (#7247 SAFETY): unquoted heredoc body with a live \$(...) write into the main checkout still denies" \
    "cat > /tmp/loom-test-$$-7247-evil.md <<EOF
\$(echo pwned > $WT_REPO/defaults/hooks/evil.sh)
EOF" "$WT_REPO"

# 7. Same safety floor via an unescaped backtick command substitution.
assert_deny "write-confinement (#7247 SAFETY): unquoted heredoc body with a live backtick-substitution write into the main checkout still denies" \
    "cat > /tmp/loom-test-$$-7247-evil2.md <<EOF
\`echo pwned > $WT_REPO/defaults/hooks/evil2.sh\`
EOF" "$WT_REPO"

# 8. Regression control: an unquoted heredoc fed to a REAL shell interpreter
#    (bash), containing a genuine write into the main checkout, must still
#    deny -- the new masking never applies when is_interpreter_opener()
#    recognizes the opener, regardless of delimiter quoting.
assert_deny "write-confinement (#7247 control, #5351/#6353 no-regression): unquoted 'bash <<EOF ... EOF' with a real write into the main checkout still denies" \
    "bash <<EOF
echo x > $WT_REPO/defaults/hooks/f.sh
EOF" "$WT_REPO"

# -------------------------------------------------------------------------
# #7421: unquoted-delimiter heredoc body that ALSO embeds one harmless,
# single-line `$(...)` command substitution elsewhere in the SAME body must
# not lose #7247's masking for the rest of the body's prose lines.
#
# HISTORY: #7247's fix (_heredoc_body_expansion_free()) disqualified masking
# for the ENTIRE heredoc body the instant ANY single line anywhere in it
# contained `$(`/backtick -- even a provably self-contained, harmless one
# like `$(date -u +%Y-%m-%dT%H:%M:%SZ)` that itself performs no write. That
# left every OTHER prose line in that same multi-paragraph body (a realistic
# shape for Champion's own "Merge-Risk Hold Digest" maintenance, #6720/
# #6851/#7020) fully exposed to the `>`/`>>` write-idiom scan, so an
# unrelated later line's bare '>=' comparison (e.g. "rotting >=3d") was
# misread as a second, phantom redirect and cwd-joined into a bogus
# worktree-write-confinement DENY -- reproduced verbatim from a real
# .loom/logs/guard-decisions.log entry cited in the issue.
assert_allow "write-confinement (#7421): unquoted heredoc body with a harmless single-line \$(date ...) PLUS a later '>=' prose line allows (Champion digest idiom)" \
    "cat > /tmp/loom-test-$$-7421-digest.md <<EOF
# Merge-Risk Hold Digest

**Last updated**: \$(date -u +%Y-%m-%dT%H:%M:%SZ)
**Aggregate**: Merge-risk holds: 3 open PR(s) -- 2 conflicting (1 rotting >=3d, 1 clean)
EOF" "$WT_REPO"

# Same shape, but the earlier \$(...) itself carries a leading '>' style
# comparison inside its own argument text (arithmetic-looking), confirming
# the narrowing is about WHICH LINE carries the live span, not about
# skipping the whole-body check outright.
assert_allow "write-confinement (#7421): unquoted heredoc, \$(...) line immediately followed by unrelated '>' prose line allows" \
    "cat > /tmp/loom-test-$$-7421-digest2.md <<EOF
Count: \$(echo 3)
Threshold check: 5 > 3, so continue
EOF" "$WT_REPO"

# --- CRITICAL SAFETY REGRESSION: a live write embedded in the SAME line as
# --- an otherwise-benign substitution must still deny.
assert_deny "write-confinement (#7421 SAFETY): a line containing both a harmless \$(date) AND a live \$(...) write into the main checkout still denies" \
    "cat > /tmp/loom-test-$$-7421-evil.md <<EOF
**Last updated**: \$(date -u +%Y-%m-%dT%H:%M:%SZ)
\$(echo pwned > $WT_REPO/defaults/hooks/evil7421.sh)
EOF" "$WT_REPO"

# --- CRITICAL SAFETY REGRESSION: a command substitution that genuinely
# --- SPANS multiple lines (opens on one line, closes on a later one) must
# --- keep EVERY line it spans visible, even when a harmless single-line
# --- \$(...) appears earlier in the same body.
assert_deny "write-confinement (#7421 SAFETY): a \$(...) command substitution spanning multiple lines, embedding a write into the main checkout on its closing line, still denies" \
    "cat > /tmp/loom-test-$$-7421-evil2.md <<EOF
**Last updated**: \$(date -u +%Y-%m-%dT%H:%M:%SZ)
\$(echo start
echo pwned > $WT_REPO/defaults/hooks/evil7421b.sh)
EOF" "$WT_REPO"

# --- CRITICAL SAFETY REGRESSION (#7425): a bare, unrelated "(...)" paren pair
# --- NESTED inside an already-open \$(...) must not close the live span
# --- early. _heredoc_mark_live_lines() only counted "\$("-prefixed opens
# --- against its depth counter, so a bare "(" inside the substitution's own
# --- text was never counted as an open -- but its matching ")" still
# --- decremented the depth counter, closing the tracked span one paren too
# --- soon. That let a genuine write on the substitution's real closing line
# --- fall outside the "live" span and get masked -- silently ALLOWED instead
# --- of denied.
assert_deny "write-confinement (#7425 SAFETY): a bare paren pair nested inside a live \$(...) does not close the span early, so a write on its real closing line still denies" \
    "cat > /tmp/loom-test-$$-7425-evil.md <<EOF
\$(echo (x)
echo pwned > $WT_REPO/defaults/hooks/evil7425.sh)
EOF" "$WT_REPO"

# -------------------------------------------------------------------------
# Tilde / $HOME expansion in the tracked `cd` ARGUMENT (#5315). Distinct from
# the #4382 block above (which expands the write TARGET): here the leading
# `~`/`$HOME` is on the `cd <dir>` prefix that seeds curcwd, resolved by
# expand_cd_arg() INSIDE the awk pass before curcwd is joined. Reported
# incident: `cd ~/GitHub/loom && ... > .loom/.daemon.pid` from a main-checkout
# cwd resolved the target as `.../loom/~/GitHub/loom/.loom/.daemon.pid` — the
# raw `cd ~/GitHub/loom` argument was joined onto curcwd with a LITERAL `~`
# mid-path instead of $HOME-expanded first.
#
# HOME_FIXTURE_OUTSIDE (defined above) is unrelated to WT_REPO, so a
# `cd ~ && write relative` that expands correctly lands OUTSIDE the checkout ->
# allow; the pre-#5315 literal-`~` join kept it under WT_REPO -> deny. The
# allow/deny flip is what proves the expansion actually happened.
assert_allow_env "write-confinement (#5315): 'cd ~ && > relative' expands ~ to \$HOME, landing outside the checkout allows" \
    "HOME=$HOME_FIXTURE_OUTSIDE" \
    "cd ~ && echo x > f.sh" "$WT_REPO"
assert_allow_env "write-confinement (#5315): 'cd ~/sub && > relative' expands ~/ to \$HOME/sub, outside the checkout allows" \
    "HOME=$HOME_FIXTURE_OUTSIDE" \
    "cd ~/sub && echo x > f.sh" "$WT_REPO"
# Exact reported shape: multi-segment `~/...` cd prefix + a relative
# .loom/.daemon.pid write. With HOME outside the checkout it now resolves out
# of tree (allow); pre-fix the literal-`~` mis-join kept it in tree (deny).
assert_allow_env "write-confinement (#5315): reported shape 'cd ~/GitHub/loom && printf > .loom/.daemon.pid' expands, allows" \
    "HOME=$HOME_FIXTURE_OUTSIDE" \
    "cd ~/GitHub/loom && printf x > .loom/.daemon.pid" "$WT_REPO"
assert_allow_env "write-confinement (#5315): 'cd \$HOME && > relative' expands bare \$HOME, outside the checkout allows" \
    "HOME=$HOME_FIXTURE_OUTSIDE" \
    'cd $HOME && echo x > f.sh' "$WT_REPO"
assert_allow_env "write-confinement (#5315): 'cd \$HOME/sub && > relative' expands \$HOME/, outside the checkout allows" \
    "HOME=$HOME_FIXTURE_OUTSIDE" \
    'cd $HOME/sub && echo x > f.sh' "$WT_REPO"

# No blanket allow: if the expanded cd lands the relative write BACK inside the
# main checkout, it still denies exactly like any other in-tree write (mirrors
# the #4382 write-target counterpart). Proves expansion uses the guard's own
# process $HOME, not a `HOME=` token scanned from the command text.
assert_deny_env "write-confinement (#5315): 'cd ~ && > relative' whose \$HOME IS the main checkout still denies" \
    "HOME=$WT_REPO" \
    "cd ~ && echo x > defaults/hooks/f.sh" "$WT_REPO"

# Quoted / escaped tildes are NOT tilde-expanded by a real shell, so the cd
# stays literal/repo-relative -> the relative write stays under the main
# checkout -> deny (with HOME OUTSIDE, an erroneous expansion would have
# allowed, so deny proves the token was left literal).
assert_deny_env "write-confinement (#5315): 'cd '\''~'\'' && > relative' (single-quoted tilde stays literal) still denies" \
    "HOME=$HOME_FIXTURE_OUTSIDE" \
    "cd '~' && echo x > f.sh" "$WT_REPO"
assert_deny_env "write-confinement (#5315): 'cd \\~ && > relative' (backslash-escaped tilde stays literal) still denies" \
    "HOME=$HOME_FIXTURE_OUTSIDE" \
    "cd \~ && echo x > f.sh" "$WT_REPO"

# ~user / ~user/rest in a cd argument is DELIBERATELY left unresolved inside awk
# (fail-closed: joined repo-relative -> classified in-tree -> deny), rather than
# resolved via a shell-injection-prone getent/dscl lookup. With HOME OUTSIDE, an
# (unwanted) expansion would land outside and allow; the deny confirms the
# fail-closed fallback. See the #5315 DECISION note in guard-destructive-generic.sh.
assert_deny_env "write-confinement (#5315): 'cd ~user && > relative' (~user left unresolved, fail-closed) still denies" \
    "HOME=$HOME_FIXTURE_OUTSIDE" \
    "cd ~${CURRENT_UNIX_USER} && echo x > f.sh" "$WT_REPO"

# -------------------------------------------------------------------------
# Quoted write targets are still classified as ABSOLUTE (#4926).
#
# extract_write_targets() emits a token with its quote characters preserved
# VERBATIM (qsplit's contract, #3755). A quoted absolute path -- '/main/evil'
# or "/main/evil" -- therefore starts with a quote character, not `/`, so the
# `[[ … == /* ]]` classification called it RELATIVE and cwd-prefixed it into a
# location the write will never actually have. From a MAIN-CHECKOUT cwd that
# fabrication still happened to land inside the main checkout, so the deny
# fired by accident. From a LINKED-WORKTREE cwd -- the canonical builder setup
# -- the very same fabrication instead walked back into the acting worktree's
# OWN `.loom-managed` sentinel and was silently ALLOWED, defeating the headline
# #4178 protection with one pair of quotes (the same masked-allow shape as the
# unresolved-`$` bypass fixed by #4921/#4927, reached through quoting instead).
#
# Every write idiom the unquoted fixtures above cover, in BOTH quote styles,
# from BOTH cwd modes.
for _q4926 in "'" '"'; do
    assert_deny "write-confinement (#4926): CWD=main checkout, ${_q4926}-quoted echo > main-checkout path denies" \
        "echo x > ${_q4926}$WT_REPO/defaults/hooks/f.sh${_q4926}" "$WT_REPO"
    assert_deny "write-confinement (#4926): CWD=main checkout, ${_q4926}-quoted echo >> main-checkout path denies" \
        "echo x >> ${_q4926}$WT_REPO/defaults/hooks/f.sh${_q4926}" "$WT_REPO"
    assert_deny "write-confinement (#4926): CWD=main checkout, ${_q4926}-quoted tee main-checkout path denies" \
        "echo x | tee ${_q4926}$WT_REPO/defaults/hooks/f.sh${_q4926}" "$WT_REPO"
    assert_deny "write-confinement (#4926): CWD=main checkout, ${_q4926}-quoted sed -i on main-checkout path denies" \
        "sed -i 's/a/b/' ${_q4926}$WT_REPO/defaults/hooks/f.sh${_q4926}" "$WT_REPO"
    assert_deny "write-confinement (#4926): CWD=main checkout, ${_q4926}-quoted cp destination in main checkout denies" \
        "cp /tmp/a.sh ${_q4926}$WT_REPO/defaults/hooks/f.sh${_q4926}" "$WT_REPO"
    assert_deny "write-confinement (#4926): CWD=main checkout, ${_q4926}-quoted mv destination in main checkout denies" \
        "mv /tmp/a.sh ${_q4926}$WT_REPO/defaults/hooks/f.sh${_q4926}" "$WT_REPO"

    # These twelve are the actual bypass: every one of them ALLOWED pre-#4926.
    assert_deny "write-confinement (#4926): CWD=linked worktree, ${_q4926}-quoted echo > main-checkout path denies" \
        "echo x > ${_q4926}$WT_REPO_LINKED/defaults/hooks/f.sh${_q4926}" "$WT_LINKED_DIR"
    assert_deny "write-confinement (#4926): CWD=linked worktree, ${_q4926}-quoted echo >> main-checkout path denies" \
        "echo x >> ${_q4926}$WT_REPO_LINKED/defaults/hooks/f.sh${_q4926}" "$WT_LINKED_DIR"
    assert_deny "write-confinement (#4926): CWD=linked worktree, ${_q4926}-quoted tee main-checkout path denies" \
        "echo x | tee ${_q4926}$WT_REPO_LINKED/defaults/hooks/f.sh${_q4926}" "$WT_LINKED_DIR"
    assert_deny "write-confinement (#4926): CWD=linked worktree, ${_q4926}-quoted sed -i on main-checkout path denies" \
        "sed -i 's/a/b/' ${_q4926}$WT_REPO_LINKED/defaults/hooks/f.sh${_q4926}" "$WT_LINKED_DIR"
    assert_deny "write-confinement (#4926): CWD=linked worktree, ${_q4926}-quoted cp destination in main checkout denies" \
        "cp /tmp/a.sh ${_q4926}$WT_REPO_LINKED/defaults/hooks/f.sh${_q4926}" "$WT_LINKED_DIR"
    assert_deny "write-confinement (#4926): CWD=linked worktree, ${_q4926}-quoted mv destination in main checkout denies" \
        "mv /tmp/a.sh ${_q4926}$WT_REPO_LINKED/defaults/hooks/f.sh${_q4926}" "$WT_LINKED_DIR"
done
unset _q4926

# Sibling-allow checks: quote removal changes only the absolute/relative
# CLASSIFICATION -- it must never widen the containment test itself, so a
# quoted target genuinely inside the worktree, or in /tmp, still allows.
assert_allow "write-confinement (#4926): CWD=linked worktree, double-quoted write inside the worktree allows" \
    "echo x > \"$WT_LINKED_DIR/src/f.sh\"" "$WT_LINKED_DIR"
assert_allow "write-confinement (#4926): CWD=linked worktree, single-quoted write to /tmp allows" \
    "echo x > '/tmp/loom-test-$$-quoted.sh'" "$WT_LINKED_DIR"

# Regression guard for the #4382 / #4921 contracts: a file genuinely named
# literally `$X` or `~` (single-quoted or backslash-escaped) is NOT an
# expansion -- strip_target_quoting() removes the quote characters but leaves
# `$`/`~` untouched, so these keep resolving as plain relative literals
# (allowed here, since they land inside the worktree the write runs from) and
# still deny when that relative literal sits under the main checkout.
assert_allow "write-confinement (#4926): CWD=linked worktree, single-quoted literal '\$X' filename allows (not a \$-expansion)" \
    "echo x > '\$X'" "$WT_LINKED_DIR"
assert_allow "write-confinement (#4926): CWD=linked worktree, backslash-escaped literal \\\$X filename allows (not a \$-expansion)" \
    "echo x > \\\$X" "$WT_LINKED_DIR"
assert_allow "write-confinement (#4926): CWD=linked worktree, single-quoted literal '~evil' filename allows (not tilde-expanded)" \
    "echo x > '~evil'" "$WT_LINKED_DIR"
assert_deny "write-confinement (#4926): CWD=linked worktree, single-quoted literal '\$X' filename UNDER the main checkout still denies" \
    "echo x > '$WT_REPO_LINKED/defaults/hooks/\$X'" "$WT_LINKED_DIR"

# Unbalanced/unterminated quote: strip_target_quoting() reports failure and the
# caller falls back to the raw, quote-preserved token -- i.e. today's verdict,
# unchanged in BOTH directions. From a main-checkout cwd the raw token is still
# read as relative and cwd-joined back inside the main checkout (deny); from a
# linked-worktree cwd the same fabrication still lands in the worktree's own
# sentinel (allow). The second case is NOT a regression -- it was already an
# allow pre-#4926; it pins that the fallback never widens a deny into an allow
# and never narrows an allow into a deny.
assert_deny "write-confinement (#4926): CWD=main checkout, unbalanced leading single-quote keeps today's deny" \
    "echo x > '$WT_REPO/defaults/hooks/f.sh" "$WT_REPO"
assert_allow "write-confinement (#4926): CWD=linked worktree, unbalanced leading single-quote keeps today's allow" \
    "echo x > '$WT_REPO_LINKED/defaults/hooks/f.sh" "$WT_LINKED_DIR"

# -------------------------------------------------------------------------
# Quote-aware whitespace masking (#4934) -- mask_ws() in extract_write_targets().
#
# A quoted write target containing a literal space (e.g.
# `echo x > '/main/checkout/evil file.sh'`) was tokenized by the plain
# `split(seg, toks, /[ \t]+/)` whitespace split into TWO fragments; only the
# FIRST fragment (carrying a dangling, unterminated quote) was ever used as
# the write target. strip_target_quoting() correctly reported that dangling
# quote as unbalanced and fell back to the raw fragment (#4926's "never widen
# a deny into an allow" contract) -- but the fallback fragment itself was then
# misclassified as a RELATIVE path and cwd-joined into the acting worktree,
# turning what should be a main-checkout DENY into a false ALLOW from a
# linked-worktree cwd (the canonical builder setup). mask_ws() fixes the
# tokenizer itself so a quoted spaced path yields exactly one token.
for _q4934 in "'" '"'; do
    assert_deny "write-confinement (#4934): CWD=main checkout, ${_q4934}-quoted spaced echo > main-checkout path denies" \
        "echo x > ${_q4934}$WT_REPO/defaults/hooks/evil file.sh${_q4934}" "$WT_REPO"
    assert_deny "write-confinement (#4934): CWD=main checkout, ${_q4934}-quoted spaced echo >> main-checkout path denies" \
        "echo x >> ${_q4934}$WT_REPO/defaults/hooks/evil file.sh${_q4934}" "$WT_REPO"
    assert_deny "write-confinement (#4934): CWD=main checkout, ${_q4934}-quoted spaced tee main-checkout path denies" \
        "echo x | tee ${_q4934}$WT_REPO/defaults/hooks/evil file.sh${_q4934}" "$WT_REPO"
    assert_deny "write-confinement (#4934): CWD=main checkout, ${_q4934}-quoted spaced sed -i on main-checkout path denies" \
        "sed -i 's/a/b/' ${_q4934}$WT_REPO/defaults/hooks/evil file.sh${_q4934}" "$WT_REPO"
    assert_deny "write-confinement (#4934): CWD=main checkout, ${_q4934}-quoted spaced cp destination in main checkout denies" \
        "cp /tmp/a.sh ${_q4934}$WT_REPO/defaults/hooks/evil file.sh${_q4934}" "$WT_REPO"
    assert_deny "write-confinement (#4934): CWD=main checkout, ${_q4934}-quoted spaced mv destination in main checkout denies" \
        "mv /tmp/a.sh ${_q4934}$WT_REPO/defaults/hooks/evil file.sh${_q4934}" "$WT_REPO"

    # The actual bypass (#4934): every one of these six ALLOWED pre-fix, from
    # a linked-worktree cwd -- exactly the #4178 protection's canonical mode.
    assert_deny "write-confinement (#4934): CWD=linked worktree, ${_q4934}-quoted spaced echo > main-checkout path denies" \
        "echo x > ${_q4934}$WT_REPO_LINKED/defaults/hooks/evil file.sh${_q4934}" "$WT_LINKED_DIR"
    assert_deny "write-confinement (#4934): CWD=linked worktree, ${_q4934}-quoted spaced echo >> main-checkout path denies" \
        "echo x >> ${_q4934}$WT_REPO_LINKED/defaults/hooks/evil file.sh${_q4934}" "$WT_LINKED_DIR"
    assert_deny "write-confinement (#4934): CWD=linked worktree, ${_q4934}-quoted spaced tee main-checkout path denies" \
        "echo x | tee ${_q4934}$WT_REPO_LINKED/defaults/hooks/evil file.sh${_q4934}" "$WT_LINKED_DIR"
    assert_deny "write-confinement (#4934): CWD=linked worktree, ${_q4934}-quoted spaced sed -i on main-checkout path denies" \
        "sed -i 's/a/b/' ${_q4934}$WT_REPO_LINKED/defaults/hooks/evil file.sh${_q4934}" "$WT_LINKED_DIR"
    assert_deny "write-confinement (#4934): CWD=linked worktree, ${_q4934}-quoted spaced cp destination in main checkout denies" \
        "cp /tmp/a.sh ${_q4934}$WT_REPO_LINKED/defaults/hooks/evil file.sh${_q4934}" "$WT_LINKED_DIR"
    assert_deny "write-confinement (#4934): CWD=linked worktree, ${_q4934}-quoted spaced mv destination in main checkout denies" \
        "mv /tmp/a.sh ${_q4934}$WT_REPO_LINKED/defaults/hooks/evil file.sh${_q4934}" "$WT_LINKED_DIR"
done
unset _q4934

# Sibling-allow: a quoted spaced path genuinely inside the acting worktree
# must still be allowed -- mask_ws() only narrows how a token is SPLIT, it
# must never widen the containment test itself (no over-blocking regression).
assert_allow "write-confinement (#4934): CWD=linked worktree, single-quoted spaced target inside the worktree allows" \
    "echo x > '$WT_LINKED_DIR/src/evil file.sh'" "$WT_LINKED_DIR"
assert_allow "write-confinement (#4934): CWD=linked worktree, double-quoted spaced target inside the worktree allows" \
    "echo x > \"$WT_LINKED_DIR/src/evil file.sh\"" "$WT_LINKED_DIR"

# -------------------------------------------------------------------------
# Whole-buffer quote masking for PLAIN multi-line quoted strings (#5157) --
# extract_write_targets() must not misread a `>` write-idiom byte sitting on
# a CONTINUATION line of an ordinary multi-line double/single-quoted shell
# string (no heredoc anywhere) as a live redirection target. Distinct from
# both #4245 (same-line quoted `>`) and #5000 (heredoc-BODY `>`): this covers
# a `>` character several PHYSICAL LINES into a plain `VAR="...\n...\n..."`
# assignment. Before this fix, mask_ws()/mask_gt() were still called per
# SEGMENT (after splitting the qsplit()-segmented buffer on "\n"), so a
# still-open quote spanning multiple physical lines reset to "unquoted" state
# at every embedded newline even though the shell never treats it that way --
# the confirmed #5157 occurrence-1 repro: a guard-test harness assigning a
# multi-line JSON/text payload to a shell variable, later only echoed/piped
# to a subprocess (never executed by the outer shell), was denied because a
# `> /path/to/pwned.txt`-shaped substring several lines into that assignment
# was misread as a real redirect target.
assert_allow "write-confinement (#5157): multi-line double-quoted VAR assignment with '>' on a continuation line allows" \
    "msg=\"line one
echo pwned > $WT_REPO/defaults/hooks/f.sh
line three\"
echo \"\$msg\"" "$WT_REPO"

assert_allow "write-confinement (#5157): same multi-line quoted VAR assignment from a linked-worktree cwd allows" \
    "msg=\"line one
echo pwned > $WT_REPO_LINKED/defaults/hooks/f.sh
line three\"
echo \"\$msg\"" "$WT_LINKED_DIR"

assert_allow "write-confinement (#5157): multi-line SINGLE-quoted VAR assignment with '>' on a continuation line allows" \
    "msg='line one
echo pwned > $WT_REPO/defaults/hooks/f.sh
line three'
echo \"\$msg\"" "$WT_REPO"

# Narrows, never widens: a REAL unquoted '>' write AFTER a multi-line quoted
# block in the same command must still deny.
assert_deny "write-confinement (#5157): real unquoted '>' write AFTER a multi-line quoted VAR assignment still denies" \
    "msg=\"line one
harmless > text
line three\"
echo pwned > $WT_REPO/defaults/hooks/g.sh" "$WT_REPO"

# ...and BEFORE it, in the same command.
assert_deny "write-confinement (#5157): real unquoted '>' write BEFORE a multi-line quoted VAR assignment still denies" \
    "echo pwned > $WT_REPO/defaults/hooks/h.sh
msg=\"line one
harmless > text
line three\"" "$WT_REPO"

# A genuine write inside the acting worktree, alongside an unrelated
# multi-line quoted block, must still allow (no over-widening the other
# direction either).
assert_allow "write-confinement (#5157): multi-line quoted VAR assignment plus a real write inside the worktree allows" \
    "msg=\"line one
harmless > text
line three\"
echo x > $WT_DIR/src/f.sh" "$WT_REPO"

# -------------------------------------------------------------------------
# `>`/`>=` inside a quoted jq/python comparison expression, real-world `gh`/
# `python3` shapes (#6023).
#
# Distinct from #4245 (same-line quoted `>`, a `--body`/-m prose value) in
# that these are ordinary READ-ONLY `gh`/`python3` invocations whose quoted
# ARGUMENT happens to be a comparison expression (a jq filter, a Python
# inequality) rather than free-form prose -- confirming mask_gt()'s existing
# quote-tracking (toggling on every bare `"`, #4245) already covers this
# shape too, with no special-casing needed for `--jq`/`-c` specifically. Also
# distinct from #5515 (unquoted arithmetic/test-context `>`/`>=`) -- both
# operators here are genuinely inside a quoted span, not bare shell syntax.
#
# Repro 1: a `>` jq comparison, double-quoted with escaped inner quotes
# (`--jq "... > \"date\" ..."`), the exact shape from the issue's field
# incident (three false DENYs in one session on ordinary `gh pr list --jq`
# queries).
assert_allow "write-confinement (#6023): '>' inside a quoted jq comparison expression allows" \
    "gh pr list --repo owner/repo --state merged --limit 30 --jq \"[.[] | select(.mergedAt > \\\"2026-08-08\\\")] | length\"" "$WT_REPO"

# Same repro split across two physical lines via a trailing `\` line
# continuation -- the literal multi-line form shown in the issue -- must
# allow identically (mirrors the #5157 whole-buffer masking: an embedded
# newline inside qsplit()'s copied span does not reset quote tracking).
assert_allow "write-confinement (#6023): '>' inside a quoted jq comparison, split across a backslash line continuation, allows" \
    "gh pr list --repo owner/repo --state merged --limit 30 \\
  --jq \"[.[] | select(.mergedAt > \\\"2026-08-08\\\")] | length\"" "$WT_REPO"

# Repro 2: a `>=` Python inequality, single-quoted operands nested inside a
# multi-line `python3 -c \"...\"` program -- the second field-incident shape,
# which previously manufactured a phantom write target of the literal `=`
# (the exact #5515-era failure mode, but reached here via genuine quoting
# rather than an unquoted arithmetic context).
assert_allow "write-confinement (#6023): '>=' inside a quoted multi-line python3 -c inequality allows" \
    "python3 -c \"
import json,sys
rows=json.load(sys.stdin)
n=sum(1 for p in rows if p['mergedAt'][:16] >= '2026-08-11T06:00')
print(n)
\"" "$WT_REPO"

# Narrows, never widens: a REAL unquoted '>' redirect immediately AFTER
# either quoted expression's closing quote must still deny.
assert_deny "write-confinement (#6023): real unquoted '>' redirect right after a quoted jq comparison still denies" \
    "gh pr list --repo owner/repo --state merged --limit 30 --jq \"[.[] | select(.mergedAt > \\\"2026-08-08\\\")] | length\" > $WT_REPO/defaults/hooks/f6023a.sh" "$WT_REPO"
assert_deny "write-confinement (#6023): real unquoted '>' redirect right after a quoted multi-line python3 -c inequality still denies" \
    "python3 -c \"
import json,sys
rows=json.load(sys.stdin)
n=sum(1 for p in rows if p['mergedAt'][:16] >= '2026-08-11T06:00')
print(n)
\" > $WT_REPO/defaults/hooks/f6023b.sh" "$WT_REPO"

# -------------------------------------------------------------------------
# Quoted `cd` ARGUMENT (not the write target) is still classified as ABSOLUTE
# (#4933). extract_write_targets()'s awk `cd` handler builds `curcwd` from
# toks[2] verbatim (qsplit's contract) -- a quoted absolute `cd` argument
# ('/main/checkout' or "/main/checkout") therefore starts with a quote
# character, not `/`, so the `toks[2] ~ /^\//` test called it RELATIVE and
# joined it onto the current curcwd instead of recognizing it as absolute.
# From a LINKED-WORKTREE cwd -- the canonical builder setup -- that
# fabrication ("<worktree>/'<main>'") walks straight back into the acting
# worktree's own `.loom-managed` sentinel, silently ALLOWING a write that
# should be denied. This is the SAME masked-allow shape as #4926, reached
# through the `cd` argument instead of the write target -- #4926's
# strip_target_quoting() cannot reach it because the decision is made
# entirely inside awk, before the shell layer ever sees a target.
#
# Mirrors the unquoted `cd $MAIN && ...` (#4210) fixture, every write idiom,
# both quote styles, from a linked-worktree cwd -- these all ALLOWED
# pre-#4933.
for _q4933 in "'" '"'; do
    assert_deny "write-confinement (#4933): CWD=linked worktree, cd ${_q4933}-quoted \$MAIN && relative echo > write denies" \
        "cd ${_q4933}$WT_REPO_LINKED${_q4933} && echo x > defaults/hooks/f.sh" "$WT_LINKED_DIR"
    assert_deny "write-confinement (#4933): CWD=linked worktree, cd ${_q4933}-quoted \$MAIN && relative echo >> write denies" \
        "cd ${_q4933}$WT_REPO_LINKED${_q4933} && echo x >> defaults/hooks/f.sh" "$WT_LINKED_DIR"
    assert_deny "write-confinement (#4933): CWD=linked worktree, cd ${_q4933}-quoted \$MAIN && relative tee write denies" \
        "cd ${_q4933}$WT_REPO_LINKED${_q4933} && echo x | tee defaults/hooks/f.sh" "$WT_LINKED_DIR"
    assert_deny "write-confinement (#4933): CWD=linked worktree, cd ${_q4933}-quoted \$MAIN && relative sed -i write denies" \
        "cd ${_q4933}$WT_REPO_LINKED${_q4933} && sed -i 's/a/b/' defaults/hooks/f.sh" "$WT_LINKED_DIR"
    assert_deny "write-confinement (#4933): CWD=linked worktree, cd ${_q4933}-quoted \$MAIN && relative cp destination denies" \
        "cd ${_q4933}$WT_REPO_LINKED${_q4933} && cp /tmp/a.sh defaults/hooks/f.sh" "$WT_LINKED_DIR"
    assert_deny "write-confinement (#4933): CWD=linked worktree, cd ${_q4933}-quoted \$MAIN && relative mv destination denies" \
        "cd ${_q4933}$WT_REPO_LINKED${_q4933} && mv /tmp/a.sh defaults/hooks/f.sh" "$WT_LINKED_DIR"
done
unset _q4933

# Sibling-allow checks: a quoted `cd` argument that genuinely lands inside the
# worktree, or in /tmp, must still allow -- quote removal changes only the
# absolute/relative CLASSIFICATION of the `cd` argument, never the
# containment test itself.
assert_allow "write-confinement (#4933): CWD=linked worktree, cd single-quoted own-worktree path && relative write inside worktree allows" \
    "cd '$WT_LINKED_DIR' && echo x > src/f.sh" "$WT_LINKED_DIR"
assert_allow "write-confinement (#4933): CWD=linked worktree, cd double-quoted /tmp && relative write allows" \
    "cd \"/tmp\" && echo x > loom-test-$$-cdquoted.sh" "$WT_LINKED_DIR"

# Unbalanced/unterminated quote in the `cd` argument: the classification copy
# falls back UNCHANGED (still starts with a quote character, not `/`), so this
# keeps today's verdict, never widening a deny into an allow. From a
# linked-worktree cwd the fabricated relative join still lands back inside the
# worktree's own sentinel -- an allow unchanged pre/post-#4933 (NOT a
# regression; mirrors #4926's identical fallback contract for the target
# side).
assert_allow "write-confinement (#4933): CWD=linked worktree, unbalanced leading single-quote in cd argument keeps today's allow" \
    "cd '$WT_REPO_LINKED && echo x > defaults/hooks/f.sh" "$WT_LINKED_DIR"

# Quote CONTEXT must survive into the shell layer (#4933 review regression
# guard). The awk `cd` handler classifies on a quote-STRIPPED copy but must
# keep building `curcwd` from the RAW, quote-preserved token, because `curcwd`
# is the only value threaded to the shell layer as `_wcwd` and the
# unresolved-`$` detector there (mark_expandable_dollars, #4921/#4927) needs
# the quote characters to tell a LITERAL `$` inside a single-quoted span from
# an EXPANDABLE one. An earlier iteration of this fix stripped the quotes
# BEFORE building curcwd, which made every `$` in the last `cd` segment look
# expandable and turned these single-quoted-literal cases into false denies.
#
# `cd '$FOO' && <relative write>` -- the shell never expands a single-quoted
# `$`, so this really is a cwd-relative directory named `$FOO` inside the
# acting worktree: ALLOW (the same "deliberately NOT denied" carve-out the
# #4926 literal-'$X'-filename fixtures above pin for the target side).
assert_allow "write-confinement (#4933): CWD=linked worktree, cd single-quoted literal '\$FOO' && relative write allows (literal \$, not an expansion)" \
    "cd '\$FOO' && echo x > defaults/hooks/f.sh" "$WT_LINKED_DIR"
assert_allow "write-confinement (#4933): CWD=linked worktree, cd backslash-escaped literal \\\$FOO && relative write allows (literal \$, not an expansion)" \
    "cd \\\$FOO && echo x > defaults/hooks/f.sh" "$WT_LINKED_DIR"
assert_allow "write-confinement (#4933): CWD=linked worktree, cd single-quoted literal '\$FOO' && relative tee write allows" \
    "cd '\$FOO' && echo x | tee defaults/hooks/f.sh" "$WT_LINKED_DIR"

# ...and the EXPANDABLE counterparts are unchanged: a bare or double-quoted
# `$` in the tracked `cd` argument is a cwd this guard cannot resolve, so the
# relative write that follows still fails CLOSED (#4921/#4927). These pin that
# the regression fix above does not widen the unresolved-`$` deny into an
# allow.
assert_deny "write-confinement (#4933): CWD=linked worktree, cd double-quoted expandable \"\$MAIN\" && relative write still denies" \
    "cd \"\$MAIN\" && echo x > defaults/hooks/f.sh" "$WT_LINKED_DIR"
assert_deny "write-confinement (#4933): CWD=linked worktree, cd bare expandable \$MAIN && relative write still denies" \
    "cd \$MAIN && echo x > defaults/hooks/f.sh" "$WT_LINKED_DIR"
assert_deny "write-confinement (#4933): CWD=linked worktree, cd \${MAIN} brace-expandable && relative write still denies" \
    "cd \${MAIN} && echo x > defaults/hooks/f.sh" "$WT_LINKED_DIR"
assert_deny "write-confinement (#4933): CWD=linked worktree, cd double-quoted expandable \"\$MAIN\" && relative tee write still denies" \
    "cd \"\$MAIN\" && echo x | tee defaults/hooks/f.sh" "$WT_LINKED_DIR"

# -------------------------------------------------------------------------
# PARTIALLY quoted absolute `cd` argument -- e.g. `'<main>'/defaults`, the
# quote closing MID-TOKEN rather than at its end -- is still classified as
# ABSOLUTE (#5363, a residual #4933/#4926 shape found during Judge review of
# PR #4941). The #4933 fix (cdqc/cdlen leading-and-matching-trailing-quote
# strip) only recognized a FULLY quoted argument ('/abs/path', "/abs/path");
# a partially quoted one still starts with a quote character, still fails
# the `~ /^\//` test, and was still misclassified as RELATIVE -- joined onto
# curcwd instead of replacing it. From a LINKED-WORKTREE cwd that
# fabrication walks straight back into the acting worktree's own
# `.loom-managed` sentinel and the write is silently ALLOWED -- the same
# masked-allow shape as #4933/#4926, reached through a partially- rather
# than fully-quoted `cd` argument.
#
# Verified NOT a regression from #4933/#4941: this shape ALLOWed on both the
# pre- and post-#4933/#4941 trees -- #4933 narrowed the surface (fixed the
# fully-quoted case, probe C in #5363) but never touched this one (probe A).
for _q5363 in "'" '"'; do
    assert_deny "write-confinement (#5363): CWD=linked worktree, cd ${_q5363}-PARTIALLY-quoted \$MAIN/defaults && relative echo > write denies (probe A)" \
        "cd ${_q5363}$WT_REPO_LINKED${_q5363}/defaults && echo x > hooks/f.sh" "$WT_LINKED_DIR"
    assert_deny "write-confinement (#5363): CWD=linked worktree, cd ${_q5363}-PARTIALLY-quoted \$MAIN/defaults && relative tee write denies" \
        "cd ${_q5363}$WT_REPO_LINKED${_q5363}/defaults && echo x | tee hooks/f.sh" "$WT_LINKED_DIR"
    assert_deny "write-confinement (#5363): CWD=linked worktree, cd ${_q5363}-PARTIALLY-quoted \$MAIN/defaults && relative sed -i write denies" \
        "cd ${_q5363}$WT_REPO_LINKED${_q5363}/defaults && sed -i 's/a/b/' hooks/f.sh" "$WT_LINKED_DIR"
done
unset _q5363

# Sibling regression guard (probe B in #5363): a `cd` argument that starts
# UNQUOTED (so it already starts with `/` and classified correctly even
# before this fix) but has a quoted SUFFIX must stay denied -- pin that the
# #5363 fix does not disturb this already-correct shape.
assert_deny "write-confinement (#5363 regression guard, probe B): CWD=linked worktree, cd \$MAIN/\"defaults\" (unquoted prefix, quoted suffix) && relative write denies" \
    "cd $WT_REPO_LINKED/\"defaults\" && echo x > hooks/f.sh" "$WT_LINKED_DIR"

# Sibling-allow check: a partially-quoted `cd` argument that genuinely lands
# INSIDE the acting worktree must still allow -- the fix changes only the
# absolute/relative CLASSIFICATION of the `cd` argument, never the
# containment test itself.
assert_allow "write-confinement (#5363): CWD=linked worktree, cd partially-quoted own-worktree path && relative write inside worktree allows" \
    "cd '$WT_LINKED_DIR'/src && echo x > f.sh" "$WT_LINKED_DIR"

# Unterminated quote in a would-be-partially-quoted `cd` argument:
# strip_cd_quoting() falls back to the RAW, unchanged token (still starting
# with a quote character, not `/`) whenever a quote is left open at
# end-of-token, so this keeps today's verdict -- an allow, unchanged
# pre/post-#5363 (same fallback contract as #4926/#4933).
assert_allow "write-confinement (#5363): CWD=linked worktree, unterminated quote in a would-be-partially-quoted cd argument keeps today's allow" \
    "cd '$WT_REPO_LINKED/defaults && echo x > hooks/f.sh" "$WT_LINKED_DIR"

# -------------------------------------------------------------------------
# Single-angle `<` stdin redirection is NOT a write-target operand (#5369).
#
# extract_write_targets()'s tee / sed -i / cp-mv operand scans treated every
# non-flag token as a write-target candidate, including a `<` redirection
# operator and the file it reads FROM. Two symptoms, in opposite directions:
#
#   * false DENY (tee / sed -i): the bare `<` and its operand resolved
#     against curcwd into phantom `<repo>/<` and `<repo>/in` targets, so a
#     wholly out-of-tree command was denied as a #4178 confinement bypass.
#   * false ALLOW (cp / mv) -- the serious one: that branch takes the LAST
#     non-flag token as the destination, so a trailing `< /tmp/in` DISPLACED
#     the real destination and a copy/move INTO the protected main checkout
#     was waved through. That is a confinement escape, not just noise.
#
# Sibling of #5232/#5233 (the `<<`/`<<-`/`<<<` heredoc half of the same
# defect class), deliberately kept disjoint from it: this exclusion matches
# only a SINGLE leading `<`, and heredoc opener tokens are left to the
# pre-tokenization heredoc machinery.

# --- false DENY, now allowed (both targets are wholly out-of-tree) ---
assert_allow "write-confinement (#5369): tee with a trailing '< /tmp/in' stdin redirect allows" \
    "tee /tmp/f.md < /tmp/in" "$WT_REPO"
assert_allow "write-confinement (#5369): sed -i with a trailing '< /tmp/in' stdin redirect allows" \
    "sed -i 's/a/b/' /tmp/z.sh < /tmp/in" "$WT_REPO"
assert_allow "write-confinement (#5369): attached-form '</tmp/in' stdin redirect on tee allows" \
    "tee /tmp/f.md </tmp/in" "$WT_REPO"
assert_allow "write-confinement (#5369): fd-prefixed '0< /tmp/in' stdin redirect on tee allows" \
    "tee /tmp/f.md 0< /tmp/in" "$WT_REPO"

# --- false ALLOW, now denied (the confinement escape this issue is about) ---
assert_deny "write-confinement (#5369): cp into the main checkout with a trailing '< /tmp/in' denies" \
    "cp /tmp/a $WT_REPO/defaults/hooks/p.sh < /tmp/in" "$WT_REPO"
assert_deny "write-confinement (#5369): mv into the main checkout with a trailing '< /tmp/in' denies" \
    "mv /tmp/a $WT_REPO/defaults/hooks/p.sh < /tmp/in" "$WT_REPO"
assert_deny "write-confinement (#5369): cp into the main checkout with an attached '</tmp/in' denies" \
    "cp /tmp/a $WT_REPO/defaults/hooks/p.sh </tmp/in" "$WT_REPO"
assert_deny "write-confinement (#5369): cp into the main checkout with a leading '< /tmp/in' operand denies" \
    "cp < /tmp/in /tmp/a $WT_REPO/defaults/hooks/p.sh" "$WT_REPO"

# --- control: same command WITHOUT the redirect is unchanged ---
assert_deny "write-confinement (#5369 control): cp into the main checkout with no redirect still denies" \
    "cp /tmp/a $WT_REPO/defaults/hooks/p.sh" "$WT_REPO"

# --- narrows, never widens: a REAL target alongside the redirect still denies ---
assert_deny "write-confinement (#5369): tee into the main checkout with a trailing '< /tmp/in' still denies" \
    "tee $WT_REPO/defaults/hooks/p.sh < /tmp/in" "$WT_REPO"
assert_deny "write-confinement (#5369): sed -i on a main-checkout file with a trailing '< /tmp/in' still denies" \
    "sed -i 's/a/b/' $WT_REPO/defaults/hooks/p.sh < /tmp/in" "$WT_REPO"
assert_deny "write-confinement (#5369): '< in' alongside a real '>' redirect into the main checkout still denies" \
    "cat < /tmp/in > $WT_REPO/defaults/hooks/p.sh" "$WT_REPO"

# --- no new escape vector: a QUOTED/ESCAPED literal filename that merely
# begins with `<` is not a redirection operator and must still be scanned as
# a write target (it stays relative, so it resolves into the main checkout).
assert_deny "write-confinement (#5369): single-quoted literal filename beginning with '<' is still a cp target" \
    "cp /tmp/a '<x'" "$WT_REPO"
assert_deny "write-confinement (#5369): double-quoted literal filename beginning with '<' is still a cp target" \
    "cp /tmp/a \"<x\"" "$WT_REPO"
assert_deny "write-confinement (#5369): backslash-escaped literal filename beginning with '<' is still a cp target" \
    "cp /tmp/a \\<x" "$WT_REPO"
assert_deny "write-confinement (#5369): quoted literal filename beginning with '<' is still a tee target" \
    "tee '<x'" "$WT_REPO"
assert_deny "write-confinement (#5369): quoted literal filename beginning with '<' is still a sed -i target" \
    "sed -i 's/a/b/' '<x'" "$WT_REPO"

# --- cd-tracking still threads through a command carrying a stdin redirect ---
assert_allow "write-confinement (#5369): cd <worktree> && tee relative target with '< /tmp/in' allows" \
    "cd $WT_DIR && tee f.sh < /tmp/in" "$WT_REPO"
assert_deny "write-confinement (#5369): cd <main root> && tee relative target with '< /tmp/in' denies" \
    "cd $WT_REPO/defaults && tee hooks/f.sh < /tmp/in" "$WT_REPO"

# -------------------------------------------------------------------------
# Numbered-fd output redirect is NOT a write-target operand (#6326).
#
# extract_write_targets()'s tee / sed -i / cp-mv operand scans treated a
# same-line numbered file-descriptor redirect (`2>/dev/null`, `2>&1`,
# `1>/tmp/x`, ...) as an ordinary non-flag token, including it as a candidate
# write-target argument. For cp/mv -- whose destination is the LAST non-flag
# token -- that phantom token DISPLACED the real destination, and because it
# does not start with `/` it was joined against curcwd and mis-resolved into
# the main checkout, producing a false DENY on a harmless `/tmp`-only write
# idiom that is one of the most common shell idioms in existence. Sibling of
# #5369 (the `<` stdin-redirect half of the same defect class) and #5232 (the
# heredoc-operator half): deliberately kept disjoint from both, matching only
# a `[0-9]+>`/`[0-9]+>>` token (at least one leading digit required) so a bare
# `>`/`>>` with NO leading digit is completely unaffected by this fix.

# --- repro from the issue: a harmless /tmp write with a trailing 2>/dev/null
# was denied quoting a bogus '<repo>/2>/dev/null' target ---
assert_allow "write-confinement (#6326): cp to /tmp with a trailing '2>/dev/null' allows" \
    "cp /bin/sleep /tmp/loom-test-$$-sleep-check 2>/dev/null" "$WT_REPO"
assert_allow "write-confinement (#6326 control): same cp with no trailing redirect already allows" \
    "cp /bin/sleep /tmp/loom-test-$$-sleep-check" "$WT_REPO"

# --- fd-to-fd dup (`2>&1`) must never be scanned as a path at all, distinct
# from the fd-to-file form (`2>/dev/null`) above ---
assert_allow "write-confinement (#6326): cp to /tmp with a trailing '2>&1' allows" \
    "cp /bin/sleep /tmp/loom-test-$$-sleep-check 2>&1" "$WT_REPO"

# --- other numbered fds and forms (1>, 2>>, spaced) ---
assert_allow "write-confinement (#6326): cp to /tmp with a trailing '1>/tmp/x' allows" \
    "cp /bin/sleep /tmp/loom-test-$$-sleep-check 1>/tmp/loom-test-$$-x" "$WT_REPO"
assert_allow "write-confinement (#6326): cp to /tmp with a trailing '2>>/tmp/x' (append) allows" \
    "cp /bin/sleep /tmp/loom-test-$$-sleep-check 2>>/tmp/loom-test-$$-x" "$WT_REPO"
assert_allow "write-confinement (#6326): sed -i on a /tmp file with a trailing '2>/dev/null' allows" \
    "sed -i 's/a/b/' /tmp/loom-test-$$-z.sh 2>/dev/null" "$WT_REPO"
assert_allow "write-confinement (#6326): tee to /tmp with a trailing '2>/dev/null' allows" \
    "tee /tmp/loom-test-$$-f.md 2>/dev/null" "$WT_REPO"
assert_allow "write-confinement (#6326): mv within /tmp with a trailing '2>/dev/null' allows" \
    "mv /tmp/loom-test-$$-a /tmp/loom-test-$$-b 2>/dev/null" "$WT_REPO"

# --- narrows, never widens: a REAL target that still resolves inside the main
# checkout must still deny, even with a trailing same-line numeric-fd redirect ---
assert_deny "write-confinement (#6326): cp into the main checkout with a trailing '2>/dev/null' still denies" \
    "cp /tmp/a $WT_REPO/defaults/hooks/p.sh 2>/dev/null" "$WT_REPO"
assert_deny "write-confinement (#6326): cp into the main checkout with a trailing '2>&1' still denies" \
    "cp /tmp/a $WT_REPO/defaults/hooks/p.sh 2>&1" "$WT_REPO"
assert_deny "write-confinement (#6326): sed -i on a main-checkout file with a trailing '2>/dev/null' still denies" \
    "sed -i 's/a/b/' $WT_REPO/defaults/hooks/p.sh 2>/dev/null" "$WT_REPO"
assert_deny "write-confinement (#6326 control): cp into the main checkout with no redirect still denies" \
    "cp /tmp/a $WT_REPO/defaults/hooks/p.sh" "$WT_REPO"

# --- no new escape vector: a bare `>`/`>>` with NO leading digit is
# completely outside this exclusion and keeps its existing (unchanged)
# behavior -- a relative destination it resolves is still scanned and still
# denies inside the main checkout ---
assert_deny "write-confinement (#6326): bare '>' with no leading digit is unaffected -- relative destination still denies" \
    "echo x > f.sh" "$WT_REPO"

# --- no new escape vector: a filename that merely ENDS in a digit, followed
# by whitespace then a separate bare '>' token, is two distinct tokens and
# must still be scanned as its own write target (not folded into the
# redirect operator it merely precedes) ---
assert_deny "write-confinement (#6326): a relative filename ending in a digit before a separate bare '>' redirect is still its own write target" \
    "tee file9 > /tmp/loom-test-$$-out.log" "$WT_REPO"

# -------------------------------------------------------------------------
# Guard-decision telemetry review false positives (#5674): four shapes
# reported denying catastrophically even though the resolved write target
# does not fall inside the main repository checkout. Each was reproduced
# live against defaults/hooks/guard-destructive-generic.sh before any code
# change to confirm which were still live bugs (as the issue explicitly
# asked for) rather than guessed at:
#
#   1. tmp-then-rename fully inside the repos own checkout -- confirmed
#      correct/intended behavior (not a redirect-target-parsing bug): once
#      ANY managed worktree exists anywhere in the repo, a genuine write
#      into the main checkout denies regardless of whether the acting
#      session cwd is itself the main checkout, because this check cannot
#      verify the write belongs to the acting session (#4245) -- the SAME
#      documented, deliberate tradeoff #5315 already declined to carve an
#      exemption out of for main-checkout-only daemon state. No fix here;
#      see the assert_deny case below that locks this DENY in on purpose.
#   2. cp -r with multiple worktree sources and a /tmp destination --
#      reproduced as an ALLOW on main already (the "last non-flag token is
#      the destination" cp/mv logic was never actually confused by extra
#      source arguments). Regression-only, no code change needed.
#   3. sed -i on a plain /tmp scratch file, BSD-style with a SEPARATE
#      (usually empty) backup-suffix argument before the script -- this WAS
#      a live bug: the "skip exactly nfargs[1]" logic assumed at most ONE
#      non-file token before the real files (true for GNU sed, where the
#      script is always nfargs[1]), so for BSD -i (separate suffix + script
#      = two non-file tokens) the SCRIPT itself fell through as a phantom
#      write target -- denied even when the real target was a harmless
#      /tmp path, and denied with the WRONG resolved path even when the
#      real target genuinely was in the main checkout. Fixed directly in
#      extract_write_targets()'s sed branch.
#   4. `read A B < /tmp/f` -- a `<` INPUT redirection on a command
#      (`read`) this scanner never treats as a write idiom at all (only
#      tee/sed -i/cp/mv/redirection do), and the command contains none of
#      the pre-filter trigger substrings (">"/"tee"/"sed"/"cp "/"mv ") --
#      confirmed the whole write-confinement block never even engages for
#      it. Reproduced as an ALLOW on main already. Regression-only.
WT5674_REPO=$(make_wt_repo)
WT5674_DIR="$WT5674_REPO/.loom/worktrees/issue-1"
mkdir -p "$WT5674_REPO/.loom/gh-config" "$WT5674_DIR/dashboard/test" "$WT5674_DIR/dashboard/src"

# --- Sample 1: intentional main-checkout protection, NOT a parsing bug ---
assert_deny "write-confinement (#5674 sample 1, intended): tmp-then-rename fully inside the main checkout still denies (cwd=main root, not a worktree escape, but session identity is unverifiable -- #4245/#5315)" \
    "mv $WT5674_REPO/.loom/gh-config/hosts.yml.tmp $WT5674_REPO/.loom/gh-config/hosts.yml" "$WT5674_REPO"

# --- Sample 2: cp -r with multiple sources, /tmp destination -- already correct ---
assert_allow "write-confinement (#5674 sample 2): cp -r with multiple worktree sources and a /tmp destination allows" \
    "cp -r $WT5674_DIR/dashboard/test $WT5674_DIR/dashboard/src /tmp/loom-test-$$-issue-5543-ci/" "$WT5674_DIR"
assert_deny "write-confinement (#5674 sample 2 control): cp -r with multiple sources still denies when the destination resolves into the main checkout" \
    "cp -r $WT5674_DIR/dashboard/test $WT5674_DIR/dashboard/src $WT5674_REPO/defaults/hooks/" "$WT5674_DIR"

# --- Sample 3: BSD `sed -i ''` (separate empty backup-suffix arg) -- fixed ---
assert_allow "write-confinement (#5674 sample 3): BSD-style sed -i with a separate empty backup-suffix arg on a /tmp scratch file allows (script argument no longer misread as a write target)" \
    "sed -i '' 's/a/b/' /tmp/loom-test-$$-scan_env_seams.py" "$WT5674_REPO"
assert_allow "write-confinement (#5674 sample 3): BSD-style sed -i with a separate empty backup-suffix arg allows from a worktree cwd too" \
    "sed -i '' 's/a/b/' /tmp/loom-test-$$-scan_env_seams2.py" "$WT5674_DIR"
assert_deny "write-confinement (#5674 sample 3 control): BSD-style sed -i with a separate empty backup-suffix arg still denies when the real file target resolves into the main checkout" \
    "sed -i '' 's/a/b/' $WT5674_REPO/defaults/hooks/f.sh" "$WT5674_REPO"
assert_allow "write-confinement (#5674 sample 3): GNU-style sed -i (attached, no separate suffix arg) on a /tmp scratch file still allows (control -- unaffected by the BSD-form fix)" \
    "sed -i 's/a/b/' /tmp/loom-test-$$-scan_env_seams3.py" "$WT5674_REPO"
assert_allow "write-confinement (#5674 sample 3): GNU-style sed -i.bak (attached suffix) on a /tmp scratch file still allows (control -- unaffected by the BSD-form fix)" \
    "sed -i.bak 's/a/b/' /tmp/loom-test-$$-scan_env_seams4.py" "$WT5674_REPO"

# --- Sample 4: `read ... < file` is a read, not a write -- already correct ---
assert_allow "write-confinement (#5674 sample 4): 'read A B < /tmp/f' input redirection is not scanned as a write at all (never a tee/sed/cp/mv/redirection idiom)" \
    "read STALE_AT DEADLINE < /tmp/loom-test-$$-claim_epochs.txt" "$WT5674_DIR"

# -------------------------------------------------------------------------
# Embedded-apostrophe idiom in a sed/cp script or operand argument (#6968).
#
# HISTORY: mask_ws()/mask_gt()/qsplit() all track quote state with a naive
# "single quote toggles mode 1<->0" char scan that does not model
# backslash-escaping. The standard shell idiom for embedding a literal
# apostrophe inside otherwise-single-quoted text -- close-quote,
# backslash-escaped literal apostrophe, reopen-quote, e.g. 's/dont'\''t/x/'
# for a script whose replacement text contains "don't" -- is, to the real
# shell, ONE unbroken word (no real separator ever sits between the three
# pieces). The old naive scan instead saw: close (mode 1->0), a literal
# backslash (mode 0), then TWO adjacent quote chars that open-then-instantly-
# close an empty span (mode 0->1->0) -- netting back to mode 0 for the wrong
# reason. Every byte after that point was then scanned in the WRONG quote
# state until the next stray quote char, which usually meant: the real
# trailing whitespace before the actual file-operand argument was
# (wrongly) treated as still "inside quotes" and swallowed into the sed
# script token, while the tail of the script text and the true file operand
# fused into a single bogus "file operand" token -- producing a phantom
# repo-relative write target and a false worktree-write-confinement DENY on
# a plain outside-the-repo edit.
#
# Fixed by teaching mask_ws()/mask_gt()/qsplit() to recognize the exact
# 4-byte close-backslash-quote-quote run and stay "inside" the logical word
# across it, rather than toggling out and back in.

# 1. BSD `sed -i ''` form, embedded-apostrophe script, outside-repo target.
assert_allow "write-confinement (#6968): BSD sed -i '' with an embedded-apostrophe script targets outside the repo -> allow (script no longer fused with the file operand)" \
    "sed -i '' 's/don'\\''t/do not/' /tmp/loom-test-$$-6968-outside.md" "$WT5674_REPO"
assert_deny "write-confinement (#6968 control): BSD sed -i '' with an embedded-apostrophe script still denies when the real file target resolves into the main checkout" \
    "sed -i '' 's/don'\\''t/do not/' $WT5674_REPO/defaults/hooks/f.sh" "$WT5674_REPO"

# 2. GNU `sed -i` (attached, no separate suffix arg) form, same idiom.
assert_allow "write-confinement (#6968): GNU sed -i (attached) with an embedded-apostrophe script targets outside the repo -> allow" \
    "sed -i 's/don'\\''t/do not/' /tmp/loom-test-$$-6968-gnu-outside.md" "$WT5674_REPO"
assert_deny "write-confinement (#6968 control): GNU sed -i (attached) with an embedded-apostrophe script still denies when the real file target resolves into the main checkout" \
    "sed -i 's/don'\\''t/do not/' $WT5674_REPO/defaults/hooks/f.sh" "$WT5674_REPO"

# 3. `|`-delimited sed script (unaffected control, per the issue's own AC --
#    the fix for the apostrophe idiom must not disturb ordinary `|`-delimited
#    scripts, which carry no embedded apostrophe at all).
assert_allow "write-confinement (#6968 control): '|'-delimited sed script (no apostrophe), outside-repo target still allows" \
    "sed -i '' 's|/old/path|/new/path|' /tmp/loom-test-$$-6968-pipe.md" "$WT5674_REPO"
assert_deny "write-confinement (#6968 control): '|'-delimited sed script (no apostrophe) still denies when the target resolves into the main checkout" \
    "sed -i '' 's|/old/path|/new/path|' $WT5674_REPO/defaults/hooks/f.sh" "$WT5674_REPO"

# 4. Literal '|' characters WITHIN a '/'-delimited script's pattern/replacement
#    text (not a delimiter at all -- mirrors the issue's own reported
#    redacted example, a markdown-table-row edit containing '|' cell
#    separators), outside-repo target.
assert_allow "write-confinement (#6968): sed script with literal '|' characters in the replacement text (table-row edit), outside-repo target allows" \
    "sed -i '' 's/old .md |/new .pdf + .md |/; s/foo/bar/' /tmp/loom-test-$$-6968-table.md" "$WT5674_REPO"

# 5. `cp`: an embedded-apostrophe SOURCE filename argument must not fuse with
#    the real destination operand (mirrors the sed case for the cp/mv branch
#    named in the issue's own acceptance criteria).
assert_allow "write-confinement (#6968): cp with an embedded-apostrophe source filename, outside-repo destination allows" \
    "cp '/tmp/loom-test-$$-dont'\\''forget.txt' /tmp/loom-test-$$-6968-cp-outside.txt" "$WT5674_REPO"
assert_deny "write-confinement (#6968 control): cp with an embedded-apostrophe source filename still denies when the destination resolves into the main checkout" \
    "cp '/tmp/loom-test-$$-dont'\\''forget.txt' $WT5674_REPO/defaults/hooks/f.sh" "$WT5674_REPO"

rm -rf "$WT5674_REPO"

rm -rf "$HOME_FIXTURE_OUTSIDE"
rm -rf "$WT_REPO" "$WT_REPO_NOWT" "$WT_REPO_OFF" "$WT_REPO_LINKED"

echo ""

# =========================================================================

print_summary
