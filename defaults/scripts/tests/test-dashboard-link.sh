#!/usr/bin/env bash
# test-dashboard-link.sh — the shell half of the dashboard-footer chokepoint
# (#9774).
#
# The footer's format and the comment POST own in Rust
# (`loom_daemon::forge_comment`, #9772). What this suite pins is everything the
# shell adds on top, hermetically — a fake loom-daemon and a stub `gh`, no
# network:
#
#   1. FORMAT PINNING. lib/dashboard-link.sh's bytes are asserted twice: against
#      a literal (so the suite is meaningful on a bare checkout with no binary)
#      AND, when a loom-daemon carrying `forge dashboard-link` is resolvable,
#      byte-for-byte against that command's output. The second assertion is what
#      makes the bash twin unable to drift from the Rust original; it SKIPS
#      rather than fails when no such binary exists, because a checkout with no
#      build is not a format regression.
#   2. Idempotence on `<!-- loom:dashboard-link -->`, the no-wrong-link rule for
#      an unresolvable target, `issues` vs `pull`, and $LOOM_DASHBOARD_URL.
#   3. forge_gh_comment_rl_safe appends the footer — i.e. the transport, not its
#      ~ten call sites, is where a script comment gets its link.
#   4. post-comment.sh: argv passthrough to `loom-daemon forge comment`, the
#      binary-absent fallback to the shell transport, and the `--body @path`
#      refusal.
#   5. loom_dashboard_patch_created: the one best-effort REST PATCH
#      create-issue.sh / create-pr.sh make after a create, including that a
#      failing PATCH is reported but never fails the caller.
#
# Usage:
#   ./.loom/scripts/tests/test-dashboard-link.sh

set -uo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$TEST_DIR/.." && pwd)"
LIB="$SCRIPTS_DIR/lib/dashboard-link.sh"
POST_COMMENT="$SCRIPTS_DIR/post-comment.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

pass() {
  TESTS_RUN=$((TESTS_RUN + 1))
  TESTS_PASSED=$((TESTS_PASSED + 1))
  echo -e "  ${GREEN}PASS${NC}: $1"
}

fail() {
  TESTS_RUN=$((TESTS_RUN + 1))
  TESTS_FAILED=$((TESTS_FAILED + 1))
  echo -e "  ${RED}FAIL${NC}: $1"
}

skip() { echo -e "  ${YELLOW}SKIP${NC}: $1"; }

assert_eq() {
  if [[ "$1" == "$2" ]]; then pass "$3"; else fail "$3 (expected [$1], got [$2])"; fi
}

assert_contains() {
  if [[ "$1" == *"$2"* ]]; then pass "$3"; else fail "$3 (missing [$2] in [$1])"; fi
}

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/test-dashboard-link.XXXXXX")"
# shellcheck disable=SC2329  # invoked indirectly via the EXIT trap below
cleanup() { rm -rf "$WORKDIR" 2>/dev/null || true; }
trap cleanup EXIT

# The REAL binary, resolved BEFORE any fake is exported, for group 1's pinning.
REAL_DAEMON=""
if [[ -n "${LOOM_DAEMON_BIN:-}" && -x "${LOOM_DAEMON_BIN}" ]]; then
  REAL_DAEMON="$LOOM_DAEMON_BIN"
elif command -v loom-daemon >/dev/null 2>&1; then
  REAL_DAEMON="$(command -v loom-daemon)"
fi

# shellcheck source=../lib/dashboard-link.sh
source "$LIB"

# ---------------------------------------------------------------------------
echo "Test group 1: the footer format"
# ---------------------------------------------------------------------------

EXPECTED_LITERAL='body text

[loom dashboard](https://dashboard.2amlogic.com/github.com/o/r/issues/9)
<!-- loom:dashboard-link -->'
# `$(...)` strips the format's single trailing newline; the file comparison
# against the Rust output below is what checks that newline.
assert_eq "$EXPECTED_LITERAL" "$(loom_dashboard_footer o/r 9 issues 'body text')" \
  "issue footer matches the canonical literal"

assert_contains "$(loom_dashboard_footer o/r 9 pull 'b')" "/github.com/o/r/pull/9)" \
  "a PR's link says /pull/N"

# --- the drift guard: same bytes as loom-daemon's own ----------------------
if [[ -n "$REAL_DAEMON" ]] && "$REAL_DAEMON" forge dashboard-link --help >/dev/null 2>&1; then
  for KIND in issues pull; do
    RUST_OUT="$WORKDIR/rust-$KIND.txt"
    BASH_OUT="$WORKDIR/bash-$KIND.txt"
    PR_FLAG=()
    [[ "$KIND" == "pull" ]] && PR_FLAG=(--pr)
    "$REAL_DAEMON" forge dashboard-link o/r 9 "${PR_FLAG[@]}" --body 'body text' > "$RUST_OUT" 2>/dev/null
    # Piped, never `$(...)`: the trailing newline is part of the format.
    loom_dashboard_footer o/r 9 "$KIND" 'body text' > "$BASH_OUT"
    if cmp -s "$RUST_OUT" "$BASH_OUT"; then
      pass "bash footer is byte-identical to 'forge dashboard-link' ($KIND)"
    else
      fail "bash/Rust footer drift ($KIND): $(diff "$RUST_OUT" "$BASH_OUT" | tr '\n' ' ')"
    fi
  done
else
  skip "no loom-daemon with 'forge dashboard-link' resolvable — format pinned against the literal only"
fi

# ---------------------------------------------------------------------------
echo "Test group 2: idempotence, unresolvable targets, base URL"
# ---------------------------------------------------------------------------

ONCE="$(loom_dashboard_footer o/r 9 issues 'body')"
assert_eq "$ONCE" "$(loom_dashboard_footer o/r 9 issues "$ONCE")" \
  "a body already carrying the marker is not double-appended"

assert_eq "body" "$(loom_dashboard_footer "" 9 issues 'body')" \
  "no slug -> body unchanged (a link to nowhere is worse than no link)"
assert_eq "body" "$(loom_dashboard_footer o/r "" issues 'body')" \
  "no number -> body unchanged"

assert_eq "https://d.example.com" "$(LOOM_DASHBOARD_URL='https://d.example.com///' loom_dashboard_base_url)" \
  '$LOOM_DASHBOARD_URL overrides the base, trailing slashes trimmed'
assert_eq "https://dashboard.2amlogic.com" "$(LOOM_DASHBOARD_URL='   ' loom_dashboard_base_url)" \
  "a blank override falls back to the default base"
assert_contains "$(LOOM_DASHBOARD_URL='https://d.example.com/' loom_dashboard_footer o/r 1 issues b)" \
  "(https://d.example.com/github.com/o/r/issues/1)" \
  "the override reaches the footer with no doubled slash"

assert_eq "9774" "$(loom_dashboard_number_from_url 'https://github.com/o/r/issues/9774')" \
  "the trailing number is read out of a created URL"
assert_eq "" "$(loom_dashboard_number_from_url 'https://github.com/o/r/issues/')" \
  "an unparseable URL yields no number (so the PATCH is skipped)"

# ---------------------------------------------------------------------------
echo "Test group 3: forge_gh_comment_rl_safe appends the footer"
# ---------------------------------------------------------------------------

# A stub `gh` that records `issue comment`'s --body, and a fake loom-daemon
# whose `forge may-write` approves the fixture repo (the #9548 write-scope vet
# forge_gh_comment_rl_safe runs first).
STUB_BIN="$WORKDIR/bin"
mkdir -p "$STUB_BIN"
cat > "$STUB_BIN/gh" <<'GH_EOF'
#!/usr/bin/env bash
LOG="${STUB_GH_LOG:?}"
printf '%s\n' "$*" >> "$LOG.argv"
if [[ "$1" == "issue" && "$2" == "comment" ]]; then
  for ((i = 1; i <= $#; i++)); do
    if [[ "${!i}" == "--body" ]]; then j=$((i + 1)); printf '%s' "${!j}" > "$LOG.body"; fi
  done
  [[ "${STUB_GH_FAIL:-0}" == "1" ]] && { echo "stub gh: API rate limit exceeded" >&2; exit 1; }
  echo "https://github.com/o/r/pull/9#issuecomment-1"
  exit 0
fi
if [[ "$1" == "api" ]]; then
  cat > "$LOG.patch-stdin" 2>/dev/null || true
  [[ "${STUB_GH_API_FAIL:-0}" == "1" ]] && { echo "stub gh: PATCH refused" >&2; exit 1; }
  echo '{"number":9}'
  exit 0
fi
exit 0
GH_EOF
chmod +x "$STUB_BIN/gh"

cat > "$WORKDIR/fake-loom-daemon" <<'FAKE_EOF'
#!/usr/bin/env bash
# `forge may-write` approves the fixture slug. `forge comment` exists only when
# FAKE_HAS_COMMENT=1 — that is how the binary-absent/verb-absent arm is driven.
if [[ "$1" == "forge" && "$2" == "may-write" ]]; then
  printf '%s\n' "${FAKE_WRITE_REPO:-o/r}"
  exit 0
fi
if [[ "$1" == "forge" && "$2" == "comment" ]]; then
  [[ "${FAKE_HAS_COMMENT:-0}" == "1" ]] || { echo "error: unrecognized subcommand 'comment'" >&2; exit 2; }
  # The capability probe is not a call: only a real post is logged, so the
  # passthrough assertion sees exactly the caller's argv.
  [[ " $* " == *" --help "* ]] && exit 0
  printf '%s\n' "$*" >> "${FAKE_LOG:-/dev/null}"
  echo "https://github.com/o/r/issues/1#issuecomment-2"
  exit 0
fi
exit 0
FAKE_EOF
chmod +x "$WORKDIR/fake-loom-daemon"

export STUB_GH_LOG="$WORKDIR/gh"
rm -f "$WORKDIR"/gh.*
(
  PATH="$STUB_BIN:$PATH"
  export PATH LOOM_DAEMON_BIN="$WORKDIR/fake-loom-daemon"
  # shellcheck source=../lib/forge-helpers.sh
  source "$SCRIPTS_DIR/lib/forge-helpers.sh"
  forge_gh_comment_rl_safe o/r 9 "the body" pull >/dev/null 2>&1
)
POSTED="$(cat "$WORKDIR/gh.body" 2>/dev/null || true)"
assert_contains "$POSTED" "the body" "the transport posts the caller's body"
assert_contains "$POSTED" "[loom dashboard](https://dashboard.2amlogic.com/github.com/o/r/pull/9)" \
  "the transport appends the footer, with /pull/N for a PR"
if [[ "$POSTED" == *"<!-- loom:dashboard-link -->"* && "$POSTED" == *"<!-- loom:dashboard-link -->"*"<!-- loom:dashboard-link -->"* ]]; then
  fail "the transport double-appended the marker"
else
  pass "exactly one marker in the posted body"
fi

# A body that already carries a footer (a re-post) is posted unchanged.
rm -f "$WORKDIR"/gh.*
PRE_FOOTERED="$(loom_dashboard_footer o/r 9 pull 'already linked')"
(
  PATH="$STUB_BIN:$PATH"
  export PATH LOOM_DAEMON_BIN="$WORKDIR/fake-loom-daemon"
  # shellcheck source=../lib/forge-helpers.sh
  source "$SCRIPTS_DIR/lib/forge-helpers.sh"
  forge_gh_comment_rl_safe o/r 9 "$PRE_FOOTERED" pull >/dev/null 2>&1
)
RE_POSTED="$(cat "$WORKDIR/gh.body" 2>/dev/null || true)"
assert_eq "1" "$(grep -c -- '<!-- loom:dashboard-link -->' <<< "$RE_POSTED")" \
  "re-posting an already-footered body does not add a second link"

# ---------------------------------------------------------------------------
echo "Test group 4: post-comment.sh"
# ---------------------------------------------------------------------------

rm -f "$WORKDIR"/gh.* "$WORKDIR/forge-comment.log"
OUT="$(FAKE_HAS_COMMENT=1 FAKE_LOG="$WORKDIR/forge-comment.log" \
  LOOM_DAEMON_BIN="$WORKDIR/fake-loom-daemon" \
  bash "$POST_COMMENT" 42 --repo o/r --body "hello" --pr 2>&1)"
RC=$?
assert_eq "0" "$RC" "passthrough: exit 0"
assert_eq "forge comment 42 --repo o/r --body hello --pr" \
  "$(cat "$WORKDIR/forge-comment.log" 2>/dev/null || true)" \
  "arguments reach 'loom-daemon forge comment' verbatim"
assert_contains "$OUT" "issuecomment-2" "the verb's output is the stub's output"

# Binary present but WITHOUT the verb (a pre-#9772 build) is the same case as no
# binary at all: fall back to the shell transport rather than refuse to post.
rm -f "$WORKDIR"/gh.*
OUT="$(
  PATH="$STUB_BIN:$PATH" \
    LOOM_DAEMON_BIN="$WORKDIR/fake-loom-daemon" \
    bash "$POST_COMMENT" 42 --repo o/r --body "fallback body" --pr 2>&1
)"
RC=$?
assert_eq "0" "$RC" "fallback: exit 0"
assert_contains "$OUT" "unavailable" "the fallback says why it took the shell path"
FELL_BACK="$(cat "$WORKDIR/gh.body" 2>/dev/null || true)"
assert_contains "$FELL_BACK" "fallback body" "the fallback actually posted the body"
assert_contains "$FELL_BACK" "/github.com/o/r/pull/42)" \
  "the fallback carries the identical footer (same bytes, via the transport)"

OUT="$(LOOM_DAEMON_BIN="$WORKDIR/fake-loom-daemon" bash "$POST_COMMENT" 42 --body "@/tmp/body.md" 2>&1)"
RC=$?
assert_eq "2" "$RC" "--body @path is refused, not posted literally"
assert_contains "$OUT" "does NOT read the file" "the refusal explains the @path anti-pattern"

OUT="$(PATH="$STUB_BIN:$PATH" LOOM_DAEMON_BIN="$WORKDIR/fake-loom-daemon" \
  bash "$POST_COMMENT" --repo o/r --body "no number" 2>&1)"
RC=$?
assert_eq "2" "$RC" "a missing NUMBER is a usage error, not a post"

# ---------------------------------------------------------------------------
echo "Test group 5: the created-body PATCH (create-issue.sh / create-pr.sh)"
# ---------------------------------------------------------------------------

rm -f "$WORKDIR"/gh.*
(
  PATH="$STUB_BIN:$PATH"
  export PATH
  loom_dashboard_patch_created "https://github.com/acme/widgets/pull/77" pull "the PR body"
) >/dev/null 2>&1
assert_contains "$(cat "$WORKDIR/gh.argv" 2>/dev/null || true)" \
  "api --method PATCH repos/acme/widgets/issues/77 --input -" \
  "the PATCH names repos/{nwo}/issues/{N} (one endpoint for issues and PRs)"
PATCHED="$(jq -r '.body' < "$WORKDIR/gh.patch-stdin" 2>/dev/null || true)"
assert_contains "$PATCHED" "the PR body" "the PATCH keeps the created body"
assert_contains "$PATCHED" "[loom dashboard](https://dashboard.2amlogic.com/github.com/acme/widgets/pull/77)" \
  "the PATCHed body ends with the footer, /pull/N for a PR"

rm -f "$WORKDIR"/gh.*
(
  PATH="$STUB_BIN:$PATH"
  export PATH
  loom_dashboard_patch_created "https://github.com/acme/widgets/issues/77" issues "already
<!-- loom:dashboard-link -->"
) >/dev/null 2>&1
if [[ -f "$WORKDIR/gh.argv" ]]; then
  fail "an already-footered body still issued a PATCH"
else
  pass "an already-footered body makes no REST call at all"
fi

rm -f "$WORKDIR"/gh.*
ERR="$(
  PATH="$STUB_BIN:$PATH" STUB_GH_API_FAIL=1 \
    bash -c "source '$LIB'; loom_dashboard_patch_created 'https://github.com/acme/widgets/issues/77' issues 'body'; echo rc=\$?" 2>&1
)"
assert_contains "$ERR" "rc=0" "a failed PATCH never fails the caller (the issue exists)"
assert_contains "$ERR" "could not append the dashboard footer" "a failed PATCH is reported on stderr"

rm -f "$WORKDIR"/gh.*
(
  PATH="$STUB_BIN:$PATH"
  export PATH
  loom_dashboard_patch_created "not-a-url" issues "body"
) >/dev/null 2>&1
if [[ -f "$WORKDIR/gh.argv" ]]; then
  fail "an unparseable created URL still issued a PATCH"
else
  pass "an unparseable created URL is skipped, not guessed at"
fi

# ---------------------------------------------------------------------------
echo ""
echo "Tests run: $TESTS_RUN, passed: $TESTS_PASSED, failed: $TESTS_FAILED"
[[ "$TESTS_FAILED" -eq 0 ]] || exit 1
