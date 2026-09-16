#!/usr/bin/env bash
# check-doc-anchors.sh - fail when a markdown link's #anchor does not resolve.
#
# WHY A TOOL, NOT MORE BASH (#7801): check-dangling-links.sh deliberately
# validates only a link's PATH. Fragments were stripped and discarded, so
# "doc.md#section" stayed green after the heading was renamed -- 15 such links
# were live on main when this was written.
#
# Anchor resolution is NOT repo-specific: it is GitHub's heading-slug algorithm,
# which lowercases, drops punctuation and replaces EACH space with "-" (runs are
# NOT collapsed, so "## Fleet -- operator" is "fleet--operator"). Hand-rolling
# that in awk was tried and produced three separate bugs in as many attempts
# (a reserved-word collision on `close`, a BSD-awk multibyte failure on UTF-8
# headings, and the space-collapsing rule itself, which mis-reported 60 broken
# anchors where 15 were real). lychee already implements it correctly and checks
# 1479 links in ~70ms, so this script is a thin filter around it.
#
# WHY WE FILTER RATHER THAN USE lychee's EXIT CODE: lychee resolves relative
# paths naively, which is correct everywhere except `defaults/**` -- that tree's
# links are written against their INSTALL destination, not their in-repo
# location (see check-dangling-links.sh's destination-aware mode, which owns
# that problem). So lychee reports path errors there that are not real. We take
# only its fragment verdicts and leave every path verdict to the existing
# checker.
#
# Usage: check-doc-anchors.sh [root]   (requires `lychee` on PATH)
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

if ! command -v lychee >/dev/null 2>&1; then
  echo "check-doc-anchors: FAIL - lychee not found on PATH." >&2
  echo "  Install: https://github.com/lycheeverse/lychee (CI pins a release binary)." >&2
  exit 127
fi

# NOTE: no `mapfile` -- it is bash 4+, and macOS ships bash 3.2 (see #7783 for
# what assuming otherwise costs on a mandatory path).
FILES=()
while IFS= read -r -d '' f; do
  FILES+=("$f")
done < <(git ls-files -z -- '*.md')

if [[ "${#FILES[@]}" -eq 0 ]]; then
  echo "check-doc-anchors: no tracked markdown files; nothing to check."
  exit 0
fi

raw="$(mktemp)"
trap 'rm -f "$raw"' EXIT

# --offline: never touch the network. External URL checking is deliberately out
# of scope -- it is rate-limited and flaky, and would turn a deterministic
# sub-second check into a source of random red builds.
lychee --offline --include-fragments=anchor-only --no-progress "${FILES[@]}" >"$raw" 2>&1 || true

if grep -q "Cannot find fragment" "$raw"; then
  {
    echo ""
    echo "check-doc-anchors: FAIL - markdown links point at headings that do not exist."
    echo ""
    # Keep lychee's "[source-file]:" section headers so each error names the
    # file that CONTAINS the bad link, not just the target it points at.
    awk -v root="file://$ROOT/" '
      /^\[.*\]:$/ { section = $0; shown = 0; next }
      /Cannot find fragment/ {
        if (!shown) { print "  " section; shown = 1 }
        line = $0
        gsub(root, "", line)
        print "    " line
      }
    ' "$raw"
    echo ""
    echo "Each line is 'file#anchor (at line:col)'. Fix the anchor to match the"
    echo "target's current heading, or update the heading. GitHub's slug rule:"
    echo "lowercase, drop punctuation, replace EACH space with '-' (runs are not"
    echo "collapsed). Headings under .loom/docs/ are symlinks into defaults/docs/ --"
    echo "edit the defaults/ copy."
  } >&2
  exit 1
fi

echo "check-doc-anchors: OK - checked ${#FILES[@]} tracked markdown file(s), all #anchors resolve."
