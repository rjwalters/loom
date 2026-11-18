#!/usr/bin/env bash
# test-create-pr-body-at-path.sh - Unit tests for create-pr.sh's lone-`@path`
# body guard (#14161).
#
# `gh` posts a `--body @path` value literally: the PR body becomes the string
# `@path`, the intended body is never written anywhere (the scratch file is
# usually untracked), and a closing keyword (`Closes #N`) in it goes with it
# -- so the PR is not linked to its issue and a merge leaves the issue open.
# The create still exits 0 and prints the URL, so nothing downstream can
# tell. create-pr.sh now refuses a `--body` whose ENTIRE value is one
# `@`-token (or `@-`), exit 2, before any forge call. A body that merely
# CONTAINS `@` (a mention, an email-shaped token, a line starting with `@`
# inside a longer body) is accepted, and `--body-file` content is the
# author's own text -- never inspected.
#
# Cases:
#   Refuse (exit 2, message names --body-file, stub gh never invoked):
#     --body @notes.md, --body @-, --body @, --body @~/r.md,
#     --body " @notes.md " and --body $'\n@notes.md\n' (surrounding whitespace)
#   Accept (PR created via the stub gh):
#     --body "Closes #14" (closing keyword body),
#     --body "cc @alice please review" (a mention),
#     --body "@a b" (whitespace inside the token run),
#     --body "contact ops@example.com about the flake" (email-shaped token),
#     a multi-line body with a line starting `@`,
#     --body-file whose content is `@x`.
#
# Strategy: run create-pr.sh directly as a subprocess (like
# test-create-pr-superseded-issue.sh) with a stub `gh` on PATH and
# LOOM_FORGE_TYPE=github forced (so forge_detect never touches git/network).
# --head is always passed explicitly so the script never needs a real git
# checkout.
#
# Usage:
#   ./.loom/scripts/tests/test-create-pr-body-at-path.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/write-scope-fixture.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/write-scope-fixture.sh"
CREATE_PR="$(cd "$SCRIPT_DIR/.." && pwd)/create-pr.sh"

# Colors
# shellcheck source=lib/assert.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/assert.sh"

if [[ ! -x "$CREATE_PR" ]]; then
  echo "ERROR: $CREATE_PR is not executable" >&2
  exit 1
fi

# --- Stub gh on PATH ---
STUB_DIR="$(mktemp -d)"
trap 'rm -rf "$STUB_DIR"' EXIT

cat > "$STUB_DIR/gh" <<'STUB'
#!/usr/bin/env bash
# Stub gh for test-create-pr-body-at-path.sh.
STUB_DIR_FROM_ENV="${LOOM_TEST_STUB_DIR:?stub gh: LOOM_TEST_STUB_DIR not set}"
echo "$*" >> "$STUB_DIR_FROM_ENV/gh-calls.log"

if [[ "$1" == "pr" && "$2" == "list" ]]; then
  # Adopt-first lookup: no existing open PR.
  cat "$STUB_DIR_FROM_ENV/adopt-url.txt" 2>/dev/null || true
  exit 0
fi

if [[ "$1" == "issue" && "$2" == "view" ]]; then
  issue_num="$3"
  jq_expr=""
  args=("$@")
  for ((i = 0; i < ${#args[@]}; i++)); do
    if [[ "${args[i]}" == "--jq" ]]; then
      jq_expr="${args[i + 1]}"
    fi
  done
  case "$jq_expr" in
    *".state"*)
      cat "$STUB_DIR_FROM_ENV/issue-$issue_num-state.txt" 2>/dev/null || true
      ;;
    *"number"*)
      cat "$STUB_DIR_FROM_ENV/issue-$issue_num-number.txt" 2>/dev/null || true
      ;;
    *"url"*)
      cat "$STUB_DIR_FROM_ENV/issue-$issue_num-url.txt" 2>/dev/null || true
      ;;
  esac
  exit 0
fi

if [[ "$1" == "pr" && "$2" == "view" ]]; then
  pr_num="$3"
  cat "$STUB_DIR_FROM_ENV/pr-$pr_num-head.txt" 2>/dev/null || true
  exit 0
fi

if [[ "$1" == "pr" && "$2" == "create" ]]; then
  echo "CREATED" >> "$STUB_DIR_FROM_ENV/created.log"
  echo "https://github.com/owner/repo/pull/9999"
  exit 0
fi

echo "stub gh: unhandled args: $*" >&2
exit 3
STUB
chmod +x "$STUB_DIR/gh"

export LOOM_TEST_STUB_DIR="$STUB_DIR"
export PATH="$STUB_DIR:$PATH"
export LOOM_FORGE_TYPE=github

# Hermetic version-file sync gate (#6730): point it at an always-clean stub
# so it never fires on this checkout's ambient state.
cat > "$STUB_DIR/version-check-ok.sh" <<'STUB'
#!/usr/bin/env bash
echo "OK        (stub): all versions in sync"
exit 0
STUB
chmod +x "$STUB_DIR/version-check-ok.sh"
export LOOM_VERSION_CHECK_SCRIPT="$STUB_DIR/version-check-ok.sh"

# #9548: create-pr.sh vets its write target through the write scope before it
# writes. It runs from a checkout registered as owner/repo (origin, .loom/,
# push reported to the permission probe), so the real decision admits it.
write_scope_register "$STUB_DIR/checkout" owner/repo
cd "$STUB_DIR/checkout"

reset_fixtures() {
  : > "$STUB_DIR/gh-calls.log"
  rm -f "$STUB_DIR/created.log" "$STUB_DIR"/issue-*.txt "$STUB_DIR"/pr-*.txt "$STUB_DIR/adopt-url.txt"
}

run_create_pr() {
  set +e
  OUTPUT=$("$CREATE_PR" "$@" 2>&1)
  EXIT_CODE=$?
  set -e
}

created_count() {
  if [[ -f "$STUB_DIR/created.log" ]]; then
    grep -c "CREATED" "$STUB_DIR/created.log" 2>/dev/null || true
  else
    echo 0
  fi
}

gh_was_called() {
  [[ -s "$STUB_DIR/gh-calls.log" ]]
}

echo "Testing create-pr.sh lone-@path body guard (#14161)..."
echo ""

# --- Refusal cases: exit 2, message names --body-file, no gh call, no PR ---

refuse_case() {
  local desc="$1"
  shift
  reset_fixtures
  run_create_pr --title "fix: something" --head "feature/issue-100" --body "$1"
  assert_eq "2" "$EXIT_CODE" "$desc: exits 2"
  assert_contains "$OUTPUT" "refusing" "$desc: message says refusing"
  assert_contains "$OUTPUT" "--body-file" "$desc: message names --body-file"
  assert_contains "$OUTPUT" "Nothing was created" "$desc: message says nothing was created"
  if gh_was_called; then
    fail "$desc: stub gh was invoked: $(cat "$STUB_DIR/gh-calls.log")"
  else
    pass "$desc: stub gh never invoked"
  fi
  assert_eq "0" "$(created_count)" "$desc: no PR created"
}

refuse_case "@notes.md path token" "@notes.md"
refuse_case "@- stdin token" "@-"
refuse_case "bare @" "@"
refuse_case "@~/r.md path with tilde" "@~/r.md"
refuse_case "surrounding spaces" " @notes.md "
refuse_case "surrounding newlines" "$(printf '\n@notes.md\n')"

# --- Accept cases: the guard passes and the PR is created ---

accept_case() {
  local desc="$1"
  shift
  reset_fixtures
  run_create_pr --title "fix: something" --head "feature/issue-100" "$@"
  assert_eq "0" "$EXIT_CODE" "$desc: exits 0"
  assert_not_contains "$OUTPUT" "lone '@' token" "$desc: no refusal message"
  assert_eq "1" "$(created_count)" "$desc: PR created"
}

# T-accept-1: a closing-keyword body is accepted (with the target issue OPEN,
# mirroring the superseded-issue suite's proceed path).
reset_fixtures
echo "OPEN" > "$STUB_DIR/issue-14-state.txt"
run_create_pr --title "fix: something" --head "feature/issue-14" --body "Fixes the thing.

Closes #14"
assert_eq "0" "$EXIT_CODE" "Closes #14 body: exits 0"
assert_not_contains "$OUTPUT" "lone '@' token" "Closes #14 body: no refusal message"
assert_eq "1" "$(created_count)" "Closes #14 body: PR created"

accept_case "mention body" --body "cc @alice please review"
accept_case "whitespace inside token run" --body "@a b"
accept_case "email-shaped token" --body "contact ops@example.com about the flake"
accept_case "multi-line body with a line starting @" --body "Summary line.

@handles owns this area."

# A --body-file whose CONTENT is `@x` is the author's own text: accepted,
# never inspected (the guard applies only to the literal --body form).
reset_fixtures
printf '@x' > "$STUB_DIR/at-x-body.md"
run_create_pr --title "fix: something" --head "feature/issue-100" --body-file "$STUB_DIR/at-x-body.md"
assert_eq "0" "$EXIT_CODE" "body-file containing @x: exits 0"
assert_not_contains "$OUTPUT" "lone '@' token" "body-file containing @x: no refusal message"
assert_eq "1" "$(created_count)" "body-file containing @x: PR created"

echo
if [[ "$TESTS_FAILED" -eq 0 ]]; then
    echo -e "${GREEN}All $TESTS_RUN checks passed${NC}"
    exit 0
fi
echo -e "${RED}$TESTS_FAILED of $TESTS_RUN checks failed${NC}"
exit 1
