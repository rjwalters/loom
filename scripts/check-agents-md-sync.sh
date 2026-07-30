#!/usr/bin/env bash
# check-agents-md-sync.sh — fail if defaults/.loom/AGENTS.md is stale.
#
# Why (#4479, epic #4167 pillar 3): AGENTS.md is the cross-runtime instruction
# anchor, but its content is NOT hand-maintained — it is generated from the
# `agents-md:include` marker ranges in defaults/.loom/CLAUDE.md by
# defaults/scripts/generate-agents-md.sh. The generated file is checked in (same
# posture as defaults/.loom/CLAUDE.md itself), so it can silently drift from its
# source if someone edits the marked ranges without regenerating. This check is
# the counter-pressure: it re-runs the generator and fails if the checked-in
# artifact differs, keeping instructions single-source (the issue's own
# non-goal: "no hand-maintained AGENTS.md").
#
# This mirrors scripts/check-claude-md-budget.sh's pattern (dedicated script,
# wired into .github/workflows/ci.yml) but checks CONTENT SYNC rather than a
# line budget.
#
# Usage:
#   check-agents-md-sync.sh
#
# Exit codes: 0 = in sync; 1 = stale (diff printed to stderr) or missing inputs.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)"; then
  :
else
  ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
fi

GENERATOR="$ROOT/defaults/scripts/generate-agents-md.sh"
CHECKED_IN="$ROOT/defaults/.loom/AGENTS.md"

if [[ ! -f "$GENERATOR" ]]; then
  echo "check-agents-md-sync: generator not found: $GENERATOR" >&2
  exit 1
fi

if [[ ! -f "$CHECKED_IN" ]]; then
  {
    echo "check-agents-md-sync: FAIL — $CHECKED_IN does not exist."
    echo ""
    echo "Generate it and commit the result:"
    echo "  bash defaults/scripts/generate-agents-md.sh"
  } >&2
  exit 1
fi

# Re-generate to stdout and compare against the checked-in artifact. Using
# --stdout avoids writing any file (no temp-file / worktree-write concerns).
if diff -u "$CHECKED_IN" <(bash "$GENERATOR" --stdout); then
  echo "check-agents-md-sync: OK — defaults/.loom/AGENTS.md is in sync with defaults/.loom/CLAUDE.md."
  exit 0
fi

{
  echo ""
  echo "check-agents-md-sync: FAIL — defaults/.loom/AGENTS.md is stale."
  echo ""
  echo "defaults/.loom/AGENTS.md is generated from the agents-md:include marker"
  echo "ranges in defaults/.loom/CLAUDE.md — it must never be hand-edited. The"
  echo "checked-in file above differs from freshly generated output (diff shown)."
  echo "Regenerate and commit the result:"
  echo "  bash defaults/scripts/generate-agents-md.sh"
  echo "  git add defaults/.loom/AGENTS.md && git commit"
} >&2
exit 1
