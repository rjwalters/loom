#!/usr/bin/env bash
# check-pipefail-early-exit.sh — ratchet the `pipefail` + early-exit-consumer
# SIGPIPE class so it stops growing (#7790, parent #7789).
#
# THE BUG CLASS. `set -o pipefail` makes a pipeline report the rightmost
# non-zero stage exit. An early-exit consumer — `head`, `grep -q`, `grep -m N`,
# `read` — closes the pipe as soon as it has what it needs. If the producer is
# still writing, it takes SIGPIPE and exits 141, and `pipefail` turns that into
# a failed pipeline even though the consumer did exactly what it was asked.
# Whether it fires depends on whether the producer finished before the consumer
# quit, which depends on output size vs. the 64K pipe buffer and on scheduling —
# so the same line passes a thousand times and then does not.
#
# Five issues in this repo were one instance of this each:
#   #7060  test-guard-codex-bridge.sh   printf into a short reader
#   #7285  test-install-stash-scope.sh  red CI on a docs-only commit
#   #7540  test-sweep-lease-fence.sh    printf | head
#   #7736  verify-proposal-refs.sh      full_tree() printf, "write error: Broken pipe"
#   #7771  verify-proposal-refs.sh      grep -qFx — NOT a flake: false `MISSING FILE`
#
# #7771 is why this file exists. The same mechanism that produces intermittent
# red CI also produced a silent wrong ANSWER in production: a path that was
# present on origin/main was reported missing, because the `grep -qFx` that
# found it killed the `printf` feeding it.
#
# WHAT THIS IS NOT: a hard ban on `cmd | grep -q`. That idiom appears several
# hundred times here and is overwhelmingly harmless — a producer whose whole
# output fits in the pipe buffer finishes before the consumer exits and never
# sees SIGPIPE. Flagging all of them would be several hundred findings that
# cannot be triaged, and a gate nobody can satisfy gets disabled.
#
# WHAT THIS IS: a one-way ratchet, the same shape as
# scripts/check-file-size-budget.sh (#7711). Every occurrence that exists today
# is recorded in scripts/pipefail-early-exit-baseline.txt as a per-file count and
# frozen. A file may shed occurrences freely; it may not gain one, and a file
# with no recorded count may not gain its first. So the gate fires at exactly the
# moment someone writes a NEW one — which is what issue #7790 asks for — without
# demanding that the existing ledger be paid off first.
#
# HOW TO FIX A FINDING (all four are pure-bash, no subshell, no pipe):
#
#   first line of a variable     ${var%%$'\n'*}            (not: printf | head -1)
#   substring / membership test  grep -q RE <<<"$var"      (not: printf | grep -q)
#   exact membership in a list   case $'\n'"$list"$'\n' in *$'\n'"$x"$'\n'*) …
#   one line from a command      IFS= read -r line < <(cmd)
#
# If a pipeline genuinely cannot be rewritten and genuinely cannot SIGPIPE,
# exempt that line with a comment naming why, on the line or the line above it:
#
#   # loom-lint: allow-pipefail-early-exit — producer is a fixed 3-line literal
#
# DETECTION, and its deliberate limits. A finding needs all three of:
#   1. the file enables pipefail (`set -o pipefail` / `set -euo pipefail` / …);
#   2. a pipeline stage is a known early-exit consumer (denylist: head, grep
#      with -q/--quiet/--silent, grep with -m/--max-count, read);
#   3. the pipeline's exit status is actually CONSUMED — it is an if/elif/while/
#      until condition, is negated with `!`, is an operand of `&&`/`||`, or the
#      file also enables `set -e` (under which any bare pipeline aborts).
#      A pipeline whose status is discarded cannot misreport anything.
# `… || true` and `… || :` are not findings: they neutralise the false failure.
#
# It is line-oriented, so a pipeline split across a `\` continuation is only seen
# via rule 3's `set -e` arm, and heredoc bodies are scanned as if they were code.
# The denylist is a denylist: a novel early-exit consumer (an `awk … {exit}`, a
# `sed … q`) is not detected. Both are accepted costs — this is a ratchet on a
# known class, not a proof of absence. The real fix for the class is #7711/#7762
# (port these scripts to Rust), against which this gate becomes dead code.
#
# Usage:
#   check-pipefail-early-exit.sh                 Check the tree against the baseline.
#   check-pipefail-early-exit.sh --update        Regenerate the baseline.
#   check-pipefail-early-exit.sh --list          Print every finding in the tree.
#   check-pipefail-early-exit.sh [file …]        Restrict the scan to these files.
#   check-pipefail-early-exit.sh --baseline F    Use baseline file F.
#   check-pipefail-early-exit.sh --require-baseline
#                                                Fail if the baseline is absent
#                                                (CI uses this; a deleted ledger
#                                                must not read as "all clear").
#   check-pipefail-early-exit.sh --quiet         Only print failures.
#   check-pipefail-early-exit.sh --help
#
# --update is for recording a REDUCTION after a real fix. Raising a number to fit
# a new occurrence is the ratchet slipping, which is the whole thing this
# prevents; write the fix or the exemption comment instead.
#
# Exit codes: 0 = no new occurrences; 1 = a new occurrence (or a missing
# required baseline); 2 = bad arguments.

set -uo pipefail

MODE="check"
QUIET=0
REQUIRE_BASELINE=0
BASELINE_OVERRIDE=""
EXPLICIT_FILES=()

usage() {
  sed -n '2,95p' "$0" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --update)           MODE="update"; shift ;;
    --list)             MODE="list"; shift ;;
    --check)            MODE="check"; shift ;;
    --quiet|-q)         QUIET=1; shift ;;
    --require-baseline) REQUIRE_BASELINE=1; shift ;;
    --baseline)
      if [[ $# -lt 2 || -z "${2:-}" ]]; then
        echo "check-pipefail-early-exit: --baseline needs a value" >&2
        exit 2
      fi
      BASELINE_OVERRIDE="$2"; shift 2 ;;
    --help|-h)          usage; exit 0 ;;
    --*)
      echo "check-pipefail-early-exit: unknown argument '$1'" >&2
      exit 2 ;;
    *)                  EXPLICIT_FILES+=("$1"); shift ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ! ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)"; then
  ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
fi

BASELINE="${BASELINE_OVERRIDE:-$ROOT/scripts/pipefail-early-exit-baseline.txt}"

# --- Exemptions -------------------------------------------------------------
#
# Only paths where a finding could never be acted on here:
#   1. INSTALLED MIRRORS — .loom/hooks/, .loom/scripts/ and the quickstart
#      .loom/ trees are resync copies of defaults/. Measuring both double-counts
#      every occurrence and churns the baseline on every resync commit; the
#      source under defaults/ is measured instead.
#   2. VENDORED — guard-destructive{,-generic}.sh and resync-installed.sh are
#      re-vendored from rjwalters/repo at release time, so a local edit is
#      reverted by the next re-vendor. Same list check-file-size-budget.sh uses.
#   3. Build output git may track in some checkouts.
is_exempt() {
  case "$1" in
    .loom/*|*/.loom/*)                            return 0 ;;
    defaults/hooks/guard-destructive-generic.sh)  return 0 ;;
    defaults/hooks/guard-destructive.sh)          return 0 ;;
    defaults/scripts/resync-installed.sh)         return 0 ;;
    target/*|node_modules/*|*/node_modules/*|*/dist/*) return 0 ;;
  esac
  return 1
}

# --- Scanning ---------------------------------------------------------------
# One awk pass over every candidate file. Deliberately plain POSIX ERE (no \b,
# no interval braces, no gawk-only builtins) so the ubuntu and macOS CI legs,
# which run different awks, agree on the finding count the baseline records.
AWK_SCAN='
function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t]+$/, "", s); return s }

# Cut an unquoted trailing comment so a `#`-commented example never counts.
function strip_comment(s,   i, n, ch, inq, q, out) {
  n = length(s); out = ""; inq = 0; q = "";
  for (i = 1; i <= n; i++) {
    ch = substr(s, i, 1);
    if (inq) { if (ch == q) inq = 0; out = out ch; continue }
    if (ch == "'"'"'" || ch == "\"") { inq = 1; q = ch; out = out ch; continue }
    if (ch == "#" && (i == 1 || substr(s, i - 1, 1) ~ /[ \t]/)) break;
    out = out ch;
  }
  return out;
}

# Which early-exit consumer, if any, a pipeline stage of s is. The leading
# (^|[^|]) keeps `||` (a list operator) from reading as a pipe.
function find_consumer(s) {
  if (s ~ /(^|[^|])[|][ \t]*head([ \t;)|&]|$)/)
    return "head";
  if (s ~ /(^|[^|])[|][ \t]*(command[ \t]+)?grep[ \t]+(-[-A-Za-z0-9]+[ \t]+)*(-[A-Za-z]*q[A-Za-z]*|--quiet|--silent)([ \t]|$)/)
    return "grep -q";
  if (s ~ /(^|[^|])[|][ \t]*(command[ \t]+)?grep[ \t]+(-[-A-Za-z0-9]+[ \t]+)*(-[A-Za-z]*m[A-Za-z]*[ \t]*[0-9]|--max-count)/)
    return "grep -m";
  if (s ~ /(^|[^|])[|][ \t]*(IFS=[^ \t]*[ \t]+)*read([ \t;)|&]|$)/)
    return "read";
  return "";
}

# Is the pipeline exit status observable? If it is discarded, a spurious 141
# cannot change any behaviour, so it is not a finding.
function status_consumed(s, errexit,   t) {
  t = trim(s);
  # `… || true` / `… || :` absorbs the spurious 141. The trailing class lets the
  # guard sit inside a command substitution — `x="$(cmd | head -1 || true)"` —
  # which is where it is most often written.
  if (t ~ /[|][|][ \t]*(true|:)([ \t;)"}`]|$)/) return 0;
  if (t ~ /^(if|elif|while|until)[ \t(!]/)       return 1;
  if (t ~ /(^|[ \t;(])![ \t]+/)                  return 1;
  if (t ~ /&&/)                                  return 1;
  if (t ~ /[|][|]/)                              return 1;
  if (errexit)                                   return 1;
  return 0;
}

function process(f,   i, l, pipefail, errexit, raw, s, cons) {
  pipefail = 0; errexit = 0;
  for (i = 1; i <= n; i++) {
    l = buf[i];
    if (l ~ /^[ \t]*set[ \t]+-[A-Za-z]*o[ \t]+pipefail/) pipefail = 1;
    if (l ~ /^[ \t]*set[ \t]+-[A-Za-z]*e/)               errexit  = 1;
    if (l ~ /^[ \t]*set[ \t]+-o[ \t]+errexit/)           errexit  = 1;
  }
  if (!pipefail) return;

  for (i = 1; i <= n; i++) {
    raw = buf[i];
    if (index(raw, "|") == 0) continue;
    if (raw ~ /^[ \t]*#/) continue;
    if (raw ~ /loom-lint:[ \t]*allow-pipefail-early-exit/) continue;
    if (i > 1 && buf[i - 1] ~ /loom-lint:[ \t]*allow-pipefail-early-exit/) continue;
    s = strip_comment(raw);
    if (index(s, "|") == 0) continue;
    cons = find_consumer(s);
    if (cons == "") continue;
    if (!status_consumed(s, errexit)) continue;
    printf "%s\t%d\t%s\t%s\n", f, i, cons, trim(raw);
  }
}

FNR == 1 {
  if (have) process(prevfile);
  prevfile = FILENAME; have = 1; n = 0;
  for (k in buf) delete buf[k];
}
{ buf[++n] = $0 }
END { if (have) process(prevfile) }
'

# Emits "path<TAB>line<TAB>consumer<TAB>source" for every finding, path-sorted.
scan_all() {
  local files=() f
  if [[ ${#EXPLICIT_FILES[@]} -gt 0 ]]; then
    for f in "${EXPLICIT_FILES[@]}"; do
      [[ -f "$f" ]] && files+=("$f")
    done
  else
    while IFS= read -r f; do
      is_exempt "$f" && continue
      [[ -f "$ROOT/$f" ]] || continue
      files+=("$f")
    done < <(git -C "$ROOT" ls-files -- '*.sh' | LC_ALL=C sort)
  fi

  [[ ${#files[@]} -eq 0 ]] && return 0

  if [[ ${#EXPLICIT_FILES[@]} -gt 0 ]]; then
    awk "$AWK_SCAN" "${files[@]}"
  else
    (cd "$ROOT" && awk "$AWK_SCAN" "${files[@]}")
  fi
}

# "count<TAB>path" for every file with at least one finding.
counts_from() {
  awk -F'\t' '{ c[$1]++ } END { for (f in c) printf "%d\t%s\n", c[f], f }' "$1" \
    | LC_ALL=C sort -k2,2
}

baseline_for() {
  [[ -f "$BASELINE" ]] || { echo 0; return 0; }
  awk -v p="$1" '$1 !~ /^#/ && $2 == p { print $1; found = 1; exit }
                 END { if (!found) print 0 }' "$BASELINE"
}

write_baseline() {
  local tmp findings
  findings="$(mktemp "${TMPDIR:-/tmp}/pipefail-findings.XXXXXX")"
  tmp="$BASELINE.tmp.$$"
  scan_all > "$findings"
  {
    echo "# pipefail-early-exit-baseline.txt — generated by"
    echo "# scripts/check-pipefail-early-exit.sh --update"
    echo "#"
    echo "# Per-file count of \`set -o pipefail\` pipelines whose status is consumed and"
    echo "# whose consumer exits early (head / grep -q / grep -m / read) — the SIGPIPE"
    echo "# class behind #7060, #7285, #7540, #7736 and #7771 (parent #7789)."
    echo "#"
    echo "# This is a debt ledger, not a target. The gate fails when a listed file GAINS"
    echo "# an occurrence, or when an unlisted file gains its first. Shedding occurrences"
    echo "# is always allowed — rerun with --update to record the reduction."
    echo "#"
    echo "# Most entries are latent, not live: a producer whose output fits the pipe"
    echo "# buffer never SIGPIPEs. Being listed here is not a bug report. It means the"
    echo "# file may not add another one, and that a reader looking for the next #7771"
    echo "# has a candidate list."
    echo "#"
    echo "# Entries under defaults/scripts/tests/ and defaults/hooks/tests/ are mostly the"
    echo "# per-file assert_contains / assert_not_contains helpers, whose rewrite is"
    echo "# issue #7862's scope, not this ledger's. They are expected to fall to 0 as"
    echo "# that work lands; the ratchet allows that without an --update."
    echo "#"
    echo "# <occurrences> <path>"
    counts_from "$findings" | awk -F'\t' '{ printf "%s %s\n", $1, $2 }'
  } > "$tmp"
  mv "$tmp" "$BASELINE"
  rm -f "$findings"
}

# --- Modes ------------------------------------------------------------------

if [[ "$MODE" == "update" ]]; then
  write_baseline
  [[ "$QUIET" -eq 1 ]] || echo "check-pipefail-early-exit: wrote $BASELINE"
  exit 0
fi

FINDINGS="$(mktemp "${TMPDIR:-/tmp}/pipefail-findings.XXXXXX")"
# shellcheck disable=SC2329  # invoked indirectly via the EXIT trap
cleanup() { rm -f "$FINDINGS" 2>/dev/null || true; }
trap cleanup EXIT

scan_all > "$FINDINGS"

if [[ "$MODE" == "list" ]]; then
  awk -F'\t' '{ printf "%s:%s: %s\n    %s\n", $1, $2, $3, $4 }' "$FINDINGS"
  awk 'END { printf "check-pipefail-early-exit: %d occurrence(s)\n", NR }' "$FINDINGS"
  exit 0
fi

if [[ ! -f "$BASELINE" ]]; then
  if [[ "$REQUIRE_BASELINE" -eq 1 ]]; then
    echo "check-pipefail-early-exit: baseline not found: $BASELINE" >&2
    echo "  A missing ledger must not read as 'all clear'. Restore it, or run --update." >&2
    exit 1
  fi
  [[ "$QUIET" -eq 1 ]] || echo "check-pipefail-early-exit: no baseline at $BASELINE; run --update to adopt the ratchet."
  exit 0
fi

REGRESSED=0
TOTAL=0
while IFS=$'\t' read -r count path; do
  TOTAL=$((TOTAL + count))
  allowed="$(baseline_for "$path")"
  if [[ "$count" -gt "$allowed" ]]; then
    REGRESSED=$((REGRESSED + 1))
    echo "" >&2
    echo "ERROR: $path has $count pipefail + early-exit-consumer pipeline(s); baseline allows $allowed." >&2
    awk -F'\t' -v p="$path" '$1 == p { printf "  %s:%s: %s\n      %s\n", $1, $2, $3, $4 }' "$FINDINGS" >&2
  fi
done < <(counts_from "$FINDINGS")

if [[ "$REGRESSED" -gt 0 ]]; then
  cat >&2 <<'EOF'

Under `set -o pipefail`, an early-exit consumer (head / grep -q / grep -m / read)
can close the pipe while the producer is still writing; the producer takes
SIGPIPE (141) and pipefail reports the whole pipeline as failed. That yields
flaky CI (#7060, #7285, #7540, #7736) and, in #7771, a silent wrong answer.

Rewrite it without a pipe — see the "HOW TO FIX A FINDING" table in
scripts/check-pipefail-early-exit.sh. If it truly cannot SIGPIPE, exempt the
line with a comment naming why:

    # loom-lint: allow-pipefail-early-exit -- <reason>

Do NOT raise the baseline number to fit a new occurrence.
EOF
  exit 1
fi

if [[ "$QUIET" -eq 0 ]]; then
  echo "check-pipefail-early-exit: OK — $TOTAL known occurrence(s), none new."
fi
exit 0
