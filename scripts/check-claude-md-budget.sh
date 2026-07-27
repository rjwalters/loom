#!/usr/bin/env bash
# check-claude-md-budget.sh — fail if CLAUDE.md exceeds its line budget.
#
# Why (#4014): CLAUDE.md is prepended to the context of every session, every
# worker role, and every sweep child — a fixed per-dispatch tax paid thousands of
# times a day. It grows monotonically because every individual addition is
# defensible while the aggregate is the problem, and nothing removes anything.
# This check is the counter-pressure: a new subsystem's inline detail now forces
# a corresponding removal (move it to `.loom/docs/` and leave a one-line pointer)
# rather than silently ratcheting the file — and the whole context — larger.
#
# The rule this enforces: durable OPERATING instructions live in CLAUDE.md;
# REFERENCE material lives in `.loom/docs/*` with a pointer; completed-migration
# HISTORY lives in `docs/migration/` + ADRs. If you are over budget, relocate —
# do not raise the budget to fit a reference dump.
#
# Usage:
#   check-claude-md-budget.sh [BUDGET]
#     BUDGET  Max allowed line count (default 320). Override for a one-off check.
#
# Exit codes: 0 = within budget; 1 = over budget (overage printed to stderr).

set -euo pipefail

BUDGET="${1:-${CLAUDE_MD_BUDGET:-320}}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)"; then
  :
else
  ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
fi

TARGET="$ROOT/CLAUDE.md"

if [[ ! -f "$TARGET" ]]; then
  echo "check-claude-md-budget: no CLAUDE.md under $ROOT — nothing to check (ok)."
  exit 0
fi

lines="$(wc -l < "$TARGET" | tr -d '[:space:]')"

if [[ "$lines" -gt "$BUDGET" ]]; then
  over=$((lines - BUDGET))
  {
    echo "check-claude-md-budget: FAIL — CLAUDE.md is $lines lines, $over over the $BUDGET-line budget."
    echo ""
    echo "CLAUDE.md is prepended to every agent's context on every dispatch, so its"
    echo "size is a fixed tax paid thousands of times a day. Keep only durable"
    echo "OPERATING instructions here. To get back under budget:"
    echo "  - Move REFERENCE detail (config tables, subsystem internals, tool"
    echo "    catalogs) to .loom/docs/*.md and leave a one-line pointer."
    echo "  - Retire completed-migration HISTORY to docs/migration/ + ADRs."
    echo "Do NOT raise the budget to fit a reference dump."
  } >&2
  exit 1
fi

echo "check-claude-md-budget: OK — CLAUDE.md is $lines lines (budget $BUDGET)."
exit 0
