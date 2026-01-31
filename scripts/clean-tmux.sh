#!/bin/bash
# Kill all loom-* tmux sessions for clean development
# Uses the loom tmux socket (-L loom) where daemon sessions live

echo "Killing all loom-* tmux sessions..."
tmux -L loom list-sessions -F '#{session_name}' 2>/dev/null | grep '^loom-' | while read -r session; do
  echo "  Killing: $session"
  tmux -L loom kill-session -t "$session" 2>/dev/null
done

echo "Done! All loom sessions cleaned."
