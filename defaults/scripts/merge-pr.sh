#!/usr/bin/env bash
# Loom PR Merge - Worktree-safe merge using forge API (GitHub or Gitea)
# Usage: ./.loom/scripts/merge-pr.sh <pr-number> [options]
#
# Merges a PR via the forge API (not `gh pr merge`) to avoid
# "already used by worktree" errors when merging from inside a worktree.
#
# Supports both GitHub and Gitea forges. Forge detection is automatic
# (see forge-helpers.sh for details).
#
# Options:
#   --no-cleanup-worktree  Skip local worktree AND local branch cleanup after
#                          merge
#   --cleanup-worktree     (no-op, worktree cleanup is now the default)
#   --worktree-path <dir>  Explicit worktree path to clean up (bypasses
#                          .loom-managed sentinel guard — caller asserts
#                          responsibility). Also deletes the matching local
#                          branch via `git branch -d` (refuses on unmerged
#                          commits — Git's own safety check).
#   --dry-run              Show what would happen without merging
#   --auto                 No-op (#12063 slice 2): merge-pr no longer waits for checks;
#                          GitHub's required checks gate the merge call itself.
#   --allow-stacked-children
#                          Bypass the pre-merge merge-ordering guard's hard-block
#                          path. That guard's DEFAULT behavior, when the parent
#                          branch (feature/issue-N) still has open stacked child
#                          PRs targeting it, is now to pin the parent's tip to
#                          refs/loom/parent/<branch> and proceed with a WARNING
#                          (see #7982) — it only still hard-blocks when the tip
#                          could not be pinned (a detached/unreadable parent).
#                          This flag skips straight past that remaining block
#                          (operator asserts responsibility, mirroring
#                          --worktree-path). See #3747 item 2.
#   --allow-red-tree       Proceed past a failing repo `merge.treeChecks` gate (warns +
#                          audit comment). See #10026.
#   --allow-red-required <context>
#                          No-op (#12063 slice 2); accepted and ignored with a warning.
#   --allow-unapproved     Bypass the pre-merge loom:pr review-signal guard,
#                          which otherwise hard-blocks merging a PR whose
#                          current head carries no loom:pr label (no
#                          forge-visible signal Judge reviewed it — e.g. a
#                          Doctor rebase cleared it via the staleness guard).
#                          The bypass is recorded as a warning and, on a real
#                          (non-dry-run) merge, best-effort as a PR comment
#                          audit trail. Operator asserts responsibility,
#                          mirroring --allow-stacked-children. See #7419.
#   --no-cleanup-primary   Skip the automatic primary-checkout branch cleanup
#                          (#5015): when the merged branch is checked out in
#                          the PRIMARY repo checkout (not a worktree) and it
#                          is provably safe (clean tree, no stash entries,
#                          tip matches the merged PR head SHA), the script
#                          normally checks out the default branch there and
#                          deletes the merged branch automatically instead of
#                          just printing manual instructions. Pass this to
#                          always print the manual instructions instead.
#   --cleanup-primary      (no-op, primary-checkout cleanup is the default)
#   --expected-head <sha>  Fail-closed head pin (#11859): refuse (exit 8) unless
#                          the PR's head is exactly this full SHA, checked on
#                          the first uncached PR read and again on the merge
#                          precondition read, which then gates the forge merge
#                          call on the CALLER's SHA instead of its own read. A
#                          targeted Champion (`/loom:champion --pr N`) passes
#                          the head it evaluated, so a push after its
#                          evaluation can never be merged under its verdict.
#                          Any later head move exits 3: under the pin the #8164
#                          self-sync retry is declined (its check reads only the
#                          merge commit's parents, never its tree), so a fresh
#                          evaluation of the new head always comes first.
#
# By default, the local worktree AND the local branch it held are cleaned up
# after a successful merge (#4100). Pass --no-cleanup-worktree to skip both
# (e.g., when other terminals may have their CWD inside the worktree, or the
# branch has unpushed commits you want to keep).
#
# Cleanup is restricted to Loom-managed worktrees (those containing the
# .loom-managed sentinel written by worktree.sh). Worktrees lacking the
# sentinel are treated as user-owned and never removed. Set
# LOOM_PRESERVE_WORKTREE=1 to disable cleanup unconditionally for a session.
#
# Local branch deletion (#4100): every cleanup path — the default
# .loom/worktrees/issue-N convention, a discovered Loom-managed worktree at a
# non-standard path, and even the case where no worktree exists at all —
# attempts to delete the merged PR's local branch. Safety is determined by
# whether the local branch tip equals the merged PR's head SHA (not by `git
# branch --merged`, which is always false for a squash merge): a matching tip
# uses `git branch -D` (every commit on the branch was part of the merged PR);
# a non-matching tip (unpushed local work) falls back to `git branch -d`,
# which keeps the branch and reports it instead of force-deleting. A branch
# checked out as the current HEAD (or in another worktree) is never deleted —
# git itself refuses, and the refusal is reported with a specific message
# rather than the generic "unmerged commits" warning. The repo's default
# branch is never a delete target.
#
# Primary-checkout auto-cleanup (#5015): the one case above that is "checked
# out as the current HEAD" AND is specifically the repo's PRIMARY checkout
# (not a linked worktree) gets one extra step: if the tip-matches-head safety
# check already passed AND the primary checkout's working tree is clean with
# no stash entries (re-checked immediately before the mutating step, not
# cached), the script checks out the default branch there and force-deletes
# the branch automatically. A dirty tree, a stash entry, a tip mismatch, or
# --no-cleanup-primary all fall back to printing the manual two-step
# instructions unchanged.
#
# Override: pass --worktree-path <dir> to opt into removing a non-Loom
# worktree (the sentinel guard is bypassed only when this flag is supplied).
# Discovery: if neither the default issue-N nor pr-N worktree exists, the
# script walks `git worktree list --porcelain` looking for a worktree whose
# branch matches the merged PR's head branch. It emits a hint (not an
# auto-remove) so the operator can re-run with --worktree-path.
#
# Exit codes (caller table: defaults/docs/merge-pr-exit-code-exceptions.md
# §"Exit-code table"). 3-7 are all "nothing merged, nothing failed": re-queue
# for a later pass, NEVER a failure comment; only exit 1 is a failure.
#   0 = merged
#   1 = failed / refused (a real rejection; see the refusal text).
#   3 = PR head moved past the SHA this attempt gated on (#5579): still
#       Judge-approved, its diff just changed underneath it. See "Merge-ancestry
#       detection trap" in the doc above for why ancestry can't verify this.
#   4 = retired (#12063 slice 1): the #8248 freshness guard and its #8508
#       re-date remedy are gone; never emitted.
#   5 = held: the forge refused the merge call because a required status
#       check is pending or red on this head (#12063 slice 2). Re-queue.
#   7 = held by a sequencing order (#11330): the PR carries loom:pr +
#       loom:sequenced and no other blocker. Prints `LOOM-SEQUENCED-HOLD
#       pr=<N> after=<M>`; the predecessor landing clears it (#9378). Re-queue
#       later; it has no cap of its own (sequence-eval owns release).
#   8 = --expected-head refused (#11859): the PR head is not the SHA the
#       caller evaluated. Prints `LOOM-EXPECTED-HEAD-MISMATCH pr=<N>
#       expected=<sha> actual=<sha|unknown>`. Nothing written; the caller's
#       evaluation is stale, so re-evaluate the new head, never retry blind.

set -euo pipefail

# ANSI color codes (one line — file-size-ratchet offset for the #8248 guard
# above; verbatim, behavior-preserving join of the five assignments).
RED='\033[0;31m'; GREEN='\033[0;32m'; BLUE='\033[0;34m'; YELLOW='\033[1;33m'; NC='\033[0m'

error() { echo -e "${RED}Error: $*${NC}" >&2; exit 1; }
# info/success/warning joined onto one line: file-size/shell-budget offset for #11859's --expected-head.
info() { echo -e "${BLUE}$*${NC}"; }; success() { echo -e "${GREEN}$*${NC}"; }; warning() { echo -e "${YELLOW}$*${NC}"; }
# #5579: distinct from error() (exit 1) — see "Exit codes" above. Emits to
# stderr like error() so it is visible in logs, but exits 3 so the caller can
# tell "re-queue" from "genuinely failed" without parsing message text.
# Parameters: $1 = error message (may include forge API text), $2 = stale SHA (optional),
# $3 = current head SHA (optional). If both SHAs provided, includes them in output.
error_head_moved() {
  local msg="$1" stale_sha="${2:-}" current_sha="${3:-}"
  if [[ -n "$stale_sha" && -n "$current_sha" ]]; then
    echo -e "${YELLOW}PR head moved during merge attempt (stale approval, not a failure):${NC}" >&2
    echo -e "${YELLOW}  Merge gated on (stale):    $stale_sha${NC}" >&2
    echo -e "${YELLOW}  Current head SHA:         $current_sha${NC}" >&2
    echo -e "${YELLOW}  Details: $msg${NC}" >&2
  else
    echo -e "${YELLOW}PR head moved during merge attempt (stale approval, not a failure): $msg${NC}" >&2
  fi
  exit 3
}

# #11859: --expected-head's fail-closed pin. Distinct exit 8: unlike 3 (a race
# lost mid-merge, re-queue) the caller's evaluation itself is of another tree.
_check_expected_head() { [[ -z "$EXPECTED_HEAD_SHA" || "$1" == "$EXPECTED_HEAD_SHA" ]] && return 0; echo "LOOM-EXPECTED-HEAD-MISMATCH pr=$PR_NUMBER expected=$EXPECTED_HEAD_SHA actual=${1:-unknown}"; echo -e "${RED}Error: Merge refused: PR #$PR_NUMBER head is ${1:-unreadable}, not the --expected-head $EXPECTED_HEAD_SHA the caller evaluated (#11859). Nothing was merged or written; re-evaluate the current head.${NC}" >&2; exit 8; }

# Which route a FAILED merge's forge error TEXT sends the retry loop below down
# (#8191 slice). Prints one of: merge-in-progress (HTTP 405), head-mismatch
# (#5579 — the PR's OWN head moved past the SHA we gated on, so retrying would
# either fail again or silently merge a diff Judge never approved),
# base-modified (the PR's BASE fell behind; rebase-and-retry is correct), other
# (stop). String provenance stays documented on forge_merge_pr in
# lib/forge-helpers.sh — GitHub REST and Gitea verified against each forge's own
# source/spec, the GitHub GraphQL `expectedHeadOid` spelling best-effort from the
# retired server-side arm (#8427).
#
# This replaces three separate `grep` matchers in three separate `if` blocks
# whose relative ORDER was the entire safety property: a head-mismatch reaching
# the base-modified arm answers a moved head with forge_update_branch and another
# merge attempt. Nothing asserted that order except an `awk` scan over THIS
# FILE's source text looking for which `grep` appeared first. It is now one
# ordered `match` in loom-daemon/src/merge_pr/response.rs, pinned by a
# differential against the frozen retired ladder
# (loom-daemon/tests/merge_pr_response_differential.rs) and by the precedence
# unit tests beside the module. Asymmetric case-sensitivity is preserved
# verbatim: the head-mismatch alternation was `grep -Ei`, its two siblings bare
# `grep -q`.
#
# Returns 3 when no route could be OBTAINED — which the caller must never
# collapse into the `other` route. Fails CLOSED there, deliberately: the routes
# are not interchangeable, so an unresolvable binary leaves only "guess a route"
# or "refuse and say which happened", and only the second is distinguishable
# from a merge verdict afterwards. It cannot stop a healthy merge — a merge that
# SUCCEEDS never reaches this function; only one that already failed does.
#
# `printf '%s'`, not the retired `echo "$1"`: bash's `echo` silently swallows an
# argument that is exactly -n/-e/-E, and the response is arbitrary forge bytes.
# The differential's corpus includes those three inputs, so that substitution is
# checked to change no answer rather than assumed equivalent.
_classify_merge_response() { local _k; _k="$(printf '%s' "$1" | "${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr classify-response 2>/dev/null)" || _k=""; [[ "$_k" == "LOOM-MERGE-RESPONSE "* ]] || return 3; printf '%s' "${_k#LOOM-MERGE-RESPONSE }"; }

# _mr_say KEY -- the KEY<TAB>text line of the current `merge-pr merge-route`
# answer ($_MR), or a generic line when the route was synthesized without one.
_mr_say() { local l="${_MR#*$'\n'"$1"$'\t'}"; [[ "$l" != "$_MR" ]] || l="PR #$PR_NUMBER: merge retry, step $1 (no wording: 'merge-pr merge-route' unavailable)"; printf '%s' "${l%%$'\n'*}"; }

# Early help check — runs before any git/forge initialization so --help works
# in any directory and without forge authentication. The text (option list,
# cleanup semantics, exit-code table) is `loom-daemon merge-pr usage`
# (loom-daemon/src/merge_pr/usage.rs + usage.txt, #8191 slice), printed only
# behind its LOOM-MERGE-PR-USAGE sentinel so no other binary's output can pass
# for it. Resolved like _mp_worktree (LOOM_DAEMON_SELF_BIN first, #8134), so
# a suite can pin the implementing binary without touching LOOM_DAEMON_BIN.
# Fails OPEN toward exit 0: a missing/older daemon gets a one-line
# usage plus a pointer on stderr. Help gates nothing, and exit 1 would read as
# a failed merge to a caller probing the script. $_MP_NL, not an inline $'\n'
# pattern: bash 3.2 reads the latter literally (see #8191's partial-comment slice).
if [[ $# -gt 0 ]] && { [[ "$1" == "--help" ]] || [[ "$1" == "-h" ]]; }; then
  _MP_NL=$'\n'; _MP_USAGE="$("${LOOM_DAEMON_SELF_BIN:-${LOOM_DAEMON_BIN:-loom-daemon}}" merge-pr usage 2>/dev/null)" || _MP_USAGE=""
  if [[ "$_MP_USAGE" == "LOOM-MERGE-PR-USAGE$_MP_NL"* ]]; then printf '%s\n' "${_MP_USAGE#*"$_MP_NL"}"; exit 0; fi
  echo "Usage: ./.loom/scripts/merge-pr.sh <pr-number> [--dry-run] [--auto] [--no-cleanup-worktree] [--worktree-path <dir>] [options]"
  echo "Full help is 'loom-daemon merge-pr usage', which '${LOOM_DAEMON_SELF_BIN:-${LOOM_DAEMON_BIN:-loom-daemon}}' could not print (missing, or predates it). Roll this host: ${BASH_SOURCE[0]%/*}/cli/loom-daemon-update.sh --fetch" >&2
  exit 0
fi

# Find the main repository root (works from worktrees too)
# When run from a worktree, git rev-parse --show-toplevel returns the worktree path,
# not the main repository. This function navigates via the gitdir to find the actual root.
find_main_repo_root() {
  local dir
  dir="$(git rev-parse --show-toplevel 2>/dev/null)" || return 1

  # Check if this is a worktree (has .git file, not directory)
  if [[ -f "$dir/.git" ]]; then
    local gitdir
    gitdir=$(cat "$dir/.git" | sed 's/^gitdir: //')
    # gitdir is like /path/to/repo/.git/worktrees/issue-123
    # main repo is 3 levels up from there
    local main_repo
    main_repo=$(dirname "$(dirname "$(dirname "$gitdir")")")
    if [[ -d "$main_repo/.loom" ]]; then
      echo "$main_repo"
      return 0
    fi
  fi

  # Not a worktree or fallback - return the git root
  echo "$dir"
}

REPO_ROOT="$(find_main_repo_root)" || \
  error "Not in a git repository"

# Source forge helpers for multi-forge support
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib/forge-helpers.sh"
# lib/worktree-root.sh is NOT sourced any more (#8191 slice): the overridden-root
# resolution (#3530) that post-merge cleanup needs is `loom-daemon merge-pr
# cleanup-paths`, which resolves it through loom-daemon's own port of that helper
# (loom-daemon/src/worktree_root.rs, `worktree_root_readable`) rather than a
# second copy reached through a sourced lib. See the cleanup block at the bottom.
# Worktree-removal ledger (#5950) — post-merge cleanup is one of several
# independent removers; every one of them records to the same file so
# "what removed this worktree?" has a single answer. Sourced defensively with a
# no-op fallback: the ledger is diagnostic only and must never be able to break
# a merge on a partially-resynced .loom/.
if [[ -f "$SCRIPT_DIR/lib/worktree-removal-log.sh" ]]; then
  # shellcheck source=lib/worktree-removal-log.sh
  source "$SCRIPT_DIR/lib/worktree-removal-log.sh"
else
  loom_record_worktree_removal() { :; }
fi
# Cargo target-dir reclaim (#7239). Post-merge cleanup is the removal path most
# worktrees actually take, so a redirected CARGO_TARGET_DIR/build.target-dir
# would leak its per-worktree build output here more than anywhere else.
#
# No lib is sourced for it any more (#9153): the resolve/reclaim pair is
# `loom-daemon cargo-target-dir resolve|reclaim`, called inline from
# `_remove_loom_worktree` below. That is the same Rust decision `worktree.sh
# remove`, `loom-daemon clean` and the reaper already share, so the fourth copy
# of the rules is gone rather than merely deduped — and the degraded path needs
# no no-op twins: both calls are `2>/dev/null || true`, so a host with no
# resolvable daemon (or one predating the verbs) simply performs no reclaim,
# which is the pre-#7239 behaviour. See the `requires-daemon:` block below.
# Shared "has this branch landed?" primitive (#7812) — the one implementation
# of the question the branch-delete and worktree-preserve guards below ask.
# Required, like forge-helpers.sh above: every branch-delete decision in this
# script depends on it, and a missing lib must fail loudly at startup rather
# than silently degrade a destructive decision to a guess.
# shellcheck source=lib/branch-landed.sh
source "$SCRIPT_DIR/lib/branch-landed.sh"
# Default-branch resolver (#4100) — the local-branch delete guard must never
# target the repo's default branch. Sourced defensively: a repo where this
# fails to resolve (e.g. no network + no origin/HEAD symref) still falls back
# to the literal "main"/"master" check in _maybe_delete_local_branch below.
DEFAULT_BRANCH_NAME=""
# Sourced UNCONDITIONALLY (#9106). This lib carries check_branch_name, the
# ref-operand validator every forge-derived branch name below must pass before
# it reaches a git argv, and a fail-closed security guard must not be optional:
# the old `if [[ -f ... ]]` guard let a partially-resynced .loom/ drop the
# validator silently. A missing lib now aborts here instead.
# shellcheck source=lib/default-branch.sh
source "$SCRIPT_DIR/lib/default-branch.sh"
DEFAULT_BRANCH_NAME="$(cd "$REPO_ROOT" && loom_default_branch 2>/dev/null || true)"
forge_detect

# Use gh-cached for read-only queries to reduce API calls (see issue #1609)
# Verify the Python interpreter works too — a broken runtime (e.g. unaccepted
# Xcode license) would make every subsequent gh call fail with a misleading error.
GH_CACHED="$REPO_ROOT/.loom/scripts/gh-cached"
if [[ "$FORGE_TYPE" == "github" ]] && [[ -x "$GH_CACHED" ]] && "$GH_CACHED" --version &>/dev/null; then
    GH="$GH_CACHED"
else
    GH="gh"
fi

# #9548: the repo is vetted, not merely resolved. forge_get_repo_nwo asked
# `gh repo view`, which prefers an `upstream` remote over `origin`, and nothing
# checked that the answer was a repo this installation manages or can write.
# loom_write_repo (lib/forge-helpers.sh, sourced above) returns origin only
# when gh resolves there too, the repo is managed here, and the credential has
# WRITE; otherwise the merge is refused before any read or write.
REPO_NWO="$(loom_write_repo "${LOOM_REPO:-}")" || error "Merge refused (#9548): this checkout's repository is not one this installation may write to (reason above)."
# Detect which merge strategy the target repo actually allows (#7754) --
# previously every call site below hardcoded "squash", which fails outright
# ("Squash merges are not allowed on this repository") on any repo that has
# squash-merge disabled. Read once per invocation; fails open to "squash"
# (this script's pre-#7754 behavior) on any probe failure.
REPO_MERGE_METHOD="$(forge_detect_merge_method "$REPO_NWO" "$GH" 2>/dev/null || echo squash)"

# Parse arguments
PR_NUMBER=""; CLEANUP_WORKTREE=true
# CLEANUP_PRIMARY_CHECKOUT (#5015): gates the automatic primary-checkout
# branch cleanup performed by _maybe_delete_local_branch. Defaults on
# (mirrors CLEANUP_WORKTREE's default); --no-cleanup-primary opts out.
CLEANUP_PRIMARY_CHECKOUT=true; DRY_RUN=false; WORKTREE_PATH_OVERRIDE=""; ALLOW_STACKED_CHILDREN=false
# ALLOW_UNAPPROVED (#7419): bypasses the loom:pr review-signal guard
# (_check_loom_pr_label below). Off by default — a missing loom:pr label
# hard-blocks the merge unless the operator explicitly opts in here.
# ALLOW_RED_TREE (#10026): bypasses _check_tree_checks. MERGE_METHOD_REQUESTED (#8845): explicit
# --merge-method override, resolved against REPO_MERGE_METHOD once parsing is done.
ALLOW_UNAPPROVED=false; ALLOW_RED_TREE=false; MERGE_METHOD_REQUESTED=""; EXPECTED_HEAD_SHA=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --cleanup-worktree|--cleanup-primary) shift ;;  # no-ops, both cleanups are now the default (one arm: #11859 offset)
    --no-cleanup-worktree) CLEANUP_WORKTREE=false; shift ;;
    --expected-head) [[ $# -ge 2 && "$2" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] || error "--expected-head requires a full lowercase hex commit SHA (40 or 64 chars)"; EXPECTED_HEAD_SHA="$2"; shift 2 ;;
    --no-cleanup-primary) CLEANUP_PRIMARY_CHECKOUT=false; shift ;;
    # The two --worktree-path arms are joined onto one line each (verbatim,
    # behavior-preserving) — the shell-budget ratchet's option 2
    # (.loom/docs/shell-language-policy.md).
    --worktree-path) [[ $# -lt 2 ]] && error "--worktree-path requires a value"; WORKTREE_PATH_OVERRIDE="$2"; shift 2 ;;
    --worktree-path=*) WORKTREE_PATH_OVERRIDE="${1#--worktree-path=}"; [[ -z "$WORKTREE_PATH_OVERRIDE" ]] && error "--worktree-path= requires a value"; shift ;;
    --dry-run) DRY_RUN=true; shift ;;
    --auto) warning "--auto is a no-op (#12063 slice 2): merge-pr no longer waits for checks"; shift ;;
    --allow-stacked-children) ALLOW_STACKED_CHILDREN=true; shift ;;
    --allow-unapproved) ALLOW_UNAPPROVED=true; shift ;;
    --allow-red-tree) ALLOW_RED_TREE=true; shift ;;
    --allow-red-required) [[ $# -lt 2 || -z "$2" ]] && error "--allow-red-required requires a context name"; warning "--allow-red-required is a no-op (#12063 slice 2): GitHub gates required checks"; shift 2 ;;
    # #8845's --merge-method arm is paid for by flattening the `*)` positional
    # arm below onto one line — the shell-budget ratchet's option 2
    # (.loom/docs/shell-language-policy.md), same offsetting convention
    # --worktree-path already documents in this file. Validation here is vocabulary-only (mirrors PR_NUMBER's own
    # ^[0-9]+$ check below); which methods the REPO actually allows is decided
    # in loom-daemon (loom-daemon/src/forge_cmd.rs), not here.
    --merge-method) [[ $# -lt 2 ]] && error "--merge-method requires a value"; case "$2" in squash|merge|rebase) ;; *) error "--merge-method must be one of: squash, merge, rebase (got: $2)" ;; esac; MERGE_METHOD_REQUESTED="$2"; shift 2 ;;
    -*)  error "Unknown option: $1" ;;
    *) if [[ -z "$PR_NUMBER" ]]; then PR_NUMBER="$1"; else error "Unexpected argument: $1"; fi; shift ;;
  esac
done

[[ -z "$PR_NUMBER" ]] && error "Usage: merge-pr.sh <pr-number> [--no-cleanup-worktree] [--no-cleanup-primary] [--worktree-path <dir>] [--dry-run] [--auto] [--allow-stacked-children] [--allow-unapproved] [--allow-red-tree] [--allow-red-required <context>]... [--merge-method squash|merge|rebase] [--expected-head <sha>]"
[[ "$PR_NUMBER" =~ ^[0-9]+$ ]] || error "PR number must be numeric: $PR_NUMBER"

# #8845: an explicit --merge-method overrides REPO_MERGE_METHOD's auto-detect
# above, but only after loom-daemon VALIDATES it against the repo's actually
# allowed strategies (loom-daemon/src/forge_cmd.rs, `forge merge-method`) --
# exit 0 = allowed (its stdout is the method, unchanged); exit 1 = requested
# but disallowed, a hard refusal naming the allowed set (never a silent
# fallback to squash, the #8845 bug); any other exit (no daemon, or a Gitea
# decline -- gitea repo-settings probing isn't native yet) degrades to the
# unvalidated request as-is, same as this script's pre-#8845 posture toward
# an unverifiable input, with a warning so the gap is visible rather than silent.
# #8878: the binary probed and invoked is LOOM_DAEMON_BIN when set (as
# every daemon-backed guard here does) -- that override is exactly what
# _mp_daemon_roll_hint tells operators to export when no released daemon carries
# `forge merge-method` yet, so probing PATH's older binary instead threw away the
# validation they had just built and degraded to the unvalidated path anyway.
# The expansion is repeated inline rather than hoisted into a variable on this
# same line: check-daemon-subcommand-versions.sh only registers a binary-holding
# variable from a LINE-LEADING assignment, so a mid-line one would make the
# requires-daemon marker below read as stale (verified -- it fires).
if [[ -n "$MERGE_METHOD_REQUESTED" ]]; then if command -v "${LOOM_DAEMON_BIN:-loom-daemon}" &>/dev/null; then _MPM_RC=0; _MPM_OUT="$("${LOOM_DAEMON_BIN:-loom-daemon}" forge merge-method --repo "$REPO_NWO" --requested "$MERGE_METHOD_REQUESTED" 2>&1)" || _MPM_RC=$?; if [[ $_MPM_RC -eq 0 ]]; then REPO_MERGE_METHOD="$_MPM_OUT"; elif [[ $_MPM_RC -eq 1 ]]; then error "Merge blocked: $_MPM_OUT"; else warning "'${LOOM_DAEMON_BIN:-loom-daemon}' forge merge-method could not validate --merge-method $MERGE_METHOD_REQUESTED (exit $_MPM_RC: $_MPM_OUT); using it unvalidated"; REPO_MERGE_METHOD="$MERGE_METHOD_REQUESTED"; fi; else warning "loom-daemon not found ('${LOOM_DAEMON_BIN:-loom-daemon}'); using --merge-method $MERGE_METHOD_REQUESTED unvalidated"; REPO_MERGE_METHOD="$MERGE_METHOD_REQUESTED"; fi; fi

# Validate --worktree-path early (before any network calls) so bad input
# fails fast. The path must be a real directory and must appear in the
# repository's worktree list. We resolve to an absolute path via cd so
# downstream comparisons against the porcelain output work cleanly.
if [[ -n "$WORKTREE_PATH_OVERRIDE" ]]; then
  if [[ ! -d "$WORKTREE_PATH_OVERRIDE" ]]; then
    error "--worktree-path does not exist or is not a directory: $WORKTREE_PATH_OVERRIDE"
  fi
  _WT_ABS="$(cd "$WORKTREE_PATH_OVERRIDE" 2>/dev/null && pwd -P)" || \
    error "--worktree-path could not be resolved: $WORKTREE_PATH_OVERRIDE"
  # Verify the path is actually a worktree of this repo (#8191 slice):
  # `loom-daemon merge-pr worktree-contains` shares the porcelain parser
  # worktree-primary/worktree-branch-for/worktree-find-by-branch already use
  # (loom-daemon/src/merge_pr/worktrees.rs), which fixed this same family's
  # #3717 space-in-path truncation. Exit 0 = registered, 1 = parsed and NOT
  # registered, anything else = the check did not run at all (missing/older
  # daemon) -- WARN and still merge, but DECLINE the override cleanup and keep
  # the worktree: a guard that did not run must refuse the removal rather than
  # wave it through (the #3710 rule; never lean on `git worktree remove` to
  # refuse an unverified path). Skipped cleanup is always recoverable.
  _WTC_RC=0; git -C "$REPO_ROOT" worktree list --porcelain 2>/dev/null | "${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr worktree-contains --path "$_WT_ABS" 2>/dev/null || _WTC_RC=$?
  if [[ $_WTC_RC -eq 1 ]]; then error "--worktree-path is not a registered worktree of this repository: $WORKTREE_PATH_OVERRIDE (resolved: $_WT_ABS)"; elif [[ $_WTC_RC -ne 0 ]]; then warning "--worktree-path's registered-worktree check (#8191 slice) did not run — 'merge-pr worktree-contains' exited $_WTC_RC (a loom-daemon predating this slice has no such verb). The merge still proceeds, but $_WT_ABS is left untouched and worktree cleanup is skipped: an unverified path is never removed. $(! declare -F _mp_daemon_roll_hint >/dev/null || _mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_BIN:-loom-daemon}" 2>/dev/null || true)")"; elif [[ "$CLEANUP_WORKTREE" == "false" ]]; then warning "--worktree-path was supplied but --no-cleanup-worktree wins; no cleanup will occur"; fi
  if [[ $_WTC_RC -eq 0 ]]; then WORKTREE_PATH_OVERRIDE="$_WT_ABS"; else WORKTREE_PATH_OVERRIDE=""; CLEANUP_WORKTREE=false; fi
  unset _WTC_RC _WT_ABS
fi

# Fetch PR state — UNCACHED (#8550). $GH may be the `gh-cached` wrapper, whose
# short TTL made this read return the PR's PRE-change label set for anything
# started inside the cache window. That is exactly the window an operator hold
# release lands in (remove `loom:operator`, merge immediately), and the #8112
# verdict-contradiction guard below — whose ONLY input is $PR_LABELS derived
# from this fetch — then correctly refused a merge on labels that no longer
# existed. Labels here are verdict-gating/merge-gating data, i.e. the
# deliberately-uncached class in docs/gh-cached.md, the same class the 15+
# `forge_get_pr_nocache` rechecks further down already belong to.
# A failed fetch names its cause -- status, meaning, forge message (#9192).
_PRF_ERR="$(mktemp)"
PR_JSON=$(forge_get_pr_nocache "$REPO_NWO" "$PR_NUMBER" "$GH" 2>"$_PRF_ERR") || error "Could not fetch PR #$PR_NUMBER -- $(forge_fetch_error_cause "$(cat "$_PRF_ERR"; rm -f "$_PRF_ERR")" "$PR_JSON")"; rm -f "$_PRF_ERR"; unset _PRF_ERR

# Combined onto two lines (net code-line offset for the #8112 guard added
# below — file-size-policy.md's "remove at least as much as you added"; a
# verbatim, behavior-preserving chain, not a reformat of anything else).
PR_STATE=$(echo "$PR_JSON" | jq -r '.state'); PR_MERGED=$(echo "$PR_JSON" | jq -r '.merged'); PR_BRANCH=$(echo "$PR_JSON" | jq -r '.head.ref')
PR_TITLE=$(echo "$PR_JSON" | jq -r '.title')
# Head SHA (#4100): the safety criterion for local-branch deletion. A local
# branch whose tip equals this SHA carries no commits absent from the merged
# PR, so it is safe to force-delete even though it will never satisfy
# `git branch --merged` after a squash merge.
PR_HEAD_SHA=$(echo "$PR_JSON" | jq -r '.head.sha // empty')
# Labels (#7419): the loom:pr review-signal guard's only input. Both forges'
# forge_get_pr responses already carry a `labels` array in this shape, so no
# extra API call is needed beyond the fetch above.
PR_LABELS=$(echo "$PR_JSON" | jq -r '.labels[]?.name // empty' 2>/dev/null || true)
# #9106: $PR_BRANCH is `.head.ref` — chosen by whoever opened the PR. Everything
# downstream (the _recheck_mergeable_before_refusal fetch, branch_landed,
# update-ref refs/loom/parent/<branch>, the local `git branch -D`) puts it in a
# git argv, so it is validated ONCE here and the merge is denied outright if it
# is not a safe ref operand. The reason names the offending ref, so the refusal
# is greppable in the run log.
check_branch_name "$PR_BRANCH" "head branch of PR #$PR_NUMBER" || error "Merge blocked: PR #$PR_NUMBER's head branch is not a safe git ref operand (see the check_branch_name refusal above, #9106). Refusing before any git command runs on '$PR_BRANCH'. Rename the branch on the PR and re-run."

# Terminal-state gate: `loom-daemon merge-pr pr-state` (#8191 slice) — merged wins over
# closed. No verdict (missing/older daemon) falls back to the retired PR_MERGED/PR_STATE predicate, like revalidate-head.
case "$("${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr pr-state --state "$PR_STATE" --merged "$PR_MERGED" 2>/dev/null)" in
  "LOOM-PR-STATE MERGED") warning "PR #$PR_NUMBER is already merged"; exit 0 ;;
  "LOOM-PR-STATE CLOSED") error "PR #$PR_NUMBER is closed (not merged)" ;; "LOOM-PR-STATE OPEN") ;;
  *) [[ "$PR_MERGED" != "true" ]] || { warning "PR #$PR_NUMBER is already merged"; exit 0; }; [[ "$PR_STATE" != "closed" ]] || error "PR #$PR_NUMBER is closed (not merged)" ;; esac
# #11859: pin the caller's evaluated head BEFORE any gate below can write (a
# parent-ref pin, a label) on behalf of a tree it never saw.
_check_expected_head "$PR_HEAD_SHA"

# ---------------------------------------------------------------------------
# Pre-merge merge-ordering guard (#3747, stacked-PR v2 item 2; reshaped by
# #7982 into pin-and-warn).
#
# Runs BEFORE both the auto-merge and synchronous-merge paths (that is why it is
# invoked here, above the "Merging PR" line — not next to item 1's POST-merge
# _auto_reconcile_stacked_children at the bottom of the merge flow).
#
# The decision is `loom-daemon merge-pr stacked-children` (Rust,
# loom-daemon/src/merge_pr/stacked_children.rs — #8191 slice): discover open
# CHILD PRs still targeting this parent branch via a LIVE forge query (never the
# daemon registry), then ESTABLISH the postcondition reconcile-stack.sh needs by
# pinning the parent tip to refs/loom/parent/<branch> rather than merely
# asserting it — GitHub deletes the branch synchronously inside the merge call,
# so item 1's post-merge rebase would otherwise race the repo's own
# delete_branch_on_merge setting and lose. It hard-blocks on exactly one case:
# the tip could not be pinned at all. --allow-stacked-children skips past that
# block; --dry-run reports the would-be outcome and writes no ref.
#
# It emits `CHILDREN`/`PIN-WRITTEN`/`WARNING`/`BLOCK<TAB>line` records, replayed
# below through this script's own warning/error so the operator-visible text is
# unchanged from before the port. `CHILDREN` carries the pre-merge snapshot the
# post-merge reconcile prefers (#8010 item 2); `PIN-WRITTEN` fires only where a
# ref was actually written, which is what the item-3 re-pin must gate on (a
# bypass finds children without pinning). The two globals are plain (non-local)
# assignments so they survive this function returning, and are read as
# `${VAR:-}` everywhere else so a standalone invocation still behaves as before.
#
# Exit 1 WITH a BLOCK record refuses; any other non-zero is a guard FAULT and
# warns-and-proceeds. Unlike every gate below this one, that is fail-OPEN, and
# deliberately: this guard protects a best-effort POST-merge cleanup step, not
# the question of whether this tree may merge, so a skip costs one manual
# reconcile against a SHA the warning prints — while failing closed would stop
# every merge on a host whose daemon lags one release, nearly all of which have
# no stacked children at all. It is also safe by construction: the fail-CLOSED
# `verdict-contradiction` gate runs on this same binary further down, so a
# daemon too old for this verb cannot reach the merge anyway. Full argument:
# the module docs on loom-daemon/src/merge_pr/stacked_children.rs.
#
# _mp_daemon_roll_hint is not yet defined this early in the script (it lands
# with the version-floor block below), hence the `declare -F` probe and the
# inline roll command — a best-effort diagnostic must never be able to abort
# the warning it decorates.
_check_no_open_stacked_children() {
  # Only GitHub can have Loom-style stacked children; the parent-branch shape
  # gate is the Rust side's (it returns before any forge call).
  [[ "$FORGE_TYPE" == "github" ]] || return 0
  local out rc=0 level text children_json="" warn="" blk="" flags=()
  [[ "${DRY_RUN:-false}" != "true" ]] || flags+=(--dry-run)
  [[ "${ALLOW_STACKED_CHILDREN:-false}" != "true" ]] || flags+=(--allow-stacked-children)
  out="$("${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr stacked-children --repo "$REPO_NWO" --repo-root "$REPO_ROOT" --branch "$PR_BRANCH" --head-sha "$PR_HEAD_SHA" --pr "$PR_NUMBER" ${flags[@]+"${flags[@]}"})" || rc=$?
  while IFS=$'\t' read -r level text; do case "$level" in
    CHILDREN) children_json="$text" ;;
    PIN-WRITTEN) STACKED_CHILDREN_PIN_WRITTEN=true ;;
    WARNING) warn+="${warn:+$'\n'}$text" ;;
    BLOCK) blk+="${blk:+$'\n'}$text" ;;
  esac; done <<< "$out"
  [[ -z "$children_json" ]] || STACKED_CHILDREN_JSON="$children_json"
  [[ -z "$warn" ]] || warning "$warn"
  [[ $rc -ne 1 || -z "$blk" ]] || error "$blk"
  [[ $rc -eq 0 ]] || warning "The merge-ordering guard (#3747 item 2) did not run — '${LOOM_DAEMON_BIN:-loom-daemon} merge-pr stacked-children' exited $rc (a loom-daemon predating #8191's slice has no such verb). Proceeding, because this guard establishes a postcondition for the POST-merge stacked reconcile rather than judging this tree: a skip costs at most one manual './.loom/scripts/reconcile-stack.sh <child-pr> $PR_BRANCH' against tip $PR_HEAD_SHA, never a wrong merge. Restore it by rolling this host: ${SCRIPT_DIR:-.loom/scripts}/cli/loom-daemon-update.sh --fetch. $(! declare -F _mp_daemon_roll_hint >/dev/null || _mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_BIN:-loom-daemon}" 2>/dev/null || true)")"
  return 0
}

# Invoke the guard before either merge path attempts the actual merge API call.
_check_no_open_stacked_children

# ---------------------------------------------------------------------------
# Pre-merge version policy guard (#7827): feature PRs must not hand-edit a
# version-bearing value — the merge workflow owns bumps (#7743). The guard is
# `loom-daemon merge-pr version-policy` (Rust, loom-daemon/src/merge_pr/
# version_policy.rs — #8191 slice): it runs the canonical
# check-defaults-version-bump.sh --forbid-bump against the merge base, from the
# PR head's copy when the PR's own commits change the version-policy machinery
# (#8284), and classifies pass / skip / block. It prints `WARNING`/`BLOCK<TAB>
# line` records, replayed here through warning/error; exit 1 = confirmed edit.
# Any other exit (a missing or older daemon) is a guard fault, and guard faults
# have always skipped with a warning here, never blocked: CI's
# defaults-version-bump-check job is the policy's primary enforcement (see the
# CLI module docs for why this gate alone does not fail closed).
_check_defaults_version_bump_collision() {
  local out rc=0 level text blk="" dry=(); [[ "${DRY_RUN:-false}" != "true" ]] || dry=(--dry-run)
  out="$("${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr version-policy --repo-root "$REPO_ROOT" --default-branch "${DEFAULT_BRANCH_NAME:-}" --branch "${PR_BRANCH:-}" --head-sha "${PR_HEAD_SHA:-}" --pr "${PR_NUMBER:-}" ${dry[@]+"${dry[@]}"})" || rc=$?
  while IFS=$'\t' read -r level text; do case "$level" in WARNING) warning "$text" ;; BLOCK) blk+="${blk:+$'\n'}$text" ;; esac; done <<< "$out"
  [[ $rc -ne 1 || -z "$blk" ]] || error "$blk"
  [[ $rc -eq 0 ]] || warning "Version policy guard (#7827) did not run — '${LOOM_DAEMON_BIN:-loom-daemon} merge-pr version-policy' exited $rc (a loom-daemon predating #8191's slice has no such verb). Skipping, as for any guard fault: CI's defaults-version-bump-check job still enforces the policy. $(! declare -F _mp_daemon_roll_hint >/dev/null || _mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_BIN:-loom-daemon}" 2>/dev/null || true)")"
  return 0
}

# Invoke this guard too, before either merge path attempts the actual merge
# API call — same reasoning as _check_no_open_stacked_children above.
_check_defaults_version_bump_collision

# Pre-merge workflow-scope guard (#10539): `loom-daemon merge-pr workflow-scope`.
# Exit 1 = PR touches .github/workflows/ and the token lacks `workflow`; anything else proceeds (fail open, so an older daemon is a skip).
_check_workflow_scope() { local out rc=0; [[ "${FORGE_TYPE:-github}" == "github" ]] || return 0; out="$("${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr workflow-scope --repo "$REPO_NWO" --pr "$PR_NUMBER" 2>/dev/null)" || rc=$?; [[ $rc -eq 1 ]] || return 0; [[ "${DRY_RUN:-false}" != "true" ]] || { warning "[dry-run] Would BLOCK: $out"; return 0; }; error "$out"; }
_check_workflow_scope

# _trusted_pr_comments <nwo> <pr> -- emit the bodies of `<nwo>`'s PR <pr>`
# comments whose authors Loom trusts as control-signal sources (#9548): a repo
# insider by author_association, one of THIS fleet's Apps, this daemon's own
# identity, or forge.trustedCommenters. The listing is fetched via the REST
# API (which carries user.login + author_association);
# forge_get_pr_comments's bodies-only shape drops authorship -- the exact
# hazard #9548 names -- so every control-phrase reader over PR comments must
# go through here instead (that helper is retired from lib/forge-helpers.sh,
# so no new reader can reach for it). An untrusted or unattributed marker
# counts as ABSENT. Gitea listings carry no author_association and filter to absence
# until a trust mapping exists (said on stderr, never swallowed silently).
# Always exits 0: a filter that cannot run yields nothing, and the CALLER
# decides whether that is fail-open advisory or fail-closed for its signal.
_trusted_pr_comments() {
  local nwo="$1" pr="$2" comments
  if [[ "${FORGE_TYPE:-github}" == "gitea" ]]; then
    echo "trusted-comments: comment authorship cannot be authenticated on Gitea yet (#9548) — comments read as absent" >&2
    return 0
  fi
  comments="$(gh api "repos/$nwo/issues/$pr/comments" --paginate 2>/dev/null || true)"
  [[ -n "$comments" ]] || return 0
  local filtered
  filtered="$(printf '%s\n' "$comments" | "${LOOM_DAEMON_BIN:-loom-daemon}" forge trusted-comments 2>/dev/null)" || {
    echo "trusted-comments: could not authenticate comment authors ('loom-daemon forge trusted-comments' failed); comments read as absent (#9548)" >&2
    return 0
  }
  [[ -n "$filtered" ]] || return 0
  printf '%s\n' "$filtered" | jq -r '.[].body' 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Pre-merge loom:pr review-signal guard (#7419).
#
# `loom:pr` is the only forge-visible statement that the CURRENT head passed
# Judge review. Champion's auto-merge path (champion-pr-merge.md) already
# refuses to merge without it, but this shared script — driven directly by
# humans and in-session agents, not just Champion — previously merged
# whatever PR it was pointed at regardless of label state. That gap let a
# real incident through: a Doctor rebase cleared `loom:pr` via the staleness
# guard, and a human running this script directly moments later squash-merged
# a head no Judge had reviewed, with zero friction at the one point it was
# cheap (#7419).
#
# Default is a hard block (error, exit 1) — the same shape as
# _check_no_open_stacked_children / _check_defaults_version_bump_collision
# above — printing the CURRENT label set and head SHA so the operator can see
# exactly what they are about to merge. --allow-unapproved bypasses the
# block (operator asserts responsibility, mirroring --allow-stacked-children
# and --worktree-path); the bypass is always recorded as a loud warning
# (mirrors --allow-stacked-children's own override warning) and, on a REAL
# run only (never --dry-run — a preview must have zero forge side effects),
# best-effort recorded as a PR comment audit trail too, mirroring the
# _post_premature_close_comment / partial-increment comment pattern already
# used elsewhere in this file. --dry-run reports the would-be block without
# exiting 1 or merging, same dry-run contract as both guards above.
#
# A present `loom:pr` is the overwhelmingly common case and must add zero
# overhead on that path: labels were already extracted from the initial
# $PR_JSON fetch into $PR_LABELS above, so this needs no extra API call to
# pass through.
#
# AC #3: when `loom:pr` IS present, also WARN (never hard-block — presence of
# loom:pr already means Judge approved SOME head, just possibly not the
# current one, which is a softer signal than the missing-label case above) if
# a Champion `<!-- champion:hold-state head=<sha> -->` marker (see
# champion-pr-merge.md's own hold-state tracking) names a SHA that differs
# from the current head. forge_get_pr's response has no `.comments` (unlike
# champion-pr-merge.md's own `gh pr view --json comments,...` fetch), so this
# reads them via _trusted_pr_comments (above), which keeps only
# trusted-authored comments (#9548).
#
# The marker extraction and the staleness comparison are
# `loom-daemon merge-pr hold-state` (Rust, loom-daemon/src/merge_pr/
# hold_state.rs -- #8191 slice). Only the forge READ stays here, so this
# script keeps owning the GitHub/Gitea split _trusted_pr_comments encodes.
# The retired `grep -o '...head=[0-9a-f]*' | tail -1 | sed` pipeline lost this
# warning silently in two ways the port fixes: `[0-9a-f]*` also matched the
# documentation line `head=<sha>` (quoted in champion-pr-merge.md and in this
# file), and `tail -1` then let that empty capture erase a real hold's SHA;
# and a bare substring anywhere -- prose, backticks, an example -- counted as
# recorded state, the hazard Champion's own reader answered with `startswith`
# (#5371). See the module docs for both, and for the fence-stripping
# divergence deliberately NOT taken.
#
# Advisory, so it fails OPEN, unlike every gate around it: a binary that
# cannot run this check has not found a reason to stop the merge, and turning
# "could not warn" into a refusal would make an advisory note more fatal than
# the gates. The fault is still said out loud rather than swallowed.
_check_champion_hold_state_staleness() {
  local comments msg rc=0
  # #9548: the champion:hold-state marker is a control phrase (it records
  # which tree Champion's hold/approval state was recorded against) and counts
  # only from a trusted author. _trusted_pr_comments emits bodies of
  # trusted-authored comments only; an untrusted or unattributed marker counts
  # as ABSENT -- it can neither raise a stale-hold warning nor mask a real
  # one. Gitea listings filter to absence (the helper says so on stderr).
  #
  # Advisory, so it fails OPEN, unchanged from #8191: a filter that cannot run
  # has not found a reason to stop the merge, and turning "could not warn"
  # into a refusal would make an advisory note more fatal than the gates. The
  # fault is still said out loud rather than swallowed.
  comments="$(_trusted_pr_comments "$REPO_NWO" "$PR_NUMBER")"
  [[ -n "$comments" ]] || return 0
  msg="$(printf '%s\n' "$comments" | "${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr hold-state --pr "$PR_NUMBER" --head-sha "$PR_HEAD_SHA" 2>/dev/null)" || rc=$?
  [[ $rc -eq 0 ]] || { warning "The champion:hold-state staleness check (#7419) did not run — '${LOOM_DAEMON_BIN:-loom-daemon} merge-pr hold-state' exited $rc (a loom-daemon predating #8191's slice has no such verb). Advisory only: the merge is NOT blocked by this, but nothing verified that Champion's recorded hold head matches the head being merged. Build or install loom-daemon (cargo build --release -p loom-daemon, or re-run the Loom installer) to restore it."; return 0; }
  [[ "$msg" == "LOOM-HOLD-STATE-CLEAN" || -z "$msg" ]] || warning "$msg"
}

# The decision itself (loom:pr present? overridden? blocked, and the exact
# message) is `loom-daemon merge-pr loom-pr-guard` (Rust,
# loom-daemon/src/merge_pr/loom_pr_guard.rs -- #7419, a slice of #8191). No
# requires-daemon floor of its own: an older binary that does not know this
# verb exits non-zero without the CLEAN sentinel like any other guard fault,
# and the fail-closed branch below refuses the merge exactly as if `loom:pr`
# were genuinely absent with no override -- the safe direction, never a
# silent pass. --allow-unapproved is threaded through as a flag because it
# changes the VERDICT (override vs. block), not just how a verdict is
# displayed -- unlike --dry-run, which stays entirely shell-side, wrapping
# the same "would block" text in a warning instead of an error, same shape as
# every other guard in this file.
_check_loom_pr_label() {
  local msg rc=0 flags=()
  [[ "$ALLOW_UNAPPROVED" == "true" ]] && flags+=(--allow-unapproved)
  msg="$(printf '%s\n' "$PR_LABELS" | "${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr loom-pr-guard --pr "$PR_NUMBER" --head-sha "$PR_HEAD_SHA" ${flags[@]+"${flags[@]}"} 2>/dev/null)" || rc=$?
  if [[ $rc -eq 0 && "$msg" == "LOOM-PR-GUARD-CLEAN" ]]; then
    _check_champion_hold_state_staleness
    return 0
  fi
  if [[ $rc -eq 0 && "$ALLOW_UNAPPROVED" == "true" && "$msg" == "loom:pr guard:"* ]]; then
    warning "$msg"
    # #8896: the audit comment is posted AT MOST ONCE per merge-pr.sh run. On
    # the --auto path this guard runs twice against the same merge — once at
    # queue time, once from _revalidate_merge_guards() after the settle-wait —
    # and an --allow-unapproved run with no loom:pr posted the identical
    # "Merge Proceeded Without loom:pr" comment both times. The warning above
    # is unconditional (it is the log record, and the second evaluation is a
    # real re-check worth logging); only the durable forge comment is deduped.
    # The flag is set only once the comment actually LANDS, so a failed first
    # post still leaves the post-wait re-check free to record the override.
    if [[ "$DRY_RUN" != "true" && "${_LOOM_PR_OVERRIDE_COMMENTED:-false}" != "true" ]]; then
      # The body is `loom-daemon merge-pr loom-pr-override-comment` (Rust,
      # loom-daemon/src/merge_pr/loom_pr_guard.rs, `override_comment` -- #8191
      # slice), byte-frozen from this shell. Same LOOM-MERGE-PR-COMMENT
      # sentinel protocol `_mp_post_partial_comment` uses, inlined here rather
      # than reused because this comment posts on $PR_NUMBER itself, not an
      # issue. Fails OPEN with a warning naming the gap: the merge and the
      # override both already happened, so a missing body costs only the note.
      local _lpoc_out _lpoc_rc=0 nl=$'\n'
      _lpoc_out="$(printf '%s\n' "$PR_LABELS" | "${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr loom-pr-override-comment --pr "$PR_NUMBER" --head-sha "$PR_HEAD_SHA" 2>/dev/null)" || _lpoc_rc=$?
      if [[ $_lpoc_rc -eq 0 && "$_lpoc_out" == "LOOM-MERGE-PR-COMMENT$nl"* ]]; then
        if forge_gh_comment_rl_safe "$REPO_NWO" "$PR_NUMBER" "${_lpoc_out#LOOM-MERGE-PR-COMMENT"$nl"}" 2>/dev/null; then _LOOM_PR_OVERRIDE_COMMENTED=true; else warning "Could not post loom:pr override audit comment on PR #$PR_NUMBER (merge proceeds anyway; the warning above is still the log record)"; fi
      else
        warning "The loom:pr override audit comment for PR #$PR_NUMBER (#7419) was NOT posted — 'merge-pr loom-pr-override-comment' exited $_lpoc_rc without the LOOM-MERGE-PR-COMMENT sentinel (a loom-daemon predating #8191's slice has no such verb). Advisory only: the merge and the override both already happened — only this explanatory note is missing, and an empty comment is never posted in its place. $(! declare -F _mp_daemon_roll_hint >/dev/null || _mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_BIN:-loom-daemon}" 2>/dev/null || true)")"
      fi
    fi
    return 0
  fi
  [[ $rc -eq 1 && "$msg" == "Merge blocked:"* ]] || msg="Merge blocked: PR #$PR_NUMBER's loom:pr review-signal guard (#7419) could not run — '${LOOM_DAEMON_BIN:-loom-daemon} merge-pr loom-pr-guard' exited $rc without a recognized verdict. A guard that cannot run refuses the merge rather than passing it: a caller cannot tell 'reviewed' from 'never checked', so only a positive signal is accepted. Build or install loom-daemon (cargo build --release -p loom-daemon, or re-run the Loom installer), then re-run this merge."
  if [[ "$DRY_RUN" == "true" ]]; then
    warning "[dry-run] Would BLOCK merge of PR #$PR_NUMBER: $msg"
    return 0
  fi
  error "$msg"
}

# Invoke the guard before either merge path attempts the actual merge API
# call — same reasoning as the two guards above.
_check_loom_pr_label

# #8191: prove the closing-reference analysis can actually RUN, here, while
# `set -e` is live and before anything has been mutated.
#
# _mp_refs()'s own `error` exit cannot do this. Every consumer calls it inside
# `$(...)` inside _check_partial_increment_close_conflict /
# _reset_partial_increment_labels, and both of those are invoked as `... ||
# true`. Bash suppresses `set -e` in that context, so the exit only kills the
# subshell: the caller receives an EMPTY string and reads it as "no references
# found" — the reading that closes an unfinished issue or reopens a correctly
# closed one. Review reproduced all five skew shapes reaching the merge that
# way, including the default rollout state (a resynced script against an
# installed binary that predates this subcommand and answers exit 2).
#
# Verifying in isolation, as the original fix did, tested the function and not
# the call shape. This is the call shape.

# ---------------------------------------------------------------------------
# Pre-merge verdict-label contradiction guard (#8112).
#
# A PR can carry BOTH `loom:pr` (approved) and a blocking verdict/hold label
# simultaneously — two concurrent Judge passes reaching different verdicts on
# the same head ~45s apart is a real, observed incident (PR #8076), not a
# hypothetical. `_check_loom_pr_label` above only checks loom:pr's ABSENCE; it
# has no way to see a contradicting label standing beside a PRESENT loom:pr.
#
# Judge/Doctor's Verdict-Time CAS Recheck (judge.md / doctor.md) and
# Champion's Verdict-State Janitor Part 1 (champion-pr-merge.md) already exist
# to prevent and auto-resolve exactly this contradiction — but both live in
# markdown-orchestrated agent steps, not in this script, so a human or agent
# invoking merge-pr.sh directly (the documented, canonical way to merge —
# never `gh pr merge`, see CLAUDE.md) bypasses them entirely. This guard is
# the backstop at the one point every merge path actually funnels through.
#
# Deliberately no bypass flag: --allow-unapproved overrides "nobody reviewed
# this head" (an absence of signal); this guard blocks "a reviewer explicitly
# said no" (a present, contradicting signal). Those are different acts, and
# only the first has a documented override. The fix for a real block here is
# a fresh Judge verdict, not a flag.
#
# ---------------------------------------------------------------------------
# DAEMON VERSION FLOOR (#8285)
# ---------------------------------------------------------------------------
# This script hard-requires two loom-daemon subcommands and fails CLOSED when
# the resolved binary predates either. Failing closed is right — an empty
# answer from the closing-reference analysis is indistinguishable from "no
# closing refs", which would silently close an unfinished issue — but the
# version floor has to be SAYABLE, or the refusal cannot tell an operator what
# to roll to. These two markers are that declaration. They are the single
# source of truth: `_mp_daemon_roll_hint` below reads them back out of
# ${BASH_SOURCE[0]} at refusal time, and
# scripts/check-daemon-subcommand-versions.sh enforces that no NEW daemon
# dependency lands here (or in any other shell script) without one.
#
# On 2026-09-18 a host running 0.19.161 against a `main` that carried 0.19.170+
# stopped every merge outright — `.loom/scripts` is a symlink into
# `defaults/scripts` in the primary checkout, so the floor moved the instant
# `main` was pulled, while the auto-update loop deferred ~4.5h behind the
# build-stampede guard (#8252). The refusal named neither the version nor the
# roll command. That is what these markers and the hint below fix.
#
# requires-daemon: merge-pr >= 0.19.464   the NEWEST fail-closed verb in this family, not the oldest (#8967): partial-conflict (#9246); the other fail-closed verbs are classify-response >= 0.19.456 (#9228), loom-pr-guard >= 0.19.375 (#7419/#8926) and verdict-contradiction >= 0.19.172 (#8112/#8124). #12063 slice 1 LOWERED it from 0.19.763 by deleting ci-result (#10444) and stale-checks (#8248), and slice 2 from 0.19.465 by deleting checks-failure: removing a verb never makes an older binary refuse. One marker covers the whole `merge-pr` family, so it MUST name the highest of them — a host that satisfied an older floor but not the newest fail-closed verb had every merge refused while the hint quoted a floor it already met. Fail-open verbs (tree-checks — opt-in, called only when merge.treeChecks is declared, then refusing on an older binary (#10026); hold-state, delete-branch, stacked-children, retarget-children — any non-0 exit keeps the merged parent's remote branch rather than deleting it (#9372), pr-state — no verdict falls back to the retired PR_MERGED/PR_STATE predicate (#8191 slice), version-policy, partial-reset, partial-comment, loom-pr-override-comment, closed-building, issue-close-gate, dirty-guard, worktree-preserve, worktree-contains, cleanup-paths — partial-comment and loom-pr-override-comment each render a POST-merge audit comment and skip the note rather than posting an empty one; the last four decline only the post-merge worktree removal, never the merge; worktree-preserve preserves the worktree when the verb is missing, worktree-contains declines --worktree-path's override cleanup and keeps that path when it is missing, and cleanup-paths leaves the cleanup targets unnamed so nothing is removed) deliberately do NOT raise it; the fail-direction table in tests/test-merge-pr-daemon-version-floor.sh enforces both halves.
# requires-daemon: merge-pr-refs >= 0.19.170   closing-reference analysis (#8191, landed in #8199)
# requires-daemon: forge optional   --merge-method validation (#8845); command -v probes first, and any non-0/1 exit (older daemon lacking the subcommand, or a Gitea decline) falls back to the unvalidated request with a warning
# The `merge-pr >=` floor above covers the whole subcommand group, including
# #8191's post-merge porcelain lookups (`merge-pr worktree-primary` /
# `worktree-branch-for` / `worktree-find-by-branch`, see _mp_worktree, plus the
# --worktree-path parse-time `worktree-contains` check and the `remove-gate`
# primary/sentinel guard, which fails CLOSED to a refused removal), and it is
# deliberately NOT raised to their landing version. Raising it refuses the MERGE
# on every host one release behind — the 2026-09-18 incident above — whereas a
# daemon missing only those leaf verbs declines post-merge CLEANUP: the two
# branch lookups degrade to "delete nothing" and the #3710 primary-worktree guard
# refuses the removal rather than force-removing on no evidence. Skipped cleanup
# is recoverable (`loom-clean`, the daemon's reaper, `worktree.sh remove`);
# removing the primary checkout is not. Leaving the floor where it is also keeps
# _mp_daemon_roll_hint's `${sub} >= ` lookup resolving to the merge-gate version,
# which is the one a refused MERGE should name.
# requires-daemon: cargo-target-dir optional   #9153 — the post-merge #7239 target-dir reclaim; without the resolve|reclaim verbs a daemon prints nothing, `$target_dir_resolved` stays empty and no reclaim is attempted, which is the pre-#7239 behaviour. A missed disk reclaim, never a failed merge: post-merge cleanup is best-effort by design and `loom-clean`, the daemon's reaper and `worktree.sh remove` all reclaim the same directory on their own schedule.
# requires-daemon: notify-cleared-blockers optional   #9102 — the post-merge close-triggered loom:blocked re-check; a daemon lacking the verb exits non-zero, `_notify_cleared_blockers` prints one warning and the merge proceeds. A delayed notice, never a failed merge: the next sweep's `check-stale-blocked` pre-wave pass reports the same stale block.
#
# Per-verb landing versions for FAIL-OPEN verbs (#9377, via #11167). MESSAGE
# TEXT ONLY, never gating: check-daemon-subcommand-versions.sh does not read
# this prefix, and nothing refuses on it. A fail-open verb deliberately does not
# raise the family `requires-daemon:` floor above, so without these the hint
# for its failure quoted that (unrelated, fail-closed) floor — which a host can
# already exceed while still lacking the verb (rjwalters/repo#494: told to roll
# to 0.19.172 while running 0.19.461, which predated issue-close-gate). Each
# version is `git show <landing commit>:VERSION` + 1. An unannotated verb falls
# back to the family floor, which is exactly the pre-#9377 message.
# daemon-verb-hint: merge-pr issue-close-gate >= 0.19.462   f4517bb46 (#9240), first released in v0.19.462
# daemon-verb-hint: merge-pr-refs loom-issue-trailer-warnings >= 0.19.512   d8fafb19a (merge of #9501, #9465), first released in v0.19.512
#
# _mp_daemon_roll_hint <subcommand> [resolved-bin] [verb] -- the concrete,
# host-local remediation for "your loom-daemon is too old for <subcommand>":
# the declared floor, what the resolved binary actually reports, the
# artifact-first roll command for THIS host, and the two fallbacks when no
# artifact carries the floor yet. [verb] is the sub-subcommand that actually
# failed: when it has a `daemon-verb-hint:` above, the message names
# '<subcommand> <verb>' and that verb's own version; otherwise the family
# floor. Written as a one-liner because merge-pr.sh is frozen by the
# file-size ratchet (scripts/check-file-size-budget.sh) and may not grow.
#
# Both reads carry `|| true` deliberately. merge-pr.sh runs under `set -euo
# pipefail`, and a bare `x="$(cmd)"` assignment adopts cmd's status — so an
# unreadable ${BASH_SOURCE[0]} or a `--version` that exits non-zero would abort
# THIS function partway, and the caller (an `error "… $(…)"` argument) would
# print a refusal with the remediation silently truncated off the end. A
# best-effort diagnostic must never be able to degrade the message it is
# decorating. (The `if` over a `[[ … ]] && { … }` is for the same reason stated
# defensively; bash exempts AND-lists from `set -e`, so that one is style.)
_mp_daemon_roll_hint() { local sub="${1:-}" bin="${2:-}" verb="${3:-}" min="" have=""; [[ -z "$verb" ]] || min="$(sed -n "/^# daemon-verb-hint: ${sub} ${verb} >= /{s|^# daemon-verb-hint: ${sub} ${verb} >= \\([0-9][0-9.]*\\).*|\\1|p;q;}" "${BASH_SOURCE[0]}" 2>/dev/null || true)"; if [[ -n "$min" ]]; then sub="$sub $verb"; else min="$(sed -n "/^# requires-daemon: ${sub} >= /{s|^# requires-daemon: ${sub} >= \\([0-9][0-9.]*\\).*|\\1|p;q;}" "${BASH_SOURCE[0]}" 2>/dev/null || true)"; fi; if [[ -n "$bin" && -x "$bin" ]]; then have="$("$bin" --version 2>/dev/null || true)"; have="${have%%$'\n'*}"; fi; printf "REMEDIATION: this script requires loom-daemon >= %s for '%s'%s. Roll THIS host, artifact-first: %s/cli/loom-daemon-update.sh --fetch — it resolves the newest published release >= the installed version, verifies its checksum (and signature when present), provisions it and restarts the daemon under its supervisor; then re-run this merge. If no release artifact carries %s yet (releases are cut at fleet-rollable boundaries, not on every VERSION bump — see .loom/docs/release-cadence.md), either build it yourself — cargo build --release -p loom-daemon — and export LOOM_DAEMON_BIN=<repo>/target/release/loom-daemon, or pin LOOM_DAEMON_BIN to an existing build that already has '%s'. Confirm before re-running: %s --version && %s %s --help" "${min:-<undeclared>}" "$sub" "${have:+ (the resolved binary reports: $have)}" "${SCRIPT_DIR:-.loom/scripts}" "${min:-that version}" "$sub" "${bin:-loom-daemon}" "${bin:-loom-daemon}" "$sub"; }
_check_verdict_label_contradiction() { local msg rc=0; msg="$(printf '%s\n' "$PR_LABELS" | LOOM_SEQUENCED_HOLD_REPO="${REPO_NWO:-}" "${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr verdict-contradiction --pr "$PR_NUMBER" --head-sha "$PR_HEAD_SHA" 2>/dev/null)" || rc=$?; [[ $rc -eq 0 && "$msg" == "LOOM-VERDICT-CLEAN" ]] && return 0; if [[ $rc -eq 7 && "$msg" == "LOOM-SEQUENCED-HOLD "* ]]; then [[ "$DRY_RUN" != "true" ]] || { warning "[dry-run] Would HOLD merge of PR #$PR_NUMBER (exit 7): $msg"; return 0; }; echo "${msg%%$'\n'*}"; warning "${msg#*$'\n'}" >&2; warning "Exiting 7: held by a sequencing order (#11330); nothing was written. Re-queue — never a failure comment." >&2; exit 7; fi; [[ $rc -eq 1 && "$msg" == "Merge blocked:"* ]] || msg="Merge blocked: PR #$PR_NUMBER's verdict-label contradiction guard (#8112) could not run — '${LOOM_DAEMON_BIN:-loom-daemon} merge-pr verdict-contradiction' exited $rc without the LOOM-VERDICT-CLEAN signal. A guard that cannot run refuses the merge rather than passing it: a caller cannot tell 'found nothing' from 'never ran', so only a positive clean signal is accepted. $(_mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_BIN:-loom-daemon}" 2>/dev/null || true)")"; if [[ "$DRY_RUN" == "true" ]]; then warning "[dry-run] Would BLOCK merge of PR #$PR_NUMBER: $msg"; return 0; fi; error "$msg"; }
_check_verdict_label_contradiction

# The #8248 required-check freshness guard, its #8508 re-date remedy (exit 4),
# the #10388 merge-tree re-verification and the #10444/#10567 CI-run
# conclusion gate were deleted by #12063 slice 1: required checks at the head
# are GitHub's to enforce (the ruleset's required contexts, strict=false), and
# a semantic conflict that lands is main CI's to catch and revert.
# #10026: repo-declared merge.treeChecks run on the merge tree (base + PR head) by `loom-daemon merge-pr tree-checks`. Strict no-op (no daemon call) unless .loom/config.json declares checks; once declared it fails CLOSED, so a missing/older daemon refuses (deliberately no requires-daemon floor: repos that do not opt in are unaffected). --allow-red-tree warns; the daemon records the audit comment. See defaults/docs/merge-tree-checks.md.
_check_tree_checks() { local n msg rc=0 base_ref flags=(); n="$(jq -r '(.merge.treeChecks // []) | length' "${REPO_ROOT:-.}/.loom/config.json" 2>/dev/null)" || true; [[ -f "${REPO_ROOT:-.}/.loom/config.json" && "$n" != "0" ]] || return 0; base_ref="$(echo "$PR_JSON" | jq -r '.base.ref // empty')"; [[ "$DRY_RUN" != "true" ]] || flags+=(--dry-run); [[ "$ALLOW_RED_TREE" != "true" ]] || flags+=(--allow-red-tree); msg="$("${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr tree-checks --pr "$PR_NUMBER" --repo "$REPO_NWO" --head-sha "$PR_HEAD_SHA" --base-ref "${base_ref:-${DEFAULT_BRANCH_NAME:-main}}" --config "${REPO_ROOT:-.}/.loom/config.json" ${flags[@]+"${flags[@]}"})" || rc=$?; [[ $rc -eq 0 && "$msg" == "LOOM-TREE-CHECKS-CLEAN" ]] && return 0; [[ $rc -eq 0 && "$msg" == "LOOM-TREE-CHECKS-BYPASSED"* ]] && { warning "$msg"; return 0; }; [[ $rc -eq 1 ]] || msg="Merge blocked: PR #$PR_NUMBER's merge.treeChecks gate (#10026) could not run (exit $rc): $msg. A gate that cannot run refuses the merge; fix the cause (build/roll loom-daemon, config, fetch); --allow-red-tree overrides only a failing check, not a gate that cannot run."; [[ "$DRY_RUN" != "true" ]] || { warning "[dry-run] Would BLOCK merge of PR #$PR_NUMBER: $msg"; return 0; }; error "$msg"; }
_check_tree_checks

# ---------------------------------------------------------------------------
# Partial-increment closing-keyword conflict detection (#4569, extended by
# #4595 to cover commit messages).
#
# ROOT CAUSE (established from the rjwalters/censusapi#5 -> censusapi#2 incident,
# NOT from the branch-name theory the report floated):
#
#   GitHub's closing-reference parser scans the ENTIRE PR body and honors ANY
#   `close|closes|closed|fix|fixes|fixed|resolve|resolves|resolved` that is
#   IMMEDIATELY followed by `#N` — wherever it appears, including buried in
#   prose, inside a list item, or mid-sentence. It is NOT limited to a
#   line-leading trailer.
#
#   censusapi PR #5 ended with the deliberate non-closing trailer
#   `Contributes to #2`, but an earlier "Operator follow-up (after merge)" step
#   read "...then close #2". GitHub honored that `close #2` as a real closing
#   reference and closed issue #2 on squash-merge, silently defeating the #3599
#   partial-increment convention. The evidence:
#     - issue #2's timeline has NO `connected` event, so there was no
#       Development-sidebar / branch-name link -> the `feature/issue-N`
#       branch-name auto-link hypothesis is RULED OUT;
#     - the squash commit message contained only `Contributes to #2` (no closing
#       keyword), so the close did not come from the commit message either;
#     - the `closed` event has `commit_id: null` with the merger as actor at the
#       merge instant — the signature of a PR-body closing-reference close.
#
# SECOND SOURCE (#4595): the same parser also runs over the SQUASH COMMIT
#   MESSAGE. forge_merge_pr() squash-merges with no commit_title/commit_message
#   override, so GitHub composes that message from the PR's own commit messages —
#   a stray `close #N` in any commit message closes #N on merge even when the PR
#   body only ever says `Part of #N`. That close is fully attributable (the
#   `closed` event carries the merge `commit_id`), it is just invisible to both
#   the body regex and `closingIssuesReferences`, so the commit messages are
#   consulted as a third signal below.
#
# Fix shape: DETECT (here, pre-merge, with a loud actionable warning) plus
# SELF-HEAL (post-merge reopen in _reset_one_partial_issue below). Prevention by
# rewriting the PR body at merge time was rejected — mutating a body the Judge
# already reviewed is a worse failure mode than a seconds-long close/reopen.
#
# Two globals are published for the post-merge pass, both space-separated:
#   PARTIAL_OPEN_BEFORE_MERGE - partial-increment refs that were OPEN right
#       before the merge (so "closed afterwards" is attributable to this merge).
#   PARTIAL_CONFLICT_ISSUES   - the subset that ALSO carries a closing reference
#       from this PR, i.e. the ones GitHub is about to close against the
#       declared intent. Only these are auto-reopened; that keeps a deliberate
#       human close inside the merge window (which carries no closing reference)
#       from being reverted.
PARTIAL_OPEN_BEFORE_MERGE=""; PARTIAL_CONFLICT_ISSUES=""

# Closing-reference / partial-increment analysis, ported to Rust (#8191).
#
# These predicates decide whether merging would close an issue this PR
# only declared itself a PART OF. Their whole bug history is about what a
# stacked grep/sed/awk pipeline accidentally matched -- #5234 (a backticked
# hypothetical mention read as a declaration, reopening a correctly closed
# issue) and the numbered-list ordinal trap (`3. Part of #789` read as
# referencing both 3 and 789). Both are ordering bugs inside a pipeline.
#
# The body goes over STDIN, never argv: it is untrusted external content and
# routinely tens of kilobytes.
#
# A missing binary is FATAL here rather than degrading. Every one of these
# feeds a merge-or-refuse decision, and an empty answer is not "no references
# found" -- it is "no answer", which would silently close an unfinished issue
# or reopen a correctly closed one. Failing loudly is the safe direction.
_mp_refs() {
  # Resolved inline, not via lib/locate-daemon-bin.sh: the retained suites
  # extract these functions and source them alone, with no libs present.
  # LOOM_DAEMON_SELF_BIN first, per #8134 — it means "the binary that
  # IMPLEMENTS this entry point", which is exactly what this is.
  local bin out rc=0
  bin="${LOOM_DAEMON_SELF_BIN:-${LOOM_DAEMON_BIN:-}}"
  if [[ -n "$bin" ]]; then
    # A PINNED path that is unusable must refuse, not quietly resolve a
    # different binary off PATH — that substitutes an unknown version for the
    # one an operator deliberately selected.
    [[ -x "$bin" ]] || error "merge-pr.sh: LOOM_DAEMON_SELF_BIN/LOOM_DAEMON_BIN is set to '$bin', which is not executable. Refusing rather than silently falling back to a different loom-daemon."
  else
    bin="$(command -v loom-daemon 2>/dev/null || printf '%s' "$HOME/.local/bin/loom-daemon")"
    [[ -x "$bin" ]] || error "merge-pr.sh needs loom-daemon for its closing-reference analysis (#8191) and could not resolve one. Refusing rather than proceeding with no answer: an empty result is indistinguishable from 'no references', which would close an unfinished issue or reopen a correctly closed one. $(_mp_daemon_roll_hint merge-pr-refs)"
  fi
  # `|| rc=$?`, not `; rc=$?`: under `set -e` a failing command substitution in
  # a bare assignment aborts the script AT THAT LINE, so the check below never
  # ran and the refusal was silent — fail-closed, but with nothing said.
  out="$("$bin" merge-pr-refs "$@" 2>/dev/null)" || rc=$?
  [[ "$rc" -eq 0 ]] || error "merge-pr.sh's closing-reference analysis failed: '$bin merge-pr-refs $*' exited $rc. A loom-daemon predating #8191 has no such subcommand. Refusing rather than treating an empty result as 'no references'. $(_mp_daemon_roll_hint merge-pr-refs "$bin")"
  printf '%s' "$out"
}

# Issue numbers referenced with a NON-closing partial-increment keyword
# (`Part of #N` / `Contributes to #N`, case-insensitive) as a DECLARATION, one
# per line, deduped.
#
# "Declaration" is deliberately narrower than "appears anywhere in the body"
# (#5234): a bare `grep` over the whole text also matched a mid-sentence,
# backticked, conditional mention like "...say so and I will switch the
# reference to `Part of #4574`" — prose describing a hypothetical, not a
# declared intent — and treated it as authoritative, reopening a correctly
# closed issue. To count as a declaration here the keyword must be
# line-leading, optionally preceded by a list marker (`-`/`*`/`+`/numbered
# `1.`/blockquote `>`) and/or whitespace — matching the actual shape this
# convention produces (a one-line `Part of #123` / `Contributes to #456`
# trailer, per builder-pr.md), not prose that merely references the pattern.
#
# Fenced code blocks are stripped first, then inline code spans (`` `...` ``)
# are blanked so a backticked mention cannot itself satisfy the line-leading
# anchor (the #5234 repro used backticks specifically to mark the reference as
# hypothetical, not live).
#
# The second stage extracts `#N` tokens FIRST and only then strips the `#`.
# Scanning the whole matched span for any digit run would also pick up the
# numbered-list marker's own ordinal (`3. Part of #789` -> `3` and `789`),
# which is exactly the false-positive-reopen class this guard exists to
# prevent: a body carrying both `3. Part of #789` and a genuine `Closes #3`
# would see #3 registered as a declared partial increment AND a closing
# reference, and get reopened right after a correct close.
_partial_increment_refs() {
  printf '%s\n' "$1" | _mp_refs partial-increment-refs
}

# Every commit message of this PR, concatenated (#4595). merge-pr.sh squash-
# merges without overriding the commit message (forge_merge_pr passes no
# commit_title/commit_message), so GitHub composes the squash message from these
# commits — a `close #N` in any of them is a real closing reference that neither
# the PR body regex nor `closingIssuesReferences` reveals.
#
# REST (not GraphQL), so it survives the same quota exhaustion the body regex
# exists for, and `--paginate` so a >30-commit PR is not silently truncated.
# Plain `gh api` (not $GH) for freshness, `--jq` deliberately avoided in favor of
# a jq pipe (jq is already a hard dependency). Best-effort: any failure yields an
# empty string, which degrades to the pre-#4595 behavior (no attribution).
_pr_commit_messages() {
  { gh api "repos/$REPO_NWO/pulls/$PR_NUMBER/commits" --paginate 2>/dev/null \
      | jq -r '.[].commit.message' 2>/dev/null; } || true
}

# Membership tests over the space-separated globals above.
_partial_ref_is_conflicted() {
  [[ -n "${PARTIAL_CONFLICT_ISSUES:-}" ]] || return 1
  [[ " $PARTIAL_CONFLICT_ISSUES " == *" $1 "* ]]
}
_partial_ref_was_open_before_merge() {
  [[ -n "${PARTIAL_OPEN_BEFORE_MERGE:-}" ]] || return 1
  [[ " $PARTIAL_OPEN_BEFORE_MERGE " == *" $1 "* ]]
}

# Populate the two globals and warn about each detected conflict. Best-effort:
# never fails the merge, and a lookup failure simply yields a smaller set.
_check_partial_increment_close_conflict() {
  [[ "$FORGE_TYPE" == "github" ]] || return 0

  local pr_body
  pr_body="$(echo "$PR_JSON" | jq -r '.body // ""')"
  [[ -n "$pr_body" ]] || return 0

  # bt_warn/bt_rc are declared here, not next to their own assignment below,
  # purely so that assignment can own a line: see the SC2046 note below.
  local partial_refs bt_warn bt_rc=0 lt_warn lt_rc=0
  partial_refs="$(_partial_increment_refs "$pr_body")"

  # Backticked-trailer warning (#5690, ported to Rust #8831 —
  # cli/merge_pr_refs.rs's `backticks-partial-increment-warnings`, which
  # recomputes both declaration sets from $pr_body itself and diffs them, so
  # this call passes nothing but the PR number and dry-run state). Runs BEFORE
  # the early return below because the case it exists for is precisely the one
  # where $partial_refs is EMPTY — a trailer the author backticked, which
  # parses as no declaration at all. Pure text analysis, no forge calls, so
  # the common (non-partial-increment) path still costs zero extra requests.
  # (The statements below share one line deliberately — #8831 pays for the
  # daemon round trip inside the shell-budget ratchet's portable pool, and
  # this keeps that cost at net zero. The unquoted $(...) is intentional: it
  # expands to a single `--dry-run` token or nothing, never anything word
  # splitting could mis-tokenize — hence the SC2046 disable directly below.
  # That directive covers only the ONE statement that follows it, which is why
  # bt_warn/bt_rc are declared up with `local partial_refs` instead of leading
  # this line: as `local bt_warn bt_rc=0; bt_warn="$(...)"` the disable landed
  # on the declaration and the real finding leaked into CI (#8985). Moving the
  # declaration rather than adding a line keeps the budget at net zero too.)
  #
  # #8897: a BARE assignment (not `local var=$(...)`) with `2>/dev/null` and
  # `|| bt_rc=$?`, mirroring _mp_refs's own `out="$(...)" || rc=$?` pattern
  # above — so a daemon that answers `closing-refs` (checked already) but
  # rejects this newer MODE (unrecognized-subcommand exit) is detected here
  # instead of only printing _mp_refs's hardcoded closing-ref "Refusing..."
  # wording to the terminal (wrong mode, wrong PR, wrong version) while the
  # merge proceeds anyway (the `local var=$(...)` exit-status swallow that made
  # this call fail-open in practice all along). This call stays advisory-only:
  # a mode failure is reported as a skipped check, never as a refusal.
  #
  # #9502 (via #11167): the `Loom-Issue: owner/repo#N` trailer's own detector
  # (`loom-issue-trailer-warnings`, #9465 — backticked or slug-less trailers)
  # rides the SAME line, after the #5690 call, with the identical posture: a
  # bare assignment + `|| lt_rc=$?`, a warning naming a too-old daemon (and
  # that verb's own landing version, via its `daemon-verb-hint:`) on a
  # non-zero exit, never a refusal. Before the early return for the same
  # reason: a PR declaring only a Loom-Issue link has no partial refs at all.
  # The `{ …; }` group exists only so the SC2046 disable covers both calls.
  # shellcheck disable=SC2046
  { bt_warn="$(printf '%s\n' "$pr_body" | _mp_refs backticks-partial-increment-warnings --pr "$PR_NUMBER" $([[ "${DRY_RUN:-false}" == "true" ]] && echo --dry-run) 2>/dev/null)" || bt_rc=$?; if [[ $bt_rc -eq 0 ]]; then [[ -z "$bt_warn" ]] || warning "$bt_warn"; else warning "Skipped backticked-trailer advisory warning check: loom-daemon rejected 'merge-pr-refs backticks-partial-increment-warnings' (exit $bt_rc) -- most likely a daemon predating this mode. Not refusing; this check is advisory-only."; fi; lt_warn="$(printf '%s\n' "$pr_body" | _mp_refs loom-issue-trailer-warnings --pr "$PR_NUMBER" $([[ "${DRY_RUN:-false}" == "true" ]] && echo --dry-run) 2>/dev/null)" || lt_rc=$?; if [[ $lt_rc -eq 0 ]]; then [[ -z "$lt_warn" ]] || warning "$lt_warn"; else warning "Skipped Loom-Issue trailer advisory warning check (#9465): loom-daemon rejected 'merge-pr-refs loom-issue-trailer-warnings' (exit $lt_rc) -- most likely a daemon predating this mode. Not refusing; this check is advisory-only. $(_mp_daemon_roll_hint merge-pr-refs "$(command -v "${LOOM_DAEMON_SELF_BIN:-${LOOM_DAEMON_BIN:-loom-daemon}}" 2>/dev/null || true)" loom-issue-trailer-warnings)"; fi; }; [[ -n "$partial_refs" ]] || return 0

  # Which declared issues are open now, and which a closing reference will
  # close anyway (#4569), from three unioned signals: the body's own closing
  # keywords (quota-free regex); this PR's COMMIT MESSAGES (#4595) — quota-free
  # REST, and the source of the squash message this script does not override;
  # and GitHub's closingIssuesReferences (best-effort — empty under GraphQL
  # exhaustion, but it alone surfaces a Development-sidebar link). The commit
  # fetch happens only past the partial_refs early-return above, so the common
  # path costs zero extra API calls.
  #
  # The decision — the union, each issue's PR/open read, the membership test
  # and the source-attributed warning — is `loom-daemon merge-pr
  # partial-conflict` (Rust, loom-daemon/src/merge_pr/partial_conflict.rs —
  # #8191 slice). Only the forge reads stay here: fresh (uncached) plain
  # `gh api`, not $GH, per issue, mirroring _reset_one_partial_issue. They go
  # over stdin NUL-framed (a bash string cannot hold NUL, so the framing is
  # lossless and has no argv size limit). The plan's OPEN/CONFLICT<TAB>n lines
  # fill the two sets the post-merge pass reads; WARNING<TAB>text is replayed.
  # Fail CLOSED without the DONE terminator: an unread plan is not an empty
  # one, and read as empty it would leave a partial increment this merge
  # closes with nothing recorded to revert it.
  local frame=() issue_num plan rc=0 kind val dr=()
  frame=("$pr_body" "$(_pr_commit_messages)" "$(forge_pr_close_targets "$PR_NUMBER" "$GH" 2>/dev/null || true)")
  while IFS= read -r issue_num; do [[ -n "$issue_num" ]] || continue; frame+=("$issue_num" "$(gh api "repos/$REPO_NWO/issues/$issue_num" 2>/dev/null || echo '{}')"); done <<< "$partial_refs"
  [[ "${DRY_RUN:-false}" != "true" ]] || dr=(--dry-run)
  plan="$(printf '%s\0' "${frame[@]}" | "${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr partial-conflict --pr "$PR_NUMBER" ${dr[@]+"${dr[@]}"} 2>/dev/null)" || rc=$?
  if [[ $rc -ne 0 || "$plan" != *"LOOM-PARTIAL-CONFLICT-DONE" ]]; then
    plan="Merge blocked: PR #$PR_NUMBER's partial-increment close-conflict guard (#4569) could not run — '${LOOM_DAEMON_BIN:-loom-daemon} merge-pr partial-conflict' exited $rc without the LOOM-PARTIAL-CONFLICT-DONE terminator (a loom-daemon predating #8191's slice has no such verb). Refusing rather than reading silence as 'no conflict': this plan records which declared partial increments a stray closing reference will close, which is what the post-merge pass reverts. $(! declare -F _mp_daemon_roll_hint >/dev/null || _mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_BIN:-loom-daemon}" 2>/dev/null || true)")"
    [[ "${DRY_RUN:-false}" == "true" ]] || error "$plan"; warning "[dry-run] Would BLOCK merge of PR #$PR_NUMBER: $plan"; return 0
  fi
  while IFS=$'\t' read -r kind val; do
    case "$kind" in
      OPEN) PARTIAL_OPEN_BEFORE_MERGE="${PARTIAL_OPEN_BEFORE_MERGE:+$PARTIAL_OPEN_BEFORE_MERGE }$val" ;;
      CONFLICT) PARTIAL_CONFLICT_ISSUES="${PARTIAL_CONFLICT_ISSUES:+$PARTIAL_CONFLICT_ISSUES }$val" ;;
      WARNING) warning "$val" ;;
    esac
  done <<< "$plan"
  return 0
}

# Runs before either merge path so the operator sees the conflict BEFORE the
# close happens, and so --dry-run reports it without merging. Best-effort.
# #8191 preflight. One call, at top level: NOT inside `$(...)`, NOT behind
# `|| true`, and AFTER the definitions (_mp_refs lives inside the span the
# retained suite extracts, so it cannot be defined any earlier). _mp_refs's own
# `error` therefore exits the script here, which it cannot do from inside a
# command substitution in a `|| true` caller — the shape review proved lets all
# five version-skew cases reach the merge with an empty "no references" answer.
_mp_refs closing-refs </dev/null >/dev/null

_check_partial_increment_close_conflict || true

info "Merging PR #$PR_NUMBER: $PR_TITLE"
info "Branch: $PR_BRANCH"

# ---------------------------------------------------------------------------
# Partial-increment label reset (#3667).
#
# A PR that implements only a slice of a family/epic issue references it with a
# NON-closing keyword — `Part of #N` / `Contributes to #N` (convention in
# builder-pr.md) — deliberately so the issue survives the merge for further
# work. GitHub never auto-closes such an issue, and the merge path otherwise
# leaves `loom:building` orphaned on it: the #2838 "skip label cleanup on close"
# decision only reasoned about the `Closes #N` auto-close case, where GitHub
# closes the issue and stale labels on closed items are harmless. Nothing else
# reclaims the label until a time-gated `/sweep all` stale-claim pass (>=2h),
# and non-aggressive sweeps hard-skip the still-`loom:building` issue
# indefinitely (issue #3667).
#
# Here — at the deterministic merge choke point — we swap each such still-open,
# still-`loom:building` referenced issue back to `loom:issue`, mirroring
# orphan_recovery.py's recover_issue() label-reset semantics (loom:building ->
# loom:issue, i.e. return to the ready queue). No liveness check is needed: a
# merge just happened on the PR that necessarily came from whoever held the
# claim, so the current increment's work is provably done — a deterministic,
# not heuristic, signal. Closing keywords (`Closes`/`Fixes`/`Resolves`) are NOT
# matched — GitHub auto-closes those and the #2838 no-cleanup path stays
# untouched.
#
# GitHub-only for v1 (guarded on FORGE_TYPE); merge-pr.sh already branches on
# forge type elsewhere. Every step is best-effort and must never fail the merge.

# Reset a single referenced issue's labels if — verified fresh at merge time —
# it is still open and still carries loom:building (reopening it first when
# this very merge auto-closed it through a stray closing keyword, #4569).
# Idempotent: a no-op when the issue is already closed, already lacks
# loom:building (e.g. re-claimed by a second builder), or is actually a PR.
#
# The decision — which of those cases this is, and the log text for each — is
# `loom-daemon merge-pr partial-reset` (Rust, loom-daemon/src/merge_pr/
# partial_reset.rs — #8191 slice), fed the fresh issue body on stdin. It prints
# the steps in order: `INFO`/`WARNING<TAB>text` to replay, `REOPEN`, `SWAP`.
# Only the mutations and their audit comments stay here. The fresh read uses
# plain `gh api` (uncached; not $GH, which may be gh-cached) so a stale cached
# view cannot mask a re-claim. Best-effort like the rest of this pass: a daemon
# that cannot plan (missing, or predating the verb) is a warning naming the
# manual swap, never a guessed mutation. The plan is read on fd 3 so no forge
# call below can consume it from stdin.
_reset_one_partial_issue() {
  local issue_num="$1" issue_json reopened=false out rc=0 level text flags=()
  issue_json="$(gh api "repos/$REPO_NWO/issues/$issue_num" 2>/dev/null || echo '{}')"
  ! _partial_ref_is_conflicted "$issue_num" || flags+=(--conflicted)
  ! _partial_ref_was_open_before_merge "$issue_num" || flags+=(--open-before-merge)
  out="$(printf '%s\n' "$issue_json" | "${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr partial-reset --issue "$issue_num" --pr "$PR_NUMBER" --repo "$REPO_NWO" ${flags[@]+"${flags[@]}"} 2>/dev/null)" || rc=$?
  if [[ $rc -ne 0 ]]; then warning "Partial-increment reset for issue #$issue_num did not run — '${LOOM_DAEMON_BIN:-loom-daemon} merge-pr partial-reset' exited $rc (a loom-daemon predating #8191's slice has no such verb). Advisory only — the merge already happened and #$issue_num was left untouched; if it is still open and loom:building, return it to the ready queue by hand: gh issue edit $issue_num --repo $REPO_NWO --remove-label loom:building --add-label loom:issue $(! declare -F _mp_daemon_roll_hint >/dev/null || _mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_BIN:-loom-daemon}" 2>/dev/null || true)")"; return 0; fi
  while IFS=$'\t' read -r -u 3 level text; do
    case "$level" in
      INFO) info "$text" ;;
      WARNING) warning "$text" ;;
      REOPEN)
        # forge_gh_reopen_issue_rl_safe (#4856): falls back to a REST PATCH
        # (state=open) when `gh issue reopen`'s GraphQL mutation is rate-limited.
        if forge_gh_reopen_issue_rl_safe "$REPO_NWO" "$issue_num" 2>/dev/null; then
          success "Issue #$issue_num reopened (premature auto-close reverted)"
          reopened=true
          _post_premature_close_comment "$issue_num"
        else
          warning "Could not reopen issue #$issue_num after its premature auto-close — reopen manually: gh issue reopen $issue_num --repo $REPO_NWO"
          return 0
        fi ;;
      SWAP)
        # forge_gh_swap_label_rl_safe (#4856): falls back to REST (DELETE the old
        # label, POST the new one) when `gh issue edit`'s GraphQL mutation is
        # rate-limited, rather than silently dropping the label swap.
        if forge_gh_swap_label_rl_safe "$REPO_NWO" "$issue_num" "loom:building" "${text:-loom:issue}" 2>/dev/null; then
          success "Issue #$issue_num: loom:building -> ${text:-loom:issue} (partial increment; issue remains open)"
          local rn=(); [[ "$reopened" != "true" ]] || rn=(--reopened); [[ -z "$text" ]] || rn+=(--held)  # held remainder (#9264)
          # forge_gh_comment_rl_safe (#4856): falls back to the REST comments
          # endpoint on a GraphQL rate-limit rejection.
          _mp_post_partial_comment partial-merged "$issue_num" "Could not post partial-increment comment on issue #$issue_num (label swap still applied)" ${rn[@]+"${rn[@]}"}
        else
          warning "Could not reset labels on issue #$issue_num (partial increment) — may need manual 'gh issue edit'"
        fi
        ;;
    esac
  done 3<<< "$out"
}

# _mp_post_partial_comment <kind> <issue_num> <post-failure warning> [--reopened]
#
# Render one of the post-merge partial-increment audit comments and post it.
# Both bodies are `loom-daemon merge-pr partial-comment` (Rust,
# loom-daemon/src/merge_pr/partial_comment.rs — #8191 slice), byte-frozen from
# the shell that used to build them here; only the POST stays, behind #4856's
# rate-limit-safe wrapper.
#
# The bodies moved for the reason this file exists: they were ~40 lines of pure
# text in a script the size ratchet freezes, and NOTHING asserted either of them
# — the retained suite stubs the comment post, so a mangled interpolation or a
# lost `$reopen_note` would have shipped silently. They now have unit tests and
# a byte-for-byte differential against the frozen retired shell
# (loom-daemon/tests/merge_pr_partial_comment_differential.rs).
#
# Fails OPEN, and deliberately: both comments are posted AFTER the mutation they
# describe (the #4569 reopen, the #3667 label swap), and the merge itself is
# long done — so a body that cannot be rendered costs the note, nothing else.
# But it must never post SILENCE over the audit trail, which is what a bare
# `comment="$(… || true)"` would do against a daemon predating the verb. Only
# output led by the LOOM-MERGE-PR-COMMENT sentinel is posted; anything else is a
# warning naming what is missing.
#
# `$nl` holds the sentinel's terminating newline in a plain variable rather than
# writing `$'\n'` inline in the two patterns below. Both places are PATTERNS —
# a `[[ ]]` right-hand side and a `${var#…}` word — and bash only began
# processing `$'…'` inside those relatively late (it is literal `$'\n'` on
# macOS's stock /bin/bash 3.2, which this repo supports and has been bitten by
# before: #7717/#7721/#4242). Getting it literal would fail in BOTH directions,
# silently and only off-Linux: the `[[ ]]` would never match, so every macOS
# host would skip both comments with a warning, and if only the strip were
# literal the sentinel line would be posted as the body's first line. A plain
# `$nl` expansion is unambiguous on every bash. The ANSI-C quote in the
# *assignment* is fine everywhere — that use has worked since bash 2. Inside the
# `${out#…}` word `"$nl"` is quoted separately (SC2295) so the newline is matched
# literally rather than as a pattern; it holds no glob metacharacters either way,
# but the quoting is what says so.
_mp_post_partial_comment() {
  local kind="$1" issue_num="$2" post_warn="$3"; shift 3
  local out rc=0 nl
  nl=$'\n'
  out="$("${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr partial-comment --kind "$kind" --issue "$issue_num" --pr "$PR_NUMBER" "$@" <<<"${issue_json-}" 2>/dev/null)" || rc=$?
  if [[ $rc -ne 0 || "$out" != "LOOM-MERGE-PR-COMMENT$nl"* ]]; then
    warning "The $kind audit comment for issue #$issue_num (#3667/#4569) was NOT posted — '${LOOM_DAEMON_BIN:-loom-daemon} merge-pr partial-comment' exited $rc without the LOOM-MERGE-PR-COMMENT sentinel (a loom-daemon predating #8191's slice has no such verb). Advisory only: the merge, the reopen and the label swap all already happened and are unaffected — only this explanatory note is missing, and an empty comment is never posted in its place. $(! declare -F _mp_daemon_roll_hint >/dev/null || _mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_BIN:-loom-daemon}" 2>/dev/null || true)")"
    return 0
  fi
  forge_gh_comment_rl_safe "$REPO_NWO" "$issue_num" "${out#LOOM-MERGE-PR-COMMENT"$nl"}" 2>/dev/null || warning "$post_warn"
}

# Audit trail for a reverted premature auto-close (#4569). Posted right after
# the reopen so the record survives even when the label swap below is skipped
# (e.g. the issue no longer carries loom:building). Best-effort.
_post_premature_close_comment() {
  _mp_post_partial_comment premature-close "$1" "Could not post premature-close comment on issue #$1 (reopen still applied)"
}

# Parse the merged PR body for non-closing partial-increment references and
# reset each referenced issue. Best-effort; returns 0 unconditionally.
_reset_partial_increment_labels() {
  [[ "$FORGE_TYPE" == "github" ]] || return 0

  local pr_body
  pr_body="$(echo "$PR_JSON" | jq -r '.body // ""')"
  [[ -n "$pr_body" ]] || return 0

  # Issue numbers referenced with a NON-closing partial-increment keyword.
  # Shares _partial_increment_refs with the pre-merge #4569 conflict guard so the
  # two passes can never disagree about which issues are partial increments.
  local refs
  refs="$(_partial_increment_refs "$pr_body")"
  [[ -n "$refs" ]] || return 0

  local issue_num
  while IFS= read -r issue_num; do
    [[ -n "$issue_num" ]] || continue
    _reset_one_partial_issue "$issue_num"
  done <<< "$refs"

  return 0
}

# ---------------------------------------------------------------------------
# Closed-issue `loom:building` cleanup (#6199).
#
# The #2838 "no label cleanup on close" decision reasoned that a stale label
# on a closed issue is harmless — every queue query filters on open state, so
# it can never cause a duplicate build or a blocked candidate. That is still
# true. What #6199 found is that the decision also has a real, if narrow,
# cost: any consumer that reasonably reads `loom:building` as "in flight"
# WITHOUT also filtering on state — a dashboard, a capacity check, an
# operator `gh issue list --label loom:building` spot-check, or a future tool
# — gets pure noise once the population of closed-but-still-labelled issues
# grows (observed: 20 stale claims on one consumer repo, 0 real ones). The
# label has stopped meaning what its name says for anyone who doesn't already
# know to filter it out.
#
# Scope decision (recorded here per #6199's own "worth deciding deliberately"
# note): this pass covers ONLY the merge-driven auto-close path (`Closes #N`
# / `Fixes #N` / `Resolves #N`, resolved the same way Champion's "Verify
# Issue Auto-Close" step does — via GitHub's GraphQL
# `closingIssuesReferences`, see forge_pr_close_targets above) — the
# deterministic case merge-pr.sh already owns and can act on right at the
# confirmed-merge choke point, with zero extra liveness/state ambiguity: a
# merge just happened on the PR that closed the issue, so the label is
# unconditionally stale. An issue closed OUTSIDE a merge (closed manually, as
# a duplicate, or `--reason "not planned"` by an autonomous role) is
# deliberately OUT OF SCOPE here — merge-pr.sh has no hook into that path at
# all, and inventing one (e.g. polling every issue close event) is
# disproportionate to a cosmetic-but-annoying defect. That population is
# instead handled by the standalone, idempotent
# `clean-stale-building-labels.sh` (same directory) — run once against this
# repo as part of #6199 to clear the accumulated backlog, and safe to re-run
# on demand (by an operator, or wired into a periodic role) for any future
# manual-close stragglers. See that script's header for the full rationale.
#
# Runs AFTER _reset_partial_increment_labels (and therefore after any #4569
# premature-auto-close revert) so an issue that pass just reopened is no
# longer `closed` by the time this pass reads it — reopened partial-increment
# issues must keep `loom:building` (they return to `loom:issue` instead, via
# that pass), never lose the label outright.
#
# GitHub-only for v1 (guarded on FORGE_TYPE), mirroring
# _reset_partial_increment_labels's gating — forge_gh_remove_label_rl_safe is
# a `gh`-specific helper. Every step is best-effort and must never fail the
# merge.

# Strip `loom:building` from one issue this merge closed, if it is still
# present. Idempotent: a no-op when the issue isn't actually closed (a
# transient PR-close-target false positive, or #4569 reopened it above),
# already lacks the label, or is actually a PR.
#
# The decision — which of those cases this is — is `loom-daemon merge-pr
# closed-building` (Rust, loom-daemon/src/merge_pr/closed_building.rs — #8191
# slice), fed the fresh issue body on stdin. It prints exactly one line:
# `STRIP`, or `SKIP<TAB><reason>` which this pass deliberately discards (the
# retired function's skips were silent and stdout stays byte-identical). Only
# the mutation stays here. The fresh read uses plain `gh api` (uncached; not
# $GH, which may be gh-cached) so a stale cached view cannot mask a fresh
# re-claim — the same freshness discipline _reset_one_partial_issue keeps.
# Best-effort like the rest of this pass: a daemon that cannot decide, or that
# answers with anything but the two known lines, is a warning naming the manual
# removal, never a guessed mutation. Silence is NOT read as `SKIP`.
_strip_one_closed_issue_building_label() {
  local issue_num="$1" issue_json out rc=0

  issue_json="$(gh api "repos/$REPO_NWO/issues/$issue_num" 2>/dev/null || echo '{}')"
  out="$(printf '%s\n' "$issue_json" | "${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr closed-building 2>/dev/null)" || rc=$?
  if [[ $rc -ne 0 || ( "$out" != STRIP && "$out" != STRIP-READY && "$out" != STRIP-BOTH && "$out" != "SKIP"$'\t'* ) ]]; then
    warning "Closed-issue loom:building cleanup for issue #$issue_num did not run — '${LOOM_DAEMON_BIN:-loom-daemon} merge-pr closed-building' exited $rc and printed '${out//$'\n'/ }' rather than STRIP or SKIP (a loom-daemon predating #8191's slice has no such verb). Advisory only — the merge already happened and #$issue_num was left untouched; if it is closed and still loom:building, drop the stale claim by hand: gh issue edit $issue_num --repo $REPO_NWO --remove-label loom:building $(! declare -F _mp_daemon_roll_hint >/dev/null || _mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_BIN:-loom-daemon}" 2>/dev/null || true)")"
    return 0
  fi
  local labels=()
  case "$out" in
    STRIP) labels=(loom:building) ;;
    # #11517 (operator ruling 2026-10-13): a closed issue also loses loom:issue.
    STRIP-READY) labels=(loom:issue) ;;
    STRIP-BOTH) labels=(loom:building loom:issue) ;;
    *) return 0 ;;
  esac

  local label
  for label in "${labels[@]}"; do
    if forge_gh_remove_label_rl_safe "$REPO_NWO" "$issue_num" "$label" 2>/dev/null; then
      success "Issue #$issue_num: removed stale $label label (closed by this merge, #6199/#11517)"
    else
      warning "Could not remove $label from closed issue #$issue_num — may need manual: gh issue edit $issue_num --repo $REPO_NWO --remove-label $label"
    fi
  done
}

# Resolve this PR's closing issue targets and strip loom:building from each
# that is (still) closed. Best-effort; returns 0 unconditionally.
_strip_closed_issue_building_labels() {
  [[ "$FORGE_TYPE" == "github" ]] || return 0

  local close_targets issue_num
  close_targets="$(forge_pr_close_targets "$PR_NUMBER" "$GH" 2>/dev/null || true)"
  [[ -n "$close_targets" ]] || return 0

  # Loop status is 0: each body command (continue / the helper) returns 0.
  while IFS= read -r issue_num; do
    [[ -n "$issue_num" ]] || continue
    _strip_one_closed_issue_building_label "$issue_num"
  done <<< "$close_targets"
}

# ---------------------------------------------------------------------------
# Close-triggered `loom:blocked` re-check (#9102; item 2 of #8927's deferred
# fix list). `loom-daemon check-stale-blocked` (#8927) finds a stale block at
# the next sweep pre-wave; this finds it the moment the blocker closes: every
# open `loom:blocked` issue/PR citing this merged PR, or an issue it closed, as
# a blocker gets a comment now (never a label edit). Resolving the closed set,
# the decision and the comment all live in `loom-daemon notify-cleared-blockers`
# (reusing the advisory's enumeration and the `dep_recheck` parsers — no second
# parser, .loom/docs/shell-language-policy.md); it always exits 0. Best-effort
# and GitHub-only, like every step in this section.
_notify_cleared_blockers() { [[ "$FORGE_TYPE" == "github" ]] || return 0; "${LOOM_DAEMON_BIN:-loom-daemon}" notify-cleared-blockers --pr "$PR_NUMBER" --repo "$REPO_NWO" --repo-root "${REPO_ROOT:-.}" --quiet || warning "Close-triggered loom:blocked re-check (#9102) did not run; the next sweep's check-stale-blocked pass still covers it."; }

# ---------------------------------------------------------------------------
# Automated stacked-PR reconciliation on parent merge (#3747, stacked-PR v2,
# item 1 of the v2 epic — the remaining five items stay deferred).
#
# When a stacked PARENT PR (branch feature/issue-<N>) squash-merges, any CHILD
# PRs based on the parent branch still carry the parent's now-squashed pre-merge
# commits. reconcile-stack.sh performs the git surgery — `git rebase --onto
# <default> <parent-branch> <child-branch>`, `push --force-with-lease`, retarget
# the child PR base to the default branch — that strips them. v1 (#3729) shipped
# reconcile-stack.sh as a STANDALONE, operator-invoked script and deliberately
# left merge-pr.sh untouched. This v2 slice fires it AUTOMATICALLY here — a
# best-effort, GitHub-only step gated so it never races a live Builder that still
# holds the child branch checked out.
#
# Discovery is via a LIVE forge query (`gh pr list --base <parent>`), NOT the
# ephemeral loom-daemon SweepRegistry: terminal registry entries are
# garbage-collected ~1h after transition and the registry only exists at all when
# loom-daemon is running, but this function may run from Champion's cron or an
# interactive /loom:sweep merge with no daemon present (see
# .loom/docs/daemon-reference.md → "Stacked-PR dependency").
#
# Safe/unsafe split per child, gated on the child ISSUE's loom:building label
# (fresh, uncached `gh api` read, mirroring _reset_one_partial_issue's freshness
# discipline):
#   - Safe   (child issue NOT loom:building): no live claim on the child, so
#            invoke reconcile-stack.sh directly.
#   - Unsafe (child issue still loom:building): a live Builder likely has the
#            child branch checked out in its own worktree; an out-of-band rebase
#            + force-with-lease would corrupt its in-progress work. Skip the
#            auto-rebase and post a comment noting reconciliation is deferred
#            until the Builder finishes (a later parent-merge-triggered pass, or
#            a manual reconcile-stack.sh run, picks it up).
#
# Idempotent by construction: once a child's base is retargeted away from the
# parent branch, `gh pr list --base <parent>` returns zero rows, so re-runs are
# no-ops and nothing double-fires.
#
# Every step is best-effort and must NEVER change merge-pr.sh's exit code — the
# parent merge already happened. Runs BEFORE branch deletion so the parent
# branch ref still resolves as reconcile-stack.sh's rebase <upstream> argument.

# The two decisions here are `loom-daemon merge-pr reconcile-plan` and
# `merge-pr reconcile-child` (Rust, loom-daemon/src/merge_pr/reconcile.rs —
# #8191 slice). The plan owns the parent-branch gate, the children-rollup parse
# and each child's derived issue number; the child verb owns safe/unsafe and the
# deferral comment's byte-frozen text. Everything with an EFFECT stays here: the
# live `gh pr list` discovery (never the daemon registry), the uncached `gh api`
# label read, the reconcile-stack.sh invocation and the #4856 comment post.
#
# Why these: the retired shell wrote the `feature/issue-<N>` predicate out TWICE
# — once as the parent gate, once as the child derivation, 90 lines apart against
# two different variables — and reached the force-push-authorising answer through
# three stacked `|| echo '{}'` / `|| true` layers that each turn a failed lookup
# into the empty string, which `grep -qx` then reports as "no claim". That is the
# one wrong answer in this file that rebases a branch a Builder still has checked
# out. The port keeps the same disposition (force-with-lease is the remaining
# protection) but makes it a decided one, and it is now impossible to match
# `loom:building-paused` by widening a `grep`.
#
# Fail direction: OPEN, like the pre-merge sibling `merge-pr stacked-children`.
# A missing or older daemon prints no sentinel, and the seam then WARNS and skips
# auto-reconciliation for this pass — exactly the disposition the pre-existing
# "reconcile-stack.sh not found" skip already has. This runs after the merge has
# already happened and cannot make a merge wrong, so it must not be able to stop
# one; the cost of a skip is the one manual reconcile-stack.sh invocation every
# message on both routes already prints. That is why it raises no
# `requires-daemon: merge-pr` floor. Silence is never a route: `reconcile` (the
# force-pushing one) is reachable only through a positive sentinel.

# The roll-this-host remediation both skip paths below append. Guarded with
# `declare -F` exactly as the other fail-open seams (#3747 item 2, #7827,
# partial-reset, closed-building) are: `_mp_daemon_roll_hint` is defined ~700
# lines above, OUTSIDE the span test-merge-pr-auto-reconcile.sh extracts and
# sources, so an unguarded call would put `command not found` into the one
# message whose whole job is to tell an operator what to do next.
#
# One line, like `_mp_daemon_roll_hint` itself: this file is over the file-size
# ratchet's threshold, so it may shrink but not grow (.loom/docs/file-size-policy.md).
_mp_reconcile_roll_hint() { declare -F _mp_daemon_roll_hint >/dev/null || return 0; _mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_SELF_BIN:-${LOOM_DAEMON_BIN:-loom-daemon}}" 2>/dev/null || true)"; }

# Reconcile (or defer) one child PR from the plan. Best-effort; returns 0.
_reconcile_one_stacked_child() {
  local child_pr="$1" child_branch="$2" parent_branch="$3" child_issue="$4"

  # Fresh (uncached) label read — mirrors _reset_one_partial_issue: use plain
  # `gh api` (not $GH, which may be gh-cached) so a stale cached view cannot mask
  # a live re-claim. Skipped entirely when the plan derived no issue number,
  # which is the "child branch is not feature/issue-<N>, so no claim to race"
  # case; the verb reaches the same answer from an empty --child-issue.
  local issue_labels=""; [[ -z "$child_issue" ]] || issue_labels="$(gh api "repos/$REPO_NWO/issues/$child_issue" 2>/dev/null | jq -r '.labels[]?.name' 2>/dev/null || true)"

  local out rc=0
  out="$(printf '%s\n' "$issue_labels" | "${LOOM_DAEMON_SELF_BIN:-${LOOM_DAEMON_BIN:-loom-daemon}}" merge-pr reconcile-child --child-pr "$child_pr" --parent-branch "$parent_branch" --child-issue "$child_issue" 2>/dev/null)" || rc=$?
  case "${out%%$'\n'*}" in
    "LOOM-RECONCILE-CHILD defer")
      # Unsafe: defer, do not rebase.
      info "Stacked reconcile: child PR #$child_pr (issue #$child_issue) is still loom:building — deferring auto-rebase to avoid racing a live Builder"
      # Everything after the marker is the comment body verbatim, so the text
      # cannot be reshaped by a line-oriented read on this side.
      local comment="${out#*$'\nLOOM-RECONCILE-COMMENT\n'}"
      # forge_gh_comment_rl_safe (#4856): the REST comments endpoint is shared
      # by issues and PRs, so the same helper covers this `gh pr comment` call
      # site's GraphQL rate-limit fallback.
      forge_gh_comment_rl_safe "$REPO_NWO" "$child_pr" "$comment" 2>/dev/null || \
        warning "Could not post deferred-reconciliation comment on PR #$child_pr"
      ;;
    "LOOM-RECONCILE-CHILD reconcile")
      # Safe: no live claim — run the existing reconcile script unmodified. Do
      # NOT re-implement the rebase/force-with-lease/retarget logic inline.
      info "Stacked reconcile: parent '$parent_branch' merged; reconciling child PR #$child_pr onto the default branch"
      if "$SCRIPT_DIR/reconcile-stack.sh" "$child_pr" "$parent_branch"; then
        success "Stacked reconcile: child PR #$child_pr reconciled onto the default branch"
      else
        warning "Stacked reconcile: reconcile-stack.sh failed for child PR #$child_pr (rebase conflict, rejected force-with-lease push, or retarget failure). The parent merge is unaffected — reconcile manually: ./.loom/scripts/reconcile-stack.sh $child_pr $parent_branch"
      fi
      ;;
    *)
      warning "Stacked reconcile: SKIPPING child PR #$child_pr — 'loom-daemon merge-pr reconcile-child' exited $rc without a LOOM-RECONCILE-CHILD verdict, so whether a Builder still holds issue #${child_issue:-?} is unknown. Rebasing on a guess could force-push over uncommitted work, so nothing was done. The parent merge is unaffected — reconcile by hand once that is answered: ./.loom/scripts/reconcile-stack.sh $child_pr $parent_branch $(_mp_reconcile_roll_hint)" ;;
  esac
  return 0
}

# Discover open child PRs stacked on the just-merged parent branch and reconcile
# (or defer) each. Best-effort; returns 0 unconditionally.
_auto_reconcile_stacked_children() {
  [[ "$FORGE_TYPE" == "github" ]] || return 0

  # Prefer the pre-merge snapshot the guard above already captured (#8010
  # item 2) over a fresh post-merge query: GitHub retargets an open child PR
  # the instant delete_branch_on_merge removes this parent branch, so a query
  # run AFTER the merge can return zero rows even though children existed
  # seconds earlier — the guard's own pre-merge query already paid for this
  # exact answer. Live forge discovery (uncached `gh`, NEVER the daemon
  # registry) is only a fallback for when the guard never ran (e.g. this
  # function invoked standalone, as the unit tests do).
  local children_json="${STACKED_CHILDREN_JSON:-}"

  # AT MOST TWO plan calls, which is what the `for` bounds structurally.
  #
  # Pass `gate`: the parent-branch gate. NOT-STACKED is decided from
  # --parent-branch alone, so feeding it `[]` when no snapshot exists answers
  # the gate without a forge round trip — which is why the live query below
  # still costs nothing on an ordinary non-stacked merge. Only a stacked parent
  # with no snapshot falls through to pass `live` and queries, precisely when
  # the retired shell queried.
  #
  # The gate is also where the fail-OPEN skip is taken, BEFORE the query rather
  # than after it. An unanswered gate is indistinguishable from NOT-STACKED, so
  # on a host whose daemon predates these verbs this path runs once per merge;
  # it must not also spend a `gh pr list` on a rollup nothing will read.
  local plan rc verdict count _pass bin="${LOOM_DAEMON_SELF_BIN:-${LOOM_DAEMON_BIN:-loom-daemon}}"
  for _pass in gate live; do
    rc=0; plan="$(printf '%s' "${children_json:-[]}" | "$bin" merge-pr reconcile-plan --parent-branch "$PR_BRANCH" 2>/dev/null)" || rc=$?
    verdict="${plan%%$'\n'*}"
    [[ "$verdict" == "LOOM-RECONCILE-PLAN NOT-STACKED" ]] && return 0
    [[ "$verdict" == "LOOM-RECONCILE-PLAN COUNT "* ]] || { warning "Stacked reconcile: skipping auto-reconciliation for '$PR_BRANCH' — 'loom-daemon merge-pr reconcile-plan' exited $rc without a plan${verdict:+ (it said: $verdict)}. The parent merge is unaffected; reconcile any stacked child by hand: ./.loom/scripts/reconcile-stack.sh <child-pr> $PR_BRANCH $(_mp_reconcile_roll_hint)"; return 0; }
    [[ -z "$children_json" ]] || break   # the snapshot (or pass `live`'s query) already answered
    children_json="$(gh pr list --repo "$REPO_NWO" --base "$PR_BRANCH" --state open \
      --json number,headRefName 2>/dev/null || echo '[]')"; children_json="${children_json:-[]}"
  done
  count="${verdict#LOOM-RECONCILE-PLAN COUNT }"; [[ "$count" -gt 0 ]] || return 0

  info "Stacked reconcile: found $count open child PR(s) based on '$PR_BRANCH'"

  if [[ ! -x "$SCRIPT_DIR/reconcile-stack.sh" ]]; then
    warning "Stacked reconcile: reconcile-stack.sh not found or not executable at $SCRIPT_DIR — skipping auto-reconciliation"
    return 0
  fi

  local line child_pr child_branch child_issue
  while IFS= read -r line; do
    case "$line" in
      "LOOM-RECONCILE-PLAN CHILD "*)
        IFS=$'\t' read -r child_pr child_branch child_issue <<<"${line#LOOM-RECONCILE-PLAN CHILD }"
        [[ -n "$child_pr" ]] || continue
        _reconcile_one_stacked_child "$child_pr" "$child_branch" "$PR_BRANCH" "$child_issue"
        ;;
      "LOOM-RECONCILE-PLAN MALFORMED "*)
        warning "Stacked reconcile: ignoring an unusable child row from the forge — ${line#LOOM-RECONCILE-PLAN MALFORMED }" ;;
    esac
  done <<< "$plan"

  return 0
}

# Handle auto-merge mode
#
# Landmark that several merge-pr test suites anchor their source-span extractions on; keep it.
# `--auto` is a no-op since #12063 slice 2: there is no settle-wait, only the one merge call below.

# Freshest possible head-SHA read for the merge's optimistic-concurrency
# precondition (#5579). $PR_HEAD_SHA (set above from the initial $PR_JSON
# fetch) may have gone through the gh-cached wrapper via $GH — fine for the
# branch-cleanup safety check it also feeds, but a merge-gating precondition
# must observe current state as closely as possible: a stale value here only
# ever produces a spurious "head moved" re-queue (fail-safe — it can never
# cause a stale-but-accepted merge, since the forge itself does the real
# comparison against its own current head), but staleness still costs an
# unneeded round trip, so read it live via the uncached helper immediately
# before either merge path runs. A lookup failure falls back to the
# already-known $PR_HEAD_SHA rather than merging with no precondition at all.
MERGE_PRECONDITION_SHA="$PR_HEAD_SHA"
_MPS_JSON="$(forge_get_pr_nocache "$REPO_NWO" "$PR_NUMBER" "$GH" 2>/dev/null || echo '{}')"
_MPS_FRESH_SHA="$(echo "$_MPS_JSON" | jq -r '.head.sha // empty' 2>/dev/null || echo '')"
# #11859: under --expected-head the fresh read must equal the caller's SHA, and the caller's SHA is what the forge merge call is gated on.
[[ -n "$_MPS_FRESH_SHA" ]] && MERGE_PRECONDITION_SHA="$_MPS_FRESH_SHA"; _check_expected_head "$MERGE_PRECONDITION_SHA"; MERGE_PRECONDITION_SHA="${EXPECTED_HEAD_SHA:-$MERGE_PRECONDITION_SHA}"
unset _MPS_JSON _MPS_FRESH_SHA

# Re-pin refs/loom/parent/<branch> to the SHA actually being merged (#8010
# item 3), when the merge-ordering guard above pinned one. That guard pinned
# $PR_HEAD_SHA — read once, at the top of the script, from the initial
# (possibly gh-cached) $PR_JSON — but the merge itself uses the
# $MERGE_PRECONDITION_SHA just refreshed above, live, ~1100 lines and one
# CI-wait later. If the parent branch was pushed in between, the two SHAs
# differ and the pin would otherwise still point at a tip that is NOT what
# gets merged. Re-pointing it here keeps reconcile-stack.sh's fallback
# rebasing from the tip that was ACTUALLY merged. Best-effort and silent: a
# failure here leaves the existing (already-correct-when-written) pin in
# place, and reconcile-stack.sh's #8010-item-4 ancestry check refuses any pin
# that is not an ancestor of the child rather than silently replaying the
# wrong commit range. Gated on `$DRY_RUN != true` — this runs unconditionally
# ahead of both merge paths' own dry-run checks below, and the guard's pin
# path honors the "dry-run never mutates local refs" contract, so this must
# too. Gated on STACKED_CHILDREN_PIN_WRITTEN, NOT STACKED_CHILDREN_JSON: the
# latter is set as soon as the guard FINDS an open child, even on the
# --allow-stacked-children bypass path that returns without ever writing a
# pin — gating on it would silently create a pin the guard itself declined
# to create.
[[ "$DRY_RUN" != "true" && "${STACKED_CHILDREN_PIN_WRITTEN:-}" == "true" && "$MERGE_PRECONDITION_SHA" != "$PR_HEAD_SHA" ]] && { git -C "$REPO_ROOT" update-ref "refs/loom/parent/$PR_BRANCH" "$MERGE_PRECONDITION_SHA" 2>/dev/null || true; }

# The --auto settle-wait, the head/label re-validation after it, the SYNC branch update and the
# stale-mergeable pre-read were deleted by #12063 slice 2: this script makes one merge call (plus a
# bounded retry on a transient forge response); GitHub enforces required checks and mergeability.

if [[ "$DRY_RUN" == "true" ]]; then
  info "[dry-run] Would merge PR #$PR_NUMBER ($REPO_MERGE_METHOD) and delete remote branch '$PR_BRANCH'"
  if [[ "$CLEANUP_WORKTREE" == "true" ]]; then
    info "[dry-run] Would clean up local worktree"
    if git -C "$REPO_ROOT" show-ref --verify --quiet "refs/heads/$PR_BRANCH"; then
      info "[dry-run] Would delete local branch '$PR_BRANCH'"
    fi
  else
    info "[dry-run] --no-cleanup-worktree: would leave local worktree and local branch '$PR_BRANCH' in place"
  fi
  exit 0
fi

# Merge via API (using the repo's detected/allowed merge method, #7754) with
# retry for stale branch
MAX_MERGE_RETRIES=3; MERGE_RETRY_DELAY=5

for MERGE_ATTEMPT in $(seq 1 $MAX_MERGE_RETRIES); do
  MERGE_RESPONSE=$(forge_merge_pr "$REPO_NWO" "$PR_NUMBER" "$MERGE_PRECONDITION_SHA" "$REPO_MERGE_METHOD" 2>&1) && break  # Success, exit loop

  # Check if it merged despite error (race condition)
  RECHECK_JSON=$(forge_get_pr_nocache "$REPO_NWO" "$PR_NUMBER" "$GH" 2>/dev/null || echo '{}')
  RECHECK=$(echo "$RECHECK_JSON" | jq -r '.merged // false')
  if [[ "$RECHECK" == "true" ]]; then
    warning "Merge reported error but PR is merged (race condition)"
    break
  fi

  # Classify the response ONCE, before any of the three routes below (#8191
  # slice). Placed after the merged-despite-error recheck above deliberately: that
  # one is a forge round-trip rather than a string test, and a PR that merged
  # underneath us has no route to choose. A helper failure here is reported as a
  # HELPER failure — never folded into the terminal "other" route, which would
  # make "could not classify" indistinguishable from "no marker matched".
  MERGE_RESPONSE_KIND="$(_classify_merge_response "$MERGE_RESPONSE")" || error "Merge blocked: PR #$PR_NUMBER's merge-response classifier could not run — '${LOOM_DAEMON_BIN:-loom-daemon} merge-pr classify-response' returned no LOOM-MERGE-RESPONSE verdict (missing binary, or one predating the subcommand). The routes it chooses between are not interchangeable: one retries after syncing the base, and one must NEVER retry a head that moved past the approved SHA (#5579). An unobtainable classification therefore refuses rather than guesses. This is a helper failure, NOT a merge verdict — nothing about this PR was rejected. The forge reported: $MERGE_RESPONSE. $(_mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_BIN:-loom-daemon}" 2>/dev/null || true)")"

  # Head-SHA-mismatch (#5579): the PR's OWN head branch moved past
  # $MERGE_PRECONDITION_SHA — distinct from "Base branch was modified" below
  # (that means the BASE fell behind; this means the branch we're trying to
  # merge changed, most commonly a session pushing new commits mid-merge). Do
  # NOT retry-and-merge: retrying would either fail again (session still
  # pushing) or silently squash a different diff than the one Judge approved.
  # Exit 3 so the caller (Champion) re-queues instead of treating this as a
  # failure. See error_head_moved()/_classify_merge_response() above.
  # No self-sync exists any more (#12063 slice 2), so every mismatch is a foreign move.
  # Head-vs-base precedence lives in the classifier's own ordered match (#8191),
  # not in statement order; and `merge-route` below refuses a head-mismatch
  # rather than syncing it, so even a reorder here could never retry a moved head.
  if [[ "$MERGE_RESPONSE_KIND" == "head-mismatch" ]]; then
    _CURRENT_HEAD_SHA=""; _CHR_JSON="$(forge_get_pr_nocache "$REPO_NWO" "$PR_NUMBER" "$GH" 2>/dev/null || echo '{}')"; _CURRENT_HEAD_SHA="$(echo "$_CHR_JSON" | jq -r '.head.sha // empty' 2>/dev/null || echo '')"
    error_head_moved "PR #$PR_NUMBER: $MERGE_RESPONSE" "$MERGE_PRECONDITION_SHA" "$_CURRENT_HEAD_SHA"
  fi

  # A required check the forge still waits on (pending or red) is a hold, not a failure (#12063 slice 2):
  # this script no longer waits for checks, GitHub is the gate. Exit 5, re-queue later.
  if [[ "$MERGE_RESPONSE" =~ [Rr]equired\ status\ check ]]; then
    warning "Merge held: PR #$PR_NUMBER has a required status check the forge has not accepted: ${MERGE_RESPONSE:0:300}"; warning "Exiting 5: not merged this pass. Re-attempt on a later pass." >&2; exit 5
  fi

  # Every other route — 405 merge-in-progress, a stale base within the retry
  # budget, and the terminal refusals — is `loom-daemon merge-pr merge-route`
  # (#8191 slice, loom-daemon/src/merge_pr/merge_route.rs): the attempt budget,
  # the doubling backoff and every line narrated on the way. This loop keeps
  # only the I/O between those lines. Fail-OPEN onto the retired behaviour: a
  # binary that cannot answer is routed here on the classified kind and the
  # same `attempt < max` budget, so a moved head is still never retried and
  # everything else still refuses; only the wording degrades (_mr_say).
  _MR="$(printf '%s' "$MERGE_RESPONSE" | "${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr merge-route --kind "$MERGE_RESPONSE_KIND" --attempt "$MERGE_ATTEMPT" --max "$MAX_MERGE_RETRIES" --delay "$MERGE_RETRY_DELAY" --pr "$PR_NUMBER" 2>/dev/null)" || _MR=""
  [[ "$_MR" == "LOOM-MERGE-ROUTE "* ]] || { warning "PR #$PR_NUMBER: 'merge-pr merge-route' gave no route (missing/older loom-daemon); routing on the classified kind '$MERGE_RESPONSE_KIND' alone. Roll this host: $SCRIPT_DIR/cli/loom-daemon-update.sh --fetch"; _MR="FAIL"; [[ "$MERGE_RESPONSE_KIND" != merge-in-progress ]] || _MR="AWAIT 5"; [[ "$MERGE_RESPONSE_KIND" != base-modified || $MERGE_ATTEMPT -ge $MAX_MERGE_RETRIES ]] || _MR="SYNC $MERGE_RETRY_DELAY $((MERGE_RETRY_DELAY * 2))"; [[ "$_MR" != FAIL ]] || _MR+=$'\n'"Failed to merge PR #$PR_NUMBER: $MERGE_RESPONSE"; _MR="LOOM-MERGE-ROUTE $_MR"; }
  read -r _ _MR_ROUTE _MR_SLEEP _MR_NEXT_DELAY <<<"${_MR%%$'\n'*}"
  case "$_MR_ROUTE" in
    AWAIT)
      info "$(_mr_say BEFORE)"; sleep "$_MR_SLEEP"
      RECHECK_JSON=$(forge_get_pr_nocache "$REPO_NWO" "$PR_NUMBER" "$GH" 2>/dev/null || echo '{}')
      RECHECK=$(echo "$RECHECK_JSON" | jq -r '.merged // false')
      if [[ "$RECHECK" == "true" ]]; then success "$(_mr_say MERGED)"; break; fi
      warning "$(_mr_say PENDING)"; continue ;;
    SYNC)
      # A moved base is a plain retry now (#12063 slice 2): no branch update, no head push.
      info "$(_mr_say BEFORE)"; sleep "$_MR_SLEEP"
      MERGE_RETRY_DELAY="$_MR_NEXT_DELAY"; continue ;;
  esac
  # FAIL (or a route word this script does not know): refuse, quoting the reason.
  error "${_MR#*$'\n'}"
done

# Verify merge
VERIFY_JSON=$(forge_get_pr_nocache "$REPO_NWO" "$PR_NUMBER" "$GH" 2>/dev/null || echo '{}')
VERIFY_MERGED=$(echo "$VERIFY_JSON" | jq -r '.merged // false')
if [[ "$VERIFY_MERGED" != "true" ]]; then
  # Defense-in-depth: a transient API error (empty/{} response) must not turn a
  # successful merge into a hard failure. Retry the verify once before failing
  # (issue #3547).
  sleep 2
  VERIFY_JSON=$(forge_get_pr_nocache "$REPO_NWO" "$PR_NUMBER" "$GH" 2>/dev/null || echo '{}')
  VERIFY_MERGED=$(echo "$VERIFY_JSON" | jq -r '.merged // false')
  if [[ "$VERIFY_MERGED" != "true" ]]; then
    error "Merge API call returned but PR #$PR_NUMBER is not merged"
  fi
fi

success "PR #$PR_NUMBER merged successfully"


# Partial-increment label reset (#3667). Runs only after a confirmed merge —
# every merge now lands on the synchronous path above (#8410), so this is
# reached for `--auto` too; only the dry-run path exits earlier.
# Best-effort — never fails the merge. See the function definitions above.
_reset_partial_increment_labels || true

# Closed-issue `loom:building` cleanup (#6199). Runs right after the partial-
# increment pass (so a #4569 revert of a premature auto-close has already
# happened, and that issue is correctly skipped here — see the function's own
# header comment above) and at the same confirmed-merge choke point.
# Best-effort — never fails the merge.
_strip_closed_issue_building_labels || true

# Close-triggered loom:blocked re-check (#9102). Runs right after the
# loom:building cleanup above, at the same confirmed-merge choke point.
# Best-effort — never fails the merge. See the function's own header above.
_notify_cleared_blockers || true

# Automated stacked-PR reconciliation (#3747, stacked-PR v2 item 1). Runs at the
# same confirmed-merge choke point, and BEFORE branch deletion below so the
# parent branch ref still resolves as reconcile-stack.sh's rebase <upstream>
# argument. Best-effort — never fails the merge. See the function above.
_auto_reconcile_stacked_children || true

# NOTE: Full label cleanup on linked issues remains intentionally skipped for
# the `Closes #N` / `Fixes #N` / `Resolves #N` auto-close case — MOST labels
# on a closed issue are harmless, since every queue-driving agent filters on
# open state. See: https://github.com/rjwalters/loom/issues/2838
#
# EXCEPTION 1 (#3667): non-closing `Part of #N` / `Contributes to #N` partial-
# increment references leave the referenced issue OPEN after merge, so its
# `loom:building` label would otherwise be orphaned. The
# _reset_partial_increment_labels call above handles exactly that case by
# swapping loom:building -> loom:issue on the still-open referenced issue.
#
# EXCEPTION 2 (#6199): `loom:building` specifically — unlike other labels — is
# read by some consumers (dashboards, capacity checks, manual spot-checks) as
# meaning "in flight" WITHOUT also filtering on issue state, so a stale claim
# left on a just-closed issue silently becomes noise rather than staying truly
# harmless. The _strip_closed_issue_building_labels call above removes it from
# each issue THIS merge closed (via `Closes`/`Fixes`/`Resolves`). #2838's core
# reasoning — "don't bother cleaning up labels on close" — still holds for
# every other label, and for issues closed by means other than a merge (see
# that function's header for the recorded scope decision).
#
# NOTE: This script does NOT close linked issues. Issue auto-close is GitHub's
# responsibility — GitHub's PR parser closes issues referenced via `Closes #N`,
# `Fixes #N`, `Resolves #N` (and the case/tense variants) on merge. Champion's
# "Verify Issue Auto-Close" step is a belt-and-suspenders check that uses
# `forge_pr_close_targets` (which delegates to GitHub's GraphQL
# `closingIssuesReferences` field) to confirm closure. If you are debugging
# why an unintended issue was closed, look at the PR body and Champion logs,
# not at this script. See: https://github.com/rjwalters/loom/issues/3267

# Delete remote branch (skip if forge auto-deletes on merge)
DELETE_BRANCH_ON_MERGE=$(forge_check_auto_delete "$REPO_NWO" "$GH")
if [[ "$DELETE_BRANCH_ON_MERGE" == "true" ]]; then
  info "Skipping remote branch deletion (auto-delete is enabled)"
else
  # #9372: a bare ref delete makes GitHub CLOSE (unrecoverably) every open PR
  # based on this branch. Retarget open stacked children onto this PR's base
  # first; keep the branch on ANY uncertainty. Only exit 0 authorizes the delete
  # (a binary predating the verb exits 2 => keep). Independent of the #9259
  # reconcile defer and of --allow-stacked-children. GitHub-only: the verb
  # drives `gh pr list/edit`, so on FORGE_TYPE=gitea (the same test
  # forge_delete_branch dispatches on) it is skipped and the delete below runs
  # exactly as before #9372; otherwise a Gitea merge would consult a same-named
  # GitHub repo, or keep every branch when none exists.
  _RC_RC=0; _RC_OUT=""; [[ "${FORGE_TYPE:-}" == "gitea" ]] || _RC_OUT="$("${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr retarget-children --repo "$REPO_NWO" --parent-branch "$PR_BRANCH" --base "$(echo "$PR_JSON" | jq -r '.base.ref // empty' 2>/dev/null)" 2>/dev/null)" || _RC_RC=$?
  while IFS=$'\t' read -r _RC_LVL _RC_MSG; do
    case "$_RC_LVL" in INFO) info "$_RC_MSG" ;; WARNING) warning "$_RC_MSG" ;; esac
  done <<< "$_RC_OUT"
  if [[ $_RC_RC -ne 0 ]]; then
    warning "Skipping remote branch deletion of '$PR_BRANCH' (stacked-child safety, #9372; retarget-children exit $_RC_RC)"
  else
    info "Deleting remote branch: $PR_BRANCH"
    forge_delete_branch "$REPO_NWO" "$PR_BRANCH" && \
      success "Remote branch '$PR_BRANCH' deleted" || \
      warning "Could not delete remote branch '$PR_BRANCH' (may already be deleted)"
  fi
  unset _RC_OUT _RC_RC _RC_LVL _RC_MSG
fi

# Cleanup worktree if requested.
#
# Ownership model (see issue #3334): Loom owns worktrees it created under
# .loom/worktrees/ (marked with a .loom-managed sentinel file by worktree.sh
# or pr-worktree.sh). Any worktree lacking the sentinel is treated as
# user-owned and is never removed by this script. Operators can also set
# LOOM_PRESERVE_WORKTREE=1 to skip cleanup unconditionally.
#
# Two worktree-path conventions are recognized:
#   - .loom/worktrees/issue-<N>/  (Loom-issue branches: feature/issue-<N>;
#     the Builder's worktree)
#   - .loom/worktrees/pr-<N>/     (external-fork / ad-hoc branches, #3358;
#     OR a Judge/Doctor review worktree for an ordinary feature/issue-<N>
#     branch created via pr-worktree.sh when no builder issue-<N> worktree
#     was present at review time, #6264)
#
# A pr-<N> worktree of the LATTER shape can exist ALONGSIDE an issue-<N>
# worktree for the same PR (the builder worktree is created/reused after the
# Judge's pr-<N> review worktree, or vice versa) — the merge-time cleanup
# below checks for both when PR_BRANCH matches feature/issue-<N>, not just
# the issue-<N> path (#6264: previously only the external-fork branch ever
# considered a pr-<N> path, so a co-existing Judge review worktree on an
# ordinary issue branch was never cleaned up and survived the merge).
#
# Branch-to-issue regex is the strict `^feature/issue-([0-9]+)$` pattern so
# branches like `release-1` or `fix-bug-42` correctly classify as PR-style
# (not issue-style) and clean up the right worktree.
# Porcelain parsing for post-merge cleanup, ported to Rust (#8191 slice).
#
# The `git worktree list --porcelain` calls stay HERE; only the parse moved
# (loom-daemon/src/merge_pr/worktrees.rs). Every one of this family's shipped
# defects was in the awk — #3671 (the `exit`-triggers-`END` double-print, which
# handed callers a `/path\n/path` that exists nowhere), #3717 ($2 truncating a
# space-containing path, so the primary-worktree guard compared a prefix and
# never fired), #4171 — and every consumer is an irreversible step: `git
# worktree remove --force`, `git branch -D`. The newline-in-path caveat (#3717)
# is unchanged: `--porcelain -z` is still the real fix and is still this
# script's to make, since the `git` invocation never left.
#
# Contract: exit 0 + the answer, exit 0 + EMPTY for "parsed, no match", and
# non-zero for "the parse could not run at all". The last two must stay
# distinguishable — _remove_loom_worktree's #3710 guard reads an empty primary
# path as "the target is not the primary checkout" and proceeds to remove it.
#
# Resolved inline, not via lib/locate-daemon-bin.sh, for the same reason
# _mp_refs is (above): the retained suites extract these functions and source
# them alone, with no libs present. LOOM_DAEMON_SELF_BIN first per #8134; a
# PINNED path that is unusable refuses rather than silently resolving a
# different binary off PATH. Unlike _mp_refs this NEVER calls `error` — it is
# on the post-merge cleanup path, where aborting mid-way through state the
# merge already committed is worse than declining to clean up — so an
# unresolvable binary is reported as rc 3 for each caller to interpret.
_mp_worktree() {
  local bin out; bin="${LOOM_DAEMON_SELF_BIN:-${LOOM_DAEMON_BIN:-}}"
  [[ -n "$bin" ]] || bin="$(command -v loom-daemon 2>/dev/null || printf '%s' "$HOME/.local/bin/loom-daemon")"
  [[ -x "$bin" ]] || return 3
  out="$("$bin" merge-pr "$@" 2>/dev/null)" || return 3
  printf '%s' "$out"
}

# Look up the branch attached to a worktree via porcelain. Prints the branch
# short-name (without refs/heads/ prefix) on stdout. Returns 0 with empty
# output for detached / bare worktrees (no branch line in the stanza) — and,
# per the `|| true`, for a failed `git`/parse too: the only thing a caller does
# with this answer is decide whether to DELETE a branch, so no answer must read
# as "delete nothing", never as an abort part-way through cleanup of state the
# merge already committed.
_worktree_branch_for() {
  local target="$1" target_abs
  target_abs="$(cd "$target" 2>/dev/null && pwd -P)" || target_abs="$target"
  git -C "$REPO_ROOT" worktree list --porcelain 2>/dev/null \
    | _mp_worktree worktree-branch-for --path "$target_abs" || true
}

# Print the absolute path of the PRIMARY (main) worktree — the FIRST `worktree`
# entry of `git worktree list --porcelain`. Git always lists the main working
# tree first, which is the whole definition. Used by _remove_loom_worktree to
# hard-refuse removing the primary checkout (#3710), which is why this one does
# NOT swallow its failures like the two neighbours: an empty answer here is read
# as "not the primary" and authorises a removal, so "could not look it up" must
# reach the caller as a non-zero return instead of as an empty string.
_primary_worktree_path() {
  local out; out="$(git -C "$REPO_ROOT" worktree list --porcelain 2>/dev/null | _mp_worktree worktree-primary)" || return 3
  printf '%s' "$out"
}

# _is_primary_worktree_path <path>
#
# True (rc 0) when <path> resolves to the same real path as the PRIMARY (main)
# working copy — the FIRST entry of `git worktree list --porcelain` — rather
# than a linked worktree. Used to distinguish "this is the main checkout, not
# a removable worktree at all" from "this is a genuine linked worktree" so
# callers never suggest `git worktree remove` / `--worktree-path` against the
# primary checkout (#4171). Returns 1 (false) if either path fails to resolve.
# Both its call sites only choose which REMEDIATION TEXT to print, so a lookup
# failure degrades to false here; the removal decision itself is guarded in
# _remove_loom_worktree, which fails closed instead.
_is_primary_worktree_path() {
  local check_path="$1" check_real primary_real
  check_real="$(cd "$check_path" 2>/dev/null && pwd -P)" || check_real="$check_path"
  primary_real="$(_primary_worktree_path)" || primary_real=""
  [[ -n "$primary_real" ]] && [[ "$check_real" == "$primary_real" ]]
}

# Walk porcelain output for a worktree whose branch matches the given branch
# short-name. Prints the worktree absolute path or nothing. Skips detached /
# bare entries (they have no `branch refs/heads/...` line). Swallows failure to
# "nothing found" for the same reason _worktree_branch_for does: the caller
# either preserves the worktree or prints advice, never destroys on an absence.
_find_worktree_by_branch() {
  git -C "$REPO_ROOT" worktree list --porcelain 2>/dev/null \
    | _mp_worktree worktree-find-by-branch --branch "$1" || true
}

# The worktree-preserve decisions below (#6694) and the branch-delete safety
# check in _maybe_delete_local_branch both ask "does the default branch
# already contain everything this branch has?" via `branch_has_landed`
# (lib/branch-landed.sh, #7812), replacing the private
# `_worktree_branch_fully_captured` tip-vs-merged-head comparison. Tip
# equality was only ever a *sufficient* proof of "fully captured", never a
# necessary one: it is false under a rebase merge (which rewrites every SHA)
# exactly as `git merge-base --is-ancestor` is false under a squash merge.
# `branch_landed`'s fail-closed `unknown` is false there, so both callers keep
# their conservative behaviour when nothing could prove the branch landed.

# Delete the matching local branch (#4100/#5015/#7812).
#
# _maybe_delete_local_branch <branch> [expected_head_sha]
#
# A thin call into `loom-daemon merge-pr delete-branch` (#8191), which shares
# the exact squash-aware `-d`/`-D` rule and #5015 primary-checkout
# auto-cleanup `worktree.sh remove` already uses
# (`worktree_cli::branch_delete`, #8195 slice 3) — one implementation instead
# of two `awk`-and-`eval` copies. `expected_head_sha` is the merged PR's
# `head.sha` (already parsed into $PR_HEAD_SHA); a tip matching it is landed
# with no forge round-trip, per the shared `branch_landed` primitive (#7812).
# `--no-cleanup-primary` (CLEANUP_PRIMARY_CHECKOUT=false) opts out of #5015.
#
# The daemon emits one `LEVEL<TAB>message` line per decision on stdout, which
# is replayed here through this script's own info/warning/success — the
# operator-visible text and coloring are unchanged from before the port.
#
# Never fails the cleanup pipeline — always returns 0; the merge already
# happened by the time this runs. A daemon that is missing, stale (exit 2) or
# non-executable fails SAFE: it never ran, so nothing was deleted, and that is
# a warning, not a blocker. A pinned $LOOM_DAEMON_BIN is used as-is, never
# swapped for another binary off PATH. Whatever the daemon DID print is
# replayed before a non-zero exit is reported, so a crash after a delete cannot
# hide the delete. Only stdout is parsed — stderr (clap errors, logs) passes
# through untouched, never replayed as a bogus INFO line. `${a[@]+…}` is bash
# 3.2's `set -u` empty-array guard; test-merge-pr-local-branch-cleanup.sh evals
# this body without `_mp_daemon_roll_hint`, hence the `declare -F` probe.
# (cleanup-branches.sh used to eval it too — since #8968 it calls the same
# subcommand itself, so this body has exactly one caller: merge-pr.sh.)
_maybe_delete_local_branch() {
  local branch="$1" expected_head_sha="${2:-}"
  [[ -n "$branch" ]] || return 0
  local flags=() out rc=0 level text
  [[ -z "${DEFAULT_BRANCH_NAME:-}" ]] || flags+=(--default-branch "$DEFAULT_BRANCH_NAME")
  [[ "${CLEANUP_PRIMARY_CHECKOUT:-true}" == "true" ]] || flags+=(--no-cleanup-primary)
  out="$("${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr delete-branch --repo-root "$REPO_ROOT" --branch "$branch" --expected-head-sha "$expected_head_sha" ${flags[@]+"${flags[@]}"})" || rc=$?
  while IFS=$'\t' read -r level text; do
    [[ -n "$level" ]] || continue
    case "$level" in
      SUCCESS) success "$text" ;;
      WARNING) warning "$text" ;;
      *) info "$text" ;;
    esac
  done <<< "$out"
  [[ $rc -eq 0 ]] || warning "The local-branch cleanup guard for '$branch' did not complete — '${LOOM_DAEMON_BIN:-loom-daemon} merge-pr delete-branch' exited $rc. Advisory only — the merge already happened; any branch action it did take is reported above, otherwise '$branch' is left as-is. $(! declare -F _mp_daemon_roll_hint >/dev/null || _mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_BIN:-loom-daemon}" 2>/dev/null || true)")"
  return 0
}

# _remove_loom_worktree <path> [allow_unmanaged]
#
# When allow_unmanaged is "true" (only set by the --worktree-path code path),
# the .loom-managed sentinel check is skipped — the caller has taken explicit
# responsibility for the cleanup decision. The default (no second arg, or
# "false") preserves the original sentinel guard.
_remove_loom_worktree() {
  local worktree_path="$1"
  local allow_unmanaged="${2:-false}"
  if [[ ! -d "$worktree_path" ]]; then
    info "No worktree found at $worktree_path"
    return 0
  fi
  # Resolve to a canonical absolute path once; reused for both the primary-
  # worktree guard immediately below and the "is our CWD inside it?" check
  # further down.
  local worktree_real
  worktree_real="$(cd "$worktree_path" 2>/dev/null && pwd -P || echo "$worktree_path")"
  # Hard guard (#3710): NEVER attempt to remove the primary/main worktree — the
  # FIRST entry of `git worktree list --porcelain` — regardless of a
  # .loom-managed sentinel, the checked-out branch, --worktree-path, or a
  # customized worktree.root. This is the single choke point for all three
  # removal call-sites (default issue/pr path, --worktree-path override, and the
  # non-standard-path discovery fallback). Without it, a repo whose primary
  # checkout (a) sits at a non-standard path relative to a customized
  # worktree.root, (b) carries a .loom-managed sentinel, and (c) has the PR
  # branch checked out will reach `git worktree remove` on the main working
  # tree: git fails safe ("Could not remove worktree"), but the attempt is a
  # logic error and emits a misleading Removing/Could-not-remove pair. Refuse
  # here, before any sentinel or CWD handling.
  #
  # #8191 slice: the lookup itself can now FAIL (the parse moved into
  # loom-daemon, which can be missing, unreadable, or predate the subcommand) as
  # distinct from returning nothing. Those must not be conflated — an empty
  # answer means "git reported no worktrees", while a failed lookup means this
  # guard did not run, and a guard that did not run must refuse the removal
  # rather than wave it through. Skipped cleanup is always recoverable
  # (loom-clean, the daemon's reaper); removing the primary checkout is not.
  # The decision itself — the primary comparison, the sentinel test, the
  # --worktree-path bypass and their message text — is `loom-daemon merge-pr
  # remove-gate` (Rust, loom-daemon/src/merge_pr/remove_gate.rs, #8191 slice).
  # The `git worktree list` call stays here. Fails CLOSED: a gate that did not
  # run (missing/older daemon, off-protocol first line) refuses the removal
  # rather than waving it through.
  local gate verdict gate_rc=0 level text
  gate="$(git -C "$REPO_ROOT" worktree list --porcelain 2>/dev/null | _mp_worktree remove-gate --path "$worktree_path" --real "$worktree_real" --allow-unmanaged "$allow_unmanaged")" || gate_rc=$?
  verdict="${gate%%$'\n'*}"
  if [[ $gate_rc -ne 0 || ( "$verdict" != "LOOM-REMOVE-GATE PROCEED" && "$verdict" != "LOOM-REMOVE-GATE REFUSE" ) ]]; then
    warning "Refusing to remove worktree at $worktree_real — the primary-worktree guard (#3710) could not run: 'loom-daemon merge-pr remove-gate' failed, so whether this path IS the primary checkout is unknown. Best-effort cleanup only; the merge itself already succeeded and is unaffected. Remove it by hand once loom-daemon is available, if it really is a worktree: git -C \"$REPO_ROOT\" worktree remove \"$worktree_real\" --force $(! declare -F _mp_daemon_roll_hint >/dev/null || _mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_SELF_BIN:-${LOOM_DAEMON_BIN:-loom-daemon}}" 2>/dev/null || true)")"; return 0
  fi
  while IFS=$'\t' read -r level text; do
    [[ -n "$level" ]] || continue
    case "$level" in WARNING) warning "$text" ;; *) info "$text" ;; esac
  done <<<"${gate#"$verdict"}"
  [[ "$verdict" == "LOOM-REMOVE-GATE PROCEED" ]] || return 0
  # Record the attached branch BEFORE removing the worktree (the porcelain
  # entry vanishes once the worktree is gone). Only relevant when allow_unmanaged
  # — the default issue/pr path already has the branch encoded in PR_BRANCH.
  local attached_branch=""
  if [[ "$allow_unmanaged" == "true" ]]; then
    attached_branch="$(_worktree_branch_for "$worktree_path")"
  fi
  # If our shell is inside the worktree we're removing, hop out first.
  # ($worktree_real was already resolved above for the primary-worktree guard.)
  local current_dir in_worktree=false
  current_dir="$(pwd -P 2>/dev/null || pwd)"
  if [[ "$current_dir" == "$worktree_real"* ]]; then
    in_worktree=true
    cd "$REPO_ROOT"
  fi
  # Data-loss guard (#5031): NEVER force-remove a worktree that still holds
  # uncommitted work. Post-merge cleanup keys the worktree only by branch name
  # (feature/issue-<N>) — the worktree.sh naming convention makes that name
  # collide deterministically whenever two hosts/sessions independently claim
  # the same issue number. When a *different*, still-live builder has that
  # branch checked out with unsaved edits, the blanket `git worktree remove
  # --force` below would silently destroy them (observed 2026-08-03 on #5001:
  # a live sibling session lost its in-flight, never-committed edits). The
  # neighbouring guards check only *identity/ownership* (primary-worktree #3710,
  # .loom-managed sentinel), never *live content*, so none of them catch this.
  # Refuse-and-warn instead: the merge itself already succeeded, so skipping
  # cleanup is a safe no-op for the merge while the sibling's work survives on
  # disk to be committed/pushed. The clean common case is unaffected — a
  # worktree that already committed+pushed the merged PR reports no changes, and
  # Loom's own gitignored runtime markers (.loom-managed / .loom-in-use /
  # .loom-checkpoint / .no-changes-needed / .loom-cargo-target-dir / .snapshots/)
  # are filtered out so a bare/stale checkout that surfaces them as untracked is
  # still removed. `.loom-cargo-target-dir` (#8458) matters most here: it is born
  # in EVERY opted-in worktree, so a consumer repo with a stale `.gitignore` block
  # would otherwise see every post-merge cleanup refuse — turning the per-worktree
  # target dir into the very leak the scheme exists to close. That filter is not
  # spelled here: the list is `worktree_ops::safety::LOOM_OWN_UNTRACKED_FILES`,
  # asked through `is_loom_own_untracked_path` by the `merge-pr dirty-guard` port
  # below (#8191 slice), which is also what the removal-side paths consult.
  #
  # Not to be re-conflated in triage (distinct root causes):
  #   - #4463 (closed): same-HOST duplicate dispatch — fixed lock *ownership* so
  #     a live sweep's lock is not stolen by a peer's reaper. That is about lock
  #     records, not worktree/branch-name collision, and would not have caught
  #     #5031 (the colliding worktree came from a separate host that never
  #     touched this daemon's lock).
  #   - #4146 (open, operator-held): *measures* the cross-host collision
  #     rate (observation only, behavior unchanged on collision by design). This
  #     guard is the behavioral mitigation for the data-loss instance #4146 is
  #     meant to eventually quantify.
  #
  # The three decisions — which porcelain lines are user work rather than Loom's
  # own runtime markers, whether the remainder plausibly IS in-flight work
  # (#5658), and the refusal text — are `loom-daemon merge-pr dirty-guard`
  # (Rust, loom-daemon/src/merge_pr/dirty_guard.rs — #8191 slice). Only the
  # porcelain READ stays here, on purpose: `git status` FAILING has always meant
  # "no dirt, proceed" on this path (the #5177 orphaned-directory case cleanup
  # exists for), and the `|| true` that encodes that must not migrate into an
  # exit code the guard would read as a fault. The port retires the THIRD copy
  # of Loom's marker list (#8195 slice 3 deleted worktree.sh's twin after
  # #8279) and closes two holes in the retired `grep -vE`: a rename INTO a
  # marker name was filtered as bookkeeping though it is a tracked-file change,
  # and a marker under a git-quoted path was counted as user work.
  #
  # The refusal arrives as `LEVEL<TAB>message` lines replayed through this
  # script's own warning/echo — the shape `merge-pr delete-branch` established —
  # so the operator-visible text is unchanged. `PLAIN` is the uncolored `echo`
  # the remediation command is pasted from. $live_branch is now resolved on
  # every removal rather than only the dirty path (one extra local
  # `git worktree list`), because the wrapper cannot know which it needs until
  # the guard has answered.
  #
  # Unlike every other step in this function this one fails CLOSED: it gates
  # `git worktree remove --force`, and destroyed uncommitted work is
  # unrecoverable where a skipped removal is not (loom-clean, the daemon's
  # reaper, or the next merge all retry it).
  local dirty dirty_out dirty_rc=0 live_branch level text
  dirty="$(git -C "$worktree_path" status --porcelain 2>/dev/null || true)"
  live_branch="$(_worktree_branch_for "$worktree_path" 2>/dev/null || true)"
  dirty_out="$(printf '%s\n' "$dirty" | "${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr dirty-guard --worktree-path "$worktree_path" --repo-root "$REPO_ROOT" --branch "$live_branch")" || dirty_rc=$?
  if [[ $dirty_rc -ne 0 ]] || [[ "$dirty_out" != "LOOM-DIRTY-GUARD-CLEAN" ]]; then
    if [[ $dirty_rc -eq 1 ]] && [[ "$dirty_out" == *$'\t'* ]]; then
      while IFS=$'\t' read -r level text; do
        [[ -n "$level" ]] || continue
        case "$level" in
          PLAIN) echo "$text" ;;
          *) warning "$text" ;;
        esac
      done <<<"$dirty_out"
    else
      warning "Refusing to remove worktree at $worktree_path — its uncommitted-work data-loss guard, #5031, could not run: '${LOOM_DAEMON_BIN:-loom-daemon} merge-pr dirty-guard' exited $dirty_rc without the LOOM-DIRTY-GUARD-CLEAN signal (a loom-daemon predating this slice has no such verb). Only a positive clean signal authorizes the force-remove: a caller cannot tell 'nothing to save' from 'never looked', and this is the one cleanup step whose wrong answer is unrecoverable. $(! declare -F _mp_daemon_roll_hint >/dev/null || _mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_BIN:-loom-daemon}" 2>/dev/null || true)")"
    fi
    return 0
  fi
  # #7239: resolve the worktree's cargo target dir BEFORE removing it —
  # `cargo metadata` needs the manifest that is about to disappear. Acted on
  # only after a successful removal, below. `loom-daemon cargo-target-dir
  # resolve` since #9153, so this body stays self-contained with no lib to
  # source: `2>/dev/null || true` means a missing/stale daemon leaves this empty
  # and the reclaim below is skipped, which is the pre-#7239 behaviour (declared
  # as `requires-daemon: cargo-target-dir optional` above).
  local target_dir_resolved="$("${LOOM_DAEMON_BIN:-loom-daemon}" cargo-target-dir resolve "$worktree_path" 2>/dev/null || true)"
  info "Removing worktree: $worktree_path"
  # The removal itself — `git worktree remove --force`, #6372's one `git
  # worktree prune` + retry on failure (a stale registration can fail the first
  # attempt with nothing holding the worktree open), and the success/failure
  # report naming git's real error and the remediation — is `loom-daemon
  # merge-pr worktree-teardown` (Rust, loom-daemon/src/merge_pr/
  # worktree_teardown.rs, #8191 slice). Best-effort like all cleanup: a failed
  # removal is a warning, never error(), because the merge already succeeded.
  # Fails CLOSED: a verb that did not run (missing/older daemon, or a first line
  # that is neither verdict) removes nothing and skips the success-only steps.
  local tear tear_rc=0
  tear="$(_mp_worktree worktree-teardown --repo-root "$REPO_ROOT" --path "$worktree_path")" || tear_rc=$?
  if [[ $tear_rc -ne 0 || ( "${tear%%$'\n'*}" != "LOOM-WORKTREE-TEARDOWN REMOVED" && "${tear%%$'\n'*}" != "LOOM-WORKTREE-TEARDOWN FAILED" ) ]]; then
    warning "Could not remove worktree at $worktree_path (best-effort cleanup — the merge itself already succeeded and is unaffected): 'loom-daemon merge-pr worktree-teardown' exited $tear_rc without a LOOM-WORKTREE-TEARDOWN verdict (a loom-daemon predating #8191's slice has no such verb), so no removal was attempted. Remediation: git worktree prune && git -C \"$REPO_ROOT\" worktree remove \"$worktree_path\" --force $(! declare -F _mp_daemon_roll_hint >/dev/null || _mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_SELF_BIN:-${LOOM_DAEMON_BIN:-loom-daemon}}" 2>/dev/null || true)")"; return 0
  fi
  while IFS=$'\t' read -r level text; do
    case "$level" in SUCCESS) success "$text" ;; WARNING) warning "$text" ;; esac
  done <<<"${tear#*$'\n'}"
  if [[ "${tear%%$'\n'*}" == "LOOM-WORKTREE-TEARDOWN REMOVED" ]]; then
    # #5950: attribute the removal in the shared ledger. `attached_branch` is
    # only resolved on the unmanaged/explicit-override path; on the default
    # issue/PR path the branch is already `PR_BRANCH`, so fall back to that
    # rather than recording a null branch for the common case.
    loom_record_worktree_removal "$REPO_ROOT" "merge-pr.sh" "$worktree_path" \
      "${attached_branch:-${PR_BRANCH:-}}" "post_merge_cleanup"
    if [[ "$in_worktree" == "true" ]]; then
      echo ""
      warning "Your shell's working directory was inside the removed worktree."
      warning "Run this command to fix:"
      echo "  cd $REPO_ROOT"
    fi
    # For the explicit-override path, also tidy up the attached local branch.
    # We defer this to AFTER `git worktree remove` succeeds so the worktree's
    # checkout lock is released first.
    if [[ "$allow_unmanaged" == "true" ]] && [[ -n "$attached_branch" ]]; then
      _maybe_delete_local_branch "$attached_branch"
    fi
    # #7239: reclaim a REDIRECTED cargo target dir now that the worktree is
    # gone — only when it is outside the worktree, unshared with every other
    # live worktree, and held open by no running process. Those gates are
    # `worktree_ops::cargo_target::plan_reclaim` (#9153), the same decision
    # `worktree.sh remove`, `loom-daemon clean` and the reaper make; at most one
    # `LEVEL<TAB>message` record comes back, replayed through this script's own
    # logging exactly as `merge-pr delete-branch` and `dirty-guard` do. Silent
    # for the default (un-redirected) layout, and best-effort like every other
    # step here: the merge already succeeded and is unaffected either way.
    if [[ -n "$target_dir_resolved" ]]; then
      # `read` clears both names even on empty input, so a daemon that printed
      # nothing (or none at all) falls through every `case` arm and says nothing.
      IFS=$'\t' read -r level text < <("${LOOM_DAEMON_BIN:-loom-daemon}" cargo-target-dir reclaim "$worktree_path" --resolved "$target_dir_resolved" --repo-root "$REPO_ROOT" 2>/dev/null || true) || true
      case "$level" in
        SUCCESS) success "$text" ;;
        WARNING) warning "$text" ;;
        ?*) info "$text" ;;
      esac
    fi
  fi
}

# _issue_is_closed_for_cleanup <issue_number>
#
# Async-close-race adaptation (#4186, adapted from fork PR #77's open-issue
# worktree guard).
#
# Removing a worktree unconditionally after merge breaks the partial-
# increment lifecycle (#3667): a `Part of #N` / `Contributes to #N` PR merges
# while issue N stays open, and the next Builder increment (or an agent still
# inside it) needs that worktree. But naively querying the issue's LIVE state
# right after merge has a race: GitHub closes `Closes #N` issues
# ASYNCHRONOUSLY, after the merge webhook fires — so a lookup taken here
# would see "open" for essentially every normal merge and silently defeat
# cleanup entirely. Gate the live lookup on whether this PR is actually a
# close target of the issue:
#
#   - $issue_number IS a close target of $PR_NUMBER -> the merge itself
#     closes it; clean up exactly as before this change (no lookup, no
#     race).
#   - $issue_number is NOT a close target (partial increment, or no closing
#     keyword at all) -> query live state via forge_get_issue_state and
#     preserve the worktree unless that state is CLOSED.
#
# Fail-unsafe-to-preserve: any lookup failure (forge_pr_close_targets
# returning nothing, forge_get_issue_state failing / returning an unknown
# state) is treated as "preserve" — cleanup must never destroy a worktree it
# isn't certain is safe to remove. A skipped cleanup here is always
# recoverable later (loom-clean, or a future merge that actually closes the
# issue).
#
# The decision — close-target membership, then (only if needed) the live
# state comparison — is `loom-daemon merge-pr issue-close-gate` (Rust,
# loom-daemon/src/merge_pr/issue_close_gate.rs — #8191 slice), fed
# $close_targets on stdin. The fast-path call (no --state) answers from
# membership alone for the overwhelming common case (`Closes #N`); only when
# it answers NEED-STATE (exit 3) does this wrapper pay for the extra
# forge_get_issue_state round trip and call again with --state. Both forge
# reads stay here.
#
# A daemon that cannot decide (missing, older than this slice, or answering
# off-protocol) is warned about and resolves to PRESERVE — the same fail
# direction the original in-shell comparison already had for any lookup
# failure, now also covering "the decision could not be delegated at all".
#
# Returns 0 (true — safe to clean up) or 1 (false — preserve the worktree).
_issue_is_closed_for_cleanup() {
  local issue_number="$1" close_targets out rc=0 state
  close_targets="$(forge_pr_close_targets "$PR_NUMBER" "$GH" 2>/dev/null || true)"
  out="$(printf '%s\n' "$close_targets" | "${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr issue-close-gate --issue "$issue_number" 2>/dev/null)" || rc=$?
  [[ $rc -eq 3 && "$out" == "LOOM-ISSUE-CLEANUP NEED-STATE" ]] && { state="$(forge_get_issue_state "$REPO_NWO" "$issue_number" "$GH" 2>/dev/null || true)"; rc=0; out="$(printf '%s\n' "$close_targets" | "${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr issue-close-gate --issue "$issue_number" --state "$state" 2>/dev/null)" || rc=$?; }
  [[ $rc -eq 0 ]] && return 0
  [[ $rc -eq 1 ]] && return 1
  warning "The async-close-race cleanup gate for issue #$issue_number (#4186) did not run — '${LOOM_DAEMON_BIN:-loom-daemon} merge-pr issue-close-gate' exited $rc rather than 0/1 (a loom-daemon predating #8191's slice has no such verb). Preserving the worktree rather than guessing (fail-unsafe-to-preserve) — if #$issue_number is actually closed, a future check will clean it up, or remove it by hand once confirmed. $(! declare -F _mp_daemon_roll_hint >/dev/null || _mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_BIN:-loom-daemon}" 2>/dev/null || true)" issue-close-gate)"
  return 1
}

# _worktree_cleanup_decide <kind: default|discovered|judge-pr> <path>
#
# The #6694/#6264 remove-vs-preserve decision, now shared verbatim across
# worktree cleanup's three call sites (the Loom-convention path, the porcelain
# discovery fallback, and a co-existing Judge/Doctor review worktree) instead
# of tripled: `loom-daemon merge-pr worktree-preserve` (Rust,
# loom-daemon/src/merge_pr/worktree_preserve.rs — #8191 slice). Only the two
# already-run checks it needs (_issue_is_closed_for_cleanup, immediately
# above, and the shared branch_has_landed primitive, #7812) and the
# _remove_loom_worktree mutation stay here — the two-input decision plus its
# message text moved. Fails toward PRESERVE on any guard fault (missing/older
# daemon, an unrecognized first line) — the same fail-unsafe-to-preserve
# direction _issue_is_closed_for_cleanup already takes just above, because a
# guessed REMOVE risks the #5031 worktree data-loss class this whole pass
# exists to avoid, while a skipped cleanup is always recoverable later.
_worktree_cleanup_decide() {
  local kind="$1" path="$2" flags=()
  if [[ -n "${ISSUE_NUM:-}" ]] && ! _issue_is_closed_for_cleanup "$ISSUE_NUM"; then
    flags+=(--preserve-check)
    ! branch_has_landed "$PR_BRANCH" "$DEFAULT_BRANCH_NAME" "$PR_HEAD_SHA" || flags+=(--landed)
  fi
  local out rc=0
  out="$("${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr worktree-preserve --kind "$kind" --path "$path" --repo-root "$REPO_ROOT" --pr "$PR_NUMBER" --branch "$PR_BRANCH" --issue-num "${ISSUE_NUM:-}" "${flags[@]+"${flags[@]}"}" --landed-verdict "${BRANCH_LANDED_VERDICT:-}" --landed-evidence "${BRANCH_LANDED_EVIDENCE:-}" 2>/dev/null)" || rc=$?
  local action="${out%%$'\n'*}"
  if [[ $rc -ne 0 || ( "$action" != "REMOVE" && "$action" != "PRESERVE" ) ]]; then
    warning "Worktree cleanup's #6694/#6264 remove-vs-preserve decision for $path did not run — 'loom-daemon merge-pr worktree-preserve' exited $rc without a recognized verdict (a loom-daemon predating #8191's slice has no such verb). Preserving rather than guessing REMOVE: a skipped cleanup is always recoverable (loom-clean, the daemon's reaper, or a future merge), an incorrectly removed worktree is not. $(! declare -F _mp_daemon_roll_hint >/dev/null || _mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_BIN:-loom-daemon}" 2>/dev/null || true)")"
    return 0
  fi
  local lines="" level text
  [[ "$out" != *$'\n'* ]] || lines="${out#*$'\n'}"
  while IFS=$'\t' read -r level text; do
    [[ -n "$level" ]] || continue
    case "$level" in WARNING) warning "$text" ;; *) info "$text" ;; esac
  done <<< "$lines"
  [[ "$action" == "REMOVE" ]] && _remove_loom_worktree "$path"
  return 0
}

if [[ "$CLEANUP_WORKTREE" == "true" ]]; then
  if [[ "${LOOM_PRESERVE_WORKTREE:-0}" == "1" ]]; then
    info "Worktree cleanup skipped (LOOM_PRESERVE_WORKTREE=1) — local branch left in place"
  elif [[ -n "$WORKTREE_PATH_OVERRIDE" ]]; then
    # Explicit operator opt-in: bypass the sentinel guard for THIS path only.
    # The path was already validated at parse time (exists + is a registered
    # worktree of this repo). _remove_loom_worktree will also delete the
    # matching local branch via `git branch -d` (refuses on unmerged commits)
    # — this is the pre-#4100 caller, so no head-SHA safety check is passed;
    # behaviour is unchanged from before #4100.
    info "Cleanup target overridden by --worktree-path: $WORKTREE_PATH_OVERRIDE"
    _remove_loom_worktree "$WORKTREE_PATH_OVERRIDE" "true"
  else
    # Which worktree paths this merge owns, ported to Rust (#8191 slice):
    # `loom-daemon merge-pr cleanup-paths` (loom-daemon/src/merge_pr/
    # cleanup_paths.rs) holds the strict `^feature/issue-([0-9]+)$`
    # classification — only that anchored form matches, so `release-1` /
    # `fix-bug-42` stay PR-style and a trailing-number heuristic cannot aim
    # cleanup at issue-1 / issue-42 — the #3530 overridden-root resolution
    # (through loom-daemon's own port of lib/worktree-root.sh, so the precedence,
    # the repo-basename namespacing, the relative-override rejection and the
    # unreadable-target fallback come from ONE implementation instead of two),
    # $DEFAULT_WT_PATH, and #6264's $JUDGE_PR_WT_PATH — the co-existing
    # Judge/Doctor pr-$PR_NUMBER review worktree, checked below ALONGSIDE (not
    # instead of) the issue-$ISSUE_NUM path, and deliberately empty for a
    # non-issue branch whose default path is already pr-$PR_NUMBER.
    #
    # Every filesystem question stays here: the `[[ -d ]]` tests below, the
    # .loom-managed sentinel, the remove-vs-preserve decision (`merge-pr
    # worktree-preserve`) and the removal itself. Naming a path is not deciding
    # to remove it.
    #
    # Fail direction OPEN, and specifically toward REMOVING NOTHING: a
    # missing/older daemon leaves all three names empty, and an empty
    # $DEFAULT_WT_PATH gates the ENTIRE removal path off — both the convention
    # call site and the porcelain discovery fallback (hence the `elif [[ -n
    # "$DEFAULT_WT_PATH" ]]` below, not a bare `else`). Gating both is the whole
    # point and is NOT redundant: falling into discovery with no plan would be
    # strictly MORE destructive than the healthy path, because discovery finds
    # this very worktree by branch (a Loom builder worktree at issue-<N> tracks
    # feature/issue-<N> and carries .loom-managed) while $ISSUE_NUM is now
    # empty, so _worktree_cleanup_decide omits --preserve-check and #4186's
    # still-open-issue protection is skipped — a degraded daemon would delete a
    # worktree the healthy one preserves, in exactly the #5031 data-loss class
    # this pass exists to prevent. The merge already happened; skipped cleanup is
    # recoverable (loom-clean, the daemon's reaper, the next merge) and every
    # guard that would authorise a removal needs this same binary anyway. The
    # local-branch delete below still runs — it is keyed on $PR_BRANCH, not on
    # any of these names, and was always reached.
    _CP_RC=0; _CP_OUT="$("${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr cleanup-paths --repo-root "$REPO_ROOT" --branch "$PR_BRANCH" --pr "$PR_NUMBER" 2>/dev/null)" || _CP_RC=$?
    [[ $_CP_RC -eq 0 && "$_CP_OUT" == "LOOM-CLEANUP-PATHS"$'\t'* ]] || { warning "Post-merge worktree cleanup for PR #$PR_NUMBER did not run — '${LOOM_DAEMON_BIN:-loom-daemon} merge-pr cleanup-paths' exited $_CP_RC without a LOOM-CLEANUP-PATHS line (a loom-daemon predating #8191's slice has no such verb), so which worktree paths this merge owns is unknown. Nothing is removed rather than guessed — the merge itself already succeeded and is unaffected. Clean up by hand once loom-daemon is available: ${SCRIPT_DIR:-.loom/scripts}/worktree.sh remove <issue>, or loom-clean. $(! declare -F _mp_daemon_roll_hint >/dev/null || _mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_BIN:-loom-daemon}" 2>/dev/null || true)")"; _CP_OUT="LOOM-CLEANUP-PATHS"$'\t\t\t'; }
    # Field order is $DEFAULT_WT_PATH FIRST, and that is load-bearing, not
    # cosmetic: tab is an IFS *whitespace* character, so `read` strips a leading
    # run of IFS whitespace and collapses runs of it — an empty LEADING field
    # cannot survive this read at all. With $ISSUE_NUM first (the order the
    # retired inline shell assigned these in), a non-feature/issue-<N> branch
    # rendered `…\t\t<default>\t`, the `\t\t` run collapsed, $DEFAULT_WT_PATH
    # landed in $ISSUE_NUM and both path names came out EMPTY — silently skipping
    # cleanup for every PR-only branch (docs/…, security/…, slice branches) with
    # no warning at all, because the verb had exited 0 with a well-formed line.
    # Empty TRAILING fields `read` does preserve, and $DEFAULT_WT_PATH is the one
    # field the verb never leaves empty (the other two are empty together, by
    # #6264's asymmetry), so leading with it keeps both empties in the tail.
    IFS=$'\t' read -r DEFAULT_WT_PATH ISSUE_NUM JUDGE_PR_WT_PATH <<<"${_CP_OUT#*$'\t'}"
    if [[ -d "$DEFAULT_WT_PATH" ]]; then
      # Close-target-aware gate (#4186): ISSUE_NUM is only set when
      # PR_BRANCH matched the feature/issue-<N> convention above. When it's
      # unset (the pr-<N> path) this check is skipped entirely — unchanged
      # behavior. See _worktree_cleanup_decide above for the #6694 landed-
      # branch override this now shares with the two call sites below.
      _worktree_cleanup_decide default "$DEFAULT_WT_PATH"
    elif [[ -n "$DEFAULT_WT_PATH" ]]; then
      # Discovery fallback (warn-only): the Loom-convention path is missing,
      # so walk porcelain looking for any worktree tracking $PR_BRANCH. We
      # never auto-remove a discovered worktree — that would violate the
      # ownership model from #3334. Instead we surface the path so the
      # operator can re-run with --worktree-path.
      DISCOVERED_WT="$(_find_worktree_by_branch "$PR_BRANCH")"
      if [[ -n "$DISCOVERED_WT" ]]; then
        # Classification (primary checkout / managed / user-owned) and its
        # message text are `loom-daemon merge-pr discovered-worktree` (#8191
        # slice); the primary comparison and the removal stay here. Only a
        # positively received DECIDE can lead to a removal — a missing/older
        # daemon yields no verdict and removes nothing.
        _DW_OUT="$(_mp_worktree discovered-worktree --branch "$PR_BRANCH" --path "$DISCOVERED_WT" --primary "$(_is_primary_worktree_path "$DISCOVERED_WT" && echo true || echo false)")" || _DW_OUT=""; _DW_ACT="${_DW_OUT%%$'\n'*}"
        case "$_DW_ACT" in
          "LOOM-DISCOVERED DECIDE"|"LOOM-DISCOVERED NOTE")
            while IFS=$'\t' read -r level text; do [[ -z "$level" ]] || { [[ "$level" == WARNING ]] && warning "$text" || info "$text"; }; done <<<"${_DW_OUT#"$_DW_ACT"}"
            [[ "$_DW_ACT" != *DECIDE ]] || _worktree_cleanup_decide discovered "$DISCOVERED_WT" ;;
          *) warning "Discovered worktree $DISCOVERED_WT for '$PR_BRANCH' was left in place — 'loom-daemon merge-pr discovered-worktree' gave no verdict (a loom-daemon predating #8191's slice has no such verb)." ;;
        esac
      else
        info "No worktree found at $DEFAULT_WT_PATH (and none tracking '$PR_BRANCH' in 'git worktree list')"
      fi
    fi

    # #6264: independently check for a co-existing Judge/Doctor review
    # worktree at pr-$PR_NUMBER, alongside whatever the issue-$ISSUE_NUM
    # handling above did. Only set when PR_BRANCH matched feature/issue-<N>
    # (the external-fork branch above already used pr-$PR_NUMBER as
    # DEFAULT_WT_PATH and handled it there — this block would be a pure
    # duplicate for that branch, so JUDGE_PR_WT_PATH stays empty there).
    #
    # Checked by PATH existence, not by the branch checked out inside it —
    # pr-worktree.sh creates this worktree via `git worktree add --detach`
    # then `gh pr checkout --force`; the latter fails (and leaves the
    # worktree on a detached HEAD) when the branch collides with one already
    # checked out elsewhere (e.g. this same issue's issue-$ISSUE_NUM
    # worktree) — see pr-worktree.sh's collision handling. A path-based check
    # here removes the worktree either way, matching reap_pr_worktrees'
    # (loom-daemon's #5939 periodic backstop) own PR-number+path keyed
    # eligibility, which is likewise branch-state-independent.
    if [[ -n "$JUDGE_PR_WT_PATH" ]] && [[ -d "$JUDGE_PR_WT_PATH" ]]; then
      # See the matching comment at the default-path call site above — the
      # same #4186/#6694 decision, shared via _worktree_cleanup_decide.
      _worktree_cleanup_decide judge-pr "$JUDGE_PR_WT_PATH"
    fi
    # Local-branch delete (#4100): the default-convention path, the
    # discovered-Loom-managed-non-standard-path, and the no-worktree-at-all
    # case (rows 2-4 of the issue's path table) all funnel through here —
    # none of them call _maybe_delete_local_branch internally the way the
    # --worktree-path override does above. Passing $PR_HEAD_SHA is a hint that
    # lets the helper answer "has it landed?" without a forge round-trip when
    # the tip matches the merged PR; `branch_landed` covers the squash and
    # rebase cases where neither `git branch --merged` nor a tip match is
    # correct (#7812). If the discovered worktree above was user-owned and left in
    # place, its branch is still checked out there, so this call is a
    # harmless no-op that reports the specific "checked out" refusal instead
    # of attempting a real delete.
    _maybe_delete_local_branch "$PR_BRANCH" "$PR_HEAD_SHA"
  fi
else
  info "Worktree cleanup skipped (--no-cleanup-worktree) — local branch left in place"
fi

success "Done"
