#!/usr/bin/env bash
# test-resync-installed-source.sh - resync-installed.sh's --source flag (#14677):
# an explicit defaults/ SOURCE checkout, the first and only resolution rung
# when given.
#
# Split out of test-resync-installed-output-staging.sh rather than grown in
# place. The scenario is the one #14677 reports: an agent confined to a
# managed worktree of a CONSUMER repo (no defaults/ tree, no sidecar it is
# allowed to write -- the gitignored .loom/loom-source-path lives only in the
# primary checkout and the worktree-isolation guard denies writing it) still
# needs to stage a complete resync via --output. --source names the Loom
# checkout explicitly, so the documented command works as written.
#
#   (1) --source + --output from a linked worktree stages from the named
#       checkout, never reading or writing the sidecar.
#   (2) An unusable --source (missing dir, or one with an empty defaults/) is
#       a hard error naming the path -- no fallthrough to the automatic rungs
#       or the daemon-payload fallback.
#   (3) SOURCE_ROOT follows from --source, so the version re-stamp reads the
#       given checkout, and a foreign source keeps the consumer (real-copy)
#       path -- no symlinks.
#
# Usage:
#   ./.loom/scripts/tests/test-resync-installed-source.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPERS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SCRIPT="$HELPERS_DIR/resync-installed.sh"

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/assert.sh"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/test-resync-source.XXXXXX")"
# shellcheck disable=SC2329  # invoked indirectly via the EXIT trap below
cleanup() { rm -rf "$WORKDIR" 2>/dev/null || true; }
trap cleanup EXIT

export GIT_AUTHOR_NAME="test" GIT_AUTHOR_EMAIL="test@example.com"
export GIT_COMMITTER_NAME="test" GIT_COMMITTER_EMAIL="test@example.com"

# A Loom SOURCE checkout: defaults/hooks + defaults/scripts populated, and a
# root package.json whose version the #13592 re-stamp must read FROM THE GIVEN
# CHECKOUT (not from the consumer).
SRC="$WORKDIR/loom-src"
mkdir -p "$SRC/defaults/hooks" "$SRC/defaults/scripts"
git -C "$SRC" init -q -b main
printf 'NEW\n' > "$SRC/defaults/hooks/guard.sh"
chmod +x "$SRC/defaults/hooks/guard.sh"
printf 'tool\n' > "$SRC/defaults/scripts/tool.sh"
chmod +x "$SRC/defaults/scripts/tool.sh"
printf '{\n  "version": "9.9.9"\n}\n' > "$SRC/package.json"
git -C "$SRC" add -A >/dev/null 2>&1
git -C "$SRC" commit -qm "feat: source fixture" >/dev/null 2>&1

# A CONSUMER repo: installed .loom/ surfaces stale against the source, NO
# defaults/ tree, NO .loom/loom-source-path sidecar, no loom_source metadata
# field -- exactly the #14677 dead end. The install subject matches the
# routine-lineage pattern so the #7864 local-fix guard never gates the update.
CONS="$WORKDIR/consumer"
# `.loom/scripts/placeholder.sh` exists so the destination .loom/scripts/ dir
# is TRACKED and therefore present in the --output staging checkout (a plain
# empty dir would not be; the scripts walk is gated on the dir existing).
# It has no source counterpart, so resync must leave it alone.
mkdir -p "$CONS/.loom/hooks" "$CONS/.loom/scripts"
git -C "$CONS" init -q -b main
printf 'OLD\n' > "$CONS/.loom/hooks/guard.sh"
printf 'OLD\n' > "$CONS/.loom/scripts/placeholder.sh"
printf '{\n  "loom_version": "0.0.0",\n  "loom_commit": "old",\n  "install_date": "2020-01-01",\n  "installed_files": []\n}\n' \
    > "$CONS/.loom/install-metadata.json"
git -C "$CONS" add -A >/dev/null 2>&1
git -C "$CONS" commit -qm "chore: install Loom v0.0.0" >/dev/null 2>&1
CONS_MAIN_HEAD="$(git -C "$CONS" rev-parse HEAD)"

# A linked worktree in the consumer, one commit AHEAD of its main -- the
# documented place to run --output from (the #4563 restriction forbids
# writing to the primary from here).
WT="$WORKDIR/wt"
git -C "$CONS" worktree add -q -b feature/x "$WT" >/dev/null 2>&1
printf 'feature work\n' > "$WT/feature-file.txt"
git -C "$WT" add -A >/dev/null 2>&1
git -C "$WT" commit -qm "feat: an earlier commit on the feature branch" >/dev/null 2>&1
WT_HEAD="$(git -C "$WT" rev-parse HEAD)"

if [[ "$WT_HEAD" != "$CONS_MAIN_HEAD" ]]; then
    pass "fixture precondition: the consumer worktree's HEAD is ahead of the primary's"
else
    fail "fixture precondition unmet: the two HEADs are equal"
fi
if [[ ! -e "$CONS/.loom/loom-source-path" && ! -e "$WT/.loom/loom-source-path" ]]; then
    pass "fixture precondition: no loom-source-path sidecar anywhere"
else
    fail "fixture precondition unmet: a sidecar exists"
fi

# drop_staging <dir> -- unregister a staging worktree the script intentionally
# left behind (KEEP_STAGING_WORKTREE on a successful run).
drop_staging() {
    git -C "$CONS" worktree remove --force "$1" >/dev/null 2>&1 || rm -rf "$1"
}

meta_ver() { sed -n 's/.*"loom_version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$1" 2>/dev/null | head -1; }

echo ""
echo "Test group 1: --source + --output from a linked worktree stages from the named checkout (#14677)"
STAGING1="$WORKDIR/staging1"
OUT1="$(cd "$WT" && bash "$SCRIPT" --output "$STAGING1" --source "$SRC" 2>&1)"
RC1=$?
if [[ $RC1 -eq 0 ]]; then
    pass "(#14677) the run succeeds from the worktree"
else
    fail "(#14677) the run failed (rc=$RC1); out=$OUT1"
fi
if [[ "$(cat "$STAGING1/.loom/hooks/guard.sh" 2>/dev/null)" == "NEW" ]] \
    && [[ "$(cat "$STAGING1/.loom/scripts/tool.sh" 2>/dev/null)" == "tool" ]] \
    && [[ "$(cat "$STAGING1/.loom/scripts/placeholder.sh" 2>/dev/null)" == "OLD" ]] \
    && [[ "$(cat "$CONS/.loom/hooks/guard.sh")" == "OLD" ]] \
    && [[ "$(cat "$WT/.loom/hooks/guard.sh")" == "OLD" ]]; then
    pass "(#14677) the resync came from --source and landed in the staging worktree only"
else
    fail "(#14677) the staged tree does not reflect the named source; out=$OUT1"
fi
if [[ ! -e "$CONS/.loom/loom-source-path" && ! -e "$WT/.loom/loom-source-path" \
    && ! -e "$STAGING1/.loom/loom-source-path" ]]; then
    pass "(#14677) no .loom/loom-source-path sidecar was read or written anywhere"
else
    fail "(#14677) a sidecar appeared somewhere (primary/worktree/staging)"
fi
# The staging base is still the invoking worktree's HEAD (#9550 unaffected).
if [[ "$(git -C "$STAGING1" rev-parse HEAD 2>/dev/null)" == "$WT_HEAD" ]]; then
    pass "(#14677) --source does not change the --output staging base (#9550)"
else
    fail "(#14677) the staging base changed; out=$OUT1"
fi

echo ""
echo "Test group 2: a foreign --source keeps the consumer (real-copy) path -- no symlinks"
if [[ -f "$STAGING1/.loom/hooks/guard.sh" && ! -L "$STAGING1/.loom/hooks/guard.sh" ]] \
    && [[ -f "$STAGING1/.loom/scripts/tool.sh" && ! -L "$STAGING1/.loom/scripts/tool.sh" ]]; then
    pass "(#14677) staged surfaces are real file copies, not symlinks"
else
    fail "(#14677) a staged surface is missing or a symlink"
fi

echo ""
echo "Test group 3: the version re-stamp reads the given checkout (#14677, #13592)"
STAGING1_META_VER="$(meta_ver "$STAGING1/.loom/install-metadata.json")"
if [[ "$STAGING1_META_VER" == "9.9.9" ]]; then
    pass "(#14677) staging metadata carries the --source checkout's version"
else
    fail "(#14677) staging metadata version is '$STAGING1_META_VER', not 9.9.9"
fi
if [[ "$(meta_ver "$CONS/.loom/install-metadata.json")" == "0.0.0" ]] \
    && [[ "$(meta_ver "$WT/.loom/install-metadata.json")" == "0.0.0" ]]; then
    pass "(#14677) the primary and invoking worktree metadata still match their files"
else
    fail "(#14677) the --source stamp leaked outside the staging worktree"
fi
drop_staging "$STAGING1"

echo ""
echo "Test group 4: --source=<dir> is equivalent to --source <dir>"
STAGING2="$WORKDIR/staging2"
OUT2="$(cd "$WT" && bash "$SCRIPT" --output "$STAGING2" --source="$SRC" 2>&1)"
RC2=$?
if [[ $RC2 -eq 0 ]] && [[ "$(cat "$STAGING2/.loom/hooks/guard.sh" 2>/dev/null)" == "NEW" ]]; then
    pass "(#14677) the --source=<dir> form stages the same resync"
else
    fail "(#14677) the --source=<dir> form did not work; out=$OUT2"
fi
drop_staging "$STAGING2"

echo ""
echo "Test group 5: a relative --source resolves against the invocation directory"
STAGING3="$WORKDIR/staging3"
OUT3="$(cd "$WT" && bash "$SCRIPT" --output "$STAGING3" --source ../loom-src 2>&1)"
RC3=$?
if [[ $RC3 -eq 0 ]] && [[ "$(cat "$STAGING3/.loom/hooks/guard.sh" 2>/dev/null)" == "NEW" ]]; then
    pass "(#14677) a relative --source is anchored to the caller's directory"
else
    fail "(#14677) a relative --source did not resolve; out=$OUT3"
fi
drop_staging "$STAGING3"

echo ""
echo "Test group 6: an unusable --source is a hard error naming the path, with no residue"
# (a) a directory that does not exist at all
STAGING4="$WORKDIR/staging4"
OUT4="$(cd "$WT" && bash "$SCRIPT" --output "$STAGING4" --source "$WORKDIR/no-such-loom" 2>&1)"
RC4=$?
if [[ $RC4 -eq 1 ]] && grep -q -- "--source is not a usable Loom source checkout" <<<"$OUT4" \
    && grep -q "$WORKDIR/no-such-loom" <<<"$OUT4" && [[ ! -e "$STAGING4" ]]; then
    pass "(#14677) a missing --source dir exits 1, names the path, leaves no staging worktree"
else
    fail "(#14677) a missing --source dir was not refused cleanly (rc=$RC4, exists=$([[ -e "$STAGING4" ]] && echo yes || echo no)); out=$OUT4"
fi
# (b) a directory that exists but carries only an EMPTY defaults/ (#6780 shape)
EMPTY_SRC="$WORKDIR/empty-src"
mkdir -p "$EMPTY_SRC/defaults"
STAGING5="$WORKDIR/staging5"
OUT5="$(cd "$WT" && bash "$SCRIPT" --output "$STAGING5" --source "$EMPTY_SRC" 2>&1)"
RC5=$?
if [[ $RC5 -eq 1 ]] && grep -q -- "--source is not a usable Loom source checkout" <<<"$OUT5" \
    && grep -q "$EMPTY_SRC" <<<"$OUT5" && [[ ! -e "$STAGING5" ]]; then
    pass "(#14677) an empty-defaults --source is refused the same way"
else
    fail "(#14677) an empty-defaults --source was not refused cleanly (rc=$RC5); out=$OUT5"
fi
# (c) without --output too, and NEVER via the generic could-not-locate error
#     or the daemon-payload fallback: an explicit source must not fall through.
OUT6="$(cd "$CONS" && bash "$SCRIPT" --source "$WORKDIR/no-such-loom" 2>&1)"
RC6=$?
if [[ $RC6 -eq 1 ]] && grep -q -- "--source is not a usable Loom source checkout" <<<"$OUT6" \
    && ! grep -q "Could not locate a defaults/ source tree" <<<"$OUT6" \
    && ! grep -q "resync-payload" <<<"$OUT6"; then
    pass "(#14677) an unusable --source is a named refusal even without --output (no fallback)"
else
    fail "(#14677) an unusable --source fell through somewhere else (rc=$RC6); out=$OUT6"
fi
if [[ "$(cat "$CONS/.loom/hooks/guard.sh")" == "OLD" ]]; then
    pass "(#14677) the refused run changed nothing in the consumer"
else
    fail "(#14677) the refused run touched the consumer's installed files"
fi

echo ""
echo "Test group 7: --dry-run + --source previews without applying and leaves no residue"
STAGING6="$WORKDIR/staging6"
OUT7="$(cd "$WT" && bash "$SCRIPT" --output "$STAGING6" --source "$SRC" --dry-run 2>&1)"
RC7=$?
if [[ $RC7 -eq 2 ]] && grep -q "guard.sh" <<<"$OUT7" && [[ ! -e "$STAGING6" ]]; then
    pass "(#14677) --dry-run reports the drift (exit 2) and auto-removes the staging worktree"
else
    fail "(#14677) --dry-run misbehaved (rc=$RC7, exists=$([[ -e "$STAGING6" ]] && echo yes || echo no)); out=$OUT7"
fi
if [[ "$(cat "$CONS/.loom/hooks/guard.sh")" == "OLD" ]]; then
    pass "(#14677) the preview applied nothing"
else
    fail "(#14677) the preview changed the consumer"
fi

echo ""
echo "Test group 8: --source alone from a linked worktree does not bypass the #4563 refusal"
OUT8="$(cd "$WT" && bash "$SCRIPT" --source "$SRC" 2>&1)"
RC8=$?
if [[ $RC8 -eq 1 ]] && grep -q "Refusing to run: invoked from a linked git worktree" <<<"$OUT8" \
    && [[ "$(cat "$CONS/.loom/hooks/guard.sh")" == "OLD" ]]; then
    pass "(#4563) --source alone still refuses from a linked worktree (writes would target the primary)"
else
    fail "(#4563) --source weakened the linked-worktree refusal (rc=$RC8); out=$OUT8"
fi

echo ""
echo "Test group 9: without --source, the consumer dead end is unchanged and now hints at --source"
STAGING7="$WORKDIR/staging7"
OUT9="$(cd "$WT" && bash "$SCRIPT" --output "$STAGING7" 2>&1)"
RC9=$?
if [[ $RC9 -eq 1 ]] && grep -q "Could not locate a defaults/ source tree to sync from" <<<"$OUT9" \
    && grep -q -- "--source <loom-checkout>" <<<"$OUT9" && [[ ! -e "$STAGING7" ]]; then
    pass "(#14677) the no-source failure keeps its text, names --source, and leaves no residue"
else
    fail "(#14677) the no-source failure changed shape (rc=$RC9); out=$OUT9"
fi

# --- summary -----------------------------------------------------------------
echo ""
echo "========================================"
echo "Results: $TESTS_PASSED/$TESTS_RUN passed"
echo "========================================"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "${RED}$TESTS_FAILED test(s) failed${NC}"
    exit 1
fi
echo -e "${GREEN}All tests passed${NC}"
exit 0
