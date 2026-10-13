#!/usr/bin/env bash
# check-guard-destructive-drift.sh — ADVISORY, non-blocking drift-visibility
# check between the vendored `defaults/hooks/guard-destructive-generic.sh`
# and its declared upstream, Repo Skills' canonical
# `hooks/repo/guard-destructive.sh` (https://github.com/rjwalters/repo).
#
# Why (issue #5660, resolution 3): `defaults/hooks/guard-destructive-generic.sh`
# is documented (its own header, lines ~4-21) as a VENDORED COPY — "DO NOT
# hand-edit generic pattern behavior here — send fixes upstream" — but that
# convention was unenforced and the vendored copy drifted ~2,200+ lines ahead
# of upstream (18 generic functions added locally, never ported back) before
# anyone noticed. Two of the three resolutions issue #5660 originally asked
# for have since shipped (the dispatcher's capability probe now requires a
# four-marker set, #4894/#5916; the known drift was reconciled upstream via
# rjwalters/repo#188/#192). This script is the third: a recurring, cheap
# signal so the *next* multi-thousand-line drift is visible in weeks, not
# months (the 2026-08-18 gf180-sram#81 incident showed the same drift
# recurring independently on both sides of this exact file).
#
# What it compares (best-effort, deliberately coarse — see NOTE below):
#   1. The top-level FUNCTION NAME set: every `name() {` declaration at
#      column 0 (nested/local functions are not declared this way in either
#      copy today, so this is a faithful "public API surface" proxy).
#   2. The top-level PATTERN-TABLE set: every `NAME=(` bash-array
#      declaration at column 0 (e.g. ALWAYS_BLOCK_PATTERNS, ASK_PATTERNS) —
#      diffed by ARRAY NAME and ENTRY COUNT, not by literal pattern text.
#      The individual regex/glob strings inside each table are free-form and
#      the two copies may legitimately phrase an equivalent rule differently
#      even at full parity, so diffing literal contents would be noisy in
#      exactly the way the function-name diff is not. Array name + count is
#      the cheap, robust signal that a whole *category* of coverage was
#      added on one side and not the other.
#
# NOTE on scope (documented per issue #5660's own escape valve): a
# structurally sound diff of full pattern-table *contents* (matching
# individual entries across two copies whose comment wording and ordering
# differ) was judged too fragile to be a reliable non-blocking signal and is
# NOT attempted here. The function-name-set diff plus the pattern-TABLE-set
# diff (name + count) is judged sufficient to satisfy "make the drift
# visible in weeks rather than months" without producing so much noise that
# the job summary gets ignored.
#
# This script NEVER fails the build — it always exits 0 (warn-only), even on
# a fetch/network failure. Its only side effect is printing a report to
# stdout and (if set) $GITHUB_STEP_SUMMARY.
#
# Usage:
#   scripts/check-guard-destructive-drift.sh [--vendored PATH] [--ref REF] [--repo-root DIR]
#
#   --vendored PATH   path to the vendored copy (default:
#                      <repo-root>/defaults/hooks/guard-destructive-generic.sh)
#   --ref REF         a specific rjwalters/repo git ref (tag/branch/sha) to
#                      diff against, skipping version resolution entirely
#                      (mainly for local testing / --self-test)
#   --repo-root DIR   override repo-root autodetection (mainly for tests)
#
# Env:
#   GITHUB_STEP_SUMMARY   if set, the report is ALSO appended there (CI does
#                         this automatically; useful locally via
#                         `GITHUB_STEP_SUMMARY=/tmp/summary.md`)
#   GH_TOKEN / GITHUB_TOKEN   optional; if set, sent as a bearer token on the
#                         GitHub API tag-listing call only (raises the rate
#                         limit; never required for a public repo, and never
#                         sent to raw.githubusercontent.com)

set -uo pipefail

CANONICAL_OWNER_REPO="rjwalters/repo"
CANONICAL_PATH_IN_REPO="hooks/repo/guard-destructive.sh"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
VENDORED_PATH="$REPO_ROOT/defaults/hooks/guard-destructive-generic.sh"
EXPLICIT_REF=""
SELF_TEST=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --vendored)
      VENDORED_PATH="$2"
      shift 2
      ;;
    --ref)
      EXPLICIT_REF="$2"
      shift 2
      ;;
    --repo-root)
      REPO_ROOT="$2"
      VENDORED_PATH="$REPO_ROOT/defaults/hooks/guard-destructive-generic.sh"
      shift 2
      ;;
    --self-test)
      SELF_TEST=true
      shift
      ;;
    *)
      echo "check-guard-destructive-drift.sh: unrecognized argument: $1" >&2
      shift
      ;;
  esac
done

REPORT_FILE="$(mktemp)"
trap 'rm -f "$REPORT_FILE" "${CANONICAL_TMP:-}"' EXIT

_report() {
  printf '%s\n' "$*" >>"$REPORT_FILE"
}

# Extract the top-level function-name set from a file, one name per line,
# sorted. Matches `name() {` (or `name ()`) anchored at column 0.
_extract_function_names() {
  local file="$1"
  grep -oE '^[A-Za-z_][A-Za-z0-9_]*[[:space:]]*\(\)' "$file" 2>/dev/null \
    | sed -E 's/[[:space:]]*\(\)$//' \
    | sort -u
}

# Extract the top-level pattern-table (array) name set from a file, one
# "NAME:COUNT" pair per line, sorted by name. An array is `NAME=(` at
# column 0, closed by a lone `)` at column 0.
_extract_pattern_tables() {
  local file="$1"
  awk '
    /^[A-Za-z_][A-Za-z0-9_]*=\($/ {
      name = $0
      sub(/=\($/, "", name)
      count = 0
      in_array = 1
      next
    }
    in_array && /^\)$/ {
      print name ":" count
      in_array = 0
      next
    }
    in_array {
      # Count non-blank, non-comment-only lines as entries. This slightly
      # over-counts multi-line entries and under-counts nothing that
      # matters for a coarse "did a whole category change size" signal.
      line = $0
      gsub(/^[[:space:]]+/, "", line)
      if (line != "" && line !~ /^#/) {
        count++
      }
    }
  ' "$file" 2>/dev/null | sort
}

# Resolve which rjwalters/repo ref to diff against.
#   1. --ref, if given.
#   2. A pin recorded in scripts/install/manifest.sh, if one is ever added
#      (none exists as of #5660 — this is forward-looking, not dead code:
#      grep for a REPO_SKILLS_PIN-style marker so a future pin is honored
#      without another CI edit).
#   3. The latest GitHub release tag on rjwalters/repo.
#   4. "main", as a last-resort fallback if tag resolution fails.
_resolve_canonical_ref() {
  if [[ -n "$EXPLICIT_REF" ]]; then
    echo "$EXPLICIT_REF"
    return 0
  fi

  local manifest="$REPO_ROOT/scripts/install/manifest.sh"
  if [[ -r "$manifest" ]]; then
    local pinned
    pinned="$(grep -oE 'REPO_SKILLS_PIN=["'"'"']?[^"'"'"'[:space:]]+' "$manifest" 2>/dev/null \
      | head -1 | sed -E 's/^REPO_SKILLS_PIN=["'"'"']?//')"
    if [[ -n "$pinned" ]]; then
      echo "$pinned"
      return 0
    fi
  fi

  local auth_header=()
  local token="${GH_TOKEN:-${GITHUB_TOKEN:-}}"
  if [[ -n "$token" ]]; then
    auth_header=(-H "Authorization: Bearer $token")
  fi

  local latest_tag
  latest_tag="$(curl -fsSL --max-time 15 "${auth_header[@]}" \
    "https://api.github.com/repos/${CANONICAL_OWNER_REPO}/tags" 2>/dev/null \
    | grep -m1 '"name":' | sed -E 's/.*"name":[[:space:]]*"([^"]+)".*/\1/')"

  if [[ -n "$latest_tag" ]]; then
    echo "$latest_tag"
    return 0
  fi

  echo "main"
  return 0
}

# Fetch the canonical file's raw content at $1 (a ref) into a temp file.
# Prints the temp file path on success, prints nothing and returns non-zero
# on failure.
_fetch_canonical() {
  local ref="$1"
  local tmp
  tmp="$(mktemp)"
  if curl -fsSL --max-time 20 \
      "https://raw.githubusercontent.com/${CANONICAL_OWNER_REPO}/${ref}/${CANONICAL_PATH_IN_REPO}" \
      -o "$tmp" 2>/dev/null \
      && [[ -s "$tmp" ]]; then
    echo "$tmp"
    return 0
  fi
  rm -f "$tmp"
  return 1
}

_diff_names() {
  local left_label="$1" left_file="$2" right_label="$3" right_file="$4"
  local only_left only_right

  only_left="$(comm -23 "$left_file" "$right_file")"
  only_right="$(comm -13 "$left_file" "$right_file")"

  if [[ -z "$only_left" && -z "$only_right" ]]; then
    _report "  No difference."
    return
  fi
  if [[ -n "$only_left" ]]; then
    _report "  Only in **${left_label}**:"
    while IFS= read -r name; do
      [[ -n "$name" ]] && _report "  - \`${name}\`"
    done <<<"$only_left"
  fi
  if [[ -n "$only_right" ]]; then
    _report "  Only in **${right_label}**:"
    while IFS= read -r name; do
      [[ -n "$name" ]] && _report "  - \`${name}\`"
    done <<<"$only_right"
  fi
}

_run() {
  if [[ ! -r "$VENDORED_PATH" ]]; then
    _report "## Guard-Destructive Drift Check"
    _report ""
    _report "**INCONCLUSIVE**: vendored copy not found at \`${VENDORED_PATH#"$REPO_ROOT"/}\`."
    return 0
  fi

  local ref
  ref="$(_resolve_canonical_ref)"

  local canonical_tmp
  if ! canonical_tmp="$(_fetch_canonical "$ref")"; then
    _report "## Guard-Destructive Drift Check"
    _report ""
    _report "**INCONCLUSIVE**: could not fetch \`${CANONICAL_PATH_IN_REPO}\`"
    _report "from \`${CANONICAL_OWNER_REPO}@${ref}\` (network error, rate limit, or"
    _report "the file/ref does not exist). Not treated as drift — will retry"
    _report "on the next run."
    return 0
  fi
  CANONICAL_TMP="$canonical_tmp"

  local vendored_funcs canonical_funcs vendored_tables canonical_tables
  vendored_funcs="$(mktemp)"
  canonical_funcs="$(mktemp)"
  vendored_tables="$(mktemp)"
  canonical_tables="$(mktemp)"

  _extract_function_names "$VENDORED_PATH" >"$vendored_funcs"
  _extract_function_names "$canonical_tmp" >"$canonical_funcs"
  _extract_pattern_tables "$VENDORED_PATH" >"$vendored_tables"
  _extract_pattern_tables "$canonical_tmp" >"$canonical_tables"

  local vendored_lines canonical_lines vendored_fn_count canonical_fn_count
  vendored_lines="$(wc -l <"$VENDORED_PATH" | tr -d ' ')"
  canonical_lines="$(wc -l <"$canonical_tmp" | tr -d ' ')"
  vendored_fn_count="$(wc -l <"$vendored_funcs" | tr -d ' ')"
  canonical_fn_count="$(wc -l <"$canonical_funcs" | tr -d ' ')"

  _report "## Guard-Destructive Drift Check (advisory, non-blocking)"
  _report ""
  _report "Vendored \`defaults/hooks/guard-destructive-generic.sh\` vs. canonical"
  _report "\`${CANONICAL_OWNER_REPO}\`'s \`${CANONICAL_PATH_IN_REPO}\` @ \`${ref}\`."
  _report "See issue #5660 for background — this check is warn-only by design."
  _report ""
  _report "| | vendored | canonical (\`${ref}\`) |"
  _report "|---|---|---|"
  _report "| Lines | ${vendored_lines} | ${canonical_lines} |"
  _report "| Top-level functions | ${vendored_fn_count} | ${canonical_fn_count} |"
  _report ""
  _report "### Function-name delta"
  _report ""
  _diff_names "vendored" "$vendored_funcs" "canonical" "$canonical_funcs"
  _report ""
  _report "### Pattern-table delta (array name + entry count; NOT literal contents — see script header)"
  _report ""
  _diff_names "vendored" "$vendored_tables" "canonical" "$canonical_tables"

  rm -f "$vendored_funcs" "$canonical_funcs" "$vendored_tables" "$canonical_tables"
  return 0
}

_self_test() {
  local tmp_dir
  tmp_dir="$(mktemp -d)"
  local ok=true

  cat >"$tmp_dir/a.sh" <<'EOF'
#!/usr/bin/env bash
foo() {
  echo hi
}

bar() {
  echo bye
}

MY_PATTERNS=(
    'one'
    'two'
    'three'
)
EOF

  cat >"$tmp_dir/b.sh" <<'EOF'
#!/usr/bin/env bash
foo() {
  echo hi
}

baz() {
  echo new
}

MY_PATTERNS=(
    'one'
    'two'
)

OTHER_PATTERNS=(
    'x'
)
EOF

  local a_funcs b_funcs
  a_funcs="$(_extract_function_names "$tmp_dir/a.sh")"
  b_funcs="$(_extract_function_names "$tmp_dir/b.sh")"
  if [[ "$a_funcs" != $'bar\nfoo' ]]; then
    echo "self-test FAILED: unexpected function set for a.sh: [$a_funcs]" >&2
    ok=false
  fi
  if [[ "$b_funcs" != $'baz\nfoo' ]]; then
    echo "self-test FAILED: unexpected function set for b.sh: [$b_funcs]" >&2
    ok=false
  fi

  local a_tables b_tables
  a_tables="$(_extract_pattern_tables "$tmp_dir/a.sh")"
  b_tables="$(_extract_pattern_tables "$tmp_dir/b.sh")"
  if [[ "$a_tables" != "MY_PATTERNS:3" ]]; then
    echo "self-test FAILED: unexpected table set for a.sh: [$a_tables]" >&2
    ok=false
  fi
  if [[ "$b_tables" != $'MY_PATTERNS:2\nOTHER_PATTERNS:1' ]]; then
    echo "self-test FAILED: unexpected table set for b.sh: [$b_tables]" >&2
    ok=false
  fi

  rm -rf "$tmp_dir"

  if [[ "$ok" == "true" ]]; then
    echo "self-test PASSED"
    return 0
  fi
  return 1
}

if [[ "$SELF_TEST" == "true" ]]; then
  _self_test
  exit $?
fi

_run
cat "$REPORT_FILE"
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  cat "$REPORT_FILE" >>"$GITHUB_STEP_SUMMARY"
fi

# Always succeed — this is a warn-only, advisory check (issue #5660).
exit 0
