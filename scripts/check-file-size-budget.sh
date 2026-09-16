#!/usr/bin/env bash
# check-file-size-budget.sh — ratchet oversized source files so they stop growing.
#
# Why (#7711): large files are hard for an LLM coding agent to work with. Opening
# one spends context on everything you did not need, edits land further from the
# code that constrains them, and a failed attempt has to re-read the whole thing
# to try again. Loom's source grows monotonically toward that state because every
# individual addition is defensible while the aggregate is the problem — the same
# failure mode check-claude-md-budget.sh (#4014) was built to counter. The
# measurements that motivated this live in #7711; they are deliberately not
# repeated here, because a number in a comment goes stale and then misleads.
#
# WHAT THIS IS NOT: a hard line limit, and NOT a "you touched a big file, now
# refactor it" rule. That rule was considered and rejected in #7711 — the hottest
# files are touched more than once a day, so it would tax nearly every sweep,
# bloat feature diffs far past a typical touch, and collide across the parallel
# worktrees the fleet runs. Judge review gets worse, not better.
#
# WHAT THIS IS: a one-way ratchet. A file already over threshold is frozen at its
# CURRENT size — it may shrink freely, it may not grow. Files under threshold are
# unconstrained. Nothing has to be refactored up front; the gate fires only at
# the moment a change makes a known-bad file worse.
#
# The rule this enforces: when you need to add to an over-threshold file, put the
# new code in a NEW sibling module and leave a small dispatch/match arm behind.
# In Rust that is cheap (`mod foo;` plus a new file). Do NOT raise the threshold
# or hand-edit a baseline entry upward to fit an addition — that is the ratchet
# slipping, which is the whole thing this prevents.
#
# Counting: code lines only — blank lines and comment-only lines never count.
# The WHOLE file counts, including inline `#[cfg(test)]` modules. A big file is
# hard to work with whatever is in it, and an agent editing the production half
# still pays for the test half sitting in the same buffer. Extracting a test
# module to a sibling file is a real improvement and the ratchet records it as
# one: the parent shrinks, and the extracted file is measured on its own terms.
#
# Usage:
#   check-file-size-budget.sh              Check the tree against the baseline.
#   check-file-size-budget.sh --update     Regenerate the baseline (see below).
#   check-file-size-budget.sh --list       Print every file's code-line count.
#   check-file-size-budget.sh --self-test  Verify the gate itself still works.
#   check-file-size-budget.sh --threshold N  Override the default threshold.
#   check-file-size-budget.sh --help
#
# --update is for two legitimate cases: (1) recording shrinkage after a real
# refactor, so the ratchet tightens; (2) a deliberate, reviewed decision to admit
# a new over-threshold file — for example a split that moves lines OUT of a
# larger file into a new one. It is NOT a way to silence a failure. A reviewer
# should treat an --update that raises an existing number, or that grows the
# ledger total, as a red flag.
#
# Exit codes: 0 = within budget; 1 = a file grew or newly crossed; 2 = bad args.

set -euo pipefail

THRESHOLD_DEFAULT=1000
MODE="check"
THRESHOLD="${FILE_SIZE_THRESHOLD:-$THRESHOLD_DEFAULT}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --update)    MODE="update"; shift ;;
    --list)      MODE="list"; shift ;;
    --self-test) MODE="self-test"; shift ;;
    --threshold) THRESHOLD="${2:?--threshold needs a value}"; shift 2 ;;
    --help|-h)   sed -n '2,52p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)           echo "check-file-size-budget: unknown argument '$1'" >&2; exit 2 ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ! ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)"; then
  ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
fi
cd "$ROOT"

BASELINE="$ROOT/scripts/file-size-baseline.txt"

# --- Exemptions -------------------------------------------------------------
#
# EXEMPT means "not measured at all". Only cases where a violation would be
# permanently unfixable in this repo qualify:
#
#   1. VENDORED — the canonical copy lives in rjwalters/repo and is re-vendored
#      here at release time. The guard's own header explicitly forbids
#      hand-editing generic pattern behavior, so a local split would be reverted
#      by the next re-vendor. Structural fixes belong upstream.
#   2. INSTALLED MIRRORS — .loom/hooks/, .loom/scripts/, .loom/docs/ etc. are
#      resync copies of defaults/. Measuring both would double-count every
#      violation and churn the baseline on every resync commit.
#
# Bootstrap scripts (install.sh, install-loom.sh, loom-daemon-{update,start}.sh)
# are deliberately NOT exempt. They run before, or manage, the loom-daemon binary
# and so can never be ported into it — but "cannot be ported to Rust" is not "may
# grow without bound". The ratchet is exactly the right mechanism for them.
#
# Test files are NOT exempt either, in any language. A large test file is just as
# awkward to open and edit as a large production file.
is_exempt() {
  case "$1" in
    # Installed mirrors of defaults/ — measured at their defaults/ source.
    .loom/*)                                      return 0 ;;
    # Vendored from rjwalters/repo; upstream owns the structure.
    defaults/hooks/guard-destructive-generic.sh)  return 0 ;;
    defaults/hooks/guard-destructive.sh)          return 0 ;;
    defaults/scripts/resync-installed.sh)         return 0 ;;
    # Build/dependency output that git may track in some checkouts.
    target/*|node_modules/*|*/node_modules/*|*/dist/*) return 0 ;;
  esac
  return 1
}

# --- Measurement ------------------------------------------------------------
# One awk pass over every candidate file: count lines that are neither blank nor
# comment-only. No language-specific structure is parsed. An earlier revision
# tried to separate production from test lines by tracking `#[cfg(test)]`, and
# that produced two silent bugs in opposite directions before it was abandoned
# (#7711) — counting the whole file needs none of that machinery.
measure_all() {
  local files=()
  while IFS= read -r f; do
    is_exempt "$f" && continue
    [[ -f "$f" ]] || continue
    files+=("$f")
  done < <(git ls-files -- '*.rs' '*.sh' '*.ts' | LC_ALL=C sort)

  [[ ${#files[@]} -eq 0 ]] && return 0

  awk '
    FNR == 1 { if (!(FILENAME in count)) count[FILENAME] = 0 }
    {
      line = $0
      sub(/^[ \t]+/, "", line)
      if (line == "") next
      if (FILENAME ~ /\.rs$/ && line ~ /^\/\//) next
      if (FILENAME ~ /\.sh$/ && line ~ /^#/)    next
      if (FILENAME ~ /\.ts$/ && (line ~ /^\/\// || line ~ /^\*/ || line ~ /^\/\*/)) next
      count[FILENAME]++
    }
    END { for (f in count) printf "%d %s\n", count[f], f }
  ' "${files[@]}" | LC_ALL=C sort -k2,2
}

baseline_for() {
  awk -v p="$1" '$1 !~ /^#/ && $2 == p { print $1; found = 1; exit } END { if (!found) print "" }' "$BASELINE"
}

write_baseline() {
  {
    echo "# file-size-baseline.txt — generated by scripts/check-file-size-budget.sh --update"
    echo "#"
    echo "# Every source file currently OVER the ${THRESHOLD}-line code-line threshold, with"
    echo "# its size at the time of recording. check-file-size-budget.sh fails when a"
    echo "# listed file GROWS past its recorded number, or when an unlisted file crosses"
    echo "# the threshold. Shrinking is always allowed."
    echo "#"
    echo "# This is a debt ledger, not a target. The numbers should only ever go DOWN, and"
    echo "# the list should only ever get shorter. A diff that raises a number, or that"
    echo "# grows the total, needs a clear reason in the PR description — see"
    echo "# .loom/docs/file-size-policy.md."
    echo "#"
    echo "# <code-lines> <path>"
    measure_all | while read -r n f; do
      if [[ -n "$n" ]] && (( n > THRESHOLD )); then
        printf '%d %s\n' "$n" "$f"
      fi
    done
  } > "$BASELINE.tmp"
  mv "$BASELINE.tmp" "$BASELINE"
}

# --- Self-test ---------------------------------------------------------------
# A ratchet that silently stops measuring is worse than no ratchet: it reports OK
# forever while the files it guards keep growing. This exercises the counting
# rules, the exemptions and all three verdicts against synthetic fixtures in a
# throwaway repo, so a future edit fails loudly in CI rather than quietly
# disarming the gate. Mirrors the --self-test convention already used by
# check-docs-defaults-parity.sh.
self_test() {
  local tmp rc=0 out S T
  tmp="$(mktemp -d)"
  trap 'cleanup_self_test "$tmp"' RETURN

  mkdir -p "$tmp/scripts" "$tmp/src" "$tmp/.loom/scripts" "$tmp/defaults/hooks"
  cp "${BASH_SOURCE[0]}" "$tmp/scripts/check-file-size-budget.sh"
  chmod +x "$tmp/scripts/check-file-size-budget.sh"
  git -C "$tmp" init -q
  git -C "$tmp" config user.email t@t.test
  git -C "$tmp" config user.name t

  # 12 code lines plus blanks and comment-only lines, which must not count.
  {
    for i in $(seq 1 12); do echo "pub const C_$i: u8 = $i;"; done
    echo ""
    echo "// a comment"
    echo ""
    echo "   // an indented comment"
  } > "$tmp/src/small.rs"

  # An inline test module DOES count toward the file's size.
  {
    for i in $(seq 1 12); do echo "pub const D_$i: u8 = $i;"; done
    echo "#[cfg(test)]"
    echo "mod tests {"
    for i in $(seq 1 12); do echo "    pub const T_$i: u8 = $i;"; done
    echo "}"
  } > "$tmp/src/withtests.rs"

  # 30 code lines -> over a threshold of 20.
  for i in $(seq 1 30); do echo "pub const B_$i: u8 = $i;"; done > "$tmp/src/big.rs"

  # Exemption fixtures: both are over threshold and must never be measured.
  for i in $(seq 1 50); do echo "echo $i"; done > "$tmp/.loom/scripts/mirror.sh"
  for i in $(seq 1 50); do echo "echo $i"; done > "$tmp/defaults/hooks/guard-destructive-generic.sh"

  git -C "$tmp" add -A >/dev/null
  git -C "$tmp" commit -qm fixtures

  S="$tmp/scripts/check-file-size-budget.sh"
  T="--threshold 20"

  _expect() {
    local label="$1" want="$2" got="$3"
    if [[ "$want" == "$got" ]]; then
      echo "  ok   $label"
    else
      echo "  FAIL $label (want $want, got $got)" >&2
      rc=1
    fi
  }

  out="$( (cd "$tmp" && $S $T --list) | awk '$2=="src/small.rs"{print $1}')"
  _expect "blank and comment-only lines excluded" "12" "$out"

  # 12 production + 1 attribute + 1 mod line + 12 test consts + 1 closing brace.
  out="$( (cd "$tmp" && $S $T --list) | awk '$2=="src/withtests.rs"{print $1}')"
  _expect "inline test module counts toward file size" "27" "$out"

  out="$( (cd "$tmp" && $S $T --list) | grep -c 'mirror\.sh\|guard-destructive-generic' || true)"
  _expect "installed-mirror and vendored paths not measured" "0" "$out"

  (cd "$tmp" && $S $T --update >/dev/null)

  out="$(awk '$2=="src/big.rs"{print "y"}' "$tmp/scripts/file-size-baseline.txt")"
  _expect "baseline records the over-threshold file" "y" "$out"
  out="$(awk '$2=="src/small.rs"{print "y"}' "$tmp/scripts/file-size-baseline.txt")"
  _expect "baseline omits the under-threshold file" "" "$out"

  (cd "$tmp" && $S $T >/dev/null 2>&1) && out=0 || out=$?
  _expect "clean tree passes" "0" "$out"

  echo "pub const EXTRA: u8 = 0;" >> "$tmp/src/big.rs"
  (cd "$tmp" && $S $T >/dev/null 2>&1) && out=0 || out=$?
  _expect "growth of an over-threshold file fails" "1" "$out"

  for i in $(seq 1 25); do echo "pub const B_$i: u8 = $i;"; done > "$tmp/src/big.rs"
  (cd "$tmp" && $S $T >/dev/null 2>&1) && out=0 || out=$?
  _expect "shrinking passes" "0" "$out"

  for i in $(seq 1 30); do echo "pub const B_$i: u8 = $i;"; done > "$tmp/src/big.rs"
  for i in $(seq 1 40); do echo "pub const N_$i: u8 = $i;"; done > "$tmp/src/newbig.rs"
  git -C "$tmp" add -A >/dev/null
  (cd "$tmp" && $S $T >/dev/null 2>&1) && out=0 || out=$?
  _expect "new file crossing the threshold fails" "1" "$out"

  # Growing a test module is NOT a free pass under whole-file counting.
  : > "$tmp/src/newbig.rs"
  {
    echo "#[cfg(test)]"
    echo "mod more {"
    for i in $(seq 1 99); do echo "    pub const M_$i: u8 = 1;"; done
    echo "}"
  } >> "$tmp/src/big.rs"
  (cd "$tmp" && $S $T >/dev/null 2>&1) && out=0 || out=$?
  _expect "inline test-module growth is not exempt" "1" "$out"

  if [[ $rc -eq 0 ]]; then
    echo "check-file-size-budget --self-test: all checks passed."
  else
    echo "check-file-size-budget --self-test: FAILURES above." >&2
  fi
  return $rc
}

# Separated so the trap above never expands a bare variable into a delete.
cleanup_self_test() {
  case "$1" in
    /tmp/*|/var/folders/*|/private/var/folders/*) rm -rf -- "$1" ;;
    *) echo "self-test: refusing to clean unexpected temp path '$1'" >&2 ;;
  esac
}

case "$MODE" in
  self-test)
    self_test
    exit $?
    ;;
  list)
    measure_all | sort -rn
    exit 0
    ;;
  update)
    write_baseline
    n="$(grep -cv '^#' "$BASELINE" || true)"
    echo "check-file-size-budget: baseline regenerated — $n file(s) over $THRESHOLD lines."
    echo "Review the diff: numbers should only go DOWN."
    exit 0
    ;;
esac

if [[ ! -f "$BASELINE" ]]; then
  echo "check-file-size-budget: no baseline at $BASELINE." >&2
  echo "Create it once with: scripts/check-file-size-budget.sh --update" >&2
  exit 1
fi

grew=() crossed=() shrank=()
while read -r n f; do
  if [[ -z "$n" ]]; then continue; fi
  base="$(baseline_for "$f")"
  if [[ -n "$base" ]]; then
    if (( n > base )); then
      grew+=("$f|$base|$n")
    elif (( n < base )); then
      shrank+=("$f|$base|$n")
    fi
  elif (( n > THRESHOLD )); then
    crossed+=("$f|$n")
  fi
done < <(measure_all)

if (( ${#grew[@]} > 0 || ${#crossed[@]} > 0 )); then
  {
    echo "check-file-size-budget: FAIL"
    echo ""
    if (( ${#grew[@]} > 0 )); then
      echo "These files are already over the ${THRESHOLD}-line threshold and GREW:"
      for e in "${grew[@]}"; do
        IFS='|' read -r f b n <<< "$e"
        printf '  %-58s %s -> %s  (+%s)\n' "$f" "$b" "$n" "$((n - b))"
      done
      echo ""
    fi
    if (( ${#crossed[@]} > 0 )); then
      echo "These files newly CROSSED the ${THRESHOLD}-line threshold:"
      for e in "${crossed[@]}"; do
        IFS='|' read -r f n <<< "$e"
        printf '  %-58s %s lines\n' "$f" "$n"
      done
      echo ""
    fi
    echo "An over-threshold file is frozen at its current size: it may shrink, not grow."
    echo "To land this change:"
    echo "  - Put the new code in a NEW sibling module and leave a dispatch/match arm"
    echo "    behind (in Rust: 'mod foo;' plus a new file). This is the intended path."
    echo "  - Or remove at least as much as you added from the same file."
    echo ""
    echo "If this change legitimately SPLITS a large file into smaller ones, run"
    echo "--update and say so in the PR description: the ledger total should drop."
    echo ""
    echo "Do NOT raise the threshold, and do NOT hand-edit the baseline upward — that"
    echo "is the ratchet slipping. See .loom/docs/file-size-policy.md."
  } >&2
  exit 1
fi

msg="check-file-size-budget: OK — no over-threshold file grew (threshold $THRESHOLD)."
if (( ${#shrank[@]} > 0 )); then
  msg="$msg ${#shrank[@]} file(s) shrank; run --update to tighten the ratchet."
fi
echo "$msg"
exit 0
