#!/usr/bin/env bash
# Development mode using tmux split for daemon + Tauri
# Creates a tmux session with daemon monitoring in top pane, Tauri in bottom pane

set -e

SESSION_NAME="loom-dev"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color
BOLD='\033[1m'

# Check if we're in an interactive terminal with a TTY
if [ ! -t 0 ] && [ ! -t 1 ]; then
    # Non-interactive - show instructions instead
    ./scripts/dev-app-instructions.sh
    exit 0
fi

# Check if tmux is available
if ! command -v tmux &> /dev/null; then
    echo -e "${RED}Error: tmux is not installed${NC}"
    echo "Install with: brew install tmux"
    echo ""
    echo -e "${YELLOW}Alternative: Run in two separate terminals:${NC}"
    echo "  Terminal 1: pnpm daemon:dev"
    echo "  Terminal 2: pnpm tauri dev"
    exit 1
fi

# Check if session already exists
if tmux has-session -t "$SESSION_NAME" 2>/dev/null; then
    echo -e "${YELLOW}⚠ Development session already running${NC}"
    echo -e "${CYAN}Attaching to existing session...${NC}"
    tmux attach-session -t "$SESSION_NAME"
    exit 0
fi

echo -e "${BOLD}${CYAN}╔════════════════════════════════════════╗${NC}"
echo -e "${BOLD}${CYAN}║   Loom Development Mode (tmux split)  ║${NC}"
echo -e "${BOLD}${CYAN}╚════════════════════════════════════════╝${NC}"
echo ""
echo -e "${BLUE}Creating tmux session: $SESSION_NAME${NC}"
echo -e "${BLUE}  Top pane: Daemon monitoring${NC}"
echo -e "${BLUE}  Bottom pane: Tauri dev server${NC}"
echo ""
echo -e "${CYAN}Keyboard shortcuts:${NC}"
echo -e "  Ctrl+B then ↑/↓  - Switch between panes"
echo -e "  Ctrl+B then [    - Scroll mode (q to exit)"
echo -e "  Ctrl+B then d    - Detach (leave running)"
echo -e "  Ctrl+C twice     - Stop both processes"
echo ""
echo -e "${GREEN}Starting in 2 seconds...${NC}"
sleep 2

# Setup MCP first
./scripts/setup-mcp.sh > /dev/null

# Get absolute path
WORKSPACE="$(pwd)"

# Create new session with daemon in first window
# Use -d to start detached
tmux new-session -d -s "$SESSION_NAME" -n "loom"

# Send daemon command to first pane
tmux send-keys -t "$SESSION_NAME:0.0" "cd '$WORKSPACE' && pnpm daemon:dev" C-m

# Split horizontally and create bottom pane
tmux split-window -v -t "$SESSION_NAME:0"

# Send Tauri command to second pane after a delay
tmux send-keys -t "$SESSION_NAME:0.1" "cd '$WORKSPACE' && sleep 5 && pnpm tauri dev" C-m

# Adjust pane sizes (60% top daemon, 40% bottom Tauri)
tmux resize-pane -t "$SESSION_NAME:0.0" -y 60%

# Select the top pane (daemon) by default for monitoring
tmux select-pane -t "$SESSION_NAME:0.0"

# Check if we're in an interactive terminal
if [ -t 1 ]; then
    # Interactive mode - attach to the session
    echo -e "${GREEN}Attaching to session...${NC}"
    tmux attach-session -t "$SESSION_NAME"
else
    # Non-interactive mode (e.g., run from Claude Code) - leave detached
    echo -e "${GREEN}✓ Development session started in background${NC}"
    echo -e "${CYAN}Session name: $SESSION_NAME${NC}"
    echo ""
    echo -e "${YELLOW}To attach to the session, run:${NC}"
    echo -e "  ${BOLD}tmux attach-session -t $SESSION_NAME${NC}"
    echo ""
    echo -e "${YELLOW}To view session status:${NC}"
    echo -e "  ${BOLD}tmux list-sessions${NC}"
    echo ""
    echo -e "${YELLOW}To stop the session:${NC}"
    echo -e "  ${BOLD}tmux kill-session -t $SESSION_NAME${NC}"
    echo -e "  ${BOLD}# or use: pnpm app:quit${NC}"
fi
