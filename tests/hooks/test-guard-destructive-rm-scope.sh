#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — rm scope.
#
# One slice of the former monolithic tests/hooks/test-guard-destructive.sh,
# split per #7741. Shared fixtures, assertions and catastrophic-phrase payloads
# live in tests/hooks/lib/guard-destructive-harness.sh.
#
# Usage: ./tests/hooks/test-guard-destructive-rm-scope.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

echo -e "${YELLOW}--- Repo-scoped rm guard (guards.rmScope / LOOM_RM_SCOPE) (#3610, #3628) ---${NC}"
# =========================================================================
#
# As of #3628 (ADR Option B) the guard ships with rmScope REPO by default:
# catastrophic top-level targets deny in every mode, AND an outside-repo deep
# path is DENIED unless it is under the repo/worktree areas or on the built-in
# ephemeral allowlist (system temp dirs + the Claude scratchpad). The legacy
# permissive behaviour (allow every deeper subpath, including outside-repo) is
# now an explicit opt-out via guards.rmScope:"off"/"permissive" or
# LOOM_RM_SCOPE=off. The 8-case matrix from the issue is asserted in BOTH
# states, plus worktree-root and env-override cases.
#
# NB: normalize_abs_path() is LEXICAL (no symlink resolution), so the allowlist
# lists both /tmp and /private/tmp (and the /var/tmp, /var/folders pairs). These
# temp-root cases pass in both toggle states — under OFF because a deep subpath
# is always allowed, under repo because they are on the ephemeral allowlist.

# ---- Matrix in the DEFAULT state: repo semantics (safe-by-default, #3628). ----
# rmScope absent → repo. Uses the real REPO_ROOT (loom checkout) as cwd.
assert_allow "rmScope default: rm -f /tmp/x/foo.tsv allowed (ephemeral)" \
    "rm -f /tmp/x/foo.tsv" "$REPO_ROOT"
assert_allow "rmScope default: rm -rf scratchpad path allowed (ephemeral)" \
    "rm -rf /private/tmp/claude-501/-Users-x/abc/scratchpad/z" "$REPO_ROOT"
assert_allow "rmScope default: rm -rf \$TMPDIR /var/folders path allowed (ephemeral)" \
    "rm -rf /var/folders/ab/cd/T/tmp.123" "$REPO_ROOT"
assert_deny "rmScope default: rm -rf bare /tmp still denied (top-level rule)" \
    "rm -rf /tmp" "$REPO_ROOT"
assert_deny "rmScope default: rm -rf / still denied (catastrophic rule)" \
    "rm -rf /" "$REPO_ROOT"
# The key behaviour-change rows: outside-repo deep paths are now DENIED by default.
assert_deny "rmScope default: rm -rf outside-repo /opt path denied (NEW default)" \
    "rm -rf /opt/some-vendor/important" "$REPO_ROOT"
assert_deny "rmScope default: rm -rf outside-repo /Users path denied (NEW default)" \
    "rm -rf /Users/someone/important" "$REPO_ROOT"
assert_allow "rmScope default: rm -rf under repo root allowed" \
    "rm -rf $REPO_ROOT/.loom/tmp/x" "$REPO_ROOT"

# ---- Explicit opt-out block: guards.rmScope:"off"/"permissive" restores the
# ---- OLD permissive behaviour (outside-repo deep rm allowed again). ----
RMSCOPE_OFF_REPO=$(make_sql_repo '{"guards":{"rmScope":"off"}}')
assert_allow "rmScope config-off: outside-repo path allowed again (opt-out)" \
    "rm -rf /opt/some-vendor/important" "$RMSCOPE_OFF_REPO"
assert_allow "rmScope config-off: outside-repo /Users path allowed again (opt-out)" \
    "rm -rf /Users/someone/important" "$RMSCOPE_OFF_REPO"
assert_deny "rmScope config-off: bare /tmp still denied (catastrophic rule holds)" \
    "rm -rf /tmp" "$RMSCOPE_OFF_REPO"
assert_deny "rmScope config-off: / still denied (catastrophic rule holds)" \
    "rm -rf /" "$RMSCOPE_OFF_REPO"

# "permissive" is a recognized synonym for "off".
RMSCOPE_PERM_REPO=$(make_sql_repo '{"guards":{"rmScope":"permissive"}}')
assert_allow "rmScope config-permissive: outside-repo path allowed (synonym for off)" \
    "rm -rf /opt/some-vendor/important" "$RMSCOPE_PERM_REPO"
assert_deny "rmScope config-permissive: bare /tmp still denied" \
    "rm -rf /tmp" "$RMSCOPE_PERM_REPO"

# Env opt-out: LOOM_RM_SCOPE=off / permissive restore permissive behaviour even
# with no config key present (default would otherwise be repo).
assert_allow_env "rmScope env-off: outside-repo path allowed (env opt-out)" \
    "LOOM_RM_SCOPE=off" "rm -rf /opt/some-vendor/important" "$REPO_ROOT"
assert_allow_env "rmScope env-permissive: outside-repo path allowed (env synonym)" \
    "LOOM_RM_SCOPE=permissive" "rm -rf /opt/some-vendor/important" "$REPO_ROOT"
assert_deny_env "rmScope env-off: bare /tmp still denied (catastrophic rule holds)" \
    "LOOM_RM_SCOPE=off" "rm -rf /tmp" "$REPO_ROOT"

# ---- Matrix in the repo (on) state, driven by the env toggle. ----
assert_allow_env "rmScope repo: rm -f /tmp/x/foo.tsv allowed (ephemeral)" \
    "LOOM_RM_SCOPE=repo" "rm -f /tmp/x/foo.tsv" "$REPO_ROOT"
assert_allow_env "rmScope repo: scratchpad path allowed (ephemeral)" \
    "LOOM_RM_SCOPE=repo" "rm -rf /private/tmp/claude-501/-Users-x/abc/scratchpad/z" "$REPO_ROOT"
assert_allow_env "rmScope repo: \$TMPDIR /var/folders path allowed (ephemeral)" \
    "LOOM_RM_SCOPE=repo" "rm -rf /var/folders/ab/cd/T/tmp.123" "$REPO_ROOT"
assert_deny_env "rmScope repo: bare /tmp denied (top-level rule)" \
    "LOOM_RM_SCOPE=repo" "rm -rf /tmp" "$REPO_ROOT"
assert_deny_env "rmScope repo: / denied (catastrophic rule)" \
    "LOOM_RM_SCOPE=repo" "rm -rf /" "$REPO_ROOT"
# The new row: an outside-repo deep path is now DENIED under repo mode.
assert_deny_env "rmScope repo: outside-repo path denied (NEW)" \
    "LOOM_RM_SCOPE=repo" "rm -rf /opt/some-vendor/important" "$REPO_ROOT"
assert_deny_env "rmScope repo: outside-repo /Users path denied (NEW)" \
    "LOOM_RM_SCOPE=repo" "rm -rf /Users/someone/important" "$REPO_ROOT"
assert_allow_env "rmScope repo: under repo root allowed" \
    "LOOM_RM_SCOPE=repo" "rm -rf $REPO_ROOT/.loom/tmp/x" "$REPO_ROOT"
assert_allow_env "rmScope repo: relative subpath under repo allowed" \
    "LOOM_RM_SCOPE=repo" "rm -rf build-artifacts/tmp/x" "$REPO_ROOT"

# ---- #6814: a QUOTED absolute rm target must classify the same as its
# ---- unquoted twin -- quoting alone must never flip DENY into ALLOW.
#
# extract_rm_targets() emits tokens with quote characters preserved verbatim
# (qsplit's contract), so a quoted absolute target ('/opt/evil', "/opt/evil")
# starts with a quote character rather than `/`. Without unquoting the target
# first (mirroring the write-confinement fix, #4926), the `= /*` classification
# test wrongly calls it RELATIVE and cwd-joins it into
# "$REPO_ROOT/'/opt/evil'", which lexically starts with $REPO_ROOT and so
# wrongly passes the IN_SCOPE prefix check -- admitting an out-of-repo rm by
# simply quoting it.
for _q6814 in "'" '"'; do
    assert_deny_env "rmScope repo (#6814): ${_q6814}-quoted out-of-repo absolute rm target denies" \
        "LOOM_RM_SCOPE=repo" "rm -rf ${_q6814}/opt/some-vendor/important${_q6814}" "$REPO_ROOT"
done
unset _q6814

# Control: a quoted IN-repo absolute path still allows -- unquoting changes
# only the absolute/relative classification, never the containment test.
assert_allow_env "rmScope repo (#6814): double-quoted in-repo absolute rm target still allows" \
    "LOOM_RM_SCOPE=repo" "rm -rf \"$REPO_ROOT/.loom/tmp/x\"" "$REPO_ROOT"

# Control: a quoted /tmp path still allows via the ephemeral allowlist.
assert_allow_env "rmScope repo (#6814): single-quoted /tmp path still allows (ephemeral allowlist)" \
    "LOOM_RM_SCOPE=repo" "rm -rf '/tmp/x/foo'" "$REPO_ROOT"

# Edge case: an unbalanced/unterminated quote. strip_target_quoting() reports
# failure and the caller falls back to the raw, quote-preserved token -- i.e.
# today's (pre-#6814) verdict for this exact shape, never a NEW widening. The
# raw token still starts with `'`, not `/`, so it is still misclassified as
# relative and cwd-joined into $REPO_ROOT, which is in scope -- the same
# ALLOW this command already produced before this fix. Per
# strip_target_quoting()'s documented contract, an unbalanced quote may only
# ever keep today's verdict, never widen a deny into an allow (and, by the
# same token, never narrow an existing allow into a deny either).
assert_allow_env "rmScope repo (#6814): unbalanced leading quote in out-of-repo target keeps pre-fix verdict (allow)" \
    "LOOM_RM_SCOPE=repo" "rm -rf '/opt/some-vendor/important" "$REPO_ROOT"

# Prefix-boundary precision: /tmpfoo is NOT admitted by the /tmp/ allowlist
# entry (the trailing slash prevents a name-prefix sibling from slipping in).
assert_deny_env "rmScope repo: /tmpfoo/x denied (not the /tmp/ allowlist prefix)" \
    "LOOM_RM_SCOPE=repo" "rm -rf /tmpfoo/x" "$REPO_ROOT"

# ---- Worktree-root cases (configured external volume + env override). ----
# Configured worktree.root in .loom/config.json admits its subtree. The temp
# repo's basename namespaces the resolved root (mirrors loom_worktree_root()).
RMSCOPE_WT_REPO=$(make_sql_repo '{"guards":{"rmScope":"repo"},"worktree":{"root":"/Volumes/scratch/loom-wt"}}')
RMSCOPE_WT_BN=$(basename "$RMSCOPE_WT_REPO")
assert_allow "rmScope repo: configured external worktree.root subtree allowed" \
    "rm -rf /Volumes/scratch/loom-wt/$RMSCOPE_WT_BN/issue-5/foo" "$RMSCOPE_WT_REPO"
assert_deny "rmScope repo: path outside configured worktree.root still denied" \
    "rm -rf /Volumes/other/loom-wt/$RMSCOPE_WT_BN/issue-5/foo" "$RMSCOPE_WT_REPO"

# LOOM_WORKTREE_ROOT env override wins over config default. Config enables
# rmScope; the single env slot carries the worktree-root override.
RMSCOPE_ENVWT_REPO=$(make_sql_repo '{"guards":{"rmScope":"repo"}}')
RMSCOPE_ENVWT_BN=$(basename "$RMSCOPE_ENVWT_REPO")
assert_allow_env "rmScope repo: LOOM_WORKTREE_ROOT env override admits external worktree" \
    "LOOM_WORKTREE_ROOT=/Volumes/ext/wt" "rm -rf /Volumes/ext/wt/$RMSCOPE_ENVWT_BN/issue-9/x" "$RMSCOPE_ENVWT_REPO"

# ---- Env-overrides-config for the toggle itself. ----
RMSCOPE_ON_REPO=$(make_sql_repo '{"guards":{"rmScope":"repo"}}')
# Config repo + no env → outside-repo denied.
assert_deny "rmScope config-on: outside-repo path denied" \
    "rm -rf /opt/some-vendor/important" "$RMSCOPE_ON_REPO"
# LOOM_RM_SCOPE=off overrides config repo → back to permissive (outside allowed).
assert_allow_env "rmScope: LOOM_RM_SCOPE=off overrides config repo (outside allowed)" \
    "LOOM_RM_SCOPE=off" "rm -rf /opt/some-vendor/important" "$RMSCOPE_ON_REPO"

# ---- Malformed config falls through to REPO (the safe default, #3628). ----
# The jq parse failure is caught by the `|| mode=repo` fallback, so a broken
# config now resolves to repo — outside-repo deep rm is denied, not allowed.
RMSCOPE_BAD_REPO=$(make_sql_repo '{ this is not valid json ')
assert_deny "rmScope malformed-config: outside-repo path denied (falls through to repo)" \
    "rm -rf /opt/some-vendor/important" "$RMSCOPE_BAD_REPO"
# The malformed config must still not trip the ERR trap or weaken other guards.
assert_deny "rmScope malformed-config: bare /tmp still denied" \
    "rm -rf /tmp" "$RMSCOPE_BAD_REPO"

# ---- Repo mode must NOT weaken unrelated guards. ----
assert_deny_env "rmScope repo: force-push to main still blocked" \
    "LOOM_RM_SCOPE=repo" "git push --force origin main" "$REPO_ROOT"
assert_deny_env "rmScope repo: gh repo delete still blocked" \
    "LOOM_RM_SCOPE=repo" "gh repo delete myrepo --yes" "$REPO_ROOT"

# ---- Unresolved-variable fail-closed branch (rjwalters/repo#244, fixing
# ---- #239; issue #5928). A target whose PATH ROOT is an unexpanded shell
# ---- variable cannot be classified against $REPO_ROOT by the string-prefix
# ---- scope check — `$CWD/$target` concatenation would build a literal
# ---- string that lexically starts with $REPO_ROOT regardless of what the
# ---- variable actually expands to at runtime — so it must fail closed
# ---- instead of falling through to that check.
assert_deny_env "rmScope repo: rm -rf \"\$p\" (double-quoted var, whole target) denies" \
    "LOOM_RM_SCOPE=repo" 'rm -rf "$p"' "$REPO_ROOT"
assert_deny_env "rmScope repo: rm -f \"\$TMP\" denies" \
    "LOOM_RM_SCOPE=repo" 'rm -f "$TMP"' "$REPO_ROOT"
assert_deny_env "rmScope repo: sudo rm -f \"\$DROPIN\" denies" \
    "LOOM_RM_SCOPE=repo" 'sudo rm -f "$DROPIN"' "$REPO_ROOT"
assert_deny_env "rmScope repo: rm -rf \$p (bare/unquoted var) denies" \
    "LOOM_RM_SCOPE=repo" 'rm -rf $p' "$REPO_ROOT"
# #239's exact regression shape: the variable is assigned in the SAME
# command to a value outside the repo, then rm'd unexpanded by this guard —
# must still deny (the guard cannot see the assignment, only the literal
# rm argument text).
assert_deny_env "rmScope repo: #239 regression — p=<outside-repo path>; rm -rf \"\$p\" denies" \
    "LOOM_RM_SCOPE=repo" 'p=/opt/vendor/important; rm -rf "$p"' "$REPO_ROOT"
# Deliberately NOT denied: a `$` only in the FINAL path component is a known
# directory with an unresolved filename — out of scope for this branch (the
# existing string-prefix scope check still classifies it correctly).
assert_allow_env "rmScope repo: rm -rf ./build/out-\$STAMP.log (var in final component only) allowed" \
    "LOOM_RM_SCOPE=repo" 'rm -rf ./build-artifacts/out-$STAMP.log' "$REPO_ROOT"
# Deliberately NOT denied: a LITERAL `$` (single-quoted) is a real file named
# `$p` under the repo, not an unresolved variable — quoting must not be
# treated as an expansion.
assert_allow_env "rmScope repo: rm -rf './\$p' (literal filename, single-quoted) allowed" \
    "LOOM_RM_SCOPE=repo" "rm -rf './\$p'" "$REPO_ROOT"
# The opt-out (guards.rmScope=off/permissive) must remain byte-for-byte
# permissive — the new branch lives entirely inside the rm_scope_repo_enabled()
# gate and must not fire when that gate is off.
assert_allow_env "rmScope off: rm -rf \"\$p\" allowed (unresolved-var check does not apply)" \
    "LOOM_RM_SCOPE=off" 'rm -rf "$p"' "$REPO_ROOT"
RMSCOPE_UNRESOLVED_PERM_REPO=$(make_sql_repo '{"guards":{"rmScope":"permissive"}}')
assert_allow "rmScope config-permissive: rm -rf \"\$p\" allowed (unresolved-var check does not apply)" \
    'rm -rf "$p"' "$RMSCOPE_UNRESOLVED_PERM_REPO"
[[ -n "$RMSCOPE_UNRESOLVED_PERM_REPO" && "$RMSCOPE_UNRESOLVED_PERM_REPO" != "/" && -d "$RMSCOPE_UNRESOLVED_PERM_REPO/.loom" ]] && rm -rf "$RMSCOPE_UNRESOLVED_PERM_REPO"

# ---- Same-command mktemp resolution (#6520) — a NARROW escape hatch inside
# ---- the unresolved-var branch above: when the rm target variable is
# ---- assigned earlier in the SAME command via the plain, default-rooted
# ---- `$(mktemp -d)`/`$(mktemp)` form, its value is provably /tmp-or-$TMPDIR
# ---- rooted, so it resolves and allows instead of failing closed.
assert_allow_env "rmScope repo (#6520): tmpdir=\$(mktemp -d) same-command rm -rf \"\$tmpdir\" allows" \
    "LOOM_RM_SCOPE=repo" 'tmpdir=$(mktemp -d) && cd "$tmpdir" && git init -q . ; rm -rf "$tmpdir"' "$REPO_ROOT"
assert_allow_env "rmScope repo (#6520): f=\$(mktemp) same-command rm -f \"\$f\" allows" \
    "LOOM_RM_SCOPE=repo" 'f=$(mktemp) && rm -f "$f"' "$REPO_ROOT"
# Control: the variable is instead assigned from a non-mktemp command
# substitution — still unresolved, must still deny.
assert_deny_env "rmScope repo (#6520): x=\$(cat foo) same-command rm -rf \"\$x\" still denies (non-mktemp)" \
    "LOOM_RM_SCOPE=repo" 'x=$(cat foo.txt) && rm -rf "$x"' "$REPO_ROOT"
# Control: the mktemp-assigned variable is REASSIGNED by a second, non-mktemp
# assignment in the same command — ambiguity must fail closed, not resolve
# through the first (safe) assignment.
assert_deny_env "rmScope repo (#6520): tmpdir reassigned after mktemp still denies (ambiguous)" \
    "LOOM_RM_SCOPE=repo" 'tmpdir=$(mktemp -d) && tmpdir=/opt/vendor/important && rm -rf "$tmpdir"' "$REPO_ROOT"
# Edge case: a custom template/prefix flag on mktemp could point output
# outside the default temp root — excluded from the fast path (fail closed),
# per #6520's own scope note.
assert_deny_env "rmScope repo (#6520): mktemp -d with a custom template excluded from fast path, still denies" \
    "LOOM_RM_SCOPE=repo" 'tmpdir=$(mktemp -d /opt/other/XXXXXX) && rm -rf "$tmpdir"' "$REPO_ROOT"
# The ONE chained exception to the ambiguity rule asserted just above — a
# second assignment of the exact `NAME=$(cd "$NAME" && pwd -P)` /
# `NAME=$(realpath "$NAME")` canonicalization form (#7986) — is covered in its
# own suite alongside the identical write-confinement half:
# tests/hooks/test-guard-destructive-mktemp-canon.sh.

# ---- Same-command LITERAL-path resolution (#6676) — a SIBLING fast path to
# ---- the mktemp one above: a same-command `NAME=<literal absolute path>`
# ---- assignment resolves the rm target instead of denying unconditionally —
# ---- unlike the mktemp form, the resolved literal is still judged by the
# ---- normal repo/worktree/tmp scope check (so it can allow OR deny).
# ---- Exact repro from the issue report.
assert_allow_env "rmScope repo (#6676): FARM=/tmp/nofile-bin-6662; rm -rf \"\$FARM\" allows (literal repro)" \
    "LOOM_RM_SCOPE=repo" 'FARM=/tmp/nofile-bin-6662; rm -rf "$FARM"; mkdir -p "$FARM"' "$REPO_ROOT"
assert_allow_env "rmScope repo (#6676): FARM=/tmp/x; rm -rf \"\$FARM\" allows" \
    "LOOM_RM_SCOPE=repo" 'FARM=/tmp/x; rm -rf "$FARM"' "$REPO_ROOT"
# Double- and single-quoted literal RHS forms must resolve identically
# (record_assign()'s own DQ/SQ-aware one-layer quote stripping, reused here).
assert_allow_env "rmScope repo (#6676): FARM=\"/tmp/x\" (double-quoted RHS) allows" \
    "LOOM_RM_SCOPE=repo" 'FARM="/tmp/x"; rm -rf "$FARM"' "$REPO_ROOT"
assert_allow_env "rmScope repo (#6676): FARM='/tmp/x' (single-quoted RHS) allows" \
    "LOOM_RM_SCOPE=repo" "FARM='/tmp/x'; rm -rf \"\$FARM\"" "$REPO_ROOT"
# A literal absolute path inside the repo itself must also resolve and allow.
assert_allow_env "rmScope repo (#6676): literal RHS resolving inside the repo allows" \
    "LOOM_RM_SCOPE=repo" "FARM=\"$REPO_ROOT/scratch-dir\"; rm -rf \"\$FARM\"" "$REPO_ROOT"
# A literal RHS that resolves OUTSIDE /tmp/the repo must still fail closed via
# the NORMAL scope check (this fix must not blanket-allow arbitrary
# same-command literal assignments) — acceptance criterion #3.
assert_deny_env "rmScope repo (#6676): FARM=/etc/foo; rm -rf \"\$FARM\" still denies (outside scope)" \
    "LOOM_RM_SCOPE=repo" 'FARM=/etc/foo; rm -rf "$FARM"' "$REPO_ROOT"
# A conflicting same-command re-assignment to the same variable name must
# still fail closed (AMBIG), mirroring the mktemp fast path's existing rule —
# acceptance criterion #4.
assert_deny_env "rmScope repo (#6676): FARM reassigned to a second literal still denies (ambiguous)" \
    "LOOM_RM_SCOPE=repo" 'FARM=/tmp/a; FARM=/tmp/b; rm -rf "$FARM"' "$REPO_ROOT"
# A literal-looking RHS assignment followed by a DIFFERENT-shaped reassignment
# (mirrors the existing #6520 mktemp-then-reassign control) — ambiguity must
# still fail closed, not resolve through the first (safe-looking) assignment.
assert_deny_env "rmScope repo (#6676): FARM=/tmp/a then FARM=\$(cat foo) still denies (ambiguous, mixed shapes)" \
    "LOOM_RM_SCOPE=repo" 'FARM=/tmp/a; FARM=$(cat foo.txt); rm -rf "$FARM"' "$REPO_ROOT"
# A RHS that still carries an unresolved expansion (not a pure literal) must
# NOT be trusted by the literal fast path — fails closed, same as before.
assert_deny_env "rmScope repo (#6676): FARM=\"\$OTHER/sub\" (RHS itself unresolved) still denies" \
    "LOOM_RM_SCOPE=repo" 'FARM="$OTHER/sub"; rm -rf "$FARM"' "$REPO_ROOT"
# The existing $(mktemp -d)/$(mktemp) same-command fast path (#6520) must
# continue to work UNCHANGED — this is an additive extension, not a
# replacement. (Regression guard; duplicates the #6520 assertions above with
# an explicit #6676 label so a future refactor that narrows the mktemp path
# is caught here too.)
assert_allow_env "rmScope repo (#6676 regression): mktemp fast path (#6520) still allows unchanged" \
    "LOOM_RM_SCOPE=repo" 'tmpdir=$(mktemp -d) && rm -rf "$tmpdir"' "$REPO_ROOT"

# ---- Same-command literal resolution through a LITERAL SUFFIX (#6805) —
# ---- #6676 above only accepted a BARE `$NAME` rm target, so the two most
# ---- frequent real shapes in .loom/logs/guard-decisions.log —
# ---- `rm -f "$WORKTREE_ABS"/.merge_file_*` and `rm -rf "$WT/.snapshots"` —
# ---- still fell through to the catastrophic-tier `rm-scope-unresolved-var`
# ---- deny even though the variable was assigned a literal, in-scope path in
# ---- the very same command. The resolver now carries the literal suffix
# ---- through (mirroring resolve_var()'s `rest` handling in the sibling
# ---- write-confinement guard, #4881/#6152) and the RESOLVED path is judged
# ---- by the normal scope check — so this is a false-positive refinement,
# ---- not a relaxation.
WT_LITERAL_6805="$REPO_ROOT/.loom/worktrees/pr-6742"
# Exact repro #1 from the issue report (quote closes before the suffix).
assert_allow_env "rmScope repo (#6805): WORKTREE_ABS=<literal>; rm -f \"\$WORKTREE_ABS\"/.merge_file_* allows" \
    "LOOM_RM_SCOPE=repo" \
    "WORKTREE_ABS=\"$WT_LITERAL_6805\"
rm -f \"\$WORKTREE_ABS\"/.merge_file_*
git -C \"\$WORKTREE_ABS\" rebase --skip 2>&1" "$REPO_ROOT"
# Exact repro #2 from the issue report (unquoted RHS, suffix inside the quotes).
assert_allow_env "rmScope repo (#6805): WT=<literal>; rm -rf \"\$WT/.snapshots\" allows" \
    "LOOM_RM_SCOPE=repo" \
    "WT=$REPO_ROOT/.loom/worktrees/issue-6334; rm -rf \"\$WT/.snapshots\"" "$REPO_ROOT"
# Same shape, /tmp-rooted literal (ephemeral allowlist).
assert_allow_env "rmScope repo (#6805): WT=/tmp/x; rm -rf \"\$WT/sub\" allows" \
    "LOOM_RM_SCOPE=repo" 'WT=/tmp/x; rm -rf "$WT/sub"' "$REPO_ROOT"
# Braced reference with a suffix must resolve identically to the bare form.
assert_allow_env "rmScope repo (#6805): \${WT}/sub braced reference with suffix allows" \
    "LOOM_RM_SCOPE=repo" 'WT=/tmp/x; rm -rf "${WT}/sub"' "$REPO_ROOT"
# Unquoted target with a suffix normalizes to the same shape (quoting must not
# change the verdict in either direction).
assert_allow_env "rmScope repo (#6805): unquoted \$WT/sub target allows" \
    "LOOM_RM_SCOPE=repo" 'WT=/tmp/x; rm -rf $WT/sub' "$REPO_ROOT"
# NOT A RELAXATION — the resolved path is still scope-checked: an out-of-repo
# literal with the identical suffix shape must STILL deny.
assert_deny_env "rmScope repo (#6805): WT=/etc/foo; rm -rf \"\$WT/.snapshots\" still denies (outside scope)" \
    "LOOM_RM_SCOPE=repo" 'WT=/etc/foo; rm -rf "$WT/.snapshots"' "$REPO_ROOT"
# ... and the catastrophic top-level deny still fires on the RESOLVED path, so
# a suffix cannot be used to launder a system-directory target.
assert_deny_env "rmScope repo (#6805): WT=/; rm -rf \"\$WT/usr\" still denies (resolves to a top-level dir)" \
    "LOOM_RM_SCOPE=repo" 'WT=/; rm -rf "$WT/usr"' "$REPO_ROOT"
# A `..` inside the suffix is collapsed by normalize_abs_path() BEFORE the
# scope check, so it cannot climb out of the resolved literal unnoticed.
assert_deny_env "rmScope repo (#6805): suffix with ../ escaping the repo still denies" \
    "LOOM_RM_SCOPE=repo" "WT=$REPO_ROOT/.loom/worktrees/issue-1; rm -rf \"\$WT/../../../../../../etc/foo\"" "$REPO_ROOT"
# A suffix that itself carries a SECOND unresolved expansion is not a literal —
# fail closed.
assert_deny_env "rmScope repo (#6805): suffix containing another unresolved var still denies" \
    "LOOM_RM_SCOPE=repo" 'WT=/tmp/x; rm -rf "$WT/$SUB"' "$REPO_ROOT"
# A suffix that does not begin at a path boundary names a SIBLING of the
# resolved path — excluded from the fast path rather than guessed.
assert_deny_env "rmScope repo (#6805): non-path-boundary suffix (\"\$WT\".bak) still denies" \
    "LOOM_RM_SCOPE=repo" 'WT=/tmp/x; rm -rf "$WT".bak' "$REPO_ROOT"
# The ambiguity rule inherited from #6520/#6676 still applies with a suffix.
assert_deny_env "rmScope repo (#6805): conflicting reassignment with a suffix still denies (ambiguous)" \
    "LOOM_RM_SCOPE=repo" 'WT=/tmp/a; WT=/tmp/b; rm -rf "$WT/sub"' "$REPO_ROOT"
# An UNASSIGNED variable with a literal suffix is still completely unresolvable
# — the pre-#6805 fail-closed deny is untouched.
assert_deny_env "rmScope repo (#6805): unassigned \$WT with a suffix still denies (no same-command assignment)" \
    "LOOM_RM_SCOPE=repo" 'rm -rf "$WT/.snapshots"' "$REPO_ROOT"
# A SINGLE-quoted `$` is literal data (a file genuinely named `$WT`), not an
# expansion — mark_expandable_dollars() must not route it into the resolver,
# so it keeps behaving exactly like the existing `rm -rf './$p'` case above
# (an ordinary in-repo relative path) even though a same-command assignment to
# that very name exists.
assert_allow_env "rmScope repo (#6805): single-quoted literal '\$WT/sub' is not resolved as a variable" \
    "LOOM_RM_SCOPE=repo" "WT=/etc/foo; rm -rf './\$WT/sub'" "$REPO_ROOT"

# ---- Decoy-heredoc mktemp-escape-hatch bypass (#6549) — rm_scope_mktemp_same_
# ---- command_safe() used to scan the raw (heredoc-unmasked) command text one
# ---- physical line at a time, so a NEVER-EXECUTED `NAME=$(mktemp -d)` line
# ---- planted inside an inert heredoc body satisfied its `total==1 && safe==1`
# ---- same-command-safe check exactly as well as a live top-level assignment.
# ---- Combined with setting the REAL runtime value via a shape that does not
# ---- match the function's exact `NAME=` prefix scan (e.g. `export NAME=...`),
# ---- this let an attacker's genuinely unresolved `rm -rf "$NAME"` slip past
# ---- the guard as an ALLOW. This is this issue's own reproduction, verbatim.
DECOY_HEREDOC_BYPASS_CMD=$(cat <<'TESTCMD_EOF'
export tmpdir=$(malicious_setter); rm -rf "$tmpdir"
cat <<'EOF' > /tmp/notes.txt
tmpdir=$(mktemp -d)
EOF
TESTCMD_EOF
)
assert_deny_env "rmScope repo (#6549): decoy NAME=\$(mktemp -d) inside a quoted-delimiter heredoc body does not launder a real export-set unresolved var" \
    "LOOM_RM_SCOPE=repo" "$DECOY_HEREDOC_BYPASS_CMD" "$REPO_ROOT"

# Same bypass shape, but the decoy heredoc uses an UNQUOTED delimiter
# (`<<EOF`, not `<<'EOF'`) — must still deny. rm_scope_mktemp_same_command_
# safe()'s heredoc-body masking is unconditional (unlike COMMAND_ASK_SCAN's
# masking elsewhere in this file, it does not leave an unquoted-delimiter body
# visible), since no heredoc body of any shape can ever be a live top-level
# assignment in the current shell.
DECOY_HEREDOC_BYPASS_UNQUOTED_CMD=$(cat <<'TESTCMD_EOF'
export tmpdir=$(malicious_setter); rm -rf "$tmpdir"
cat <<EOF > /tmp/notes.txt
tmpdir=$(mktemp -d)
EOF
TESTCMD_EOF
)
assert_deny_env "rmScope repo (#6549): decoy NAME=\$(mktemp -d) inside an UNQUOTED-delimiter heredoc body still denies" \
    "LOOM_RM_SCOPE=repo" "$DECOY_HEREDOC_BYPASS_UNQUOTED_CMD" "$REPO_ROOT"

# Two heredocs in the same command, one of which carries the decoy — must
# still deny (masking is not confined to "the first heredoc only").
DECOY_HEREDOC_BYPASS_MULTI_CMD=$(cat <<'TESTCMD_EOF'
export tmpdir=$(malicious_setter); rm -rf "$tmpdir"
cat <<'NOTES' > /tmp/notes.txt
just some unrelated notes
NOTES
cat <<'EOF' > /tmp/decoy.txt
tmpdir=$(mktemp -d)
EOF
TESTCMD_EOF
)
assert_deny_env "rmScope repo (#6549): decoy assignment in the SECOND of two heredocs in the same command still denies" \
    "LOOM_RM_SCOPE=repo" "$DECOY_HEREDOC_BYPASS_MULTI_CMD" "$REPO_ROOT"

# Narrowing check: a heredoc body sitting near a LEGITIMATE same-command
# mktemp assignment must not accidentally hide it — masking heredoc bodies is
# only ever supposed to remove FALSE assignment matches, never the real one.
# The heredoc here decoys a DIFFERENT variable name than the one referenced by
# the rm target, and the real `tmpdir=$(mktemp -d)` assignment is live,
# top-level code outside any heredoc — must still allow.
REAL_ASSIGN_NEAR_HEREDOC_CMD=$(cat <<'TESTCMD_EOF'
tmpdir=$(mktemp -d) && rm -rf "$tmpdir"
cat <<'EOF' > /tmp/notes.txt
other=$(mktemp -d)
EOF
TESTCMD_EOF
)
assert_allow_env "rmScope repo (#6549): real top-level tmpdir=\$(mktemp -d) still allows despite a nearby heredoc decoying a DIFFERENT variable" \
    "LOOM_RM_SCOPE=repo" "$REAL_ASSIGN_NEAR_HEREDOC_CMD" "$REPO_ROOT"

# Clean up rm-scope temp repos.
for _rmscope_dir in "$RMSCOPE_OFF_REPO" "$RMSCOPE_WT_REPO" "$RMSCOPE_ENVWT_REPO" "$RMSCOPE_ON_REPO" "$RMSCOPE_BAD_REPO"; do
    [[ -n "$_rmscope_dir" && "$_rmscope_dir" != "/" && -d "$_rmscope_dir/.loom" ]] && rm -rf "$_rmscope_dir"
done

echo ""

# =========================================================================
echo -e "${YELLOW}--- #6519: rm-scope heredoc-in-substitution / write-to-file-then-reference masking gap ---${NC}"
# =========================================================================
#
# #5216 closed the false-positive where a `<flag> "$(cat <<'EOF' … EOF)"`
# command-substitution value quotes a dangerous rm example as inert prose, by
# scanning COMMAND_NO_LITERAL_TEXT (strip_literal_text()'s mask_flag_cat_heredocs()
# narrowing) for extract_rm_targets(). That fix is shape-specific: it only
# recognizes a heredoc DIRECTLY wrapped in `$(cat <<'DELIM' … )` immediately
# after a text-carrying flag. A SIBLING shape -- writing the same inert prose
# to a file with a plain `cat <<'DELIM' > file` heredoc, then referencing that
# file LATER (`--body-file file`, or any other non-substitution consumer) --
# was never covered, because no flag ever sits directly before the heredoc
# opener. Since extract_rm_targets() segments the raw command one PHYSICAL
# LINE at a time, a heredoc body line whose own first word happens to be `rm`
# (e.g. a standalone "rm -rf /opt/vendor/important" example line in
# acceptance-criteria prose) still manufactured a phantom local `rm` segment
# and hard-denied a write that deletes nothing (#6519, reproduced against
# rjwalters/anvil#1073's shape). Fixed by switching extract_rm_targets() to
# scan COMMAND_ASK_SCAN (comment-stripped AND heredoc-body-masked via
# mask_heredoc_bodies_selective()/mask_unquoted_cat_heredoc_bodies(), gated
# only on heredoc/flag PRESENCE) -- the same working copy
# parse_force_ops()/lifecycle_or_cloud_reason() already use for the identical
# failure family.
RMSCOPE_6519_REPO=$(make_sql_repo '{"guards":{"rmScope":"repo"}}')

# ---- ALLOW: previously-gapped shape (write-to-file-then-reference, heredoc
# ---- body line STARTS with the rm example). ----
assert_allow "#6519: write-to-file-then-reference heredoc, standalone 'rm -rf <outside-repo>' example line allowed" \
    'cat <<'"'"'HEREDOC'"'"' > /tmp/curator_1060_comment.md
Example of what NOT to run:
rm -rf /opt/vendor/important
HEREDOC
gh issue comment 1073 -R rjwalters/anvil --body-file /tmp/curator_1060_comment.md' \
    "$RMSCOPE_6519_REPO"

# ---- ALLOW: same write-to-file-then-reference shape, rm mention inline in a
# ---- sentence (already covered pre-#6519, kept as a non-regression case). ----
assert_allow "#6519: write-to-file-then-reference heredoc, inline-in-sentence rm mention allowed" \
    'cat <<'"'"'HEREDOC'"'"' > /tmp/curator_1060_comment.md
Acceptance criteria: never run `rm -rf /opt/vendor/important` on this repo.
HEREDOC
gh issue comment 1073 -R rjwalters/anvil --body-file /tmp/curator_1060_comment.md' \
    "$RMSCOPE_6519_REPO"

# ---- ALLOW: direct $(cat <<'EOF' … EOF) substitution, standalone example
# ---- line (the #5216 shape, re-verified after the switch to COMMAND_ASK_SCAN). ----
assert_allow "#6519: direct \$(cat<<'EOF'...) substitution, standalone rm -rf example line allowed" \
    'gh issue comment 1073 -R rjwalters/anvil --body "$(cat <<'"'"'INNEREOF'"'"'
Example of what NOT to run:
rm -rf /opt/vendor/important
INNEREOF
)"' \
    "$RMSCOPE_6519_REPO"

# ---- DENY (anti-smuggling floor, narrows never widens): a REAL rm CHAINED
# ---- after the heredoc closes must still deny. ----
assert_deny "#6519 regression: a real rm chained after an unrelated heredoc closes still denied" \
    'cat <<'"'"'HEREDOC'"'"' > /tmp/x.md
Just an inert note, nothing dangerous here.
HEREDOC
rm -rf /opt/vendor/important' \
    "$RMSCOPE_6519_REPO"

# ---- DENY: a REAL rm INSIDE an interpreter-fed heredoc (`bash <<EOF … EOF`)
# ---- must still deny -- mask_heredoc_bodies_selective() never masks
# ---- interpreter-fed bodies. ----
assert_deny "#6519 regression: a real rm inside an interpreter-fed heredoc (bash <<EOF) still denied" \
    'bash <<'"'"'EOF'"'"'
rm -rf /opt/vendor/important
EOF' \
    "$RMSCOPE_6519_REPO"

# ---- DENY: a genuinely out-of-scope BARE rm (no heredoc at all) must still
# ---- deny -- confirms rm-scope-outside-repo itself is unaffected. ----
assert_deny "#6519 regression: a genuine bare out-of-repo rm still denied" \
    "rm -rf /opt/vendor/important" "$RMSCOPE_6519_REPO"

# ---- DENY: a genuine bare top-level-path rm (rm-protected-path, unconditional
# ---- floor) must still deny even outside any rmScope opt-in. ----
assert_deny "#6519 regression: a genuine bare rm -rf /tmp still denied (rm-protected-path, unconditional)" \
    "rm -rf /tmp" "$REPO_ROOT"

[[ -n "$RMSCOPE_6519_REPO" && "$RMSCOPE_6519_REPO" != "/" && -d "$RMSCOPE_6519_REPO/.loom" ]] && rm -rf "$RMSCOPE_6519_REPO"

echo ""

# =========================================================================

print_summary
