#!/usr/bin/env bash
set -euo pipefail

# velocity.sh - Analyze project development velocity metrics
# Calculates lines of code per day, commits per day, and other productivity metrics

# Colors for output
BOLD='\033[1m'
BLUE='\033[0;34m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RESET='\033[0m'

echo -e "${BOLD}📊 Project Velocity Analysis${RESET}\n"

# Get first and last commit dates
FIRST_COMMIT=$(git log --reverse --format="%ai" | head -1)
LAST_COMMIT=$(git log --format="%ai" | head -1)
FIRST_DATE=$(echo "$FIRST_COMMIT" | cut -d' ' -f1)
LAST_DATE=$(echo "$LAST_COMMIT" | cut -d' ' -f1)

# Calculate duration in days
if [[ "$OSTYPE" == "darwin"* ]]; then
    # macOS
    FIRST_EPOCH=$(date -j -f "%Y-%m-%d" "$FIRST_DATE" +%s)
    LAST_EPOCH=$(date -j -f "%Y-%m-%d" "$LAST_DATE" +%s)
else
    # Linux
    FIRST_EPOCH=$(date -d "$FIRST_DATE" +%s)
    LAST_EPOCH=$(date -d "$LAST_DATE" +%s)
fi

DURATION_DAYS=$(( (LAST_EPOCH - FIRST_EPOCH) / 86400 + 1 ))

# Get total commits
TOTAL_COMMITS=$(git rev-list --count HEAD)

# Get code statistics using cloc
echo -e "${BLUE}Running cloc analysis...${RESET}\n"
CLOC_OUTPUT=$(cloc --vcs=git --quiet --csv .)

# Parse cloc CSV output
TOTAL_LINES=$(echo "$CLOC_OUTPUT" | grep "SUM" | cut -d',' -f5)

# Get language breakdown
MARKDOWN_LINES=$(echo "$CLOC_OUTPUT" | grep "^Markdown," | cut -d',' -f5 || echo "0")
TYPESCRIPT_LINES=$(echo "$CLOC_OUTPUT" | grep "^TypeScript," | cut -d',' -f5 || echo "0")
RUST_LINES=$(echo "$CLOC_OUTPUT" | grep "^Rust," | cut -d',' -f5 || echo "0")
YAML_LINES=$(echo "$CLOC_OUTPUT" | grep "^YAML," | cut -d',' -f5 || echo "0")
SHELL_LINES=$(echo "$CLOC_OUTPUT" | grep "^Bourne Shell," | cut -d',' -f5 || echo "0")
JSON_LINES=$(echo "$CLOC_OUTPUT" | grep "^JSON," | cut -d',' -f5 || echo "0")

# Calculate daily metrics
LINES_PER_DAY=$(( TOTAL_LINES / DURATION_DAYS ))
COMMITS_PER_DAY=$(echo "scale=1; $TOTAL_COMMITS / $DURATION_DAYS" | bc)

# Display results
echo -e "${BOLD}Project Timeline:${RESET}"
echo -e "  First commit: ${GREEN}$FIRST_DATE${RESET}"
echo -e "  Last commit:  ${GREEN}$LAST_DATE${RESET}"
echo -e "  Duration:     ${GREEN}$DURATION_DAYS days${RESET}"
echo ""

echo -e "${BOLD}Total Output:${RESET}"
echo -e "  Lines of code: ${GREEN}$(printf "%'d" $TOTAL_LINES)${RESET}"
echo -e "  Total commits: ${GREEN}$TOTAL_COMMITS${RESET}"
echo ""

echo -e "${BOLD}Velocity Metrics:${RESET}"
echo -e "  ${YELLOW}~$(printf "%'d" $LINES_PER_DAY) lines/day${RESET}"
echo -e "  ${YELLOW}~$COMMITS_PER_DAY commits/day${RESET}"
echo ""

echo -e "${BOLD}Code Distribution:${RESET}"
if [ "$MARKDOWN_LINES" != "0" ]; then
    MARKDOWN_PCT=$(echo "scale=1; $MARKDOWN_LINES * 100 / $TOTAL_LINES" | bc)
    echo -e "  Markdown:   $(printf "%'8d" $MARKDOWN_LINES) lines (${MARKDOWN_PCT}%)"
fi
if [ "$TYPESCRIPT_LINES" != "0" ]; then
    TYPESCRIPT_PCT=$(echo "scale=1; $TYPESCRIPT_LINES * 100 / $TOTAL_LINES" | bc)
    echo -e "  TypeScript: $(printf "%'8d" $TYPESCRIPT_LINES) lines (${TYPESCRIPT_PCT}%)"
fi
if [ "$RUST_LINES" != "0" ]; then
    RUST_PCT=$(echo "scale=1; $RUST_LINES * 100 / $TOTAL_LINES" | bc)
    echo -e "  Rust:       $(printf "%'8d" $RUST_LINES) lines (${RUST_PCT}%)"
fi
if [ "$YAML_LINES" != "0" ]; then
    YAML_PCT=$(echo "scale=1; $YAML_LINES * 100 / $TOTAL_LINES" | bc)
    echo -e "  YAML:       $(printf "%'8d" $YAML_LINES) lines (${YAML_PCT}%)"
fi
if [ "$SHELL_LINES" != "0" ]; then
    SHELL_PCT=$(echo "scale=1; $SHELL_LINES * 100 / $TOTAL_LINES" | bc)
    echo -e "  Shell:      $(printf "%'8d" $SHELL_LINES) lines (${SHELL_PCT}%)"
fi
if [ "$JSON_LINES" != "0" ]; then
    JSON_PCT=$(echo "scale=1; $JSON_LINES * 100 / $TOTAL_LINES" | bc)
    echo -e "  JSON:       $(printf "%'8d" $JSON_LINES) lines (${JSON_PCT}%)"
fi
echo ""

echo -e "${BOLD}Daily Breakdown (avg):${RESET}"
if [ "$MARKDOWN_LINES" != "0" ]; then
    echo -e "  Markdown:   ~$(( MARKDOWN_LINES / DURATION_DAYS )) lines/day"
fi
if [ "$TYPESCRIPT_LINES" != "0" ]; then
    echo -e "  TypeScript: ~$(( TYPESCRIPT_LINES / DURATION_DAYS )) lines/day"
fi
if [ "$RUST_LINES" != "0" ]; then
    echo -e "  Rust:       ~$(( RUST_LINES / DURATION_DAYS )) lines/day"
fi
if [ "$YAML_LINES" != "0" ]; then
    echo -e "  YAML:       ~$(( YAML_LINES / DURATION_DAYS )) lines/day"
fi
if [ "$SHELL_LINES" != "0" ]; then
    echo -e "  Shell:      ~$(( SHELL_LINES / DURATION_DAYS )) lines/day"
fi
if [ "$JSON_LINES" != "0" ]; then
    echo -e "  JSON:       ~$(( JSON_LINES / DURATION_DAYS )) lines/day"
fi
