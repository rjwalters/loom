#!/usr/bin/env bash
# test-sweep-lease-publish.sh - Unit tests for sweep-lease-publish.sh (#6320).
#
# Black-box tests, mirroring test-sweep-lease-renew.sh's harness:
# sweep-lease-publish.sh is a full CLI script, so `gh` is stubbed on PATH
# (real `jq`/`date`/`awk` are used unstubbed — their logic is what's under
# test) and the real script runs as a subprocess, asserting on
# stdout/stderr/exit code and on the exact request body the stub captured.
#
# The stub reproduces real `gh api` -f/-F semantics: only `-F` applies the
# `@-` read-from-stdin magic, `-f` sends the literal string. That
# distinction is the subject of test (h) — the same trap that silently
# destroyed live lease records via sweep-lease-renew.sh.
#
# Covers:
#   (a) publish on an issue with no lease comment posts a well-formed record
#       whose LITERAL first line is the marker, and prints "<host> <sweep-id>"
#   (b) publish is idempotent for the same host+sweep-id with a fresh lease
#       (no duplicate comment posted, still exit 0)
#   (c) a fresh lease held by a DIFFERENT host -> exit 4, nothing posted
#   (d) a STALE lease (past TTL), same host or a peer's, does not block
#       publication
#   (e) a fresh lease from a different sweep on the SAME host -> publishes
#       anyway (this sweep is the one working the issue now)
#   (f) a `gh` READ failure fails open: publishes anyway
#   (g) a `gh` WRITE failure -> exit 2 (caller proceeds without a lease)
#   (h) the POST uses `-F body=@-` (stdin), never `-f` (literal "@-")
#   (i) usage errors: bad issue number, unknown flag, bad --ttl-minutes,
#       whitespace/`-->` in --host/--sweep-id
#   (j) comments that merely MENTION the marker mid-body are not leases
#       (startswith, not substring)
#
# Usage:
#   ./.loom/scripts/tests/test-sweep-lease-publish.sh

set -uo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$TEST_DIR/.." && pwd)"
SCRIPT="$SCRIPTS_DIR/sweep-lease-publish.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

assert_eq() {
    local expected="$1" actual="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$expected" == "$actual" ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg"
        echo "    Expected: '$expected'"
        echo "    Actual:   '$actual'"
    fi
}

assert_contains() {
    local haystack="$1" needle="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if printf '%s' "$haystack" | grep -qF -- "$needle"; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg"
        echo "    Expected substring: '$needle'"
        echo "    In: '$haystack'"
    fi
}

assert_true() {
    local cond="$1" msg="$2"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$cond" == "true" ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg"
    fi
}

if [[ ! -x "$SCRIPT" ]]; then
    echo -e "${RED}FATAL${NC}: $SCRIPT not found or not executable" >&2
    exit 2
fi

STUB_DIR="$(mktemp -d)"
trap 'rm -rf "$STUB_DIR" 2>/dev/null || true' EXIT

# --- Stub gh on PATH ------------------------------------------------------
#   gh api [-R repo] repos/{owner}/{repo}/issues/<N>/comments --paginate --jq F
#       -> applies the real `jq` filter F to $STUB_DIR/comments.json (or "[]");
#          fails if $STUB_DIR/comments-fail exists
#   gh api [-R repo] --method POST repos/{owner}/{repo}/issues/<N>/comments -F body=@-
#       -> resolves the body EXACTLY as real gh would (only -F reads stdin;
#          -f is a literal string), writes it to $STUB_DIR/post-<n>.body,
#          appends a line to $STUB_DIR/post-calls.log, prints a comment JSON;
#          fails if $STUB_DIR/post-fail exists
cat > "$STUB_DIR/gh" <<'STUB'
#!/usr/bin/env bash
D="${LOOM_TEST_STUB_DIR:?stub gh: LOOM_TEST_STUB_DIR not set}"
if [[ "$1" == "api" ]]; then
  shift
  method="GET"
  path=""
  jq_filter=""
  raw_body=""; have_raw_body=""
  typed_body=""; have_typed_body=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --method) method="$2"; shift 2 ;;
      -R) shift 2 ;;
      --paginate) shift ;;
      --jq) jq_filter="$2"; shift 2 ;;
      -f)
        if [[ "${2:-}" == body=* ]]; then raw_body="${2#body=}"; have_raw_body=1; fi
        echo "flag:-f" >> "$D/post-flags.log"
        shift 2
        ;;
      -F)
        if [[ "${2:-}" == body=* ]]; then typed_body="${2#body=}"; have_typed_body=1; fi
        echo "flag:-F" >> "$D/post-flags.log"
        shift 2
        ;;
      *)
        if [[ -z "$path" ]]; then path="$1"; fi
        shift
        ;;
    esac
  done
  resolve_body() {
    if [[ -n "$have_typed_body" ]]; then
      case "$typed_body" in
        "@-") cat ;;
        @*) cat "${typed_body#@}" ;;
        *) printf '%s' "$typed_body" ;;
      esac
      return 0
    fi
    if [[ -n "$have_raw_body" ]]; then
      # -f is a LITERAL string: "@-" stays "@-", stdin is never read.
      printf '%s' "$raw_body"
      return 0
    fi
    return 0
  }
  if [[ "$method" == "GET" && "$path" == repos/*/issues/*/comments ]]; then
    if [[ -f "$D/comments-fail" ]]; then
      echo "stub gh: comments fetch failed" >&2
      exit 1
    fi
    canned="$D/comments.json"
    [[ -f "$canned" ]] || echo "[]" > "$canned"
    if [[ -n "$jq_filter" ]]; then
      jq -c "$jq_filter" "$canned"
    else
      cat "$canned"
    fi
    exit 0
  fi
  if [[ "$method" == "POST" && "$path" == repos/*/issues/*/comments ]]; then
    if [[ -f "$D/post-fail" ]]; then
      echo "stub gh: comment post failed" >&2
      exit 1
    fi
    n=$(( $(cat "$D/post-count" 2>/dev/null || echo 0) + 1 ))
    echo "$n" > "$D/post-count"
    resolve_body > "$D/post-$n.body"
    echo "$path" >> "$D/post-calls.log"
    echo "{\"id\": $((9000 + n))}"
    exit 0
  fi
  echo "stub gh: unhandled api args: method=$method path=$path" >&2
  exit 3
fi
echo "stub gh: unhandled args: $*" >&2
exit 3
STUB
chmod +x "$STUB_DIR/gh"

export LOOM_TEST_STUB_DIR="$STUB_DIR"
export PATH="$STUB_DIR:$PATH"
# Deterministic identity + clock for every case below.
export LOOM_HOST_ID="studio-host"

reset_state() {
    rm -f "$STUB_DIR"/comments.json "$STUB_DIR"/comments-fail "$STUB_DIR"/post-fail
    rm -f "$STUB_DIR"/post-*.body "$STUB_DIR"/post-count "$STUB_DIR"/post-calls.log
    rm -f "$STUB_DIR"/post-flags.log
    unset LOOM_LEASE_PUBLISH_NOW
}

post_count() {
    cat "$STUB_DIR/post-count" 2>/dev/null || echo 0
}

run_script() {
    OUT="$("$SCRIPT" "$@" 2> "$STUB_DIR/stderr.log")"
    RC=$?
    ERR="$(cat "$STUB_DIR/stderr.log" 2> /dev/null || true)"
}

# A lease comment fixture: $1 = host, $2 = sweep id, $3 = updated_at
lease_json() {
    jq -n --arg host "$1" --arg sweep "$2" --arg ts "$3" \
        '[{updated_at: $ts, body: ("<!-- loom:lease host=" + $host + " sweep=" + $sweep + " -->\nprose")}]'
}

NOW_ISO="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
NOW_EPOCH="$(date -u +%s)"
FRESH_ISO="$(date -u -d "@$((NOW_EPOCH - 120))" +"%Y-%m-%dT%H:%M:%SZ" 2> /dev/null \
    || date -u -r "$((NOW_EPOCH - 120))" +"%Y-%m-%dT%H:%M:%SZ")"
STALE_ISO="$(date -u -d "@$((NOW_EPOCH - 3600))" +"%Y-%m-%dT%H:%M:%SZ" 2> /dev/null \
    || date -u -r "$((NOW_EPOCH - 3600))" +"%Y-%m-%dT%H:%M:%SZ")"

echo "Testing sweep-lease-publish.sh (now=$NOW_ISO)..."

# --- (a) no existing lease -> publishes a well-formed record --------------
reset_state
run_script publish 6320 --sweep-id sweep-run-A
assert_eq "0" "$RC" "(a) publish exits 0 when no lease exists"
assert_eq "1" "$(post_count)" "(a) exactly one comment posted"
assert_eq "studio-host sweep-run-A" "$OUT" "(a) stdout is the resolved '<host> <sweep-id>' for threading into sweep-lease-renew.sh"
BODY_A="$(cat "$STUB_DIR/post-1.body" 2>/dev/null || echo MISSING)"
FIRST_LINE_A="$(head -n1 "$STUB_DIR/post-1.body" 2>/dev/null || echo MISSING)"
assert_eq "<!-- loom:lease host=studio-host sweep=sweep-run-A -->" "$FIRST_LINE_A" "(a) the marker is the LITERAL first line (lease-record.md format contract)"
assert_contains "$BODY_A" "in-session" "(a) prose identifies the record as an in-session publication"
assert_contains "$BODY_A" "defaults/docs/lease-record.md" "(a) prose points at the format contract"

# --- (h) the POST used -F (stdin), never -f (literal "@-") ----------------
assert_contains "$(cat "$STUB_DIR/post-flags.log" 2>/dev/null || echo NONE)" "flag:-F" "(h) POST passes the body with -F (stdin magic), not -f"
assert_true "$([[ "$BODY_A" != "@-" ]] && echo true || echo false)" "(h) posted body is the real comment text, not the literal '@-'"

# --- (b) idempotent for the same host+sweep-id with a fresh lease ---------
reset_state
lease_json "studio-host" "sweep-run-A" "$FRESH_ISO" > "$STUB_DIR/comments.json"
run_script publish 6320 --sweep-id sweep-run-A
assert_eq "0" "$RC" "(b) an existing fresh lease for this host+sweep-id exits 0"
assert_eq "0" "$(post_count)" "(b) no duplicate comment is posted"
assert_eq "studio-host sweep-run-A" "$OUT" "(b) identity is still printed for the renewal call"
assert_contains "$ERR" "not publishing a duplicate" "(b) stderr explains the no-op"

# --- (c) a fresh lease held by a DIFFERENT host -> exit 4 ----------------
reset_state
lease_json "peer-host" "sweep-peer-1" "$FRESH_ISO" > "$STUB_DIR/comments.json"
run_script publish 6320 --sweep-id sweep-run-A
assert_eq "4" "$RC" "(c) a fresh peer-host lease -> exit 4 (caller skips the issue)"
assert_eq "0" "$(post_count)" "(c) nothing is posted over a live peer's lease"
assert_contains "$ERR" "different host" "(c) stderr names the peer-host condition"

# --- (d) a STALE lease does not block publication ------------------------
reset_state
lease_json "peer-host" "sweep-peer-1" "$STALE_ISO" > "$STUB_DIR/comments.json"
run_script publish 6320 --sweep-id sweep-run-A
assert_eq "0" "$RC" "(d) a stale peer lease (past TTL) does not block publication"
assert_eq "1" "$(post_count)" "(d) the abandoned claim's lease is superseded by a new record"

reset_state
lease_json "peer-host" "sweep-peer-1" "$FRESH_ISO" > "$STUB_DIR/comments.json"
run_script publish 6320 --sweep-id sweep-run-A --ttl-minutes 1
assert_eq "0" "$RC" "(d) --ttl-minutes tightens freshness: a 2-min-old peer lease is stale at ttl=1"
assert_eq "1" "$(post_count)" "(d) ... and publication proceeds"

# --- (e) fresh lease, same host, DIFFERENT sweep id -> publish anyway -----
reset_state
lease_json "studio-host" "sweep-run-OLD" "$FRESH_ISO" > "$STUB_DIR/comments.json"
run_script publish 6320 --sweep-id sweep-run-A
assert_eq "0" "$RC" "(e) same-host/different-sweep fresh lease still exits 0"
assert_eq "1" "$(post_count)" "(e) this sweep publishes its own record on top"
assert_contains "$ERR" "different sweep on this same host" "(e) stderr explains the same-host case"

# --- (f) a `gh` READ failure fails open ----------------------------------
reset_state
touch "$STUB_DIR/comments-fail"
run_script publish 6320 --sweep-id sweep-run-A
assert_eq "0" "$RC" "(f) a comments-read failure fails OPEN (exit 0)"
assert_eq "1" "$(post_count)" "(f) the lease is published despite unreadable evidence"
assert_contains "$ERR" "publishing anyway" "(f) stderr explains the fail-open"
rm -f "$STUB_DIR/comments-fail"

# --- (g) a `gh` WRITE failure -> exit 2 ----------------------------------
reset_state
touch "$STUB_DIR/post-fail"
run_script publish 6320 --sweep-id sweep-run-A
assert_eq "2" "$RC" "(g) a failed comment POST exits 2 (best-effort; caller proceeds without a lease)"
assert_contains "$ERR" "failed to publish lease comment" "(g) stderr names the write failure"
rm -f "$STUB_DIR/post-fail"

# --- (i) usage errors -----------------------------------------------------
reset_state
run_script publish notanumber
assert_eq "1" "$RC" "(i) a non-numeric issue number is a usage error"
run_script publish 6320 --bogus-flag x
assert_eq "1" "$RC" "(i) an unknown flag is a usage error"
run_script publish 6320 --ttl-minutes abc
assert_eq "1" "$RC" "(i) a non-numeric --ttl-minutes is a usage error"
run_script publish 6320 --sweep-id "has space"
assert_eq "1" "$RC" "(i) whitespace in --sweep-id is rejected (the marker grammar is space-delimited)"
run_script publish 6320 --host "evil --> host"
assert_eq "1" "$RC" "(i) a '-->' in --host is rejected (it would truncate the marker)"
assert_eq "0" "$(post_count)" "(i) no usage-error path ever posts a comment"

# --- (j) startswith, not substring ---------------------------------------
reset_state
jq -n '[{updated_at: "2999-01-01T00:00:00Z", body: "discussing `<!-- loom:lease host=peer-host sweep=s -->` as prose, not a record"}]' \
    > "$STUB_DIR/comments.json"
run_script publish 6320 --sweep-id sweep-run-A
assert_eq "0" "$RC" "(j) a comment merely MENTIONING the marker is not a lease (publication proceeds)"
assert_eq "1" "$(post_count)" "(j) ... and the record is published"

# --- Contract checks (mirrors test-sweep-lease-renew.sh) ------------------
"$SCRIPT" --help > "$STUB_DIR/help.out" 2>&1
HELP_RC=$?
assert_true "$([[ -s "$STUB_DIR/help.out" ]] && echo true || echo false)" "--help prints usage text"
assert_eq "1" "$HELP_RC" "--help exits 1 (usage-exit convention, matches sweep-lease-renew.sh)"
"$SCRIPT" bogus-command > /dev/null 2>&1
BOGUS_RC=$?
assert_true "$([[ "$BOGUS_RC" -ne 0 ]] && echo true || echo false)" "an unknown command exits non-zero"

echo ""
echo "Results: $TESTS_PASSED/$TESTS_RUN passed"
if ((TESTS_FAILED > 0)); then
    echo -e "${RED}FAILED${NC}: $TESTS_FAILED test(s) failed"
    exit 1
fi
echo -e "${GREEN}ALL PASSED${NC}"
exit 0
