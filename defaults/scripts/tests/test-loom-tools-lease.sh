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
#       (release), the offline renewer actions still work; explicit modes
#       use the expected reader and writer across the actual lease lifecycle
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
method="GET"; path=""; jq_filter=""; body=""; have_body=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --method|-X) method="$2"; shift 2 ;;
    --paginate|--silent) shift ;;
    --jq) jq_filter="$2"; shift 2 ;;
    -f|-F) [[ "${2:-}" == body=* ]] && { body="${2#body=}"; have_body=1; }; shift 2 ;;
    -H|--header) shift 2 ;;
    *) [[ -n "$path" ]] || path="$1"; shift ;;
  esac
done
[[ "$body" == "@-" ]] && body="$(cat)"
echo "$method $path" >> "$D/calls.log"
operation=""
case "$method $path" in
  "GET repos/acme/widget") operation=permissions ;;
  GET\ repos/*/issues/*/comments|GET\ repos/*/issues/*/comments\?*) operation=comments ;;
  GET\ repos/*/issues/*) operation=state ;;
  POST\ repos/*/issues/*/comments)
    case "$body" in
      "<!-- loom:lease "*) operation=publish ;;
      "<!-- loom:lease-yield "*) operation=yield ;;
    esac ;;
  PATCH\ repos/acme/widget/issues/comments/*) operation=renew ;;
esac
if [[ -n "${LOOM_TEST_PROFILES:-}" ]]; then
  case "${GH_CONFIG_DIR:-}" in
    "$LOOM_TEST_PROFILES/.loom/gh-config-gateway/acme") identity=reader ;;
    "$LOOM_TEST_PROFILES/writer") identity=writer ;;
    "$LOOM_TEST_PROFILES/local") identity=local ;;
    *) identity=unexpected ;;
  esac
  printf '%s %s\n' "$identity" "${operation:-unexpected}" >> "$D/routes.log"
fi
now() { date -u +%Y-%m-%dT%H:%M:%SZ; }
if [[ "$operation" == permissions ]]; then
  echo '{"push":true}'; exit 0
fi
if [[ "$operation" == comments ]]; then
  [[ ! -f "$D/comments-fail" ]] || { echo "stub gh: comments read failed" >&2; exit 1; }
  [[ -f "$D/comments.json" ]] || echo '[]' > "$D/comments.json"
  if [[ -n "$jq_filter" ]]; then jq -c "$jq_filter" "$D/comments.json"; else cat "$D/comments.json"; fi
  exit 0
fi
if [[ "$operation" == state ]]; then
  state="$(cat "$D/issue-state" 2> /dev/null || echo open)"
  if [[ "$jq_filter" == .state ]]; then echo "$state"; else printf '{"state":"%s"}\n' "$state"; fi
  exit 0
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
    LOOM_LEASE_PUBLISH_HOSTNAME LOOM_FORCE_SCOPE LOOM_GUARD_DECISION_LOG GH_CONFIG_DIR \
    LOOM_TEST_PROFILES LOOM_READ_ROUTING LOOM_FACADE_CWD_ROUTING 2> /dev/null || true
export LOOM_MODE=session

# shellcheck source=lib/write-scope-fixture.sh
source "$TEST_DIR/lib/write-scope-fixture.sh"
write_scope_register "$CO" acme/widget
cd "$CO" || exit 1

assert_eq "" "$(command -v loom-daemon || true)" "the host really has no loom-daemon"

ISSUE=4242
reset_forge() {
    rm -f "$FORGE"/comments.json "$FORGE"/comments-fail "$FORGE"/posts.log "$FORGE"/patches.log \
        "$FORGE"/calls.log "$FORGE"/routes.log "$FORGE"/renew-bins.log "$FORGE"/issue-state
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
deferred_publishers() { pgrep -f "loom-tools lease ensure $ISSUE --deferred .*--watch-pid $SESSION_PID( |$)"; }
no_deferred_publishers() { ! deferred_publishers > /dev/null; }
session() { sleep 600 < /dev/null > /dev/null 2>&1 & SESSION_PID=$!; disown "$SESSION_PID" 2> /dev/null || true; }
wait_for() { # <seconds> <command...>
    local n=$(($1 * 4)); shift
    while ((n-- > 0)); do "$@" && return 0; sleep 0.25; done
    return 1
}
ensure() { CLAUDE_PID="$SESSION_PID" loom-tools lease ensure "$ISSUE" --watch-pid "$SESSION_PID" 2>&1; }
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
wait_for 5 deferred_publishers > /dev/null
assert_eq 1 "$(deferred_publishers | wc -l | tr -d ' ')" "(d) the deferred publisher is a loom-tools process"
ensure > /dev/null
sleep 1
assert_eq 1 "$(deferred_publishers | wc -l | tr -d ' ')" "(d) repeating ensure for the session adds no second publisher"
kill "$SESSION_PID" 2> /dev/null
wait_for 20 no_deferred_publishers
assert_eq 0 "$(deferred_publishers | wc -l | tr -d ' ')" "(d) the deferred publisher stopped when its session ended"
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

# Both profiles return the same fixture data. Check the effective route on
# every operation, including permission reads which must identify the writer.
check_routes() {
    local reader="$1" writer="$2" log="$3" required actual op expected failed=0
    shift 3
    [[ -s "$log" ]] || { echo "no forge calls in $log"; return 1; }
    while read -r actual op; do
        case "$op" in
            comments|state) expected="$reader" ;;
            permissions|publish|renew|yield) expected="$writer" ;;
            *) echo "unexpected forge operation: $actual $op"; failed=1; continue ;;
        esac
        if [[ "$actual" != "$expected" ]]; then
            echo "$op used $actual, expected $expected"
            failed=1
        fi
    done < "$log"
    for required in "$@"; do
        grep -q " $required\$" "$log" || { echo "missing $required call"; failed=1; }
    done
    return "$failed"
}

check_phase_routes() {
    local result
    sort -u "$FORGE/routes.log" > "$FORGE/phase.log"
    result="$(check_routes "$reader" "$writer" "$FORGE/phase.log" "$@")"
    assert_eq "" "$result" "$LOOM_MODE $phase uses the expected routes"
    : > "$FORGE/routes.log"
}

owner_pids() {
    find "$LOOM_LEASE_RENEW_STATE_DIR" -name '*.owner' -exec jq -r 'select(.released == false) | .pid' '{}' + \
        2> /dev/null
}

routed_lifecycle() {
    local LOOM_MODE="$1" reader="$2" writer="$3"
    local LOOM_TEST_PROFILES="$T/profiles" GH_CONFIG_DIR="$T/profiles/$writer"
    local LOOM_GH_GATEWAY_DIR="$T/profiles/.loom/gh-config-gateway" LOOM_GH_BIN="$BIN/gh"
    local LOOM_REPO=acme/widget
    local FORGE="$T/forge-$LOOM_MODE" LOOM_LEASE_RENEW_STATE_DIR="$T/renew-$LOOM_MODE"
    local LOOM_TEST_FORGE="$FORGE" LOOM_WRITE_SCOPE_CACHE_DIR="$T/scope-$LOOM_MODE"
    local SESSION_PID OUT loop owner before phase peer_rows wrong
    export LOOM_MODE LOOM_TEST_PROFILES GH_CONFIG_DIR LOOM_GH_GATEWAY_DIR LOOM_GH_BIN
    export LOOM_TEST_FORGE LOOM_LEASE_RENEW_STATE_DIR LOOM_WRITE_SCOPE_CACHE_DIR LOOM_REPO
    mkdir -p "$FORGE" "$GH_CONFIG_DIR" "$LOOM_GH_GATEWAY_DIR/acme"
    printf '%s' "$BIN/gh" > "$LOOM_GH_GATEWAY_DIR/gh-ok"
    echo 'github.com: {}' > "$LOOM_GH_GATEWAY_DIR/acme/hosts.yml"
    chmod 700 "$LOOM_GH_GATEWAY_DIR" "$LOOM_GH_GATEWAY_DIR/acme"
    chmod 600 "$LOOM_GH_GATEWAY_DIR/gh-ok" "$LOOM_GH_GATEWAY_DIR/acme/hosts.yml"
    reset_forge

    phase=ensure
    session
    OUT="$(ensure)"
    assert_match "renewal loop" "$OUT" "$LOOM_MODE ensure starts a real renewer"
    if [[ "$LOOM_MODE" != fleet ]]; then
        assert_match "ignoring inherited LOOM_GH_GATEWAY_DIR" "$OUT" "$LOOM_MODE selects the local route"
    fi
    loop="$(renewer_pids)"
    owner="$(jq -r '.[0].id' "$FORGE/comments.json")"
    assert_eq 1 "$(posts)" "$LOOM_MODE ensure publishes one lease"
    assert_eq "$loop" "$(owner_pids)" "$LOOM_MODE ensure records its exact renewer as owner"
    OUT="$(ensure)"
    assert_match "renewal loop $loop is running" "$OUT" "$LOOM_MODE repeat reuses its renewer"
    assert_eq "$loop" "$(renewer_pids)" "$LOOM_MODE repeat leaves the same single detached process"
    assert_eq 1 "$(posts)" "$LOOM_MODE repeat reuses its lease"
    assert_eq "$loop" "$(owner_pids)" "$LOOM_MODE repeat leaves the same owner record"
    wait_for 10 has_patch
    assert_eq "$owner" "$(sort -u "$FORGE/patches.log")" "$LOOM_MODE renewal PATCH targets its exact comment"

    OUT="$(loom-tools lease co-occupancy "$ISSUE" --repo "$CO" 2>&1)"
    assert_eq 0 "$?" "$LOOM_MODE allows co-occupancy with one lease"
    OUT="$("$FENCE" check "$ISSUE" --branch "$BR" 2>&1)"
    assert_eq 0 "$?" "$LOOM_MODE fence allows the owner with an absent branch"

    OUT="$(loom-tools lease release "$ISSUE" 2>&1)"
    assert_eq 0 "$?" "$LOOM_MODE release succeeds with a live watch"
    assert_match "posted 1 yield" "$OUT" "$LOOM_MODE release yields the lease"
    wait_for 10 no_renewers
    assert_eq 0 "$(renewers)" "$LOOM_MODE release stops the detached renewer"
    assert_eq "" "$(owner_pids)" "$LOOM_MODE release clears active ownership"
    before="$(posts)"
    OUT="$(loom-tools lease release "$ISSUE" 2>&1)"
    assert_eq 0 "$?" "$LOOM_MODE repeating release succeeds"
    assert_match "posted 0 yield" "$OUT" "$LOOM_MODE repeating release leaves the yield alone"
    assert_eq "$before" "$(posts)" "$LOOM_MODE release is idempotent"
    kill "$SESSION_PID" 2> /dev/null
    phase="publication, renewal, release"
    check_phase_routes comments permissions state publish renew yield

    # A fresh session owns a fresh lease. Watching it exit stops the loop;
    # the owner record remains and can be reclaimed because its PID is dead.
    session
    OUT="$(ensure)"
    assert_match "renewal loop" "$OUT" "$LOOM_MODE starts a new loop for the new watch"
    loop="$(renewer_pids)"
    kill "$SESSION_PID" 2> /dev/null
    wait_for 10 no_renewers
    assert_eq 0 "$(renewers)" "$LOOM_MODE watch exit stops renewal"
    assert_eq "$loop" "$(owner_pids)" "$LOOM_MODE watch exit leaves only the stopped process's record"
    before="$(patches)"
    sleep 1
    assert_eq "$before" "$(patches)" "$LOOM_MODE watch exit stops further PATCHes"

    cp "$FORGE/comments.json" "$FORGE/own.json"
    stale_peer other-host-aaaa peer-sweep 30
    peer_rows="$(cat "$FORGE/comments.json")"
    jq -s 'add' "$FORGE/own.json" "$FORGE/comments.json" > "$FORGE/combined.json"
    mv "$FORGE/combined.json" "$FORGE/comments.json"
    : > "$FORGE/routes.log"
    OUT="$(loom-tools lease co-occupancy "$ISSUE" --repo "$CO" 2>&1)"
    assert_eq 1 "$?" "$LOOM_MODE refuses co-occupancy with two live leases"
    phase="co-occupancy refusal"
    check_phase_routes comments
    if [[ "$LOOM_MODE" == fleet ]]; then
        # A single live lease gives the same successful co-occupancy result
        # whether fleet selects its reader or session selects its writer.
        cp "$FORGE/own.json" "$FORGE/comments.json"
        wrong="$(LOOM_MODE=session loom-tools lease co-occupancy "$ISSUE" --repo "$CO" 2>&1)"
        assert_eq 0 "$?" "wrong route serves valid data and co-occupancy still succeeds"
        assert_match "ignoring inherited LOOM_GH_GATEWAY_DIR" "$wrong" "wrong-route control selects the writer credential"
        assert_eq "comments used writer, expected reader" \
            "$(check_routes "$reader" "$writer" "$FORGE/routes.log" comments)" \
            "route assertions reject the successful wrong-route read"
        : > "$FORGE/routes.log"
    fi

    printf '%s\n' "$peer_rows" > "$FORGE/comments.json"
    OUT="$("$FENCE" check "$ISSUE" --branch "$BR" 2>&1)"
    assert_eq 4 "$?" "$LOOM_MODE fence refuses the competing owner"
    assert_match "SUPERSEDED" "$OUT" "$LOOM_MODE fence found the live competing owner"
    phase="competing fence"
    check_phase_routes comments
    session
    OUT="$(ensure)"
    assert_match "deferred publisher" "$OUT" "$LOOM_MODE cannot publish over the peer"
    OUT="$(loom-tools lease release "$ISSUE" 2>&1)"
    assert_eq 1 "$?" "$LOOM_MODE cannot release the peer"
    assert_match "another host" "$OUT" "$LOOM_MODE finds the peer's owner"
    kill "$SESSION_PID" 2> /dev/null
    wait_for 20 no_deferred_publishers
    assert_eq 0 "$(renews_for_peer)" "$LOOM_MODE never PATCHes the peer"
    assert_eq "$peer_rows" "$(cat "$FORGE/comments.json")" "$LOOM_MODE preserves the peer without a publish or yield"
    assert_eq 0 "$(renewers)" "$LOOM_MODE starts no renewer for the peer"
    assert_eq "$loop" "$(owner_pids)" "$LOOM_MODE writes no owner for the peer"
    phase="competing ensure and release"
    check_phase_routes comments
}

renews_for_peer() {
    grep -c '^77$' "$FORGE/patches.log"
}

routed_lifecycle fleet reader writer
routed_lifecycle local-daemon local local
routed_lifecycle session local local

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

echo
echo "Results: $TESTS_PASSED/$TESTS_RUN passed"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "${RED}$TESTS_FAILED FAILED${NC}"
    exit 1
fi
echo -e "${GREEN}ALL PASSED${NC}"
