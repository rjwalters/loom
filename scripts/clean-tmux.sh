#!/bin/bash
# Kill all loom-* tmux sessions for clean development

echo "Killing all loom-* tmux sessions..."
tmux list-sessions 2>/dev/null | grep '^loom-' | cut -d: -f1 | while read session; do
  echo "  Killing: $session"
  tmux kill-session -t "$session" 2>/dev/null
done

echo "Done! All loom sessions cleaned."
