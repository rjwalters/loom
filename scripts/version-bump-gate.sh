#!/usr/bin/env bash
# version-bump-gate.sh — whether version-bump-on-merge.yml bumps VERSION now (#11174).
#
# Release cadence: at most one `chore: bump version` per RELEASE_MIN_INTERVAL
# (default 86400 s = 24 h). Merges inside the window accumulate, unreleased;
# the next eligible run (a push, the hourly schedule, or a forced dispatch)
# ships them all in one +1 patch. This script is the pure decision: it reads
# local git history only (no forge calls; the workflow's bypass ledger pins it
# at one API call), so every case is unit-tested by test-version-bump-gate.sh
# against fixture repos with a fixed --now.
#
# "Last bump" = the newest FIRST-PARENT commit on --ref that changed VERSION,
# timed by its COMMITTER time (%ct). PRs cannot change VERSION
# (check-defaults-version-bump.sh --forbid-bump), so only bump commits match.
# Tags and Release dates are never read: tags skip versions by design (#10826).
#
# Usage:
#   version-bump-gate.sh --event push|schedule|workflow_dispatch
#                        [--force true|false] [--reason <text>] [--actor <login>]
#                        [--now <epoch>] [--interval <seconds>] [--ref <rev>]
#
#   --interval defaults to $RELEASE_MIN_INTERVAL, then 86400 when that is unset
#   or empty. A non-integer or negative value is an error, never "always bump".
#   0 is accepted and means "no rate limit" (the pre-#11174 behaviour).
#   --force (workflow_dispatch only) skips the interval check, never the
#   "is there anything unreleased under defaults/" check; it needs a --reason.
#
# Output (stdout, key=value): decision=bump|deferred|nothing|error,
#   last_bump_sha=, last_bump_at=, eligible_at=, forced=, reason=
#
# Exit: 0 decision made (bump, deferred or nothing); 2 bad input or history
# that cannot be read (fails closed: a gate wrong in the "defer" direction
# silently stops releases, so it must be red, not quiet). Shell, not a
# loom-daemon subcommand, because it runs in a hosted job before any binary is
# built — the same reason as scripts/release-decision.sh.
set -euo pipefail

event="" force="false" reason="" actor="" now="" ref="origin/main"
interval="${RELEASE_MIN_INTERVAL:-}"
interval_given=0
last_sha="" last_ct="" eligible_ct=""

utc() { # epoch -> ISO-8601 UTC (GNU date, then BSD date)
  date -u -d "@$1" '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || date -u -r "$1" '+%Y-%m-%dT%H:%M:%SZ'
}

emit() { # decision reason
  local at="" el=""
  [[ -n "$last_ct" ]] && at="$(utc "$last_ct")"
  [[ -n "$eligible_ct" ]] && el="$(utc "$eligible_ct")"
  printf 'decision=%s\nlast_bump_sha=%s\nlast_bump_at=%s\neligible_at=%s\nforced=%s\nreason=%s\n' \
    "$1" "$last_sha" "$at" "$el" "$force" "$2"
}

die() { emit error "$1"; exit 2; }

while [[ $# -gt 0 ]]; do
  [[ $# -ge 2 ]] || die "argument '$1' needs a value"
  case "$1" in
    --event) event="${2-}"; shift 2 ;;
    --force) force="${2-}"; shift 2 ;;
    --reason) reason="${2-}"; shift 2 ;;
    --actor) actor="${2-}"; shift 2 ;;
    --now) now="${2-}"; shift 2 ;;
    --interval) interval="${2-}"; interval_given=1; shift 2 ;;
    --ref) ref="${2-}"; shift 2 ;;
    *) die "unknown argument '$1'" ;;
  esac
done

case "$event" in
  push|schedule|workflow_dispatch) ;;
  *) die "--event '${event}' is not push, schedule or workflow_dispatch" ;;
esac
case "$force" in
  true|false) ;;
  *) die "--force '${force}' is not true or false" ;;
esac
if [[ "$force" == "true" ]]; then
  [[ "$event" == "workflow_dispatch" ]] || die "force is only accepted from workflow_dispatch, not ${event}"
  [[ "$reason" =~ [^[:space:]] ]] || die "force=true needs a non-empty reason (it is recorded in the bump commit)"
  [[ "$actor" != *"[bot]" ]] || die "force=true from a bot actor ('${actor}') is refused: forcing is an operator path"
fi

if [[ -z "$interval" && "$interval_given" -eq 0 ]]; then
  interval=86400
fi
[[ "$interval" =~ ^[0-9]{1,10}$ ]] || die "RELEASE_MIN_INTERVAL '${interval}' is not a non-negative integer number of seconds"
interval=$((10#$interval))

if [[ -z "$now" ]]; then
  now="$(date +%s)"
fi
[[ "$now" =~ ^[0-9]{1,12}$ ]] || die "--now '${now}' is not an epoch in seconds"

git rev-parse --verify --quiet "${ref}^{commit}" >/dev/null || die "ref '${ref}' does not resolve to a commit (is the history fetched?)"
# A shallow clone's boundary commit looks like it ADDED VERSION, which would
# read as a bump that just happened and defer forever. Refuse instead.
if [[ "$(git rev-parse --is-shallow-repository)" != "false" ]]; then
  die "shallow clone: the last bump cannot be found reliably (check out with fetch-depth: 0)"
fi

line="$(git log -1 --first-parent --format='%H %ct %s' "$ref" -- VERSION)" \
  || die "git log over '${ref}' failed"
if [[ -z "$line" ]]; then
  die "no commit on ${ref}'s first-parent history changed VERSION: cannot find the last bump (shallow clone?), refusing to guess"
fi
read -r last_sha last_ct subject <<<"$line"
if [[ "$subject" != "chore: bump version to "* ]]; then
  echo "warning: last VERSION change ${last_sha:0:12} is not a 'chore: bump version to' commit: '${subject}'" >&2
fi
eligible_ct=$((last_ct + interval))

# Anything unreleased? Same filter as the push trigger's `paths: defaults/**`,
# which a scheduled or dispatched run does not get. diff-tree compares tree
# ids only, so a blob:none partial clone answers it without fetching blobs.
rc=0
git diff-tree --quiet -r "$last_sha" "$ref" -- defaults/ || rc=$?
case "$rc" in
  0)
    eligible_ct=""
    emit nothing "nothing to release: no defaults/ change on ${ref} since the last bump ${last_sha:0:12}"
    exit 0
    ;;
  1) ;;
  *) die "git diff-tree ${last_sha:0:12} ${ref} -- defaults/ failed (rc=${rc})" ;;
esac

if [[ "$force" == "true" ]]; then
  emit bump "forced by ${actor:-<unknown>}: ${reason}"
  exit 0
fi

if ((now - last_ct >= interval)); then
  emit bump "${interval}s elapsed since the last bump ${last_sha:0:12} and defaults/ changed since it"
  exit 0
fi

emit deferred "deferred: last bump ${last_sha:0:12} at $(utc "$last_ct"); next bump eligible at $(utc "$eligible_ct")"
