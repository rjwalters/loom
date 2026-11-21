#!/usr/bin/env bash
# test-loom-tools-lease.sh - the issue lease chain on a tools-only host (#14488).
#
# Runs the REAL `loom-tools lease ...` (publish, renew, co-occupancy, release,
# the builder's fence) with NO `loom-daemon` anywhere on PATH, against a
# stateful stub `gh` (the controlled forge). Everything the scripts call back
# into (`forge may-write`, `forge trusted-comments`, `forge check-branch`,
# `lease renewer ...`) must be answered by loom-tools itself.
#
#   (a) ensure with no agent session / an already-dispatched issue prints its
#       message and starts nothing (no publisher, no renewer, no POST)
#   (b) ensure publishes ONE lease and starts ONE renewer; repeating it for the
#       same session leaves one owner and one renewer
#   (c) the script chain ran on loom-tools (the pin reaches the renewer)
#   (d) a live peer's lease is kept: ensure is declined, nothing is posted over
#       it, and a deferred publisher is started that stops with its session
#   (e) the watched process ending stops renewal
#   (f) release is idempotent, stops the renewer and refuses a live peer's record
#   (g) co-occupancy: two live leases -> exit 1; with one -> 0
#   (h) the builder's fence: a competing live owner aborts (exit 4); the
#       applicable branch probe answers through loom-tools
#   (i) routing: with `mode` unset and inherited fleet variables, a lease verb
#       that reaches the forge fails open (ensure/co-occupancy exit 0) or exits 1
#       (release), the offline renewer actions still work; `mode=session` drops
#       the inherited fleet variables before any child sees them
#   (j) daemon-only `lease pr attach` is refused before mode resolution
#
# Needs a built loom-tools: LOOM_TOOLS_TEST_BIN=<path>. Without one the suite
# prints SKIP and exits 0 (a bare shell CI leg has none); the cargo test
# loom-daemon/tests/loom_tools_lease.rs hands it the binary cargo just built.

set -uo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS="$(cd "$TEST_DIR/.." && pwd)"
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m'
TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

TOOLS_SRC="${LOOM_TOOLS_TEST_BIN:-}"
if [[ -z "$TOOLS_SRC" || ! -x "$TOOLS_SRC" ]]; then
    echo "SKIP: LOOM_TOOLS_TEST_BIN does not name a built loom-tools"
    exit 0
fi

assert_eq() {
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$1" == "$2" ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $3"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $3"
        echo "    Expected: '$1'"
        echo "    Actual:   '$2'"
    fi
}
assert_match() {
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$2" == *$1* ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $3"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $3"
        echo "    Expected to contain: '$1'"
        echo "    Actual:              '$2'"
    fi
}

T="$(mktemp -d)" || exit 1
T="$(cd "$T" && pwd -P)" || exit 1
cleanup() {
    jobs -p | xargs kill 2> /dev/null
    pkill -f "$T/" 2> /dev/null || true
    rm -rf "$T" 2> /dev/null || true
}
trap cleanup EXIT

# --- The tools-only host: a bin dir holding loom-tools and the stub gh ------
BIN="$T/bin"
mkdir -p "$BIN" "$T/home" "$T/forge"
ln -s "$TOOLS_SRC" "$BIN/loom-tools"
FORGE="$T/forge"

# A stateful stub forge: comments live in $FORGE/comments.json. POST appends
# a comment authored by the fleet App family (trusted by the real predicate),
# PATCH refreshes updated_at (the lease's liveness signal), GET filters with
# the caller's own jq program.
cat > "$BIN/gh" <<'STUB'
#!/usr/bin/env bash
D="${LOOM_TEST_FORGE:?stub gh: LOOM_TEST_FORGE not set}"
[[ "$1" == "api" ]] || { echo "stub gh: unhandled: $*" >&2; exit 3; }
shift
method="GET"; path=""; jq_filter=""; body=""; have_body=""; include=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --method|-X) method="$2"; shift 2 ;;
    --paginate|--silent) shift ;;
    --include|-i) include=1; shift ;;
    --jq) jq_filter="$2"; shift 2 ;;
    -f|-F) [[ "${2:-}" == body=* ]] && { body="${2#body=}"; have_body=1; }; shift 2 ;;
    -H|--header|--hostname) shift 2 ;;
    *) [[ -n "$path" ]] || path="$1"; shift ;;
  esac
done
[[ "$body" == "@-" ]] && body="$(cat)"
echo "$method $path" >> "$D/calls.log"
echo "gateway=${LOOM_GH_GATEWAY_DIR-<unset>}" >> "$D/env.log"
printf '%s %s %s\n' "$method" "$path" "${GH_CONFIG_DIR-<unset>}" >> "$D/routes.log"
now() { date -u +%Y-%m-%dT%H:%M:%SZ; }
if [[ "$method" == "GET" && "$path" == repos/*/pulls\?* ]]; then
  [[ ! -f "$D/pulls-fail" ]] || exit 1
  [[ "$include" -eq 0 ]] || printf 'HTTP/2 200 OK\r\n\r\n'
  repo="${path#repos/}"; repo="${repo%%/pulls\?*}"
  file="$D/${repo//\//-}-pulls.json"
  if [[ -f "$file" ]]; then cat "$file"; else printf '[]\n'; fi
  exit 0
fi
if [[ "$method" == "GET" && "$path" =~ ^repos/[^/]+/[^/]+$ && "$include" -eq 1 ]]; then
  repo="${path#repos/}"
  [[ ! -f "$D/wrong-repo" ]] || read -r repo < "$D/wrong-repo"
  printf 'HTTP/2 200 OK\r\n\r\n'
  jq -nc --arg repo "$repo" '{id:101,full_name:$repo,owner:{login:($repo|split("/")[0])},name:($repo|split("/")[1])}'
  exit 0
fi
if [[ "$path" == graphql ]]; then
  printf '%s\n' '{"data":{"repository":{"issue":{"closedByPullRequestsReferences":{"nodes":[]}}}}}'
  exit 0
fi
if [[ "$method" == "GET" && "$path" == repos/*/issues/*/timeline ]]; then
  [[ -f "$D/timeline.json" ]] || exit 1
  cat "$D/timeline.json"
  exit 0
fi
if [[ "$method" == "GET" && "$path" == repos/*/issues/*/comments ]]; then
  [[ ! -f "$D/comments-fail" ]] || { echo "stub gh: comments read failed" >&2; exit 1; }
  [[ -f "$D/comments.json" ]] || echo '[]' > "$D/comments.json"
  if [[ -n "$jq_filter" ]]; then jq -c "$jq_filter" "$D/comments.json"; else cat "$D/comments.json"; fi
  exit 0
fi
if [[ "$method" == "GET" && "$path" == repos/*/issues/[0-9]* ]]; then
  echo "{\"state\":\"$(cat "$D/issue-state" 2> /dev/null || echo open)\"}"; exit 0
fi
if [[ "$method" == "POST" && "$path" == repos/*/issues/*/comments ]]; then
  [[ -n "$have_body" ]] || { echo "stub gh: POST without a body" >&2; exit 1; }
  [[ -f "$D/comments.json" ]] || echo '[]' > "$D/comments.json"
  n=$(( $(jq 'length' "$D/comments.json") + 1000 ))
  jq --arg b "$body" --argjson id "$n" --arg t "$(now)" \
    '. + [{id: $id, body: $b, created_at: $t, updated_at: $t,
           user: {login: "loom-fleet-dispatch[bot]", type: "Bot"}, author_association: "CONTRIBUTOR"}]' \
    "$D/comments.json" > "$D/c.tmp" && mv "$D/c.tmp" "$D/comments.json"
  echo "POST $path" >> "$D/posts.log"
  echo "{\"id\": $n}"; exit 0
fi
if [[ "$method" == "PATCH" && "$path" == repos/*/issues/comments/* ]]; then
  id="${path##*/}"
  jq --argjson id "$id" --arg t "$(now)" \
    'map(if .id == $id then .updated_at = $t else . end)' "$D/comments.json" > "$D/c.tmp" \
    && mv "$D/c.tmp" "$D/comments.json"
  printf '%s\n' "${LOOM_DAEMON_BIN-<unset>}" >> "$D/renew-bins.log"
  echo "$id" >> "$D/patches.log"
  echo '{}'; exit 0
fi
echo "stub gh: unhandled: $method $path" >&2; exit 3
STUB
chmod +x "$BIN/gh"

# --- The checkout: installed scripts, an origin the write scope admits ------
CO="$T/checkout"
mkdir -p "$CO/.loom"
cp -r "$SCRIPTS" "$CO/.loom/scripts"
rm -rf "$CO/.loom/scripts/tests"
git -C "$CO" init -q
git -C "$CO" config user.email t@example.invalid
git -C "$CO" config user.name t

export HOME="$T/home"
export PATH="$BIN:/usr/bin:/bin"
export LOOM_TEST_FORGE="$FORGE"
export LOOM_LEASE_RENEW_STATE_DIR="$T/renew-state"
export LOOM_HOST_ID="tools-host"
export SWEEP_LEASE_RENEW_INTERVAL_SECS=1
unset LOOM_DAEMON_BIN LOOM_TOOLS_BIN LOOM_REPO GH_REPO GH_TOKEN GITHUB_TOKEN LOOM_TERMINAL_ID \
    LOOM_SWEEP_ID LOOM_SWEEP_RUN_ID LOOM_SWEEP_LEASE_RENEW_DISPATCHED LOOM_AGENT_SESSION_PID \
    CLAUDE_PID CLAUDECODE CLAUDE_CODE_ENTRYPOINT LOOM_GH_GATEWAY_DIR LOOM_GH_READER_ROOT \
    LOOM_LEASE_PUBLISH_HOSTNAME LOOM_FORCE_SCOPE LOOM_GUARD_DECISION_LOG 2> /dev/null || true
export LOOM_MODE=session

# shellcheck source=lib/write-scope-fixture.sh
source "$TEST_DIR/lib/write-scope-fixture.sh"
write_scope_register "$CO" acme/widget
cd "$CO" || exit 1

assert_eq "" "$(command -v loom-daemon || true)" "the host really has no loom-daemon"

ISSUE=4242
reset_forge() {
    rm -f "$FORGE"/comments.json "$FORGE"/comments-fail "$FORGE"/posts.log "$FORGE"/patches.log \
        "$FORGE"/calls.log "$FORGE"/env.log "$FORGE"/renew-bins.log "$FORGE"/issue-state \
        "$FORGE"/[a-z]*-pulls.json "$FORGE"/routes.log "$FORGE"/wrong-repo \
        "$FORGE"/pulls-fail "$FORGE"/timeline.json
    rm -rf "$LOOM_LEASE_RENEW_STATE_DIR" "$CO/.loom/logs" 2> /dev/null || true
}
count_lines() { if [[ -f "$1" ]]; then wc -l < "$1" | tr -d ' '; else echo 0; fi; }
posts() { count_lines "$FORGE/posts.log"; }
patches() { count_lines "$FORGE/patches.log"; }
# Bash children can inherit the start argv while a renewal is in progress.
# One process snapshot distinguishes the detached roots from their descendants.
renewer_roots() {
    awk -v start="$CO/.loom/scripts/sweep-lease-renew.sh start $ISSUE " '
        {
            parent[$1] = $2
            if ($3 ~ /^Z/ || $4 !~ /(^|\/)bash$/) next
            command = $0
            sub(/^[[:space:]]*[0-9]+[[:space:]]+[0-9]+[[:space:]]+[^[:space:]]+[[:space:]]+[^[:space:]]+[[:space:]]+/, "", command)
            if (index(command, start) == 1) matching[$1] = 1
        }
        END {
            for (pid in matching) {
                ancestor = parent[pid]
                while (ancestor > 1 && ancestor in parent && !(ancestor in matching)) {
                    ancestor = parent[ancestor]
                }
                if (!(ancestor in matching)) print pid
            }
        }' | sort -n
}
renewer_pids() { ps -axww -o pid= -o ppid= -o stat= -o command= | renewer_roots; }
renewers() { renewer_pids | wc -l | tr -d ' '; }
has_renewer() { [[ "$(renewers)" -ge 1 ]]; }
no_renewers() { [[ "$(renewers)" -eq 0 ]]; }
has_patch() { [[ "$(patches)" -ge 1 ]]; }
no_deferred_publishers() { ! pgrep -f "$DEF" > /dev/null; }
session() { sleep 600 & SESSION_PID=$!; disown "$SESSION_PID" 2> /dev/null || true; }
wait_for() { # <seconds> <command...>
    local n=$(($1 * 4)); shift
    while ((n-- > 0)); do "$@" && return 0; sleep 0.25; done
    return 1
}
ensure() { CLAUDE_PID="$SESSION_PID" loom-tools lease ensure "$ISSUE" --watch-pid "$SESSION_PID" "$@" 2>&1; }
stale_peer() { # <host> <sweep> <age-seconds>
    local epoch t
    epoch=$(($(date +%s) - $3))
    t="$(date -u -d "@$epoch" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null ||
        date -u -r "$epoch" +%Y-%m-%dT%H:%M:%SZ)" || return 1
    jq -n --arg h "$1" --arg s "$2" --arg t "$t" \
        '[{id: 77, updated_at: $t, created_at: $t,
           body: ("<!-- loom:lease host=" + $h + " sweep=" + $s + " -->\nprose"),
           user: {login: "loom-fleet-dispatch[bot]", type: "Bot"}, author_association: "CONTRIBUTOR"}]' \
        > "$FORGE/comments.json"
}

echo "(a) settled-by-environment ensure starts nothing"
reset_forge
OUT="$(loom-tools lease ensure "$ISSUE" --watch-pid 1 2>&1)"
RC=$?
assert_eq 0 "$RC" "(a) no agent session: exit 0, even with mode set to nothing"
assert_match "no agent session" "$OUT" "(a) the no-session message is preserved"
OUT="$(env -u LOOM_MODE LOOM_GH_GATEWAY_DIR=/nonexistent CLAUDE_PID=$$ LOOM_SWEEP_LEASE_RENEW_DISPATCHED=$ISSUE \
    loom-tools lease ensure "$ISSUE" --watch-pid 1 2>&1)"
RC=$?
assert_eq 0 "$RC" "(a) already dispatched: exit 0 with mode unset"
assert_match "daemon-dispatched" "$OUT" "(a) the already-dispatched message is preserved"
assert_eq 0 "$(posts)" "(a) neither case posted a lease"
assert_eq 0 "$(renewers)" "(a) neither case started a renewer"

echo "(b) ensure publishes one lease and one renewer; repeating it adds none"
START="$CO/.loom/scripts/sweep-lease-renew.sh start $ISSUE"
ROOTS="$(renewer_roots <<EOF
101 1 S bash $START --watch-pid 42
102 101 S bash $START --watch-pid 42
103 102 S bash $START --watch-pid 42
201 1 S bash $START --watch-pid 42
301 1 Z bash $START --watch-pid 42
EOF
)"
assert_eq $'101\n201' "$ROOTS" "(b) the census detects independent loops and ignores descendants and zombies"
reset_forge
session
OUT="$(ensure)"
RC=$?
assert_eq 0 "$RC" "(b) ensure exits 0"
assert_match "renewal" "$OUT" "(b) ensure reports the renewal loop: $OUT"
assert_eq 1 "$(posts)" "(b) one lease comment was posted"
# Use the marker the actual publisher wrote, including on hosts without sha256sum.
LOCAL_LEASE_HOST="$(jq -er '.[0].body | capture("<!-- loom:lease host=(?<host>[^ ]+) sweep=").host' "$FORGE/comments.json")" || exit 1
wait_for 10 has_renewer
assert_eq 1 "$(renewers)" "(b) one renewer is running"
FIRST_LOOP="$(sed -n 's/.*renewal loop \([0-9]*\) is running.*/\1/p' <<< "$OUT")"
OUT2="$(ensure)"
assert_eq 1 "$(posts)" "(b) the repeat posted no second lease"
assert_eq "$FIRST_LOOP" "$(renewer_pids)" "(b) the repeat left the original renewer as the only loop root"
OWNERS="$(find "$LOOM_LEASE_RENEW_STATE_DIR" -type f ! -name '.lock' ! -name '*.tmp*' 2> /dev/null | wc -l | tr -d ' ')"
assert_eq 1 "$OWNERS" "(b) exactly one owner record exists"
assert_match "renewal loop $FIRST_LOOP is running" "$OUT2" "(b) the repeat re-attached to the same renewal loop ($FIRST_LOOP)"
wait_for 10 has_patch
assert_eq 0 "$([[ "$(patches)" -ge 1 ]] && echo 0 || echo 1)" "(b) the renewer refreshed the lease through the stub forge"

echo "(c) the chain ran on loom-tools"
# The stub observes the pin on the actual PATCH from the detached loop.
RENEWER_BINS="$(while IFS= read -r pin; do
    case "$pin" in
        /*) readlink -f "$pin" ;;
        *) printf '%s\n' "$pin" ;;
    esac
done < "$FORGE/renew-bins.log" | sort -u)"
assert_eq "$(readlink -f "$TOOLS_SRC")" "$RENEWER_BINS" "(c) the renewer used the running loom-tools pin"

echo "(e) the watched process ending stops renewal"
kill "$SESSION_PID" 2> /dev/null
wait_for 15 no_renewers
assert_eq 0 "$(renewers)" "(e) the renewer exited with its session"
BEFORE="$(patches)"
sleep 3
assert_eq "$BEFORE" "$(patches)" "(e) no renewal after the session ended"

echo "(f) release is idempotent and stops the renewer"
reset_forge
session
ensure > /dev/null
wait_for 10 has_renewer
OUT="$(loom-tools lease release "$ISSUE" 2>&1)"
RC=$?
assert_eq 0 "$RC" "(f) release exits 0"
assert_match "posted 1 yield" "$OUT" "(f) release posted a yield record: $OUT"
wait_for 10 no_renewers
assert_eq 0 "$(renewers)" "(f) release stopped the renewer"
OUT="$(loom-tools lease release "$ISSUE" 2>&1)"
RC=$?
assert_eq 0 "$RC" "(f) a second release exits 0"
assert_match "posted 0 yield" "$OUT" "(f) a second release posts nothing: $OUT"
kill "$SESSION_PID" 2> /dev/null
# A live peer's record is never released by this host.
reset_forge
stale_peer other-host-aaaa peer-sweep 30
OUT="$(loom-tools lease release "$ISSUE" 2>&1)"
RC=$?
assert_eq 1 "$RC" "(f) a live peer's lease: release refuses with 1"
assert_match "another host" "$OUT" "(f) the refusal names the cause"
assert_eq 0 "$(posts)" "(f) nothing was posted over the peer's record"

echo "(d) a live peer keeps its record; the deferred publisher stops with its session"
reset_forge
stale_peer other-host-aaaa peer-sweep 30
session
OUT="$(ensure)"
assert_match "deferred publisher" "$OUT" "(d) a declined publish starts a deferred publisher: $OUT"
assert_eq 0 "$(posts)" "(d) nothing was posted over the live peer"
assert_eq 0 "$(renewers)" "(d) no renewer for a declined publish"
DEF="loom-tools lease ensure $ISSUE --deferred"
wait_for 5 pgrep -f "$DEF" > /dev/null
assert_eq 1 "$(pgrep -f "$DEF" | wc -l | tr -d ' ')" "(d) the deferred publisher is a loom-tools process"
ensure > /dev/null
sleep 1
assert_eq 1 "$(pgrep -f "$DEF" | wc -l | tr -d ' ')" "(d) repeating ensure for the session adds no second publisher"
kill "$SESSION_PID" 2> /dev/null
wait_for 20 no_deferred_publishers
assert_eq 0 "$(pgrep -f "$DEF" | wc -l | tr -d ' ')" "(d) the deferred publisher stopped when its session ended"
assert_eq 0 "$(posts)" "(d) ...without ever posting over the live peer"

echo "(g) co-occupancy"
reset_forge
OUT="$(loom-tools lease co-occupancy "$ISSUE" --repo "$CO" 2>&1)"
assert_eq 0 "$?" "(g) no lease: exit 0"
t="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
jq -n --arg t "$t" '[
  {id: 1, updated_at: $t, created_at: $t, body: "<!-- loom:lease host=host-a sweep=s1 -->", user: {login: "loom-fleet-dispatch[bot]", type: "Bot"}, author_association: "CONTRIBUTOR"},
  {id: 2, updated_at: $t, created_at: $t, body: "<!-- loom:lease host=host-b sweep=s2 -->", user: {login: "loom-fleet-dispatch[bot]", type: "Bot"}, author_association: "CONTRIBUTOR"}]' > "$FORGE/comments.json"
OUT="$(loom-tools lease co-occupancy "$ISSUE" --repo "$CO" 2>&1)"
assert_eq 1 "$?" "(g) two live leases: exit 1 (the worktree is shared)"
touch "$FORGE/comments-fail"
OUT="$(loom-tools lease co-occupancy "$ISSUE" --repo "$CO" 2>&1)"
assert_eq 0 "$?" "(g) an unreadable forge fails open"

echo "(h) the builder's pre-push fence"
# A local bare repo stands in for origin, so the branch probe is a real
# `git ls-remote` answered by loom-tools.
BARE="$T/origin.git"
git init -q --bare "$BARE"
git -C "$CO" config "url.$BARE.insteadOf" "https://github.com/acme/widget.git"
FENCE="$CO/.loom/scripts/sweep-lease-fence.sh"
BR="feature/issue-$ISSUE"
reset_forge
stale_peer other-host-aaaa peer-sweep 30
OUT="$("$FENCE" check "$ISSUE" --branch "$BR" 2>&1)"
RC=$?
assert_eq 4 "$RC" "(h) a live competing owner and an absent branch: SUPERSEDED (4)"
assert_match "SUPERSEDED" "$OUT" "(h) ...named as such"
reset_forge
OUT="$("$FENCE" check "$ISSUE" --branch "$BR" 2>&1)"
assert_eq 0 "$?" "(h) no competing owner and an absent branch: proceed (0)"
git -C "$CO" commit -q --allow-empty -m seed
git -C "$CO" push -q "$BARE" "HEAD:refs/heads/$BR" 2> /dev/null
OUT="$("$FENCE" check "$ISSUE" --branch "$BR" 2>&1)"
assert_eq 5 "$?" "(h) a branch already on origin: BRANCH_COLLISION (5), ahead of the lease verdict"
assert_match "BRANCH_COLLISION" "$OUT" "(h) ...named as such"
git -C "$BARE" update-ref -d "refs/heads/$BR"
# The same fence with no binary at all keeps its fail-closed answer (6). A
# redirected CARGO_TARGET_DIR would let the locator find a repo-local build.
mkdir -p "$T/ghonly" && ln -sf "$BIN/gh" "$T/ghonly/gh"
OUT="$(env -u CARGO_TARGET_DIR PATH="$T/ghonly:/usr/bin:/bin" "$FENCE" check "$ISSUE" --branch "$BR" 2>&1)"
assert_eq 6 "$?" "(h) with neither binary the branch probe is unavailable (6), not a pass"

echo "(i) routing"
reset_forge
OUT="$(env -u LOOM_MODE LOOM_GH_GATEWAY_DIR=/nonexistent CLAUDE_PID=$$ loom-tools lease ensure "$ISSUE" --watch-pid $$ 2>&1)"
RC=$?
assert_eq 0 "$RC" "(i) ensure with mode unset and an inherited gateway fails open (exit 0)"
assert_match "mode" "$OUT" "(i) ...and says why"
assert_eq 0 "$(posts)" "(i) ...without posting"
OUT="$(env -u LOOM_MODE LOOM_GH_GATEWAY_DIR=/nonexistent loom-tools lease co-occupancy "$ISSUE" --repo "$CO" 2>&1)"
assert_eq 0 "$?" "(i) co-occupancy with mode unset exits 0, not the worktree-refusal 1"
OUT="$(env -u LOOM_MODE LOOM_GH_GATEWAY_DIR=/nonexistent loom-tools lease release "$ISSUE" 2>&1)"
assert_eq 1 "$?" "(i) release with mode unset exits 1"
OUT="$(env -u LOOM_MODE LOOM_GH_GATEWAY_DIR=/nonexistent loom-tools lease renewer sanitize-exec --check 2>&1)"
assert_eq 0 "$?" "(i) offline renewer actions work with mode unset"
# The inherited fleet routing is dropped on a session route and kept on the
# fleet route, observed from the `gh` the children actually run.
GW="$T/gateway"
mkdir -p "$GW"
reset_forge
OUT="$(LOOM_GH_GATEWAY_DIR=$GW LOOM_MODE=session CLAUDE_PID=$$ loom-tools lease ensure "$ISSUE" --watch-pid $$ 2>&1)"
assert_match "ignoring inherited" "$OUT" "(i) mode=session names and ignores the inherited fleet routing"
assert_eq 1 "$(posts)" "(i) ...and still publishes on the person's own credential"
assert_eq 0 "$(grep -c "gateway=$GW" "$FORGE/env.log")" "(i) no child saw the inherited gateway on the session route"
pkill -f "$CO/.loom/scripts/sweep-lease-renew.sh start $ISSUE " 2> /dev/null || true
reset_forge
OUT="$(LOOM_GH_GATEWAY_DIR=$GW LOOM_MODE=fleet CLAUDE_PID=$$ loom-tools lease ensure "$ISSUE" --watch-pid $$ 2>&1)"
assert_eq 1 "$(posts)" "(i) the fleet route publishes too"
assert_eq 1 "$([[ "$(grep -c "gateway=$GW" "$FORGE/env.log")" -ge 1 ]] && echo 1 || echo 0)" "(i) ...with the exported gateway still visible to its children"
pkill -f "$CO/.loom/scripts/sweep-lease-renew.sh start $ISSUE " 2> /dev/null || true

echo "(k) a write the credential cannot make is refused on the tools-only route"
write_scope_register "$CO" acme/widget pull
rm -rf "$LOOM_WRITE_SCOPE_CACHE_DIR" # the earlier push verdict is cached per repo
reset_forge
session
OUT="$(ensure)"
assert_eq 0 "$(posts)" "(k) read-only credential: no lease was posted"
# publish exits 2 when `forge may-write` (answered by loom-tools) refuses the repo.
assert_match "exited Some(2)" "$OUT" "(k) ...and the declined publish is reported, not swallowed"
stale_peer "$LOCAL_LEASE_HOST" own-old-sweep 5 || exit 1
OUT="$(loom-tools lease release "$ISSUE" 2>&1)"
assert_eq 1 "$?" "(k) release refuses the write from a read-only credential"
assert_match "could not post the yield" "$OUT" "(k) release reached the own-lease write"
assert_eq 0 "$(posts)" "(k) read-only credential: release posts no yield"
kill "$SESSION_PID" 2> /dev/null
pkill -f "$CO/.loom/scripts/sweep-lease-renew.sh start $ISSUE " 2> /dev/null || true
write_scope_register "$CO" acme/widget push
rm -rf "$LOOM_WRITE_SCOPE_CACHE_DIR"

echo "(j) a daemon-owned PR service action is refused before mode resolution"
OUT="$(env -u LOOM_MODE LOOM_GH_GATEWAY_DIR=/nonexistent loom-tools lease pr attach 7 --role judge 2>&1)"
assert_eq 64 "$?" "(j) lease pr attach exits 64"
assert_match "daemon service operations" "$OUT" "(j) ...naming why it is refused"

echo "(l) a linked PR stops a real detached publisher"
LEASE_GATEWAY="$T/routes/.loom/gh-config-gateway"
LEASE_READER="$LEASE_GATEWAY/acme"
LEASE_WRITER="$T/lease-writer"
mkdir -p "$LEASE_READER" "$LEASE_WRITER"
chmod 700 "$LEASE_GATEWAY" "$LEASE_READER" "$LEASE_WRITER"
printf '%s' "$(readlink -f "$LOOM_GH_BIN")" > "$LEASE_GATEWAY/gh-ok"
printf 'github.com: {}\n' > "$LEASE_READER/hosts.yml"
chmod 600 "$LEASE_GATEWAY/gh-ok" "$LEASE_READER/hosts.yml"
ensure_routed() {
    LOOM_MODE=fleet LOOM_GH_GATEWAY_DIR="$LEASE_GATEWAY" GH_CONFIG_DIR="$LEASE_WRITER" ensure --retry-interval 1
}
lease_log="$CO/.loom/logs/lease-ensure/issue-$ISSUE.log"
peer_yields() {
    jq --arg host "$1" --arg sweep "$2" --arg at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
      '. + [{id:78,updated_at:$at,body:("<!-- loom:lease-yield host=" + $host + " sweep=" + $sweep + " earliest_host=none earliest_sweep=none -->"),user:{login:"loom-fleet-dispatch[bot]",type:"Bot"},author_association:"CONTRIBUTOR"}]' \
      "$FORGE/comments.json" > "$FORGE/yield.json" && mv "$FORGE/yield.json" "$FORGE/comments.json"
}
reset_forge
stale_peer other-host-aaaa peer-sweep 30
session
OUT="$(ensure_routed)"
assert_match "deferred publisher" "$OUT" "(l) a live peer deferred the actual tools-only child"
assert_eq 0 "$(posts)" "(l) the initial call did not post over the peer"
jq -n --arg body "Part of #$ISSUE" '[{number:79,state:"open",body:$body,head:{ref:"feature/phase",repo:{full_name:"acme/widget"}},base:{repo:{full_name:"acme/widget"}}}]' \
  > "$FORGE/acme-widget-pulls.json"
peer_yields other-host-aaaa peer-sweep
wait_for 10 no_deferred_publishers
assert_eq 0 "$(posts)" "(l) the delayed child did not publish after its linked PR opened"
assert_eq 0 "$(renewers)" "(l) the delayed child did not start a renewer"
assert_match "linked PR #79" "$(cat "$lease_log")" "(l) the local diagnostic names the linked PR"
wait_for 2 grep -Eq "^GET repos/acme/widget/pulls\?.* $LEASE_READER$" "$FORGE/routes.log"
assert_eq 0 "$?" "(l) the child probed the exact publisher repo through the fleet reader"
kill "$SESSION_PID" 2> /dev/null
wait_for 15 no_renewers
wait_for 15 no_deferred_publishers

echo "(m) verified absence preserves publication and reader/writer routing"
reset_forge
same_host_peer="sweep-insession-s999999-fixture"
stale_peer "$LOCAL_LEASE_HOST" "$same_host_peer" 30
jq -n --arg mention "Discussed #$ISSUE" --arg closes "Closes #$ISSUE" '[
    {number:80,state:"open",body:$mention,head:{ref:"feature/unrelated",repo:{full_name:"acme/widget"}}},
    {number:81,state:"closed",body:$closes,head:{ref:"feature/closed",repo:{full_name:"acme/widget"}}}
  ]' > "$FORGE/acme-widget-pulls.json"
session
OUT="$(LOOM_SWEEP_RUN_ID=fixture-distinct-sweep ensure_routed)"
assert_match "deferred publisher" "$OUT" "(m) a different sweep on the same host defers"
wait_for 5 grep -q 'deferred attempt: publish .*exited Some(4)' "$lease_log"
assert_eq 0 "$?" "(m) the delayed attempt preserved peer arbitration"
assert_eq 0 "$(posts)" "(m) a PR-free read still cannot publish over the same-host peer"
peer_yields "$LOCAL_LEASE_HOST" "$same_host_peer"
wait_for 10 has_renewer
assert_eq 1 "$(posts)" "(m) exactly one publication follows the peer yield"
assert_eq 1 "$(renewers)" "(m) the ordinary renewer starts for the PR-free issue"
assert_eq 1 "$(grep -Fc "POST repos/acme/widget/issues/$ISSUE/comments $LEASE_WRITER" "$FORGE/routes.log" || true)" "(m) publication still uses the writer profile"
kill "$SESSION_PID" 2> /dev/null
wait_for 15 no_renewers

echo "(n) mismatched or incomplete evidence cannot publish for an override"
reset_forge
stale_peer other-host-aaaa peer-sweep 30
printf 'acme/widget\n' > "$FORGE/wrong-repo"
session
OUT="$(LOOM_REPO=acme/other ensure_routed)"
assert_match "deferred publisher" "$OUT" "(n) explicit target starts a deferred child"
peer_yields other-host-aaaa peer-sweep
wait_for 6 grep -q 'linked-PR probe unavailable' "$lease_log"
assert_eq 0 "$?" "(n) the wrong-target attempt was indeterminate"
assert_eq 0 "$(posts)" "(n) metadata for a different repo does not permit publication"
assert_eq 0 "$(renewers)" "(n) the mismatched repo starts no renewer"
touch "$FORGE/pulls-fail"
rm "$FORGE/wrong-repo"
wait_for 6 grep -q "GET repos/acme/other/issues/$ISSUE/timeline" "$FORGE/routes.log"
assert_eq 0 "$?" "(n) the delayed child attempted the explicit target fallback"
assert_match "GET graphql $LEASE_READER" "$(cat "$FORGE/routes.log")" "(n) the GraphQL leg answered on the fleet reader"
assert_eq 0 "$(posts)" "(n) GraphQL empty and timeline unreadable still cannot publish"
assert_eq 0 "$(renewers)" "(n) incomplete partial-link evidence starts no renewer"
jq -nc --arg body "Closes #$ISSUE" '{number:82,body:$body,author_association:"MEMBER"}' > "$FORGE/timeline.json"
wait_for 10 no_deferred_publishers
assert_match "linked PR #82" "$(cat "$lease_log")" "(n) the exact override can prove a closing link and settle"
assert_eq 0 "$(posts)" "(n) the linked override never publishes"
kill "$SESSION_PID" 2> /dev/null
wait_for 15 no_deferred_publishers

echo "(o) a verified PR-free override can still publish"
OTHER="$T/other"
WRITE_SCOPE_INNER_GH="$BIN/gh" write_scope_register "$OTHER" acme/other
write_scope_register "$CO" acme/widget
write_scope_workspaces "$T/deferred-workspaces.json" "$CO" "$OTHER"
reset_forge
stale_peer other-host-aaaa peer-sweep 30
touch "$FORGE/pulls-fail"
: > "$FORGE/timeline.json"
session
OUT="$(LOOM_REPO=acme/other ensure_routed)"
assert_match "deferred publisher" "$OUT" "(o) a verified override defers while the peer owns the lease"
peer_yields other-host-aaaa peer-sweep
wait_for 10 has_renewer
assert_eq 1 "$(posts)" "(o) a complete fallback permits one publication for the override: $(tail -5 "$lease_log")"
assert_eq 1 "$(grep -Fc "POST repos/acme/other/issues/$ISSUE/comments $LEASE_WRITER" "$FORGE/routes.log" || true)" "(o) the publication reaches the verified override on the writer"
kill "$SESSION_PID" 2> /dev/null
wait_for 15 no_renewers
wait_for 15 no_deferred_publishers

echo
echo "Results: $TESTS_PASSED/$TESTS_RUN passed"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "${RED}$TESTS_FAILED FAILED${NC}"
    exit 1
fi
echo -e "${GREEN}ALL PASSED${NC}"
