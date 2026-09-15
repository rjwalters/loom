#!/usr/bin/env bash
# land-resync-commit.sh - Conservatively land a "chore: resync installed Loom
# surfaces" change onto the PRIMARY clone's default branch (#6646).
#
# Background: `resync-installed.sh` never commits or pushes anything itself
# (see its own header) -- it only refreshes the installed `.loom/` /
# `.claude/commands/loom/` copies and, when the tree ends up dirty with only
# that output, PRINTS a suggested `git add && git commit` command. Before this
# script existed, actually LANDING that commit onto the primary clone's
# default branch was ad hoc agent behavior with no documented, safe recipe --
# and the natural-seeming approach (commit, then `git pull --rebase` /
# `git rebase origin/<default>` to reconcile with a moved origin, then push)
# is exactly what went wrong on 2026-09-15: an operator had just
# fast-forward-landed a not-yet-pushed local commit in the primary clone when
# a sweep committed its own resync change on top, rebased local `main` onto
# `origin/main` (which had gained several merged PRs), silently re-creating
# the operator's commit under a new SHA, and bypass-pushed the result past
# branch-protection required-checks. Nothing was lost (the recreated commit
# has identical content), but the operator's recorded SHA vanished from `git
# log`, a bypass push happened from automation, and establishing this was
# benign took a reflog read.
#
# This script replaces that ad hoc recipe with a deterministic, conservative
# one: it NEVER rebases and NEVER force/bypass-pushes over a commit it did
# not author.
#
#   1. If the working tree in the primary checkout has no resync-managed dirt,
#      it is a no-op (exit 0).
#   2. If the dirty set includes anything OUTSIDE the known resync-managed
#      surfaces (`.loom/hooks|scripts|roles|docs|bin|runtimes/`,
#      `.claude/commands/loom/`, and the handful of single-file targets
#      resync-installed.sh itself resyncs), it refuses to commit ANYTHING --
#      an unrelated (possibly operator) change must never be swept into a
#      "chore: resync" commit.
#   3. Otherwise it commits the resync-managed dirt, fetches origin, and
#      inspects every commit the primary checkout's default branch now has
#      that origin does not:
#        - If ANY of those commits was NOT authored by this checkout's own
#          configured git identity (`git config user.email` -- the Loom
#          automation identity; see check-git-identity.sh) -- i.e. an
#          operator's own unpushed work -- it STOPS: the resync commit stays
#          local, nothing is pushed, nothing is rebased, nothing is forced.
#          This is the fix for the incident above: an operator's in-flight
#          commit is never silently rewritten to reconcile with origin.
#        - Otherwise every commit ahead is this checkout's own automation, so
#          a plain `git push` is attempted. Ordinary git push semantics make
#          this fast-forward-only by construction -- it is rejected outright
#          if origin has advanced with commits this checkout doesn't have, or
#          if the forge's branch protection requires a PR.
#   4. If that push is rejected, it does NOT retry with a rebase or a forced
#      push. It lands the commit via a short-lived branch + PR instead (the
#      same path already used for other automated commits, via
#      create-pr.sh), then resets the primary checkout's default branch back
#      to origin's current tip so it never sits diverged waiting on that PR.
#
# See `.loom/docs/troubleshooting.md` -> "Landing a resync commit on the
# primary clone (#6646)" for the full policy, the conditions under which this
# script's caller (a sweep, a human operator) may run it, and the reflog
# recipe for telling "this script did its documented job" apart from
# "something unexpectedly rewrote my branch".
#
# Usage:
#   ./.loom/scripts/land-resync-commit.sh              # commit + land
#   ./.loom/scripts/land-resync-commit.sh --dry-run     # preview only
#   ./.loom/scripts/land-resync-commit.sh --allow-worktree  # see below
#
# Like resync-installed.sh (#4563), this ALWAYS resolves and operates on the
# PRIMARY checkout (via `git rev-parse --git-common-dir`), never a linked
# issue/PR worktree, and refuses to run from one unless --allow-worktree (or
# LOOM_RESYNC_ALLOW_WORKTREE=1) is given -- committing/pushing the default
# branch from a Builder's worktree is never the right call.
#
# Exit codes:
#   0 - Nothing to land, OR landed successfully (direct push, or via a
#       branch + PR when a direct push was rejected).
#   1 - Error: not a git repo, no resolvable default branch, wrong branch
#       checked out, non-resync dirt present, fetch failed, or the branch+PR
#       fallback itself failed (a PR-less pushed branch is reported so it can
#       be finished by hand).
#   2 - Usage error (bad argument).
#   3 - STOPPED on purpose: the resync was committed locally but NOT pushed,
#       because the primary checkout's default branch is ahead of origin by
#       one or more commits not authored by this checkout's own git identity
#       (presumed operator work). Nothing was rebased or force-pushed. A
#       human must reconcile (push, or rebase by hand) before the next
#       resync commit can land.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
# shellcheck source=./lib/default-branch.sh
source "$SCRIPT_DIR/lib/default-branch.sh"

# ---------- output helpers (mirrors resync-installed.sh) ----------

if [[ -t 1 ]]; then
    RED='\033[0;31m'
    YELLOW='\033[1;33m'
    BLUE='\033[0;34m'
    NC='\033[0m'
else
    RED=''
    YELLOW=''
    BLUE=''
    NC=''
fi

err()  { printf '%b\n' "${RED}ERROR: $*${NC}" >&2; }
warn() { printf '%b\n' "${YELLOW}WARN: $*${NC}" >&2; }
note() { printf '%b\n' "${BLUE}$*${NC}"; }

EXIT_OK=0
EXIT_ERROR=1
EXIT_USAGE=2
EXIT_STOPPED_FOREIGN_AHEAD=3

usage() {
    sed -n '2,86p' "${BASH_SOURCE[0]:-$0}" | sed 's/^# \{0,1\}//'
}

DRY_RUN=0
ALLOW_WORKTREE=0
[[ "${LOOM_RESYNC_ALLOW_WORKTREE:-}" == "1" ]] && ALLOW_WORKTREE=1

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        --allow-worktree)
            ALLOW_WORKTREE=1
            shift
            ;;
        -h | --help)
            usage
            exit "$EXIT_OK"
            ;;
        *)
            err "unknown argument: $1"
            err "Run with --help for usage."
            exit "$EXIT_USAGE"
            ;;
    esac
done

if ! git rev-parse --git-dir >/dev/null 2>&1; then
    err "Not inside a git repository."
    exit "$EXIT_ERROR"
fi

# ---------- resolve the PRIMARY checkout, refuse a linked worktree (#4563) ----------

REPO_ROOT=""
COMMON_DIR="$(git rev-parse --git-common-dir 2>/dev/null || true)"
if [[ -n "$COMMON_DIR" ]]; then
    case "$COMMON_DIR" in
        */.git) REPO_ROOT="${COMMON_DIR%/.git}" ;;
    esac
fi
if [[ -z "$REPO_ROOT" ]]; then
    REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || true)"
fi
if [[ -z "$REPO_ROOT" || ! -d "$REPO_ROOT/.git" ]]; then
    err "Could not resolve the repository root."
    exit "$EXIT_ERROR"
fi

abs_path() {
    local p="$1"
    [[ -d "$p" ]] || { printf '%s' "$p"; return 0; }
    (cd "$p" 2>/dev/null && pwd -P) || printf '%s' "$p"
}

WORKTREE_TOP="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [[ -n "$WORKTREE_TOP" && "$(abs_path "$WORKTREE_TOP")" != "$(abs_path "$REPO_ROOT")" ]]; then
    if [[ "$ALLOW_WORKTREE" -eq 1 ]]; then
        warn "Running from a linked worktree ($WORKTREE_TOP) — operating on the MAIN checkout at $REPO_ROOT (--allow-worktree)."
    else
        err "Refusing to run: invoked from a linked git worktree ($WORKTREE_TOP)."
        err "  This script commits and pushes the PRIMARY clone's default branch —"
        err "  never a Builder/issue worktree. Re-run from $REPO_ROOT, or pass"
        err "  --allow-worktree (or LOOM_RESYNC_ALLOW_WORKTREE=1) if you deliberately"
        err "  mean to operate on the main checkout from here."
        exit "$EXIT_ERROR"
    fi
fi

# ---------- resolve + verify the default branch ----------

DEFAULT_BRANCH_ERR_FILE="$(mktemp)"
DEFAULT_BRANCH="$(cd "$REPO_ROOT" && loom_default_branch 2>"$DEFAULT_BRANCH_ERR_FILE")"
_DB_RC=$?
if [[ $_DB_RC -ne 0 ]]; then
    err "Could not resolve the default branch: $(cat "$DEFAULT_BRANCH_ERR_FILE")"
    rm -f "$DEFAULT_BRANCH_ERR_FILE"
    exit "$EXIT_ERROR"
fi
rm -f "$DEFAULT_BRANCH_ERR_FILE"

CURRENT_BRANCH="$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
if [[ "$CURRENT_BRANCH" != "$DEFAULT_BRANCH" ]]; then
    err "The primary checkout ($REPO_ROOT) is on '$CURRENT_BRANCH', not '$DEFAULT_BRANCH'."
    err "  land-resync-commit.sh only ever lands changes onto the default branch —"
    err "  check out '$DEFAULT_BRANCH' there first."
    exit "$EXIT_ERROR"
fi

# ---------- identify resync-managed dirt; refuse anything else ----------
#
# Mirrors the path-prefix allowlist resync-installed.sh's own
# suggest_commit_if_resync_only_dirt() uses for its printed suggestion, so
# this script never stages (and commits) an unrelated -- possibly operator --
# change under the "chore: resync" message.

is_resync_surface_path() {
    case "$1" in
        .loom/hooks/* | .loom/scripts/* | .loom/roles/* | .loom/docs/* | \
            .loom/bin/* | .loom/runtimes/* | \
            .claude/commands/loom/* | .claude/README.md | \
            .github/CONFIGURATION.md | \
            .loom/install-metadata.json | .loom/CLAUDE.md | .gitattributes)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

# Narrower than is_resync_surface_path(): the subset resync-installed.sh calls
# a "pure-copy surface" -- a directory whose every file is copied verbatim
# from a same-shaped defaults/ subdirectory (mirrors
# _is_loom_pure_copy_surface_path()). Only this subset can be "retired but
# unlisted" (#6613/#7336): a path matching the PATTERN with no defaults/
# counterpart today, because it was removed from defaults/ without ever being
# added to defaults/.loom-retired.list. Committing such a file would
# permanently ship dead code -- so it is excluded from the commit (left dirty,
# with a warning), the same as resync-installed.sh's own suggestion already
# does, rather than treated as blocking foreign dirt.
is_pure_copy_surface_path() {
    case "$1" in
        .loom/hooks/* | .loom/scripts/* | .loom/roles/* | .loom/docs/* | \
            .loom/runtimes/* | .loom/bin/*)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

# Maps a path already matched by is_pure_copy_surface_path() to the defaults/
# source path resync would have copied it from (mirrors
# _loom_pure_copy_surface_source_path()). Only meaningful when this checkout
# IS the Loom source repo (a local defaults/ tree exists) -- see the caller.
pure_copy_surface_source_path() {
    case "$1" in
        .loom/hooks/*) printf '%s\n' "$REPO_ROOT/defaults/hooks/${1#.loom/hooks/}" ;;
        .loom/scripts/*) printf '%s\n' "$REPO_ROOT/defaults/scripts/${1#.loom/scripts/}" ;;
        .loom/roles/*) printf '%s\n' "$REPO_ROOT/defaults/roles/${1#.loom/roles/}" ;;
        .loom/docs/*) printf '%s\n' "$REPO_ROOT/defaults/docs/${1#.loom/docs/}" ;;
        .loom/runtimes/*) printf '%s\n' "$REPO_ROOT/defaults/runtimes/${1#.loom/runtimes/}" ;;
        .loom/bin/*) printf '%s\n' "$REPO_ROOT/defaults/.loom/bin/${1#.loom/bin/}" ;;
        *) return 1 ;;
    esac
}

STATUS="$(git -C "$REPO_ROOT" status --porcelain --untracked-files=all)"
if [[ -z "$STATUS" ]]; then
    note "land-resync-commit.sh: nothing to land — working tree is clean."
    exit "$EXIT_OK"
fi

IS_LOOM_SOURCE_REPO=0
[[ -d "$REPO_ROOT/defaults/hooks" || -d "$REPO_ROOT/defaults/scripts" ]] && IS_LOOM_SOURCE_REPO=1

RESYNC_PATHS=()
FOREIGN_PATHS=()
RETIRED_PATHS=()
while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    code="${line:0:2}"
    path="${line:3}"
    path="${path%\"}"
    path="${path#\"}"
    [[ "$path" == *" -> "* ]] && path="${path##* -> }"
    if ! is_resync_surface_path "$path"; then
        FOREIGN_PATHS+=("$path")
        continue
    fi
    if [[ "$IS_LOOM_SOURCE_REPO" -eq 1 && "$code" == "??" ]] && is_pure_copy_surface_path "$path"; then
        src="$(pure_copy_surface_source_path "$path")"
        if [[ ! -e "$src" ]]; then
            RETIRED_PATHS+=("$path")
            continue
        fi
    fi
    RESYNC_PATHS+=("$path")
done <<< "$STATUS"

if [[ "${#FOREIGN_PATHS[@]}" -gt 0 ]]; then
    err "Refusing to land: the working tree has non-resync dirt alongside resync output:"
    for p in "${FOREIGN_PATHS[@]}"; do
        err "    $p"
    done
    err "  This script only ever commits the known resync-managed surfaces."
    err "  Resolve (or commit) the unrelated change yourself, then re-run."
    exit "$EXIT_ERROR"
fi

if [[ "${#RETIRED_PATHS[@]}" -gt 0 ]]; then
    warn "Excluded from the commit (matches a pure-copy-surface path but has no defaults/ counterpart today -- presumed retired-but-unlisted, #6613/#7336):"
    for p in "${RETIRED_PATHS[@]}"; do
        warn "    $p"
    done
    warn "  Add it to defaults/.loom-retired.list (or delete it) if it is genuinely retired."
fi

if [[ "${#RESYNC_PATHS[@]}" -eq 0 ]]; then
    note "land-resync-commit.sh: nothing to land — no resync-managed surface is dirty."
    exit "$EXIT_OK"
fi

if [[ "$DRY_RUN" -eq 1 ]]; then
    note "[dry-run] Would commit ${#RESYNC_PATHS[@]} resync-managed path(s) as 'chore: resync installed Loom surfaces',"
    note "[dry-run] then attempt to land it onto '$DEFAULT_BRANCH' (never rebasing, never force/bypass-pushing)."
    exit "$EXIT_OK"
fi

LOOM_IDENTITY_EMAIL="$(git -C "$REPO_ROOT" config user.email 2>/dev/null || true)"
if [[ -z "$LOOM_IDENTITY_EMAIL" ]]; then
    err "git config user.email is not set in $REPO_ROOT — cannot tell a"
    err "  Loom-authored commit apart from an operator's without a configured"
    err "  identity. Configure one (see check-git-identity.sh) and re-run."
    exit "$EXIT_ERROR"
fi

# ---------- commit ----------

git -C "$REPO_ROOT" add -- "${RESYNC_PATHS[@]}"
if ! git -C "$REPO_ROOT" commit --quiet -m "chore: resync installed Loom surfaces"; then
    err "git commit failed."
    exit "$EXIT_ERROR"
fi
RESYNC_SHA="$(git -C "$REPO_ROOT" rev-parse HEAD)"
note "land-resync-commit.sh: committed $RESYNC_SHA (${#RESYNC_PATHS[@]} path(s))."

# ---------- fetch origin, then evaluate what's ahead (never rebase) ----------

FETCH_ERR_FILE="$(mktemp)"
PUSH_ERR_FILE="$(mktemp)"
trap 'rm -f "$FETCH_ERR_FILE" "$PUSH_ERR_FILE"' EXIT
if ! git -C "$REPO_ROOT" fetch --quiet origin "$DEFAULT_BRANCH" 2>"$FETCH_ERR_FILE"; then
    warn "Could not fetch origin/$DEFAULT_BRANCH: $(cat "$FETCH_ERR_FILE")"
    warn "  The resync commit ($RESYNC_SHA) stays LOCAL, uncommitted-to-origin."
    warn "  Retry once network/forge access is restored — nothing was pushed or rebased."
    exit "$EXIT_ERROR"
fi

AHEAD_SHAS="$(git -C "$REPO_ROOT" rev-list "origin/$DEFAULT_BRANCH..HEAD" 2>/dev/null || true)"
FOREIGN_COMMITS=()
while IFS= read -r sha; do
    [[ -z "$sha" ]] && continue
    author_email="$(git -C "$REPO_ROOT" log -1 --format='%ae' "$sha")"
    if [[ "$author_email" != "$LOOM_IDENTITY_EMAIL" ]]; then
        FOREIGN_COMMITS+=("$sha")
    fi
done <<< "$AHEAD_SHAS"

if [[ "${#FOREIGN_COMMITS[@]}" -gt 0 ]]; then
    note ""
    note "land-resync-commit.sh: resync committed, NOT pushed: ${#FOREIGN_COMMITS[@]} operator commit(s) ahead of origin/$DEFAULT_BRANCH:"
    for sha in "${FOREIGN_COMMITS[@]}"; do
        note "    $(git -C "$REPO_ROOT" log -1 --format='%h %an <%ae> %s' "$sha")"
    done
    note "  This script never rebases or force/bypass-pushes over commits it did not"
    note "  author. Push, reconcile, or rebase these by hand — see"
    note "  .loom/docs/troubleshooting.md \"Landing a resync commit on the primary"
    note "  clone (#6646)\"."
    exit "$EXIT_STOPPED_FOREIGN_AHEAD"
fi

# ---------- plain push (fast-forward-only by construction; never forced) ----------

if git -C "$REPO_ROOT" push origin "HEAD:$DEFAULT_BRANCH" 2>"$PUSH_ERR_FILE"; then
    note "land-resync-commit.sh: pushed $RESYNC_SHA to origin/$DEFAULT_BRANCH."
    exit "$EXIT_OK"
fi
warn "Direct push to origin/$DEFAULT_BRANCH was rejected:"
warn "$(cat "$PUSH_ERR_FILE")"
warn "Never rebasing or force/bypass-pushing to reconcile — landing via a short-lived branch + PR instead."

# ---------- branch + PR fallback (never a bypass push) ----------

BRANCH_SUFFIX="$(date -u +%Y%m%dT%H%M%SZ 2>/dev/null || date +%Y%m%d%H%M%S)"
BRANCH="chore/resync-installed-$BRANCH_SUFFIX"
if ! git -C "$REPO_ROOT" branch "$BRANCH" HEAD; then
    err "Could not create fallback branch '$BRANCH'. The resync commit ($RESYNC_SHA) stays local."
    exit "$EXIT_ERROR"
fi
if ! git -C "$REPO_ROOT" push --quiet -u origin "$BRANCH"; then
    err "Could not push fallback branch '$BRANCH'. The resync commit ($RESYNC_SHA) stays local on '$DEFAULT_BRANCH'."
    exit "$EXIT_ERROR"
fi

# The commit is now safely on the pushed side branch — reset the primary
# checkout's default branch back to origin's tip so it never sits diverged,
# waiting on a PR that may take a while to merge.
git -C "$REPO_ROOT" reset --hard "origin/$DEFAULT_BRANCH"

PR_BODY="Automated resync of installed Loom surfaces from \`defaults/\`.

Opened via a PR instead of a direct push because \`origin/$DEFAULT_BRANCH\` had
already advanced (or requires status checks this push doesn't carry) —
land-resync-commit.sh never rebases or force/bypass-pushes to reconcile. See
\`.loom/docs/troubleshooting.md\` -> \"Landing a resync commit on the primary
clone (#6646)\"."

if PR_URL="$("$SCRIPT_DIR/create-pr.sh" \
    --title "chore: resync installed Loom surfaces" \
    --body "$PR_BODY" \
    --base "$DEFAULT_BRANCH" --head "$BRANCH")"; then
    note "land-resync-commit.sh: opened $PR_URL (branch $BRANCH) — merge it through the normal review path."
    exit "$EXIT_OK"
fi

err "Pushed '$BRANCH' but could not open a PR for it. Open one by hand:"
err "  gh pr create --base $DEFAULT_BRANCH --head $BRANCH --title 'chore: resync installed Loom surfaces'"
exit "$EXIT_ERROR"
