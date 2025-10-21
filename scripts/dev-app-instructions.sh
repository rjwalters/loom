#!/usr/bin/env bash
# Instructions for app:dev - explains why it needs to be run in a real terminal

cat << 'EOF'
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  Loom Development Mode - Manual Setup Required
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

ℹ️  The `pnpm app:dev` command requires a real terminal (TTY) to create
   a tmux split-screen session for development.

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

🎯 RECOMMENDED APPROACH - Two Separate Terminals:

   Terminal 1 (Daemon Monitoring):
   $ cd /Users/rwalters/GitHub/loom
   $ pnpm daemon:dev

   Terminal 2 (Tauri App):
   $ cd /Users/rwalters/GitHub/loom
   $ pnpm tauri dev

   This gives you:
   ✅ Hot reload for frontend changes
   ✅ Interactive daemon monitoring
   ✅ Easy to automate from Claude Code

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

⚡ ALTERNATIVE - Use Preview Mode:

   $ pnpm app:preview

   This builds once and runs without hot reload.
   Recommended when agents work on Loom's codebase (prevents restart loops).

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

🤖 FOR CLAUDE CODE AUTOMATION:

   Use the two-terminal approach with Loom's MCP terminal tools:

   1. Start daemon in terminal-1:
      mcp__loom-terminals__send_terminal_input \
        --terminal-id=terminal-1 \
        --input="cd ~/GitHub/loom && pnpm daemon:dev\n"

   2. Start Tauri in terminal-2 (after 5 seconds):
      mcp__loom-terminals__send_terminal_input \
        --terminal-id=terminal-2 \
        --input="cd ~/GitHub/loom && pnpm tauri dev\n"

   3. Monitor both terminals:
      mcp__loom-terminals__get_terminal_output --terminal-id=terminal-1
      mcp__loom-terminals__get_terminal_output --terminal-2

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
EOF
