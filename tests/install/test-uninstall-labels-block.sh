#!/usr/bin/env bash
# Test suite for uninstall-loom.sh Step 6 ".github/labels.yml" smart removal (#4187).
#
# Usage: ./tests/install/test-uninstall-labels-block.sh
#
# labels.yml is co-owned: Loom ships its workflow labels wrapped in
# `# BEGIN LOOM LABELS` / `# END LOOM LABELS` markers, while a consumer may add
# their own labels to the same file. Uninstall must:
#
#   1. Mixed file      -> strip ONLY the marked range; consumer labels survive.
#   2. Pure-Loom file  -> nothing meaningful remains outside the block -> delete.
#   3. Markerless file -> delete ONLY if byte-identical to the shipped template;
#                         otherwise preserve (it may hold consumer labels).
#   4. Malformed markers -> preserve (never guess a splice range).
#
# The functional cases below replay the EXACT sed/awk/predicate pipeline the
# script uses (extracted here) on fixtures, and source-anchor asserts pin that
# the script (a) no longer hard-deletes labels.yml and (b) routes it to smart
# removal. Exit 0 = all pass, 1 = failures.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
UNINSTALL_SH="$REPO_ROOT/scripts/uninstall-loom.sh"
SHIPPED_TEMPLATE="$REPO_ROOT/defaults/.github/labels.yml"

PASS=0
FAIL=0
TOTAL=0
RED='\033[0;31m'; GREEN='\033[0;32m'; NC='\033[0m'

assert_eq() {
  local desc="$1" expected="$2" actual="$3"
  TOTAL=$((TOTAL + 1))
  if [[ "$expected" == "$actual" ]]; then
    echo -e "${GREEN}PASS${NC}: $desc"; PASS=$((PASS + 1))
  else
    echo -e "${RED}FAIL${NC}: $desc"; echo "  expected: '$expected'"; echo "  actual:   '$actual'"
    FAIL=$((FAIL + 1))
  fi
}

# Exact marker-strip pipeline mirrored from uninstall-loom.sh Step 6. Given a
# labels.yml path, either strips the Loom block (deleting the file when nothing
# meaningful remains) or preserves it, and echoes the outcome:
#   "removed" | "block-stripped" | "preserved"
strip_labels_block() {
  local f="$1"
  local begin end
  begin=$(grep -c '^[[:space:]]*# BEGIN LOOM LABELS[[:space:]]*$' "$f" 2>/dev/null || true)
  end=$(grep -c '^[[:space:]]*# END LOOM LABELS[[:space:]]*$' "$f" 2>/dev/null || true)

  if [[ "$begin" == "1" && "$end" == "1" ]] && \
     awk '/^[[:space:]]*# BEGIN LOOM LABELS[[:space:]]*$/ { b = NR }
          /^[[:space:]]*# END LOOM LABELS[[:space:]]*$/   { e = NR }
          END { exit !(b > 0 && e > 0 && b < e) }' "$f"; then
    sed '/^[[:space:]]*# BEGIN LOOM LABELS[[:space:]]*$/,/^[[:space:]]*# END LOOM LABELS[[:space:]]*$/d' \
      "$f" > "${f}.tmp" && mv "${f}.tmp" "$f"
    awk 'NF || prev_blank++ < 1 { print; if (NF) prev_blank=0 }' "$f" > "${f}.tmp" && mv "${f}.tmp" "$f"
    if [[ ! -s "$f" ]] || ! grep -qE '^[[:space:]]*-[[:space:]]*name:' "$f" 2>/dev/null; then
      rm -f "$f"; echo "removed"
    else
      echo "block-stripped"
    fi
  else
    if [[ "$begin" == "0" && "$end" == "0" ]] && cmp -s "$f" "$SHIPPED_TEMPLATE"; then
      rm -f "$f"; echo "removed"
    else
      echo "preserved"
    fi
  fi
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo ""
echo "=== Functional: labels.yml marker-block smart removal ==="

# Case 1: mixed file — Loom block plus a consumer label outside it.
MIXED="$TMP/mixed.yml"
printf '# BEGIN LOOM LABELS\n- name: loom:issue\n  color: "3B82F6"\n# END LOOM LABELS\n\n- name: team:frontend\n  color: "00ff00"\n' > "$MIXED"
OUT="$(strip_labels_block "$MIXED")"
assert_eq "mixed file: block stripped, file kept" "block-stripped" "$OUT"
assert_eq "mixed file: consumer label survives" "yes" \
  "$([[ -f "$MIXED" ]] && grep -q 'team:frontend' "$MIXED" && echo yes || echo no)"
assert_eq "mixed file: Loom label gone" "yes" \
  "$([[ -f "$MIXED" ]] && ! grep -q 'loom:issue' "$MIXED" && echo yes || echo no)"

# Case 2: pure-Loom file — only the block, nothing outside.
PURE="$TMP/pure.yml"
printf '# BEGIN LOOM LABELS\n- name: loom:issue\n  color: "3B82F6"\n# END LOOM LABELS\n' > "$PURE"
OUT="$(strip_labels_block "$PURE")"
assert_eq "pure-Loom file: deleted" "removed" "$OUT"
assert_eq "pure-Loom file: no longer exists" "gone" "$([[ -f "$PURE" ]] && echo present || echo gone)"

# Case 3a: markerless file byte-identical to a shipped template -> deleted.
# Point SHIPPED_TEMPLATE at a deterministic markerless template equal to the
# fixture so the cmp -s byte-match branch fires (the real defaults file has
# markers, so it would never byte-match a legacy markerless snapshot).
LEGACY_MATCH="$TMP/legacy-match.yml"
printf -- '- name: loom:issue\n  color: "3B82F6"\n' > "$LEGACY_MATCH"
LEGACY_TEMPLATE_BAK="$SHIPPED_TEMPLATE"
SHIPPED_TEMPLATE="$TMP/tpl.yml"; cp "$LEGACY_MATCH" "$SHIPPED_TEMPLATE"
OUT="$(strip_labels_block "$LEGACY_MATCH")"
assert_eq "markerless byte-identical-to-template: deleted" "removed" "$OUT"
SHIPPED_TEMPLATE="$LEGACY_TEMPLATE_BAK"

# Case 3b: markerless file that differs from the template -> preserved.
LEGACY_DIVERGED="$TMP/legacy-diverged.yml"
printf -- '- name: team:only\n  color: "abcdef"\n  description: consumer-authored\n' > "$LEGACY_DIVERGED"
OUT="$(strip_labels_block "$LEGACY_DIVERGED")"
assert_eq "markerless diverged file: preserved" "preserved" "$OUT"
assert_eq "markerless diverged file: still present with consumer label" "yes" \
  "$([[ -f "$LEGACY_DIVERGED" ]] && grep -q 'team:only' "$LEGACY_DIVERGED" && echo yes || echo no)"

# Case 4: malformed markers (two BEGINs) -> preserved (never guess a range).
MALFORMED="$TMP/malformed.yml"
printf '# BEGIN LOOM LABELS\n# BEGIN LOOM LABELS\n- name: loom:issue\n# END LOOM LABELS\n' > "$MALFORMED"
OUT="$(strip_labels_block "$MALFORMED")"
assert_eq "malformed markers: preserved" "preserved" "$OUT"
assert_eq "malformed markers: file untouched" "yes" \
  "$([[ -f "$MALFORMED" ]] && grep -q 'loom:issue' "$MALFORMED" && echo yes || echo no)"

echo ""
echo "=== Source anchors: uninstall-loom.sh routes labels.yml to smart removal ==="

assert_eq "registers .github/labels.yml for smart removal" "yes" \
  "$(grep -q 'SMART_REMOVE_FILES+=(".github/labels.yml")' "$UNINSTALL_SH" && echo yes || echo no)"
assert_eq "manifest path skips labels.yml hard-delete" "yes" \
  "$(grep -q 'file_path" == ".github/labels.yml"' "$UNINSTALL_SH" && echo yes || echo no)"
assert_eq "Step 6 strips BEGIN..END LOOM LABELS range" "yes" \
  "$(grep -q 'BEGIN LOOM LABELS\[\[:space:\]\]\*\$/,/\^\[\[:space:\]\]\*# END LOOM LABELS' "$UNINSTALL_SH" && echo yes || echo no)"

echo ""
echo "=== Shipped template ships the markers (parity) ==="
assert_eq "defaults template has BEGIN marker" "yes" \
  "$(grep -qx '# BEGIN LOOM LABELS' "$SHIPPED_TEMPLATE" && echo yes || echo no)"
assert_eq "defaults template has END marker" "yes" \
  "$(grep -qx '# END LOOM LABELS' "$SHIPPED_TEMPLATE" && echo yes || echo no)"
assert_eq "root registry has BEGIN marker" "yes" \
  "$(grep -qx '# BEGIN LOOM LABELS' "$REPO_ROOT/.github/labels.yml" && echo yes || echo no)"

echo ""
echo "==================================================="
echo "Results: $PASS/$TOTAL passed, $FAIL failed"
echo "==================================================="
[[ "$FAIL" -eq 0 ]]
