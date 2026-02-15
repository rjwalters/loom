#!/bin/bash
# Kill all loom-* tmux sessions for clean development
# Uses the loom tmux socket (-L loom) where daemon sessions live

# Source the process tree kill helper
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=../defaults/scripts/kill-session-tree.sh
source "$REPO_ROOT/defaults/scripts/kill-session-tree.sh"

echo "Killing all loom-* tmux sessions..."
tmux -L loom list-sessions -F '#{session_name}' 2>/dev/null | grep '^loom-' | while read -r session; do
  echo "  Killing: $session"
  kill_session_tree "$session" "--force" "loom"
done

# Sweep for any orphaned claude processes
sweep_orphaned_claude_processes "--force"

echo "Done! All loom sessions cleaned."
