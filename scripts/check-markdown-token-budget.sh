#!/usr/bin/env bash
# check-markdown-token-budget.sh — ratchet agent-facing markdown so it stops
# growing, measured in tokens rather than lines (#7725, the markdown sibling
# of scripts/check-file-size-budget.sh — see #7716/#7711).
#
# Why: role prompts and slash-command bodies are prepended into an agent's
# context on every invocation, the same load-bearing category CLAUDE.md's own
# check-claude-md-budget.sh guards (#4014). Unlike CLAUDE.md there is no single
# 320-line budget to enforce — there are dozens of independently-growing files
# — so this mirrors check-file-size-budget.sh's ratchet instead: freeze each
# file at its CURRENT size, let it shrink, never let it grow. See
# .loom/docs/file-size-policy.md for the shared ratchet philosophy.
#
# Token proxy: no real LLM tokenizer is available in CI as of 2026-09-16, so
# tokens are approximated from raw byte count: tokens = ceil(bytes / 4). This
# is the commonly-cited rough heuristic for English prose (~4 bytes/token) —
# it is intentionally NOT exact, but it is monotonic in file size, which is
# all a growth ratchet needs: a file that gets bigger in bytes gets a bigger
# token estimate, and the gate only ever compares a file's estimate to its own
# prior estimate.
#
# Measured set: EXACTLY the markdown files that get inlined into an agent's
# context —
#   1. Every *.md under defaults/.claude/commands/loom/ (role prompts AND
#      slash-command bodies live here; this is the canonical 33-file set as of
#      #7725 — 15 with no defaults/roles/ counterpart, plus 18 that do).
#   2. Every *.md directly under defaults/roles/, EXCEPT:
#        - README.md (a docs index, not a role prompt — never inlined), and
#        - any entry that is a symlink (git mode 120000). Every non-README
#          entry under defaults/roles/ today IS such a symlink, pointing at
#          its real content in defaults/.claude/commands/loom/ — measuring the
#          symlink would double-count a file already measured in (1). This is
#          detected structurally (symlink or not), never via a hardcoded
#          path/name list, so a future real (non-symlink) file dropped
#          directly into defaults/roles/ is picked up automatically.
#
# Explicitly OUT of scope: defaults/docs/*.md and its .loom/docs/*.md install
# mirror (reference documentation, never inlined into a prompt), and every
# .loom/* installed mirror generally (resync copies of defaults/, measured at
# their defaults/ source — same reasoning check-file-size-budget.sh uses for
# .loom/hooks, .loom/scripts, .loom/docs).
#
# Usage:
#   check-markdown-token-budget.sh              Check the tree against the baseline.
#   check-markdown-token-budget.sh --update     Regenerate the baseline (see below).
#   check-markdown-token-budget.sh --list       Print every file's token estimate.
#   check-markdown-token-budget.sh --self-test  Verify the gate itself still works.
#   check-markdown-token-budget.sh --threshold N  Override the default threshold.
#   check-markdown-token-budget.sh --help
#
# The baseline (scripts/markdown-token-baseline.txt) records EVERY file in the
# measured set at its current size, unconditionally — unlike
# check-file-size-budget.sh's baseline (which only lists files already over
# threshold), there is no reason to filter here: the measured set is small and
# fully enumerable, so tracking all of it costs nothing and catches growth in
# every file, not just the biggest ones. --threshold only matters for a file
# that is NOT yet in the baseline (e.g. a brand new slash command added since
# the last --update) — such a file fails immediately if it is already over
# threshold, the same "newly crossed" case check-file-size-budget.sh has.
#
# --update is for two legitimate cases: (1) recording shrinkage after a real
# trim, so the ratchet tightens; (2) admitting a new file's current size after
# a deliberate, reviewed addition. It is NOT a way to silence a failure. A
# reviewer should treat an --update that raises an existing number as a red
# flag.
#
# Exit codes: 0 = within budget; 1 = a file grew or newly crossed; 2 = bad args.

set -euo pipefail

THRESHOLD_DEFAULT=3000
MODE="check"
THRESHOLD="${MARKDOWN_TOKEN_THRESHOLD:-$THRESHOLD_DEFAULT}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --update)    MODE="update"; shift ;;
    --list)      MODE="list"; shift ;;
    --self-test) MODE="self-test"; shift ;;
    --threshold) THRESHOLD="${2:?--threshold needs a value}"; shift 2 ;;
    --help|-h)   sed -n '2,68p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)           echo "check-markdown-token-budget: unknown argument '$1'" >&2; exit 2 ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ! ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)"; then
  ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
fi
cd "$ROOT"

BASELINE="$ROOT/scripts/markdown-token-baseline.txt"

# --- Exemptions -------------------------------------------------------------
#
# Belt-and-suspenders on top of the directory-scoped discovery below: even if
# discovery is ever widened, these must never be measured.
is_exempt() {
  case "$1" in
    .loom/*)             return 0 ;;  # installed mirror — measured at defaults/ source.
    defaults/docs/*)     return 0 ;;  # reference docs, never inlined into a prompt.
    defaults/roles/README.md) return 0 ;;  # docs index, not a role prompt.
  esac
  return 1
}

# --- Discovery ---------------------------------------------------------------
# Structural, not a hardcoded filename list: every *.md under
# defaults/.claude/commands/loom/, plus every *.md directly under
# defaults/roles/ whose git mode is NOT 120000 (symlink) and isn't README.md.
discover_files() {
  git ls-files -- 'defaults/.claude/commands/loom/*.md' | while IFS= read -r f; do
    is_exempt "$f" && continue
    printf '%s\n' "$f"
  done

  git ls-files -s -- 'defaults/roles/*.md' | while IFS=$' \t' read -r mode _hash _stage path; do
    [[ "$mode" == "120000" ]] && continue
    is_exempt "$path" && continue
    printf '%s\n' "$path"
  done
}

# --- Measurement --------------------------------------------------------------
# tokens = ceil(bytes / 4). See header comment for why this proxy is
# acceptable for a growth ratchet even though it is not a real tokenizer.
measure_all() {
  local files=()
  while IFS= read -r f; do
    [[ -f "$f" ]] || continue
    files+=("$f")
  done < <(discover_files | LC_ALL=C sort -u)

  [[ ${#files[@]} -eq 0 ]] && return 0

  local f bytes tokens
  for f in "${files[@]}"; do
    bytes="$(wc -c < "$f" | tr -d '[:space:]')"
    tokens=$(( (bytes + 3) / 4 ))
    printf '%d %s\n' "$tokens" "$f"
  done | LC_ALL=C sort -k2,2
}

baseline_for() {
  awk -v p="$1" '$1 !~ /^#/ && $2 == p { print $1; found = 1; exit } END { if (!found) print "" }' "$BASELINE"
}

write_baseline() {
  {
    echo "# markdown-token-baseline.txt — generated by scripts/check-markdown-token-budget.sh --update"
    echo "#"
    echo "# Every file in the measured set (see check-markdown-token-budget.sh header),"
    echo "# with its estimated token count (ceil(bytes/4)) at the time of recording."
    echo "# check-markdown-token-budget.sh fails when a listed file GROWS past its"
    echo "# recorded number, or when a file not yet listed is already over the"
    echo "# --threshold default. Shrinking is always allowed."
    echo "#"
    echo "# This is a debt ledger, not a target. The numbers should only ever go DOWN."
    echo "# A diff that raises a number needs a clear reason in the PR description —"
    echo "# see .loom/docs/file-size-policy.md."
    echo "#"
    echo "# <token-estimate> <path>"
    measure_all
  } > "$BASELINE.tmp"
  mv "$BASELINE.tmp" "$BASELINE"
}

# --- Self-test ---------------------------------------------------------------
# Exercises discovery (dedup of the defaults/roles/ symlink vs. its
# defaults/.claude/commands/loom/ target, README.md exclusion, .loom/ and
# defaults/docs/ exemption) and all three verdicts against synthetic fixtures
# in a throwaway repo. Mirrors check-file-size-budget.sh --self-test.
self_test() {
  local tmp rc=0 out S T
  tmp="$(mktemp -d)"
  trap 'cleanup_self_test "$tmp"' RETURN

  mkdir -p "$tmp/scripts" \
    "$tmp/defaults/.claude/commands/loom" \
    "$tmp/defaults/roles" \
    "$tmp/defaults/docs" \
    "$tmp/.loom/docs" \
    "$tmp/.loom/roles"
  cp "${BASH_SOURCE[0]}" "$tmp/scripts/check-markdown-token-budget.sh"
  chmod +x "$tmp/scripts/check-markdown-token-budget.sh"
  git -C "$tmp" init -q
  git -C "$tmp" config user.email t@t.test
  git -C "$tmp" config user.name t

  # A command file with a defaults/roles/ symlink counterpart — must be
  # measured exactly once, via the commands-dir target.
  printf '%s' "$(head -c 80 /dev/zero | tr '\0' 'a')" > "$tmp/defaults/.claude/commands/loom/builder.md"
  ln -s ../.claude/commands/loom/builder.md "$tmp/defaults/roles/builder.md"

  # A command file with NO defaults/roles/ counterpart.
  printf '%s' "$(head -c 40 /dev/zero | tr '\0' 'b')" > "$tmp/defaults/.claude/commands/loom/sweep.md"

  # roles/README.md — must be excluded even though it is a real (non-symlink) file.
  printf '%s' "$(head -c 5000 /dev/zero | tr '\0' 'r')" > "$tmp/defaults/roles/README.md"

  # Out-of-scope reference doc and its installed mirror — must never be measured.
  printf '%s' "$(head -c 5000 /dev/zero | tr '\0' 'd')" > "$tmp/defaults/docs/troubleshooting.md"
  printf '%s' "$(head -c 5000 /dev/zero | tr '\0' 'd')" > "$tmp/.loom/docs/troubleshooting.md"
  printf '%s' "$(head -c 5000 /dev/zero | tr '\0' 'x')" > "$tmp/.loom/roles/builder.md"

  git -C "$tmp" add -A >/dev/null
  git -C "$tmp" commit -qm fixtures

  S="$tmp/scripts/check-markdown-token-budget.sh"
  T="--threshold 30"

  _expect() {
    local label="$1" want="$2" got="$3"
    if [[ "$want" == "$got" ]]; then
      echo "  ok   $label"
    else
      echo "  FAIL $label (want $want, got $got)" >&2
      rc=1
    fi
  }

  out="$( (cd "$tmp" && $S $T --list) | grep -c 'defaults/roles/builder.md' || true)"
  _expect "roles/ symlink counterpart not measured directly" "0" "$out"

  out="$( (cd "$tmp" && $S $T --list) | grep -c 'commands/loom/builder.md' || true)"
  _expect "symlinked file measured exactly once via its command-dir target" "1" "$out"

  out="$( (cd "$tmp" && $S $T --list) | grep -c 'roles/README.md' || true)"
  _expect "roles/README.md excluded" "0" "$out"

  out="$( (cd "$tmp" && $S $T --list) | grep -c 'defaults/docs/\|\.loom/' || true)"
  _expect "defaults/docs and .loom mirrors excluded" "0" "$out"

  out="$( (cd "$tmp" && $S $T --list) | wc -l | tr -d '[:space:]')"
  _expect "exactly two files in the measured set" "2" "$out"

  out="$( (cd "$tmp" && $S $T --list) | awk '$2 ~ /commands\/loom\/builder\.md$/{print $1}')"
  _expect "token estimate is ceil(bytes/4)" "20" "$out"

  (cd "$tmp" && $S $T --update >/dev/null)
  out="$(grep -c 'commands/loom/builder\.md' "$tmp/scripts/markdown-token-baseline.txt" || true)"
  _expect "baseline records the file unconditionally (not threshold-filtered)" "1" "$out"
  out="$(grep -c 'commands/loom/sweep\.md' "$tmp/scripts/markdown-token-baseline.txt" || true)"
  _expect "baseline records the small file too" "1" "$out"

  (cd "$tmp" && $S $T >/dev/null 2>&1) && out=0 || out=$?
  _expect "clean tree passes (within-budget)" "0" "$out"

  printf '%s' "$(head -c 200 /dev/zero | tr '\0' 'a')" > "$tmp/defaults/.claude/commands/loom/builder.md"
  (cd "$tmp" && $S $T >/dev/null 2>&1) && out=0 || out=$?
  _expect "growth of a baselined file fails" "1" "$out"

  printf '%s' "$(head -c 40 /dev/zero | tr '\0' 'a')" > "$tmp/defaults/.claude/commands/loom/builder.md"
  (cd "$tmp" && $S $T >/dev/null 2>&1) && out=0 || out=$?
  _expect "shrinking a baselined file passes" "0" "$out"

  printf '%s' "$(head -c 500 /dev/zero | tr '\0' 'n')" > "$tmp/defaults/.claude/commands/loom/imagine.md"
  git -C "$tmp" add -A >/dev/null
  (cd "$tmp" && $S $T >/dev/null 2>&1) && out=0 || out=$?
  _expect "new file crossing the threshold fails" "1" "$out"

  if [[ $rc -eq 0 ]]; then
    echo "check-markdown-token-budget --self-test: all checks passed."
  else
    echo "check-markdown-token-budget --self-test: FAILURES above." >&2
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
    echo "check-markdown-token-budget: baseline regenerated — $n file(s) tracked."
    echo "Review the diff: numbers should only go DOWN."
    exit 0
    ;;
esac

if [[ ! -f "$BASELINE" ]]; then
  echo "check-markdown-token-budget: no baseline at $BASELINE." >&2
  echo "Create it once with: scripts/check-markdown-token-budget.sh --update" >&2
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
    echo "check-markdown-token-budget: FAIL"
    echo ""
    if (( ${#grew[@]} > 0 )); then
      echo "These files are already in the baseline and GREW (estimated tokens):"
      for e in "${grew[@]}"; do
        IFS='|' read -r f b n <<< "$e"
        printf '  %-58s %s -> %s  (+%s)\n' "$f" "$b" "$n" "$((n - b))"
      done
      echo ""
    fi
    if (( ${#crossed[@]} > 0 )); then
      echo "These new files are already over the ${THRESHOLD}-token threshold:"
      for e in "${crossed[@]}"; do
        IFS='|' read -r f n <<< "$e"
        printf '  %-58s %s tokens (estimated)\n' "$f" "$n"
      done
      echo ""
    fi
    echo "A tracked markdown file is frozen at its current size: it may shrink, not grow."
    echo "To land this change:"
    echo "  - Split the addition into a new sibling doc, cross-linked from the original."
    echo "  - Or remove at least as much as you added from the same file."
    echo ""
    echo "If this change legitimately trims content, or deliberately adds a new file,"
    echo "run --update and say so in the PR description."
    echo ""
    echo "Do NOT hand-edit the baseline upward — that is the ratchet slipping. See"
    echo ".loom/docs/file-size-policy.md."
  } >&2
  exit 1
fi

msg="check-markdown-token-budget: OK — no tracked file grew (threshold $THRESHOLD, token estimate = ceil(bytes/4))."
if (( ${#shrank[@]} > 0 )); then
  msg="$msg ${#shrank[@]} file(s) shrank; run --update to tighten the ratchet."
fi
echo "$msg"
exit 0
