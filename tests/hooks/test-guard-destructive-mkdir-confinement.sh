#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — mkdir
# write-idiom recognition under worktree-write-confinement (#7945).
#
# Ported from `2AMLogic/sky130-modexp` commit `fdced41` (PR #123, closes
# #121) and `92415b2` (PR #116, closes #115), adapted to the shared
# tests/hooks/lib/guard-destructive-harness.sh fixtures/assertions used by
# every sibling test-guard-destructive-*.sh suite here, rather than
# sky130-modexp's standalone copy-the-hook TMPROOT harness.
#
# Background: extract_write_targets() (guard-destructive-generic.sh)
# recognizes `>`/`>>` redirection, `tee`, `sed -i`, and `cp`/`mv` as write
# idioms subject to the worktree-write-confinement check (#4178), but never
# recognized `mkdir` (with or without `-p`, or other flags like `-m`) at all.
# A command like `mkdir -p "../../../pwned-dir"` run from inside a
# Loom-managed worktree therefore silently ALLOWed directory creation outside
# the worktree into the main repo checkout, with zero ask/deny/telemetry -- a
# full confinement bypass for this one write idiom even though the
# equivalent cp/mv/>/tee/sed -i idioms were correctly confined and denied.
#
# This suite asserts:
#   (a) a literal `mkdir -p` escape into the main checkout is denied
#   (b) an in-worktree `mkdir -p` is allowed
#   (c) a same-command-resolvable variable-based `mkdir -p` target INSIDE the
#       worktree is allowed (same-command $VAR resolution, #4881, applies to
#       mkdir targets exactly like it already does for cp/tee/sed -i/>)
#   (d) a same-command-resolvable variable-based `mkdir -p` target OUTSIDE
#       the worktree (still inside the main checkout) is denied
#   (e) EVERY argument of a multi-directory `mkdir -p dir1 dir2` invocation
#       is checked, not just the first -- a safe first argument must not
#       mask an escaping second argument
#   (f)/(g) common safe `mkdir -p` idioms already used by legitimate Builder
#       workflows (bare relative dir, `-m MODE` flag, chained idiom) do not
#       regress into a false-positive deny
#
# Usage: ./tests/hooks/test-guard-destructive-mkdir-confinement.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

echo -e "${YELLOW}--- mkdir write-idiom confinement (guards.worktreeIsolation, #7945) ---${NC}"
# =========================================================================
#
# Fixture: a hermetic throwaway git repo (its own MAIN_ROOT) with a managed
# worktree at <repo>/.loom/worktrees/issue-1 (the `.loom-managed` sentinel
# worktree.sh writes at every worktree root). CWD is the repo root for
# absolute-path assertions and the worktree dir for relative-path ones --
# mirroring the convention already used by test-guard-destructive-write-
# confinement.sh's WT_REPO/WT_DIR fixture (writing an absolute in-worktree
# path is allowed regardless of which of the two is the acting CWD).

WT_REPO=$(make_wt_repo)
WT_DIR="$WT_REPO/.loom/worktrees/issue-1"

# --- (a) literal mkdir -p escape into the main checkout -> deny ------------
assert_deny "mkdir-confinement (a): literal 'mkdir -p ../../../pwned-dir' escaping worktree into main checkout -> deny" \
    "mkdir -p \"../../../pwned-dir\"" "$WT_DIR"
assert_deny "mkdir-confinement (a2): literal absolute 'mkdir -p <main-checkout>/pwned-dir' -> deny" \
    "mkdir -p \"$WT_REPO/pwned-dir\"" "$WT_DIR"

# --- (b) in-worktree mkdir -p -> allow --------------------------------------
assert_allow "mkdir-confinement (b): 'mkdir -p sub/newdir' relative, resolves inside worktree -> allow" \
    "mkdir -p \"sub/newdir\"" "$WT_DIR"
assert_allow "mkdir-confinement (b2): 'mkdir -p <worktree-abs>/sub/newdir2' -> allow" \
    "mkdir -p \"$WT_DIR/sub/newdir2\"" "$WT_REPO"

# --- (c) same-command-resolvable variable-based mkdir -p target INSIDE the
# worktree -> allow (mirrors the cp/tee/sed -i/> #4881 same-command
# resolution already proven for those idioms).
assert_allow "mkdir-confinement (c): same-command \$WORKTREE_ABS/newdir3 resolves inside worktree -> allow" \
    "WORKTREE_ABS=\"$WT_DIR\"
mkdir -p \"\$WORKTREE_ABS/newdir3\"" "$WT_REPO"
assert_allow "mkdir-confinement (c2): mid-path \$REC after literal prefix, resolves inside worktree -> allow" \
    "cd $WT_DIR && REC=abc123 && mkdir -p \"artifacts/\$REC/sub\"" "$WT_REPO"

# --- (d) same-command-resolvable variable-based mkdir -p target OUTSIDE the
# worktree (still inside the main checkout) -> deny.
assert_deny "mkdir-confinement (d): same-command \$EVIL/pwned resolves OUTSIDE worktree/into main checkout -> deny" \
    "EVIL=\"$WT_REPO/secrets\"
mkdir -p \"\$EVIL/pwned\"" "$WT_REPO"

# --- (e) EVERY argument of a multi-directory mkdir -p is checked -----------
# A safe FIRST argument must not mask an escaping SECOND argument.
assert_deny "mkdir-confinement (e): 'mkdir -p dir1 <escaping-dir2>' -- second (escaping) argument still checked -> deny" \
    "mkdir -p \"sub/dir1\" \"$WT_REPO/dir2\"" "$WT_DIR"
# The reverse order too -- an escaping FIRST argument must not be missed
# because a later argument happens to be safe.
assert_deny "mkdir-confinement (e2): 'mkdir -p <escaping-dir1> dir2' -- first (escaping) argument caught -> deny" \
    "mkdir -p \"$WT_REPO/dir1\" \"sub/dir2\"" "$WT_DIR"
# Both arguments safe (inside worktree) -> allow.
assert_allow "mkdir-confinement (e3): 'mkdir -p dir1 dir2' both inside worktree -> allow" \
    "mkdir -p \"sub/dir1\" \"sub/dir2\"" "$WT_DIR"

echo -e "${YELLOW}--- mkdir false-positive regression guards (#7945) ---${NC}"

# --- (f) bare relative mkdir -p, the overwhelmingly common Builder idiom ---
assert_allow "mkdir-confinement (f): bare relative 'mkdir -p verification/records/foo' inside worktree -> allow" \
    "mkdir -p verification/records/foo" "$WT_DIR"

# --- (g) -m MODE flag (separate-argument form) must not be mistaken for a
# directory target, and must not cause the REAL directory argument that
# follows to be skipped either.
assert_allow "mkdir-confinement (g): 'mkdir -m 0755 -p sub/newdir4' -- mode value not treated as a target, real dir still allowed -> allow" \
    "mkdir -m 0755 -p sub/newdir4" "$WT_DIR"
assert_deny "mkdir-confinement (g2): 'mkdir -m 0755 -p <escaping-dir>' -- mode value skipped, escaping dir still caught -> deny" \
    "mkdir -m 0755 -p \"$WT_REPO/pwned-dir4\"" "$WT_DIR"
# Attached -m0755 form -- must not consume the following token as a mode
# value (there isn't one to consume).
assert_allow "mkdir-confinement (g3): 'mkdir -m0755 -p sub/newdir5' attached mode form -> allow" \
    "mkdir -m0755 -p sub/newdir5" "$WT_DIR"

# --- (h) combined short-flag spelling (`-pv`) and a trailing `&&`-chained
# real command, mirroring the common `mkdir -p <dir> && cp ... <dir>/...`
# Builder idiom -- must not regress.
assert_allow "mkdir-confinement (h): 'mkdir -pv sub/newdir6 && cp ... sub/newdir6/x.txt' chained idiom, both inside worktree -> allow" \
    "mkdir -pv sub/newdir6 && cp /tmp/x.txt sub/newdir6/x.txt" "$WT_DIR"
assert_deny "mkdir-confinement (h2): same chained idiom, but the chained cp destination escapes into the main checkout -> deny" \
    "mkdir -pv sub/newdir7 && cp /tmp/x.txt \"$WT_REPO/pwned7/x.txt\"" "$WT_DIR"

# --- symlinked-ancestor physical-path fallback (92415b2) -------------------
# On macOS, $TMPDIR (and therefore `mktemp -d`) lives under the /var symlink
# while the guard's own MAIN_ROOT is resolved via `pwd -P` (symlink-physical),
# so it reports the /private/var spelling -- a mkdir target expressed through
# the LEXICAL /var spelling of the same directory previously fell straight
# through to allow, since the pre-fix containment check was lexical-only
# (normalize_abs_path()) with no symlink resolution at all. Reproduced here
# platform-independently (not conditioned on this host's own /var) by
# building a fixture repo behind a SECOND, symlinked path to the same
# directory: MAIN_ROOT resolves (via `pwd -P` inside the guard, same call
# `_WT_MAIN_ROOT` itself uses) to the REAL directory, while the mkdir target
# below is expressed through the SYMLINK spelling -- which normalize_abs_path()
# (lexical only, never resolves symlinks) leaves unchanged, so only the
# physical_abs_path() fallback this issue adds can still match it to
# MAIN_ROOT and deny it.
SYMLINK_BASE=$(mktemp -d 2>/dev/null)
SYMLINK_BASE=$(cd "$SYMLINK_BASE" && pwd -P)
SYMLINK_REAL_REPO="$SYMLINK_BASE/real-repo"
SYMLINK_ALIAS_REPO="$SYMLINK_BASE/alias-repo"
mkdir -p "$SYMLINK_REAL_REPO"
git -C "$SYMLINK_REAL_REPO" init -q >/dev/null 2>&1
mkdir -p "$SYMLINK_REAL_REPO/.loom/worktrees/issue-1/src"
: > "$SYMLINK_REAL_REPO/.loom/worktrees/issue-1/.loom-managed"
ln -s "$SYMLINK_REAL_REPO" "$SYMLINK_ALIAS_REPO"
SYMLINK_WT_DIR="$SYMLINK_REAL_REPO/.loom/worktrees/issue-1"
assert_deny "mkdir-confinement (92415b2): mkdir target via a symlinked-ancestor spelling of the main checkout still denies via physical_abs_path() fallback" \
    "mkdir -p \"$SYMLINK_ALIAS_REPO/pwned-symlink-dir\"" "$SYMLINK_WT_DIR"
# Sibling allow: the identical symlink-aliased fixture, target genuinely
# inside the worktree -- proves the fallback only WIDENS what counts as "the
# main checkout", it never narrows what counts as "inside the worktree".
assert_allow "mkdir-confinement (92415b2 sibling): mkdir target inside the worktree, reached via the same symlinked-ancestor repo, still allows" \
    "mkdir -p \"$SYMLINK_ALIAS_REPO/.loom/worktrees/issue-1/src/newdir\"" "$SYMLINK_WT_DIR"

# --- (i) the new mkdir idiom must not be bypassable through qsplit()'s
# backslash-newline join (#7978). `foo\\` + a real newline is an escaped
# literal backslash followed by an ORDINARY line end -- two shell
# statements. When qsplit() lacked backslash-parity tracking it joined them,
# hiding the second statement's `mkdir` command word from toks[1] so this
# whole confinement branch never ran: the idiom (a) denies was bypassable on
# this one shape from the day it was added. The parity rule itself is
# exercised across cp/mv/sed -i in
# tests/hooks/test-guard-destructive-cp-mv-continuation.sh (j)-(o); this
# case pins the mkdir half of it, in the suite that owns mkdir recognition.
MKDIR_BS2='\\'   # TWO literal backslashes -- see that suite for why this is named, not inlined
assert_deny "mkdir-confinement (i): escaping mkdir after a \\\\ + newline statement boundary is still caught (not joined away) -> deny" \
    "echo foo${MKDIR_BS2}
mkdir -p \"$WT_REPO/pwned-dir-parity\"" "$WT_DIR"

echo ""

# =========================================================================

print_summary
