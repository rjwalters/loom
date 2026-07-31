#!/usr/bin/env bash
# check-docs-defaults-parity.sh — fail if a doc was authored into .loom/docs/
# that never ships from defaults/docs/, or if a defaults/docs/*.md file links
# a same-directory sibling that will not exist once shipped.
#
# Why (#4841): consumer repos populate their .loom/docs/ FROM defaults/docs/
# (resync-installed.sh -> resync_tree "$DEFAULTS_DIR/docs" "$REPO_ROOT/.loom/docs").
# A doc authored directly into .loom/docs/ as a real file (instead of landing
# in defaults/docs/ and being installed as a symlink, the pattern the other
# entries in .loom/docs/ already follow) looks fine in-repo — it renders, it
# resolves, sibling links to it work — but it never ships. Every consumer
# install silently carries a dead link. This happened twice (`safehouse.md`,
# `token-pool.md`) before anything caught it; three shipped docs
# (fleet-comms.md, github-authentication.md, daemon-reference.md) link
# `safehouse.md` as a sibling and broke in every downstream install for as
# long as those files existed. This script is the guard against the class,
# not just the instance (the instance fix is #4796).
#
# What it checks:
#   1. Parity — every non-symlink `*.md` directly under .loom/docs/ must have
#      a same-named counterpart in defaults/docs/, or be on the ORPHAN_ALLOWLIST
#      below for docs that are intentionally repo-local (e.g. dated survey
#      notes). Symlinks are skipped structurally (`find -type f` never matches
#      a symlink) — those already round-trip through defaults/docs/ by
#      construction.
#   2. Link resolution — every same-directory sibling markdown link in
#      defaults/docs/*.md (e.g. `[x](safehouse.md)` or
#      `[x](safehouse.md#anchor)`) must resolve to a real file in
#      defaults/docs/. Links that leave the directory (`../`) or point at an
#      absolute URL are deliberately NOT checked here — those are legitimate
#      cross-references to other parts of the shipped tree or to the loom
#      repo itself (e.g. `../../docs/adr/...`, `../scripts/build-gate.sh`) and
#      are a different, pre-existing concern this issue does not take on.
#
# Usage:
#   check-docs-defaults-parity.sh [ROOT]
#     ROOT  Repository root containing defaults/docs/ and .loom/docs/.
#           Defaults to `git rev-parse --show-toplevel`, then the script's own
#           repo root. If <ROOT>/defaults does not exist (e.g. an installed
#           downstream repo with no source tree), the check is a clean no-op.
#
#   check-docs-defaults-parity.sh --self-test
#     Runs an isolated, synthetic-fixture regression test of the two checks
#     above (the "safehouse.md case": a defaults/docs/ file linking a sibling
#     that exists only in .loom/docs/) and asserts they fail on the broken
#     fixture and pass once it is corrected. Exits non-zero if either check's
#     discriminating power has regressed. Does not touch the real repo tree.
#
# Exit codes: 0 = clean (or self-test passed); 1 = violation(s) found (or
# self-test failed) — details printed to stderr.

set -euo pipefail

# --- Allowlist ---------------------------------------------------------------
# Filenames (glob patterns ok) intentionally authored ONLY in .loom/docs/ and
# never meant to ship via defaults/docs/. Add new entries with a one-line
# reason; this is a "yes, I meant it" list, not a place to silence a real
# orphan.
ORPHAN_ALLOWLIST=(
  "survey-*.md" # dated one-off survey notes (e.g. survey-orca-2026-07-31.md) — repo-local, never shipped
)

is_allowlisted() {
  local name="$1" pattern
  for pattern in "${ORPHAN_ALLOWLIST[@]}"; do
    # Intentional glob match against $pattern, not a literal string compare.
    # shellcheck disable=SC2053
    if [[ "$name" == $pattern ]]; then
      return 0
    fi
  done
  return 1
}

# --- Check 1: parity ----------------------------------------------------------
# Prints violations to stderr. Returns 0 if clean, 1 if any orphan found.
check_parity() {
  local loom_docs="$1" defaults_docs="$2"
  local fail=0 f name

  if [[ ! -d "$loom_docs" ]]; then
    echo "check-docs-defaults-parity: no $loom_docs — nothing to check for parity (ok)."
    return 0
  fi

  while IFS= read -r -d '' f; do
    name="$(basename "$f")"
    if [[ ! -e "$defaults_docs/$name" ]]; then
      if is_allowlisted "$name"; then
        continue
      fi
      echo "ORPHANED: ${loom_docs}/${name} has no defaults/docs/${name} counterpart and is not on the allowlist" >&2
      echo "  fix: add defaults/docs/${name} (it will ship), or add \"${name}\" to ORPHAN_ALLOWLIST in $(basename "$0") if it is intentionally repo-local" >&2
      fail=1
    fi
  done < <(find "$loom_docs" -maxdepth 1 -type f -name '*.md' -print0 | sort -z)

  return $fail
}

# --- Check 2: same-directory sibling links resolve ---------------------------
# Prints violations to stderr. Returns 0 if clean, 1 if any broken link found.
check_links() {
  local defaults_docs="$1"
  local fail=0 f rel

  if [[ ! -d "$defaults_docs" ]]; then
    echo "check-docs-defaults-parity: no $defaults_docs — nothing to check for links (ok)."
    return 0
  fi

  while IFS= read -r -d '' f; do
    rel="$(basename "$f")"
    # One "lineno:](target)" per match — grep -n prefixes every line, -o
    # prints only the matched span, so a line with multiple links yields
    # multiple output lines, each still correctly numbered.
    while IFS= read -r hit; do
      [[ -z "$hit" ]] && continue
      lineno="${hit%%:*}"
      match="${hit#*:}" # "](target)"

      # Strip the leading "](" and trailing ")".
      target="${match#\]\(}"
      target="${target%\)}"

      # Skip absolute URLs, mailto, and bare anchors into the same file.
      case "$target" in
        http://* | https://* | mailto:* | "#"*) continue ;;
      esac

      # Strip a leading "./" (same-directory, spelled explicitly).
      target="${target#./}"

      # Split off any #anchor fragment before path checks.
      path_part="${target%%#*}"
      [[ -z "$path_part" ]] && continue

      # Cross-directory references (contain a "/") are out of scope — see
      # header comment. Only bare same-directory sibling filenames are checked.
      [[ "$path_part" == */* ]] && continue

      if [[ ! -f "$defaults_docs/$path_part" ]]; then
        echo "BROKEN-FROM-SHIPPED-TREE: defaults/docs/${rel}:${lineno} links sibling '${path_part}' which does not exist in defaults/docs/" >&2
        echo "  line: $(sed -n "${lineno}p" "$f" | sed 's/^[[:space:]]*//')" >&2
        fail=1
      fi
    done < <(grep -noE '\]\([^)]+\)' "$f" || true)
  done < <(find "$defaults_docs" -maxdepth 1 -type f -name '*.md' -print0 | sort -z)

  return $fail
}

# --- Self-test -----------------------------------------------------------------
# Builds an isolated synthetic fixture reproducing the safehouse.md case,
# asserts both checks fail on it, "fixes" it the same way #4796 does (add the
# missing counterpart to defaults/docs/), and asserts both checks then pass.
run_self_test() {
  local tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  mkdir -p "$tmp/defaults/docs" "$tmp/.loom/docs"

  # An orphan: authored only in .loom/docs/, never shipped from defaults/docs/.
  echo "# Orphan doc" >"$tmp/.loom/docs/orphan-fixture.md"

  # A shipped doc that links the orphan as a sibling — exactly the
  # fleet-comms.md / github-authentication.md / daemon-reference.md shape.
  cat >"$tmp/defaults/docs/linker-fixture.md" <<'EOF'
# Linker fixture

See [the orphan](orphan-fixture.md#some-anchor) for details.
EOF

  local self_test_fail=0

  echo "check-docs-defaults-parity --self-test: asserting BOTH checks fail on the broken fixture..."
  if check_parity "$tmp/.loom/docs" "$tmp/defaults/docs" >/tmp/self-test-parity.$$ 2>&1; then
    echo "SELF-TEST FAIL: check_parity did not detect the orphan fixture" >&2
    cat /tmp/self-test-parity.$$ >&2
    self_test_fail=1
  else
    echo "  ok: check_parity correctly flagged the orphan"
  fi
  rm -f /tmp/self-test-parity.$$

  if check_links "$tmp/defaults/docs" >/tmp/self-test-links.$$ 2>&1; then
    echo "SELF-TEST FAIL: check_links did not detect the broken sibling link" >&2
    cat /tmp/self-test-links.$$ >&2
    self_test_fail=1
  else
    echo "  ok: check_links correctly flagged the broken link"
  fi
  rm -f /tmp/self-test-links.$$

  # Now correct the fixture the way #4796 corrects the real instance: add the
  # missing counterpart to defaults/docs/.
  cp "$tmp/.loom/docs/orphan-fixture.md" "$tmp/defaults/docs/orphan-fixture.md"

  echo "check-docs-defaults-parity --self-test: asserting BOTH checks pass once corrected..."
  if ! check_parity "$tmp/.loom/docs" "$tmp/defaults/docs"; then
    echo "SELF-TEST FAIL: check_parity still fails after the fixture was corrected" >&2
    self_test_fail=1
  else
    echo "  ok: check_parity is clean post-fix"
  fi

  if ! check_links "$tmp/defaults/docs"; then
    echo "SELF-TEST FAIL: check_links still fails after the fixture was corrected" >&2
    self_test_fail=1
  else
    echo "  ok: check_links is clean post-fix"
  fi

  if [[ "$self_test_fail" -ne 0 ]]; then
    echo "" >&2
    echo "check-docs-defaults-parity --self-test: FAIL — the checker's discriminating power has regressed." >&2
    return 1
  fi

  echo "check-docs-defaults-parity --self-test: OK — both checks fail on the broken fixture and pass once corrected."
  return 0
}

# --- Entry point ---------------------------------------------------------------
if [[ "${1:-}" == "--self-test" ]]; then
  run_self_test
  exit $?
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ $# -ge 1 && -n "${1:-}" ]]; then
  ROOT="$1"
else
  if ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)"; then
    :
  else
    ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
  fi
fi

DEFAULTS_DOCS="$ROOT/defaults/docs"
LOOM_DOCS="$ROOT/.loom/docs"

if [[ ! -d "$ROOT/defaults" ]]; then
  echo "check-docs-defaults-parity: no defaults/ under $ROOT — nothing to check (ok)."
  exit 0
fi

overall_fail=0

check_parity "$LOOM_DOCS" "$DEFAULTS_DOCS" || overall_fail=1
check_links "$DEFAULTS_DOCS" || overall_fail=1

if [[ "$overall_fail" -ne 0 ]]; then
  {
    echo ""
    echo "check-docs-defaults-parity: FAIL — see violations above."
    echo ""
    echo "Consumer repos populate .loom/docs/ FROM defaults/docs/ (resync-installed.sh)."
    echo "A doc authored only in .loom/docs/ never ships, and a defaults/docs/ link to"
    echo "a sibling that only exists in .loom/docs/ is a dead link in every install."
  } >&2
  exit 1
fi

echo "check-docs-defaults-parity: OK — .loom/docs/ has no unshipped orphans, and defaults/docs/ sibling links resolve."
exit 0
