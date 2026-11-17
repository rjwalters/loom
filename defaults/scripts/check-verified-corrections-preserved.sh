#!/usr/bin/env bash
# check-verified-corrections-preserved.sh - Diff-before-rewrite guard for the
# Curator's append-only "## Verified corrections" convention (#4135).
#
# Why: Curator's inter-phase handoff is prose that gets rewritten in place. A
# re-curation pass that regenerates an issue body wholesale can silently drop
# a *verified* finding from an earlier pass in favor of a merely *plausible*
# one -- and the loss leaves no trace in the artifact the next agent reads.
# This happened on #4042: a first Curator pass verified three corrections
# against a live host and recorded them; a second pass rewrote the body and
# dropped all three, asserting the opposite of verified fact. The corrections
# survived only in an earlier *comment*, which is not what a Builder reads
# first. See `.loom/roles/curator.md` -> "Verified Corrections Are
# Append-Only" for the full convention this script enforces.
#
# What it checks: every paragraph under the OLD body's `Verified
# corrections` heading (case-insensitive, at any ATX heading depth >= 2 --
# `##`, `###`, ... including one nested under a `## Curator Enhancement`
# parent; up to the next heading of the SAME OR SHALLOWER depth, or EOF --
# deeper subheadings stay inside the section) must still be present, verbatim
# after whitespace normalization, somewhere under the NEW body's `Verified
# corrections` section. New paragraphs may freely be appended -- this only
# detects paragraphs that would be *lost*, exactly the "append-only"
# invariant. A level-1 `# Verified corrections` heading is deliberately NOT
# matched (a lone h1 is a document title, not a section) but is named in a
# WARN on stderr, so the "no section" OK stays distinguishable from a
# found-and-checked pass (#13938). Heading recognition is fenced-code-block
# aware: a `Verified corrections` heading (or any `#`-comment) quoted inside
# a ``` / ~~~ fence is never a heading -- it can neither open a phantom
# section nor close a real one early (#14001).
#
# Usage:
#   check-verified-corrections-preserved.sh <old-body-file> <new-body-file>
#
# Exit codes:
#   0 - nothing lost (including the common case: OLD has no such section)
#   1 - a verified-corrections paragraph present in OLD is missing from NEW
#   2 - usage error (missing/unreadable arguments)
#
# This is a general-purpose diff tool, not forge-specific -- callers (a
# Curator re-curation pass, or a test suite) supply plain text files
# containing the OLD and proposed NEW issue body.

set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "Usage: $(basename "$0") <old-body-file> <new-body-file>" >&2
  exit 2
fi

OLD_FILE="$1"
NEW_FILE="$2"

for f in "$OLD_FILE" "$NEW_FILE"; do
  if [[ ! -f "$f" ]]; then
    echo "check-verified-corrections-preserved: file not found: $f" >&2
    exit 2
  fi
done

# Extract the body of a "Verified corrections" section. The heading is
# recognised case-insensitively at any ATX depth >= 2 (`##`, `###`, ...);
# the section runs to the next heading of the SAME OR SHALLOWER depth or to
# EOF, so deeper subheadings (and their text) stay inside the section while
# a sibling heading closes it. A depth-1 `# Verified corrections` heading is
# deliberately ignored, but named in a WARN on stderr so a "no section" pass
# is never silent about having seen one (#13938). The extractor is
# fenced-code-block aware (#14001): a fence opens at 3+ backticks or tildes
# after optional indentation (a backtick opener may carry an info string,
# which must itself contain no backtick) and closes only at a line of the
# SAME character, at least as long, followed solely by whitespace -- so a
# longer outer fence quoting a shorter fence stays open, and an unclosed
# fence runs to EOF. While in a fence no line is a heading: a quoted
# "Verified corrections" heading cannot open a phantom section, and a
# `#`-comment inside a real section cannot close it early. Fence lines that
# sit inside a real section remain part of its text, verbatim. Carriage
# returns (CRLF bodies) are stripped before any matching. POSIX awk only --
# match()/RLENGTH, tolower(), print-to-command are portable; no gawk
# constructs, and the depth comes from the leading '#' run via match(),
# never from index($0, " ") which breaks when a tab follows the hashes
# (#13938's sketch bug; #12044's bash-3.2 portability history is why nothing
# fancy is used at all).
extract_section() {
  local which="$2"
  awk -v which="$which" '
    BEGIN { insec = 0; depth = 0; infence = 0; fchar = ""; flen = 0 }
    {
      line = $0
      sub(/\r$/, "", line)
      lower = tolower(line)
      fline = line
      sub(/^[ \t]+/, "", fline)
      if (infence) {
        # Close only on the same fence character as the opener, at least as
        # long, with nothing but trailing whitespace after the run.
        if (match(fline, "^" fchar "+") && RLENGTH >= flen && substr(fline, RLENGTH + 1) ~ /^[ \t]*$/) {
          infence = 0; fchar = ""; flen = 0
        }
        if (insec) print line
        next
      }
      if (match(fline, /^`+/) && RLENGTH >= 3 && substr(fline, RLENGTH + 1) !~ /`/) {
        infence = 1; fchar = "`"; flen = RLENGTH
        if (insec) print line
        next
      }
      if (match(fline, /^~+/) && RLENGTH >= 3) {
        infence = 1; fchar = "~"; flen = RLENGTH
        if (insec) print line
        next
      }
      d = 0
      if (match(lower, /^#+/)) d = RLENGTH
      # A heading at or above the section depth closes it (deeper stays in).
      if (insec && d >= 1 && d <= depth && lower ~ /^#+[ \t]/) insec = 0
      if (lower ~ /^#+[ \t]+verified corrections[ \t]*$/) {
        if (d >= 2) { insec = 1; depth = d; next }
        print "check-verified-corrections-preserved: WARN — " which " body line " FNR ": ignoring heading [ " line " ] — the Verified corrections heading is only recognised at heading depth >= 2, so this section is NOT being checked." | "cat 1>&2"
        next
      }
    }
    insec { print line }
  ' "$1"
}

# Normalize a section's text into one whitespace-collapsed paragraph per
# record, separated by the ASCII Record Separator (0x1e) so downstream
# comparisons never collide with ordinary text content. Blank lines
# (paragraph breaks) delimit records via awk's RS="".
normalize_paragraphs() {
  awk -v RS='' -v ORS=$'\x1e' '
    {
      gsub(/\r/, "");
      gsub(/\n/, " ");
      gsub(/[ \t]+/, " ");
      sub(/^ /, "");
      sub(/ $/, "");
      if (length($0) > 0) print
    }
  '
}

OLD_SECTION="$(extract_section "$OLD_FILE" old)"

# "No non-space char" == the old -z "${X// /}", minus bash 3.2's ${//} cost (#12044).
if [[ "$OLD_SECTION" != *[!\ ]* ]]; then
  echo "check-verified-corrections-preserved: OK — old body has no '## Verified corrections' section (nothing to preserve)."
  exit 0
fi

NEW_SECTION="$(extract_section "$NEW_FILE" new)"

if [[ "$NEW_SECTION" != *[!\ ]* ]]; then
  echo "check-verified-corrections-preserved: FAIL — old body has a '## Verified corrections' section but the new body has none at all. A re-curation pass may append to this section but must never remove it." >&2
  exit 1
fi

OLD_PARAS="$(printf '%s' "$OLD_SECTION" | normalize_paragraphs)"
NEW_LINES="$(printf '%s' "$NEW_SECTION" | normalize_paragraphs | tr '\036' '\n')"

MISSING=0
while IFS= read -r -d $'\x1e' para; do
  [[ -z "$para" ]] && continue
  # Exact-match a normalized old paragraph against the set of normalized new
  # paragraphs. grep -F/-x avoids both regex-metacharacter surprises in
  # issue text and accidental substring matches across paragraph boundaries.
  # NOTE: match against NEW_LINES, built once above via a full-reading
  # `normalize_paragraphs | tr` (no early exit, so no SIGPIPE), fed here as a
  # here-string -- never a `tr | grep -q` pipe. Under `set -o pipefail`,
  # `grep -q` exits on its first match, SIGPIPEs the upstream `tr`, and tr's
  # non-zero exit becomes the pipeline's status: false-positive FAILs on large
  # sections (#6768). Nor a per-iteration ${NEW_PARAS//$'\x1e'/$'\n'}: bash
  # 3.2's ${//} is quadratic -- ~13 s per call on a 300-paragraph section, so
  # the loop hung past CI's 1200 s suite timeout on macOS (#12044).
  if ! grep -qFx -- "$para" <<< "$NEW_LINES"; then
    MISSING=$((MISSING + 1))
    echo "check-verified-corrections-preserved: missing verified-corrections paragraph:" >&2
    echo "  $para" >&2
  fi
done <<< "${OLD_PARAS}"$'\x1e'

if [[ "$MISSING" -gt 0 ]]; then
  echo "check-verified-corrections-preserved: FAIL — $MISSING verified-corrections paragraph(s) from the old body are missing from the new body. Append-only means these may never be deleted or rewritten — restore them verbatim (a disagreement is a new dated paragraph, not a replacement)." >&2
  exit 1
fi

echo "check-verified-corrections-preserved: OK — all verified-corrections paragraphs from the old body are preserved in the new body."
exit 0
