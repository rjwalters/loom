#!/usr/bin/env bash
# Tests scripts/version-bump-gate.sh against throwaway git fixtures (#11174).
# Every case pins --now, so no real clock is involved.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
G="$PWD/scripts/version-bump-gate.sh"
fail=0
T0=1700000000 # the fixture's last bump, committer time
DAY=86400
TMP="$(mktemp -d "${TMPDIR:-/tmp}/vbgate.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

utc() { date -u -d "@$1" '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || date -u -r "$1" '+%Y-%m-%dT%H:%M:%SZ'; }

# commit_at <epoch> <message> <path=content>...
commit_at() {
  local t="$1" msg="$2" kv
  shift 2
  for kv in "$@"; do
    mkdir -p "$(dirname "${kv%%=*}")"
    printf '%s\n' "${kv#*=}" >"${kv%%=*}"
    git add "${kv%%=*}"
  done
  GIT_AUTHOR_DATE="@$t +0000" GIT_COMMITTER_DATE="@$t +0000" git commit -q --allow-empty -m "$msg"
}

new_repo() { # dir
  rm -rf "$1"; mkdir -p "$1"
  git -C "$1" init -q -b main
  git -C "$1" config user.name t; git -C "$1" config user.email t@t
  git -C "$1" config commit.gpgsign false
}

# Fixture "base": an old bump, then the last bump at T0, then a defaults/ merge.
REPO="$TMP/base"
new_repo "$REPO"
(cd "$REPO" &&
  commit_at $((T0 - 5 * DAY)) "chore: bump version to 0.1.0" VERSION=0.1.0 defaults/a=1 &&
  commit_at $((T0 - 4 * DAY)) "feat: older work" defaults/a=2 &&
  commit_at "$T0" "chore: bump version to 0.1.1" VERSION=0.1.1 &&
  commit_at $((T0 + 100)) "feat: unreleased defaults change" defaults/a=3)
BUMP_SHA="$(git -C "$REPO" log -1 --format=%H --grep='0.1.1')"

# Fixture "quiet": the last bump, then only a non-defaults/ change.
QUIET="$TMP/quiet"
new_repo "$QUIET"
(cd "$QUIET" &&
  commit_at "$T0" "chore: bump version to 0.1.1" VERSION=0.1.1 defaults/a=1 &&
  commit_at $((T0 + 100)) "docs: root-only change" README=x)

# Fixture "merge": a --no-ff merge of a defaults/ branch after the bump; the
# merge commit (first-parent) does not change VERSION and must be ignored.
MERGE="$TMP/merge"
new_repo "$MERGE"
(cd "$MERGE" &&
  commit_at "$T0" "chore: bump version to 0.1.1" VERSION=0.1.1 defaults/a=1 &&
  git checkout -q -b side &&
  commit_at $((T0 + 50)) "feat: side work" defaults/a=2 &&
  git checkout -q main &&
  GIT_COMMITTER_DATE="@$((T0 + 60)) +0000" GIT_AUTHOR_DATE="@$((T0 + 60)) +0000" \
    git merge -q --no-ff -m "Merge pull request #1 from side" side)
MERGE_BUMP="$(git -C "$MERGE" log -1 --format=%H --grep='0.1.1')"

# Fixture "deep": the last bump is 150 first-parent commits back.
DEEP="$TMP/deep"
new_repo "$DEEP"
(cd "$DEEP" &&
  commit_at "$T0" "chore: bump version to 0.1.1" VERSION=0.1.1 defaults/a=0 &&
  for i in $(seq 1 150); do commit_at $((T0 + i)) "feat: $i" "defaults/a=$i" || exit 1; done)
DEEP_BUMP="$(git -C "$DEEP" log -1 --format=%H --grep='0.1.1')"

# Fixture "nobump": history that never touched VERSION.
NOBUMP="$TMP/nobump"
new_repo "$NOBUMP"
(cd "$NOBUMP" && commit_at "$T0" "feat: start" defaults/a=1)

# Fixture "shallow": a depth-1 clone of base (its boundary commit would look
# like it ADDED VERSION just now).
git clone -q --depth 1 "file://$REPO" "$TMP/shallow" 2>/dev/null

# run name repo expected_rc expected_decision pattern [-- env VAR=value ...] -- gate args...
run() {
  local name="$1" repo="$2" want_rc="$3" want="$4" pat="$5" out rc
  shift 5
  local envs=()
  while [[ $# -gt 0 && "$1" != "--" ]]; do envs+=("$1"); shift; done
  [[ "${1:-}" == "--" ]] && shift
  out="$(cd "$repo" && env -u RELEASE_MIN_INTERVAL "${envs[@]}" bash "$G" --ref main "$@" 2>&1)"; rc=$?
  if [[ $rc -ne $want_rc ]] || ! grep -qx "decision=$want" <<<"$out" || ! grep -q -- "$pat" <<<"$out"; then
    echo "FAIL: $name (rc=$rc want $want_rc, decision want $want, pattern '$pat')"; echo "$out"; fail=1
  else echo "ok: $name"; fi
}

E_AT="$(utc $((T0 + DAY)))"
run "push inside the window defers" "$REPO" 0 deferred "^eligible_at=$E_AT$" \
  -- --event push --now $((T0 + 3600))
run "deferred reason names the last bump and the eligible time" "$REPO" 0 deferred \
  "^reason=deferred: last bump ${BUMP_SHA:0:12} at $(utc "$T0"); next bump eligible at $E_AT$" \
  -- --event push --now $((T0 + 3600))
run "deferred names the full last bump sha" "$REPO" 0 deferred "^last_bump_sha=$BUMP_SHA$" \
  -- --event push --now $((T0 + 3600))
run "push after the window bumps" "$REPO" 0 bump "elapsed" -- --event push --now $((T0 + DAY + 1))
run "exactly at the boundary bumps (>=)" "$REPO" 0 bump "" -- --event push --now $((T0 + DAY))
run "one second before the boundary defers" "$REPO" 0 deferred "" -- --event push --now $((T0 + DAY - 1))
run "a clock behind the last bump defers" "$REPO" 0 deferred "" -- --event push --now $((T0 - 10))
run "schedule after the window with defaults/ changes bumps" "$REPO" 0 bump "" \
  -- --event schedule --now $((T0 + DAY + 3600))
run "schedule inside the window defers" "$REPO" 0 deferred "" -- --event schedule --now $((T0 + 60))
run "schedule with no defaults/ change since the bump: nothing" "$QUIET" 0 nothing "nothing to release" \
  -- --event schedule --now $((T0 + 9 * DAY))
run "push with no defaults/ change since the bump: nothing (already released)" "$QUIET" 0 nothing "" \
  -- --event push --now $((T0 + 9 * DAY))
run "dispatch without force runs the normal gate" "$REPO" 0 deferred "" \
  -- --event workflow_dispatch --force false --now $((T0 + 60))
run "force inside the window bumps" "$REPO" 0 bump "^reason=forced by alice: floor needs fix" \
  -- --event workflow_dispatch --force true --reason "floor needs fix" --actor alice --now $((T0 + 60))
run "force records forced=true" "$REPO" 0 bump "^forced=true$" \
  -- --event workflow_dispatch --force true --reason "x" --actor alice --now $((T0 + 60))
run "force with nothing unreleased is still nothing" "$QUIET" 0 nothing "" \
  -- --event workflow_dispatch --force true --reason "x" --actor alice --now $((T0 + 60))
run "force with no reason fails" "$REPO" 2 error "non-empty reason" \
  -- --event workflow_dispatch --force true --actor alice --now $((T0 + 60))
run "force with a whitespace reason fails" "$REPO" 2 error "non-empty reason" \
  -- --event workflow_dispatch --force true --reason "   " --actor alice --now $((T0 + 60))
run "force from a bot actor fails" "$REPO" 2 error "bot actor" \
  -- --event workflow_dispatch --force true --reason x --actor "renovate[bot]" --now $((T0 + 60))
run "force on a push event fails" "$REPO" 2 error "only accepted from workflow_dispatch" \
  -- --event push --force true --reason x --now $((T0 + 60))
run "bad force value fails" "$REPO" 2 error "not true or false" -- --event push --force yes
run "unknown event fails" "$REPO" 2 error "is not push" -- --event pull_request
run "missing event fails" "$REPO" 2 error "is not push"
run "unknown argument fails" "$REPO" 2 error "unknown argument" -- --event push --bogus 1
run "argument without a value fails" "$REPO" 2 error "needs a value" -- --event
run "interval unset defaults to 86400 (defer at -1s)" "$REPO" 0 deferred "" \
  -- --event push --now $((T0 + DAY - 1))
run "interval unset defaults to 86400 (bump at boundary)" "$REPO" 0 bump "" -- --event push --now $((T0 + DAY))
run "empty RELEASE_MIN_INTERVAL (unset repo var) defaults to 86400" "$REPO" 0 deferred "" \
  RELEASE_MIN_INTERVAL= -- --event push --now $((T0 + DAY - 1))
run "RELEASE_MIN_INTERVAL from the environment is honoured" "$REPO" 0 bump "" \
  RELEASE_MIN_INTERVAL=3600 -- --event push --now $((T0 + 3600))
run "RELEASE_MIN_INTERVAL=abc fails" "$REPO" 2 error "not a non-negative integer" \
  RELEASE_MIN_INTERVAL=abc -- --event push --now $((T0 + 9 * DAY))
run "RELEASE_MIN_INTERVAL=-5 fails" "$REPO" 2 error "not a non-negative integer" \
  RELEASE_MIN_INTERVAL=-5 -- --event push --now $((T0 + 9 * DAY))
run "--interval abc fails" "$REPO" 2 error "not a non-negative integer" -- --event push --interval abc
run "explicit empty --interval fails" "$REPO" 2 error "not a non-negative integer" -- --event push --interval ""
run "interval 0 means no rate limit" "$REPO" 0 bump "" -- --event push --interval 0 --now $((T0 + 101))
run "bad --now fails" "$REPO" 2 error "not an epoch" -- --event push --now tomorrow
run "no bump commit in history fails closed" "$NOBUMP" 2 error "cannot find the last bump" \
  -- --event push --now $((T0 + 9 * DAY))
run "unknown ref fails closed" "$REPO" 2 error "does not resolve" -- --event push --ref nope
run "shallow clone fails closed" "$TMP/shallow" 2 error "shallow clone" \
  -- --event push --ref origin/main --now $((T0 + 9 * DAY))
run "a first-parent merge commit that does not touch VERSION is ignored" "$MERGE" 0 deferred \
  "^last_bump_sha=$MERGE_BUMP$" -- --event push --now $((T0 + 3600))
run "the merged defaults/ change counts as unreleased" "$MERGE" 0 bump "" -- --event push --now $((T0 + DAY))
run "a bump 150 commits back is still found" "$DEEP" 0 deferred "^last_bump_sha=$DEEP_BUMP$" \
  -- --event push --now $((T0 + 3600))

# Wiring: the workflow calls the gate and never interpolates the reason in run:.
WF=.github/workflows/version-bump-on-merge.yml
if grep -q 'bash scripts/version-bump-gate.sh' "$WF"; then echo "ok: workflow invokes the gate"
else echo "FAIL: $WF does not invoke scripts/version-bump-gate.sh"; fail=1; fi
if grep -nE '^[^#]*\$\{\{ *inputs\.reason' "$WF" | grep -vE '^[0-9]+: +[A-Z_]+: \$\{\{ inputs\.reason \}\}$'; then
  echo "FAIL: inputs.reason reaches $WF outside an env: entry (template injection)"; fail=1
else echo "ok: inputs.reason reaches the workflow only through env:"; fi
exit $fail
