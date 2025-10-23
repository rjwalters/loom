#!/usr/bin/env bash
# Wait for a Cargo build to complete by monitoring its output
# Usage: wait-for-cargo-build.sh <log-file> [timeout-seconds]
#
# Monitors a Cargo build log file and waits until compilation is complete.
# Detects completion by looking for "Finished" or "Running" messages.
# Detects errors by looking for compilation failures.

set -e

LOG_FILE="$1"
TIMEOUT="${2:-300}"  # Default 5 minutes

if [ -z "$LOG_FILE" ]; then
  echo "Usage: $0 <log-file> [timeout-seconds]"
  exit 1
fi

# Colors for output
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
GRAY='\033[0;90m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

# Spinner frames
SPINNER_FRAMES=("⠋" "⠙" "⠹" "⠸" "⠼" "⠴" "⠦" "⠧" "⠇" "⠏")
SPINNER_INDEX=0

START_TIME=$(date +%s)
LAST_PROGRESS=""
ELAPSED=0
SPIN_COUNT=0

# Function to show spinner with message
show_spinner() {
  local message="$1"
  SPINNER_INDEX=$(( (SPINNER_INDEX + 1) % ${#SPINNER_FRAMES[@]} ))
  echo -ne "\r${CYAN}${SPINNER_FRAMES[$SPINNER_INDEX]}${NC} ${message}${NC}"
}

echo -e "${GRAY}Waiting for Cargo build to complete...${NC}"

while [ $ELAPSED -lt $TIMEOUT ]; do
  # Check if log file exists
  if [ ! -f "$LOG_FILE" ]; then
    show_spinner "Waiting for build to start..."
    sleep 0.1
    CURRENT_TIME=$(date +%s)
    ELAPSED=$((CURRENT_TIME - START_TIME))
    continue
  fi

  # Check for completion markers
  if tail -10 "$LOG_FILE" 2>/dev/null | grep -q "Finished.*profile"; then
    echo -e "\r${GREEN}✓ Cargo build complete                                    ${NC}"
    exit 0
  fi

  if tail -10 "$LOG_FILE" 2>/dev/null | grep -q "Running.*target/debug"; then
    echo -e "\r${GREEN}✓ Binary is running                                        ${NC}"
    exit 0
  fi

  # Check for compilation errors
  if tail -20 "$LOG_FILE" 2>/dev/null | grep -q "error\[E[0-9]\+\]"; then
    echo -e "\r${RED}✗ Compilation error detected                               ${NC}"
    echo -e "${GRAY}Check log: $LOG_FILE${NC}"
    exit 1
  fi

  if tail -20 "$LOG_FILE" 2>/dev/null | grep -q "error: could not compile"; then
    echo -e "\r${RED}✗ Compilation failed                                       ${NC}"
    echo -e "${GRAY}Check log: $LOG_FILE${NC}"
    exit 1
  fi

  # Show current compilation progress
  CURRENT_PROGRESS=$(tail -5 "$LOG_FILE" 2>/dev/null | grep -o "Compiling [^ ]* v[^ ]*" | tail -1 || true)

  if [ -n "$CURRENT_PROGRESS" ] && [ "$CURRENT_PROGRESS" != "$LAST_PROGRESS" ]; then
    LAST_PROGRESS="$CURRENT_PROGRESS"
  fi

  # Calculate elapsed time
  CURRENT_TIME=$(date +%s)
  ELAPSED=$((CURRENT_TIME - START_TIME))

  # Build status message
  if [ -n "$CURRENT_PROGRESS" ]; then
    STATUS_MSG="Building: $CURRENT_PROGRESS (${ELAPSED}s)"
  else
    STATUS_MSG="Compiling... (${ELAPSED}s)"
  fi

  # Show spinner with current status
  show_spinner "$STATUS_MSG"

  sleep 0.1
  SPIN_COUNT=$((SPIN_COUNT + 1))
done

echo -e "\r${RED}✗ Timeout waiting for compilation (${TIMEOUT}s)              ${NC}"
echo -e "${GRAY}Check log: $LOG_FILE${NC}"
exit 1
