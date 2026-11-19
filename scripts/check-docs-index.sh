#!/usr/bin/env bash
# check-docs-index.sh — fail if docs/README.md does not link every docs/
# directory, every top-level docs/*.md, and every docs/design/*.md note.
#
# Why (#14075): docs/README.md is a hand-maintained enumeration of the docs/
# tree, and nothing gated it, so it drifted every time a directory, top-level
# doc or design note landed without an index row (#13879 / PR #14051 had to
# repair a missing `observability/` row, `forge-contract.md` and `ROADMAP.md`
# rows, and nine of twenty `design/*.md` notes). The repair was verified by
# three mechanical shell loops; this script is those loops, run by CI.
#
# What it checks (exactly three sets, matched with `grep -F` so filenames with
# regex metacharacters are safe):
#   DIR     every docs/*/ directory     -> a link of the form [`name/`](name/)
#   TOP     every docs/*.md except
#           README.md itself             -> a link of the form (name.md)
#   DESIGN  every docs/design/*.md       -> a link of the form (design/name.md)
#
# Other subdirectories (guides/, philosophy/, research/, ...) are summarised in
# prose in the index on purpose and are NOT enumerated file by file.
#
# Usage:
#   check-docs-index.sh [ROOT]
#     ROOT  Repository root to scan. Defaults to `git rev-parse --show-toplevel`
#           of this script's checkout, then the script's own repo root.
#
#   check-docs-index.sh --self-test
#     Builds a synthetic docs/ fixture in a temp dir (one gap per category plus
#     a clean case) and asserts the checker reports the gaps and passes the
#     clean tree. Does not touch the real repo tree.
#
# Exit codes: 0 = index complete (prints a single OK line); 1 = one or more
# `MISSING DIR|TOP|DESIGN <name>` lines printed to stderr (or self-test failed).

set -euo pipefail

# Prints one `MISSING <KIND> <name>` line to stderr per gap under ROOT/docs.
# Returns 0 if the index is complete, 1 otherwise.
check_index() {
  local root="$1"
  local docs="$root/docs"
  local readme="$docs/README.md"
  local fail=0 d f b

  if [[ ! -f "$readme" ]]; then
    echo "check-docs-index: $readme does not exist" >&2
    return 1
  fi

  # An empty docs/design/ (or no subdirs at all) must not turn a literal glob
  # into a bogus name.
  local had_nullglob=0
  shopt -q nullglob && had_nullglob=1
  shopt -s nullglob

  for d in "$docs"/*/; do
    b="$(basename "$d")"
    if ! grep -qF "[\`$b/\`]($b/)" "$readme"; then
      echo "MISSING DIR $b" >&2
      fail=1
    fi
  done

  for f in "$docs"/*.md; do
    b="$(basename "$f")"
    [[ "$b" == "README.md" ]] && continue
    if ! grep -qF "($b)" "$readme"; then
      echo "MISSING TOP $b" >&2
      fail=1
    fi
  done

  for f in "$docs"/design/*.md; do
    b="$(basename "$f")"
    if ! grep -qF "(design/$b)" "$readme"; then
      echo "MISSING DESIGN $b" >&2
      fail=1
    fi
  done

  [[ "$had_nullglob" -eq 0 ]] && shopt -u nullglob
  return "$fail"
}

run_self_test() {
  local tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  local fail=0 out
  mkdir -p "$tmp/docs/indexed-dir" "$tmp/docs/gap-dir" "$tmp/docs/design"
  : >"$tmp/docs/indexed.md"
  : >"$tmp/docs/gap-top.md"
  : >"$tmp/docs/we.ird+name[1].md"
  : >"$tmp/docs/design/indexed-note.md"
  : >"$tmp/docs/design/gap-note.md"

  # A complete index for the clean case (README.md itself is never required).
  local complete
  # shellcheck disable=SC2016 # literal backticks are the fixture content
  complete='# Docs

| Dir | What |
|-----|------|
| [`indexed-dir/`](indexed-dir/) | indexed |
| [`gap-dir/`](gap-dir/) | indexed |
| [`design/`](design/) | [a](design/indexed-note.md), [b](design/gap-note.md) |

[indexed](indexed.md), [gap-top](gap-top.md), [weird](we.ird+name[1].md)
'

  echo "check-docs-index --self-test: asserting one gap per category is reported..."
  # Index with exactly one gap per category: no gap-dir row, no gap-top.md
  # link, no design/gap-note.md link.
  cat >"$tmp/docs/README.md" <<'EOF'
# Docs

| Dir | What |
|-----|------|
| [`indexed-dir/`](indexed-dir/) | indexed |
| [`design/`](design/) | [a](design/indexed-note.md) |

[indexed](indexed.md), [weird](we.ird+name[1].md)
EOF
  if out="$(check_index "$tmp" 2>&1)"; then
    echo "SELF-TEST FAIL: checker passed a README with three gaps" >&2
    fail=1
  else
    local expect
    for expect in "MISSING DIR gap-dir" "MISSING TOP gap-top.md" "MISSING DESIGN gap-note.md"; do
      if [[ "$out" == *"$expect"* ]]; then
        echo "  ok: reported '$expect'"
      else
        echo "SELF-TEST FAIL: expected '$expect', got: $out" >&2
        fail=1
      fi
    done
    local unexpected
    for unexpected in "indexed-dir" "indexed.md" "indexed-note.md" "we.ird" "README.md"; do
      if [[ "$out" == *"$unexpected"* ]]; then
        echo "SELF-TEST FAIL: '$unexpected' is indexed (or exempt) but was reported: $out" >&2
        fail=1
      fi
    done
    [[ "$fail" -eq 0 ]] && echo "  ok: indexed entries, README.md and metacharacter filename not flagged"
  fi

  echo "check-docs-index --self-test: asserting a complete index passes..."
  printf '%s' "$complete" >"$tmp/docs/README.md"
  if out="$(check_index "$tmp" 2>&1)"; then
    echo "  ok: complete index passes"
  else
    echo "SELF-TEST FAIL: complete index was rejected: $out" >&2
    fail=1
  fi

  echo "check-docs-index --self-test: asserting an empty docs/design/ is tolerated..."
  rm -f "$tmp/docs/design"/*.md
  if out="$(check_index "$tmp" 2>&1)"; then
    echo "  ok: empty design/ passes"
  else
    echo "SELF-TEST FAIL: empty design/ was rejected: $out" >&2
    fail=1
  fi

  if [[ "$fail" -ne 0 ]]; then
    echo "" >&2
    echo "check-docs-index --self-test: FAIL — the checker's discriminating power has regressed." >&2
    return 1
  fi
  echo "check-docs-index --self-test: OK."
  return 0
}

if [[ "${1:-}" == "--self-test" ]]; then
  run_self_test
  exit $?
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ $# -ge 1 && -n "${1:-}" ]]; then
  ROOT="$1"
elif ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)"; then
  :
else
  ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
fi

if check_index "$ROOT"; then
  echo "check-docs-index: OK — docs/README.md indexes every docs/ directory, top-level doc and design note."
  exit 0
fi

{
  echo ""
  echo "check-docs-index: FAIL — add the entries above to docs/README.md."
  echo "  DIR    -> a row linking [\`name/\`](name/)"
  echo "  TOP    -> a link to (name.md)"
  echo "  DESIGN -> a link to (design/name.md)"
} >&2
exit 1
