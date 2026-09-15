#!/usr/bin/env bash
# check-file-size-budget.sh — ratchet oversized source files so they stop growing.
#
# Why (#7711): Loom's source files grow monotonically because every individual
# addition is defensible while the aggregate is the problem — the same failure
# mode check-claude-md-budget.sh (#4014) was built to counter, now playing out
# in loom-daemon/src/*.rs and defaults/scripts/*.sh. Measured on 2026-09-15:
# 86 of 228 Rust files exceeded 1000 lines and held 79% of all Rust LOC, and the
# five worst files had reached their ENTIRE current size within 90 days at an
# add-to-delete ratio of roughly 20:1. Agents add; they essentially never
# restructure.
#
# WHAT THIS IS NOT: a hard line limit, and NOT a "you touched a big file, now
# refactor it" rule. That rule was considered and rejected in #7711 — ipc.rs is
# touched more than once a day, so it would tax nearly every sweep, bloat
# feature diffs far past the +160/-24 mean touch, and collide across the
# parallel worktrees the fleet runs. Judge review gets worse, not better.
#
# WHAT THIS IS: a one-way ratchet. A file already over threshold is frozen at
# its CURRENT size — it may shrink freely, it may not grow. Files under
# threshold are unconstrained. Nothing has to be refactored up front; the gate
# fires only at the moment an agent makes a known-bad file worse.
#
# The rule this enforces: when you need to add to an over-threshold file, put
# the new code in a NEW sibling module and leave a small dispatch/match arm
# behind. In Rust that is cheap (`mod foo;` + a new file). Do NOT raise the
# threshold or hand-edit a baseline entry upward to fit an addition — that is
# the ratchet slipping, which is the whole thing this prevents.
#
# Counting: code lines only — blank lines and comment-only lines never count.
# For Rust, only PRODUCTION lines count: everything before the first
# `#[cfg(test)]` module. Inline test modules are idiomatic Rust and are 49% of
# this repo's Rust bulk; taxing them would push tests out of the codebase for
# the wrong reason. (Extracting them to sibling files is worthwhile, but that is
# a separate track — see .loom/docs/file-size-policy.md.)
#
# Usage:
#   check-file-size-budget.sh              Check the tree against the baseline.
#   check-file-size-budget.sh --update     Regenerate the baseline (see below).
#   check-file-size-budget.sh --list       Print every file's code-line count.
#   check-file-size-budget.sh --self-test  Verify the gate itself still works.
#   check-file-size-budget.sh --threshold N  Override the 1000-line threshold.
#   check-file-size-budget.sh --help
#
# --update is for two legitimate cases: (1) recording shrinkage after a real
# refactor, so the ratchet tightens; (2) a deliberate, reviewed decision to admit
# a new over-threshold file. It is NOT a way to silence a failure — a reviewer
# should treat an --update that RAISES any number as a red flag.
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
    --help|-h)   sed -n '2,50p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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
# EXEMPT means "not measured at all". Only two categories qualify, and both are
# cases where a violation would be permanently unfixable in this repo:
#
#   1. VENDORED — the canonical copy lives in rjwalters/repo and is re-vendored
#      here at release time. guard-destructive-generic.sh is the single largest
#      shell file in the repo (8872 lines) and its own header explicitly forbids
#      hand-editing generic pattern behavior. Splitting it locally would be
#      reverted by the next re-vendor.
#   2. INSTALLED MIRRORS — .loom/hooks/, .loom/scripts/, .loom/docs/ etc. are
#      resync copies of defaults/. Measuring both would double-count every
#      violation and make the baseline churn on every resync commit.
#
# Bootstrap scripts (install.sh, install-loom.sh, loom-daemon-{update,start}.sh)
# are deliberately NOT exempt. They run before, or manage, the loom-daemon
# binary and so can never be ported into it (#7711's tier table) — but "can't be
# ported to Rust" is not "may grow without bound". The ratchet is exactly the
# right mechanism for them.
is_exempt() {
  case "$1" in
    # Installed mirrors of defaults/ — measured at their defaults/ source.
    .loom/*)                                      return 0 ;;
    # Vendored from rjwalters/repo; upstream owns the structure.
    defaults/hooks/guard-destructive-generic.sh)  return 0 ;;
    defaults/hooks/guard-destructive.sh)          return 0 ;;
    defaults/scripts/resync-installed.sh)         return 0 ;;
    # Rust test modules extracted to their own file (#7718). The policy does not
    # tax Rust tests, inline or extracted -- and counting an extracted module as
    # production would make this gate FAIL the very refactor it exists to
    # reward: moving `#[cfg(test)] mod tests` out of ipc.rs creates a 4400-line
    # ipc/tests.rs that newly "crosses" the threshold, while the production code
    # it was measuring did not change by a single line.
    */tests.rs|*/tests/*.rs)                      return 0 ;;
    # Build/dependency output that git may track in some checkouts.
    target/*|node_modules/*|*/node_modules/*|*/dist/*) return 0 ;;
  esac
  return 1
}

# --- Measurement ------------------------------------------------------------
# One awk pass over every candidate file. Code lines only; for .rs, stop at the
# first `#[cfg(test)]` so inline test modules are excluded (see header).
measure_all() {
  local files=()
  while IFS= read -r f; do
    is_exempt "$f" && continue
    [[ -f "$f" ]] || continue
    files+=("$f")
  done < <(git ls-files -- '*.rs' '*.sh' '*.ts' | LC_ALL=C sort)

  [[ ${#files[@]} -eq 0 ]] && return 0

  awk '
    FNR == 1 { skip = 0; pending = 0; if (!(FILENAME in count)) count[FILENAME] = 0 }

    # --- Rust: skip TOP-LEVEL `#[cfg(test)] mod ... { ... }` blocks only ------
    # Not "everything after the first #[cfg(test)]": that attribute also marks
    # test-only helper FUNCTIONS, which sit mid-file with thousands of
    # production lines below them (role_runner.rs line 894, work_finder.rs
    # line 1004). Truncating there silently undercounted two of the largest
    # files in the repo to ~900 lines and kept them out of the baseline.
    #
    # A top-level module ends at a `}` in column 0 — everything inside it is
    # indented — so no brace counting (and no string/comment escaping) is
    # needed. A raw string containing a column-0 `}` would end the skip early;
    # that fails toward counting MORE lines, which is the safe direction.
    FILENAME ~ /\.rs$/ {
      if (skip) { if ($0 ~ /^\}[ \t]*$/) skip = 0; next }
      if ($0 ~ /^#\[cfg\(test\)\]/) { pending = 1; next }
      if (pending) {
        # Attributes may sit between #[cfg(test)] and the item it decorates, and
        # they may span multiple lines:
        #     #[cfg(test)]
        #     #[allow(
        #         clippy::unwrap_used,
        #     )]
        #     mod tests {
        # So stay pending through anything that is not a column-0 item keyword:
        # further attributes, their indented continuation lines, the closing
        # `)]`, and blanks. Decide only when a real item starts in column 0.
        if ($0 ~ /^[ \t]/ || $0 ~ /^#\[/ || $0 ~ /^\)\]/ || $0 ~ /^[ \t]*$/) next
        if ($0 ~ /^(pub )?mod /) { pending = 0; if ($0 !~ /\}[ \t]*$/) skip = 1; next }
        # #[cfg(test)] on a non-module item (a helper fn): the item itself is
        # test-only, but production code continues after it, so resume counting.
        pending = 0
      }
    }

    {
      line = $0
      sub(/^[ \t]+/, "", line)
      if (line == "") next
      if (FILENAME ~ /\.rs$/  && line ~ /^\/\//) next
      if (FILENAME ~ /\.sh$/  && line ~ /^#/)    next
      if (FILENAME ~ /\.ts$/  && (line ~ /^\/\// || line ~ /^\*/ || line ~ /^\/\*/)) next
      count[FILENAME]++
    }
    END { for (f in count) printf "%d %s\n", count[f], f }
  ' "${files[@]}" | LC_ALL=C sort -k2,2
}

baseline_for() {
  # Baseline lines are "<count> <path>"; match the path field exactly.
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
    echo "# These numbers are a debt ledger, not a target. They should only ever go DOWN."
    echo "# A diff here that raises a number, or adds a row without a clear reason in the"
    echo "# PR description, is the ratchet slipping — see .loom/docs/file-size-policy.md."
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
# A ratchet that silently stops measuring is worse than no ratchet: it reports
# OK forever while the files it guards keep growing. This exercises the counting
# rules and all three verdicts against synthetic fixtures in a throwaway repo, so
# a future edit to the awk pass (or to the exemption list) fails loudly in CI
# rather than quietly disarming the gate. Mirrors the --self-test convention
# already used by check-docs-defaults-parity.sh.
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

  # 12 code lines, then comments, blanks and a large inline test module. Under a
  # threshold of 20 this must measure as 12, not as its 60-odd raw lines.
  {
    for i in $(seq 1 12); do echo "pub const C_$i: u8 = $i;"; done
    echo ""
    echo "// a comment"
    echo ""
    echo "   // an indented comment"
    echo "#[cfg(test)]"
    echo "#[allow("
    echo "    clippy::unwrap_used,"
    echo "    clippy::panic"
    echo ")]"
    echo "mod tests {"
    for i in $(seq 1 40); do echo "    pub const T_$i: u8 = $i;"; done
    echo "}"
  } > "$tmp/src/small.rs"

  # 30 code lines -> over a threshold of 20.
  for i in $(seq 1 30); do echo "pub const B_$i: u8 = $i;"; done > "$tmp/src/big.rs"

  # A test-only helper FN mid-file must not truncate the count: production code
  # continues below it. This is the role_runner.rs / work_finder.rs shape, which
  # an earlier revision undercounted by ~1300 lines.
  {
    for i in $(seq 1 10); do echo "pub const P_$i: u8 = $i;"; done
    echo "#[cfg(test)]"
    echo "fn helper() {}"
    for i in $(seq 1 10); do echo "pub const Q_$i: u8 = $i;"; done
  } > "$tmp/src/midhelper.rs"

  # Exemption fixtures: all are far over threshold and must never be measured.
  for i in $(seq 1 50); do echo "echo $i"; done > "$tmp/.loom/scripts/mirror.sh"
  for i in $(seq 1 50); do echo "echo $i"; done > "$tmp/defaults/hooks/guard-destructive-generic.sh"
  # An extracted Rust test module (#7718) -- test code, not production.
  mkdir -p "$tmp/src/big"
  for i in $(seq 1 60); do echo "pub const TT_$i: u8 = $i;"; done > "$tmp/src/big/tests.rs"

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

  # Counting: inline tests, comment-only lines and blanks are all excluded.
  out="$( (cd "$tmp" && $S $T --list) | awk '$2=="src/small.rs"{print $1}')"
  _expect "rust inline #[cfg(test)]/comments/blanks excluded" "12" "$out"

  # A mid-file #[cfg(test)] helper fn must not truncate: 20 consts + the fn.
  out="$( (cd "$tmp" && $S $T --list) | awk '$2=="src/midhelper.rs"{print $1}')"
  _expect "mid-file #[cfg(test)] helper fn does not truncate the count" "21" "$out"

  # Exemptions: neither fixture appears in the measured set at all.
  out="$( (cd "$tmp" && $S $T --list) | grep -c 'mirror\.sh\|guard-destructive-generic' || true)"
  _expect "installed-mirror and vendored paths not measured" "0" "$out"

  out="$( (cd "$tmp" && $S $T --list) | grep -c 'big/tests\.rs' || true)"
  _expect "extracted rust test modules not measured" "0" "$out"

  (cd "$tmp" && $S $T --update >/dev/null)

  # Assert membership, not a raw count: the fixture repo also contains the copy
  # of this script under scripts/, which is itself well over a threshold of 20.
  out="$(awk '$2=="src/big.rs"{print "y"}' "$tmp/scripts/file-size-baseline.txt")"
  _expect "baseline records the over-threshold file" "y" "$out"
  out="$(awk '$2=="src/small.rs"{print "y"}' "$tmp/scripts/file-size-baseline.txt")"
  _expect "baseline omits the under-threshold file" "" "$out"

  (cd "$tmp" && $S $T >/dev/null 2>&1) && out=0 || out=$?
  _expect "clean tree passes" "0" "$out"

  # Growth of a listed file fails.
  echo "pub const EXTRA: u8 = 0;" >> "$tmp/src/big.rs"
  (cd "$tmp" && $S $T >/dev/null 2>&1) && out=0 || out=$?
  _expect "growth of an over-threshold file fails" "1" "$out"

  # Shrinking below the recorded number passes.
  for i in $(seq 1 25); do echo "pub const B_$i: u8 = $i;"; done > "$tmp/src/big.rs"
  (cd "$tmp" && $S $T >/dev/null 2>&1) && out=0 || out=$?
  _expect "shrinking passes" "0" "$out"

  # A brand-new file crossing the threshold fails.
  for i in $(seq 1 30); do echo "pub const B_$i: u8 = $i;"; done > "$tmp/src/big.rs"
  for i in $(seq 1 40); do echo "pub const N_$i: u8 = $i;"; done > "$tmp/src/newbig.rs"
  git -C "$tmp" add -A >/dev/null
  (cd "$tmp" && $S $T >/dev/null 2>&1) && out=0 || out=$?
  _expect "new file crossing the threshold fails" "1" "$out"

  # Growing ONLY an inline test module is always allowed. (Truncating newbig.rs
  # rather than deleting it keeps this loop free of rm-on-a-variable, which the
  # repo's own destructive-command guard refuses to reason about.)
  : > "$tmp/src/newbig.rs"
  {
    echo "#[cfg(test)]"
    echo "mod more {"
    for i in $(seq 1 99); do echo "    pub const M_$i: u8 = 1;"; done
    echo "}"
  } >> "$tmp/src/big.rs"
  (cd "$tmp" && $S $T >/dev/null 2>&1) && out=0 || out=$?
  _expect "inline test-module growth is allowed" "0" "$out"

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
