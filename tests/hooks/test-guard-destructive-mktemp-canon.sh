#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — the
# SELF-REFERENTIAL CANONICALIZATION CHAIN admitted by the two same-command
# mktemp fast paths (#7986).
#
# Covers BOTH siblings in one place, because they share one mechanism
# (_mktemp_canon_mask() plus an identical two-assignment END rule) and must
# never drift apart on the shape they admit:
#   - rm_scope_mktemp_same_command_safe()   (#6520, rm-scope-unresolved-var)
#   - wt_write_mktemp_same_command_safe()   (#6949, worktree-write-confinement-
#                                            unresolved-var)
#
# WHY A SEPARATE SUITE rather than extending the two existing ones: the
# write-confinement suite is at its recorded size in
# scripts/file-size-baseline.txt (frozen — it may shrink, not grow, see
# .loom/docs/file-size-policy.md), so the ratchet's own preferred remedy is a
# new sibling file. Keeping the rm-scope half here too means the whole
# mechanism is asserted in one readable place instead of split across three.
#
# Usage: ./tests/hooks/test-guard-destructive-mktemp-canon.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

# The opaque token _mktemp_canon_mask() masks the canonicalization RHS into.
# Reproduced here ONLY to assert the fail-closed refusal when a command
# already carries those bytes — no test may rely on it for anything else.
CANON_TOK=$(printf '\001LOOM_MKTEMP_CANON\001')

echo -e "${YELLOW}--- Same-command mktemp CANONICALIZATION CHAIN (#7986) ---${NC}"
# =========================================================================
#
# `NAME=$(mktemp -d)` followed by `NAME=$(realpath "$NAME")` (the routine way
# to resolve a symlinked temp root, e.g. macOS /tmp -> /private/tmp) used to
# trip the "two or more assignments poison the resolution" ambiguity rule
# both fast paths inherit from #6520, producing a catastrophic-tier false
# deny on a provably-safe scratch sandbox. The second assignment cannot escape
# the directory the first one already proved mktemp-safe: `realpath` either
# fails (the substitution captures nothing, NAME becomes empty) or succeeds
# and prints that same directory's canonical path.
#
# ONLY `$(realpath "$NAME")` is admitted. A `$(cd "$NAME" && pwd -P)` RHS is
# deliberately NOT admitted (PR #8016 review, after this suite originally
# allowed it too): `cd ""` is a documented no-op SUCCESS on bash 3.2 (macOS
# stock `/bin/bash`), zsh, and `/bin/sh` -- so when `mktemp` fails and NAME is
# empty, a `;`/newline-joined `NAME=$(cd "$NAME" && pwd -P)` still runs and
# prints the CALLER's cwd rather than staying empty, turning a benign
# `mktemp`-failure no-op into `rm -rf <cwd>` / a write into <cwd>. `realpath`
# has no such special case -- it is a plain external command that fails (and
# prints nothing) on an empty path, on every join style.
#
# This is an EXACT-STRING admission of ONE chained shape, not a relaxation of
# the ambiguity rule: every "looks similar but isn't" variation below must
# still fail closed.

# ---- rm-scope half: newly ALLOWED shape --------------------------------
assert_allow_env "rm-scope (#7986): \$(realpath \"\$VAR\") canonicalization form allows (issue repro)" \
    "LOOM_RM_SCOPE=repo" 'TMPROOT=$(mktemp -d); TMPROOT=$(realpath "$TMPROOT"); rm -rf "$TMPROOT"' "$REPO_ROOT"
assert_allow_env "rm-scope (#7986): same chain joined with && allows" \
    "LOOM_RM_SCOPE=repo" 'TMPROOT=$(mktemp -d) && TMPROOT=$(realpath "$TMPROOT") && rm -rf "$TMPROOT"' "$REPO_ROOT"
assert_allow_env "rm-scope (#7986): both assignments double-quoted allows" \
    "LOOM_RM_SCOPE=repo" 'TMPROOT="$(mktemp -d)"; TMPROOT="$(realpath "$TMPROOT")"; rm -rf "$TMPROOT"' "$REPO_ROOT"
assert_allow_env "rm-scope (#7986): bare mktemp (file, no -d) then canonicalization allows" \
    "LOOM_RM_SCOPE=repo" 'F=$(mktemp); F=$(realpath "$F"); rm -f "$F"' "$REPO_ROOT"
assert_allow_env "rm-scope (#7986): multi-line chain (newline-separated) allows" \
    "LOOM_RM_SCOPE=repo" 'TMPROOT=$(mktemp -d)
TMPROOT=$(realpath "$TMPROOT")
rm -rf "$TMPROOT"' "$REPO_ROOT"

# ---- rm-scope half: the cd/pwd -P form is NOT admitted (mktemp-failure hazard) ----
# The `cd`/`pwd -P` idiom looks identical in shape to the admitted realpath
# form but is denied on every join style: on `mktemp` failure it resolves to
# the caller's cwd instead of staying empty (see the suite header above).
assert_deny_env "rm-scope (#7986): \$(cd \"\$VAR\" && pwd -P) is NOT admitted, denies (';'-joined)" \
    "LOOM_RM_SCOPE=repo" 'TMPROOT=$(mktemp -d); TMPROOT=$(cd "$TMPROOT" && pwd -P); rm -rf "$TMPROOT"' "$REPO_ROOT"
assert_deny_env "rm-scope (#7986): \$(cd \"\$VAR\" && pwd -P) is NOT admitted, denies (&&-joined)" \
    "LOOM_RM_SCOPE=repo" 'TMPROOT=$(mktemp -d) && TMPROOT=$(cd "$TMPROOT" && pwd -P) && rm -rf "$TMPROOT"' "$REPO_ROOT"
assert_deny_env "rm-scope (#7986): \$(cd \"\$VAR\" && pwd -P) is NOT admitted, denies (newline-joined)" \
    "LOOM_RM_SCOPE=repo" 'TMPROOT=$(mktemp -d)
TMPROOT=$(cd "$TMPROOT" && pwd -P)
rm -rf "$TMPROOT"' "$REPO_ROOT"

# ---- rm-scope half: every other shape still fails closed ---------------
# A canonicalization naming a DIFFERENT variable proves nothing about this
# one -- the mask is built from the target variable's OWN name.
assert_deny_env "rm-scope (#7986): canonicalizing a DIFFERENT variable still denies" \
    "LOOM_RM_SCOPE=repo" 'TMPROOT=$(mktemp -d); TMPROOT=$(realpath "$OTHER"); rm -rf "$TMPROOT"' "$REPO_ROOT"
# A THIRD assignment re-poisons the resolution, exactly as before #7986.
assert_deny_env "rm-scope (#7986): a third assignment after the safe chain still denies" \
    "LOOM_RM_SCOPE=repo" 'TMPROOT=$(mktemp -d); TMPROOT=$(realpath "$TMPROOT"); TMPROOT=/opt/vendor/important; rm -rf "$TMPROOT"' "$REPO_ROOT"
# Order matters: only mktemp-then-canonicalize is admitted. The reverse
# canonicalizes an AMBIENT value of unknown provenance.
assert_deny_env "rm-scope (#7986): reversed order (canonicalize then mktemp) still denies" \
    "LOOM_RM_SCOPE=repo" 'TMPROOT=$(realpath "$TMPROOT"); TMPROOT=$(mktemp -d); rm -rf "$TMPROOT"' "$REPO_ROOT"
# A RHS that merely CONTAINS the admitted form is not the admitted form.
assert_deny_env "rm-scope (#7986): canonicalization with a literal prefix on the RHS still denies" \
    "LOOM_RM_SCOPE=repo" 'TMPROOT=$(mktemp -d); TMPROOT=/etc$(realpath "$TMPROOT"); rm -rf "$TMPROOT"' "$REPO_ROOT"
assert_deny_env "rm-scope (#7986): canonicalization with a literal suffix on the RHS still denies" \
    "LOOM_RM_SCOPE=repo" 'TMPROOT=$(mktemp -d); TMPROOT=$(realpath "$TMPROOT")/sub; rm -rf "$TMPROOT"' "$REPO_ROOT"
# The brace form is deliberately NOT admitted (narrowness over convenience).
assert_deny_env "rm-scope (#7986): \${VAR} brace form inside realpath is not admitted, still denies" \
    "LOOM_RM_SCOPE=repo" 'TMPROOT=$(mktemp -d); TMPROOT=$(realpath "${TMPROOT}"); rm -rf "$TMPROOT"' "$REPO_ROOT"
# The chain cannot rescue a custom-template mktemp: the FIRST assignment must
# still match the exact #6520 shape.
assert_deny_env "rm-scope (#7986): custom-template mktemp + canonicalization still denies" \
    "LOOM_RM_SCOPE=repo" 'TMPROOT=$(mktemp -d /opt/other/XXXXXX); TMPROOT=$(realpath "$TMPROOT"); rm -rf "$TMPROOT"' "$REPO_ROOT"
# A proven chain lends nothing to a DIFFERENT, still-unresolvable rm target.
assert_deny_env "rm-scope (#7986): an unrelated rm target is unaffected by a proven chain" \
    "LOOM_RM_SCOPE=repo" 'TMPROOT=$(mktemp -d); TMPROOT=$(realpath "$TMPROOT"); rm -rf "$SOMETHINGELSE7986"' "$REPO_ROOT"
# Hand-planted mask-token bytes must never impersonate a proven
# canonicalization -- the mask refuses outright (fail closed).
assert_deny_env "rm-scope (#7986): hand-planted mask-token bytes as the RHS still deny" \
    "LOOM_RM_SCOPE=repo" "TMPROOT=\$(mktemp -d); TMPROOT=${CANON_TOK}; rm -rf \"\$TMPROOT\"" "$REPO_ROOT"
# Heredoc bodies are masked before either fast path runs (#6549), so a decoy
# chain inside one cannot launder a real, differently-set variable.
assert_deny_env "rm-scope (#7986/#6549): a decoy chain inside a heredoc body still denies" \
    "LOOM_RM_SCOPE=repo" 'export TMPROOT=$(cat /tmp/attacker-controlled.txt)
rm -rf "$TMPROOT"
cat <<'"'"'EOF'"'"'
TMPROOT=$(mktemp -d)
TMPROOT=$(realpath "$TMPROOT")
EOF' "$REPO_ROOT"

# ---- rm-scope half: #6520 regression guards (unchanged behaviour) -------
assert_allow_env "rm-scope (#7986 regression): plain single-assignment mktemp fast path still allows" \
    "LOOM_RM_SCOPE=repo" 'tmpdir=$(mktemp -d) && rm -rf "$tmpdir"' "$REPO_ROOT"
assert_deny_env "rm-scope (#7986 regression): mktemp then arbitrary reassignment still denies" \
    "LOOM_RM_SCOPE=repo" 'tmpdir=$(mktemp -d) && tmpdir=/opt/vendor/important && rm -rf "$tmpdir"' "$REPO_ROOT"
assert_deny_env "rm-scope (#7986 regression): a bare unresolved rm target still denies" \
    "LOOM_RM_SCOPE=repo" 'rm -rf "$p"' "$REPO_ROOT"

# ---- write-confinement half --------------------------------------------
WT_REPO=$(make_wt_repo)

# Newly ALLOWED: the sibling shape from the issue (a scratch sandbox created,
# canonicalized, written into, then torn down). Only the realpath form is
# admitted -- see the suite header above for why the cd/pwd -P form is not.
assert_allow "write-confinement (#7986): mkdir -p under a canonicalized mktemp dir -> allow (issue repro)" \
    'TMPROOT=$(mktemp -d); TMPROOT=$(realpath "$TMPROOT"); mkdir -p "$TMPROOT/sub"; rm -rf "$TMPROOT"' "$WT_REPO"
assert_allow "write-confinement (#7986): > redirect under a canonicalized mktemp dir -> allow" \
    'tmp=$(mktemp -d); tmp=$(realpath "$tmp"); echo x > "$tmp/out.txt"' "$WT_REPO"
assert_allow "write-confinement (#7986): tee target under a canonicalized mktemp dir -> allow" \
    'tmp=$(mktemp -d); tmp=$(realpath "$tmp"); echo x | tee "$tmp/out.txt"' "$WT_REPO"
assert_allow "write-confinement (#7986): cp destination under a canonicalized mktemp dir -> allow" \
    'tmp="$(mktemp -d)"; tmp="$(realpath "$tmp")"; cp /tmp/a.sh "$tmp/out.sh"' "$WT_REPO"
assert_allow "write-confinement (#7986): bare \$VAR target (no suffix) after the chain -> allow" \
    'TMPFILE=$(mktemp); TMPFILE=$(realpath "$TMPFILE"); echo hi > "$TMPFILE"' "$WT_REPO"

# The cd/pwd -P form is NOT admitted (mktemp-failure hazard, see suite header).
assert_deny "write-confinement (#7986): \$(cd \"\$VAR\" && pwd -P) is NOT admitted, denies" \
    'tmp=$(mktemp -d); tmp=$(cd "$tmp" && pwd -P); echo x > "$tmp/out.txt"' "$WT_REPO"

# Still fails closed -- the identical matrix as the rm-scope half.
assert_deny "write-confinement (#7986): canonicalizing a DIFFERENT variable -> denies" \
    'tmp=$(mktemp -d); tmp=$(realpath "$other"); echo x > "$tmp/out.txt"' "$WT_REPO"
assert_deny "write-confinement (#7986): a third assignment after the safe chain -> denies" \
    'tmp=$(mktemp -d); tmp=$(realpath "$tmp"); tmp=/some/other/path; echo x > "$tmp/out.txt"' "$WT_REPO"
assert_deny "write-confinement (#7986): reversed order (canonicalize then mktemp) -> denies" \
    'tmp=$(realpath "$tmp"); tmp=$(mktemp -d); echo x > "$tmp/out.txt"' "$WT_REPO"
assert_deny "write-confinement (#7986): custom-template mktemp + canonicalization -> denies" \
    'tmp=$(mktemp -d --tmpdir=/other/dir); tmp=$(realpath "$tmp"); echo x > "$tmp/out.txt"' "$WT_REPO"
# The suffix rules the write-confinement sibling adds on top (#6949) are
# untouched by #7986: a `..` traversal out of the (unknown) scratch dir still
# fails closed even on a proven chain.
assert_deny "write-confinement (#7986): '..' traversal in the suffix after a proven chain -> denies" \
    'tmp=$(mktemp -d); tmp=$(realpath "$tmp"); cp /tmp/a.sh "$tmp/../../evil.sh"' "$WT_REPO"
assert_deny "write-confinement (#7986): an unrelated unresolved \$VAR write is unaffected by a proven chain -> denies" \
    'tmp=$(mktemp -d); tmp=$(realpath "$tmp"); echo pwned > "$OTHERVAR7986/evil.sh"' "$WT_REPO"
assert_deny "write-confinement (#7986): a write resolving INSIDE the main checkout still denies" \
    "tmp=\$(mktemp -d); tmp=\$(realpath \"\$tmp\"); echo pwned > $WT_REPO/defaults/hooks/evil.sh" "$WT_REPO"
assert_deny "write-confinement (#7986): hand-planted mask-token bytes as the RHS -> denies" \
    "tmp=\$(mktemp -d); tmp=${CANON_TOK}; echo x > \"\$tmp/out.txt\"" "$WT_REPO"

# #6949 regression guards (unchanged behaviour).
assert_allow "write-confinement (#7986 regression): plain single-assignment mktemp fast path still allows" \
    'tmp=$(mktemp -d); echo x > "$tmp/out.txt"' "$WT_REPO"
assert_deny "write-confinement (#7986 regression): mktemp then arbitrary reassignment still denies" \
    'tmp=$(mktemp -d); tmp=/some/other/path; echo x > "$tmp/out.txt"' "$WT_REPO"

# ---- #8221: the write-confinement sibling of the rm-scope fix above -----
# New test cases land HERE rather than in test-guard-destructive-write-
# confinement.sh (frozen at its scripts/file-size-baseline.txt size, same
# reason #7986's own cases live in this sibling file -- see the suite header).
#
# The same ambiguity-counting gap: a same-command rebind of the mktemp-
# captured variable via a declaration keyword or an `=`-free mechanism
# (read/printf -v/for-in) was invisible to the awk scan's `varname "="`
# prefix test, so the second, dangerous rebind never poisoned the count and
# the fast path incorrectly allowed the write. Exact issue repro plus one
# case per other rebind shape.
assert_deny "write-confinement (#8221): tmp=\$(mktemp -d); export tmp=<outside path> still denies (export rebind, issue repro)" \
    'tmp=$(mktemp -d); export tmp=/Users/someone/important; echo x > "$tmp/f"' "$WT_REPO"
assert_deny "write-confinement (#8221): tmp=\$(mktemp -d); declare tmp=/ still denies (declare rebind)" \
    'tmp=$(mktemp -d); declare tmp=/; echo x > "$tmp/f"' "$WT_REPO"
assert_deny "write-confinement (#8221): tmp=\$(mktemp -d); printf -v tmp \"%s\" / still denies (printf -v rebind)" \
    'tmp=$(mktemp -d); printf -v tmp "%s" /; echo x > "$tmp/f"' "$WT_REPO"
assert_deny "write-confinement (#8221): tmp=\$(mktemp -d); read tmp < /etc/passwd still denies (read rebind)" \
    'tmp=$(mktemp -d); read tmp < /etc/passwd; echo x > "$tmp/f"' "$WT_REPO"
assert_deny "write-confinement (#8221): tmp=\$(mktemp -d); for tmp in / still denies (for-in rebind)" \
    'tmp=$(mktemp -d); for tmp in /; do :; done; echo x > "$tmp/f"' "$WT_REPO"
# Control (regression check): a genuine same-command mktemp assignment with
# no other rebind must keep allowing (duplicates the #7986-regression case
# above with an explicit #8221 label so a future change to the rebind
# detector cannot silently narrow the historic #6949 fast path).
assert_allow "write-confinement (#8221 regression): plain single-assignment mktemp fast path still allows" \
    'tmp=$(mktemp -d); echo x > "$tmp/f"' "$WT_REPO"

[[ -n "$WT_REPO" && "$WT_REPO" != "/" && -d "$WT_REPO/.loom" ]] && rm -rf "$WT_REPO"

echo ""

# =========================================================================

print_summary
