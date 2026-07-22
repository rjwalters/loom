#!/usr/bin/env bash
# loom status - Display agent pool state
#
# Usage:
#   loom status                   Show running agents + configured-but-stopped agents
#   loom status --json            Machine-readable JSON output
#   loom status --help            Show help
#
# Reports the tmux agent pool spawned by `loom start`:
#   - Running `loom-*` tmux sessions on the `loom` socket (with pane PID).
#   - Cross-references each session against .loom/config.json .terminals[]
#     to report terminal id, name, and role file.
#   - Flags agents configured in .loom/config.json that are NOT running,
#     so a crashed / never-started agent is easy to spot.
#
# Exits 0 whether or not any agents are running (an empty pool is not an error).

set -euo pipefail

# Find repository root
find_repo_root() {
    local dir="$PWD"
    while [[ "$dir" != "/" ]]; do
        if [[ -d "$dir/.loom" ]]; then
            echo "$dir"
            return 0
        fi
        if [[ -f "$dir/.git" ]]; then
            local gitdir
            gitdir=$(sed 's/^gitdir: //' "$dir/.git")
            local main_repo
            main_repo=$(dirname "$(dirname "$(dirname "$gitdir")")")
            if [[ -d "$main_repo/.loom" ]]; then
                echo "$main_repo"
                return 0
            fi
        fi
        dir="$(dirname "$dir")"
    done
    echo ""
}

REPO_ROOT=$(find_repo_root)
if [[ -z "$REPO_ROOT" ]]; then
    echo "Error: Not in a Loom workspace (.loom directory not found)" >&2
    exit 1
fi

CONFIG_FILE="$REPO_ROOT/.loom/config.json"
TMUX_SOCKET="loom"

# ANSI colors
if [[ -t 1 ]]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    CYAN='\033[0;36m'
    GRAY='\033[0;90m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    RED=''
    GREEN=''
    YELLOW=''
    CYAN=''
    GRAY=''
    BOLD=''
    NC=''
fi

# Show help
show_help() {
    cat <<EOF
${BOLD}loom status - Display agent pool state${NC}

${YELLOW}USAGE:${NC}
    loom status                   Show running + configured-but-stopped agents
    loom status --json            Machine-readable JSON output
    loom status --help            Show this help

${YELLOW}OUTPUT:${NC}
    Running agents are the ${CYAN}loom-*${NC} tmux sessions on the ${CYAN}loom${NC} socket
    spawned by 'loom start'. Each is cross-referenced against
    .loom/config.json .terminals[] to show its id, name, and role file.
    Agents present in the config but not currently running are listed
    separately so a crashed or never-started agent is easy to spot.

${YELLOW}EXIT STATUS:${NC}
    Always 0 when the workspace resolves — an empty pool is not an error.

${YELLOW}RELATED COMMANDS:${NC}
    loom start       Spawn the agent pool from config
    loom attach <id> Attach to a running agent's tmux session
    loom logs <id>   Tail an agent's output
    loom stop        Graceful shutdown of the agent pool
EOF
}

# List running loom-* sessions on the loom socket (one per line, may be empty)
get_running_sessions() {
    command -v tmux &>/dev/null || return 0
    tmux -L "$TMUX_SOCKET" list-sessions -F "#{session_name}" 2>/dev/null \
        | grep "^loom-" || true
}

# Pane PID for a session (first pane); empty if unavailable
session_pid() {
    local session_name="$1"
    tmux -L "$TMUX_SOCKET" list-panes -t "$session_name" -F "#{pane_pid}" 2>/dev/null \
        | head -1 || true
}

# Read the configured terminals as compact JSON array (or "[]")
read_terminals() {
    if [[ -f "$CONFIG_FILE" ]] && command -v jq &>/dev/null; then
        jq -c '.terminals // []' "$CONFIG_FILE" 2>/dev/null || echo "[]"
    else
        echo "[]"
    fi
}

# ---- JSON output ---------------------------------------------------------
emit_json() {
    if ! command -v jq &>/dev/null; then
        echo '{"error":"jq not installed"}'
        return 0
    fi

    local running
    running=$(get_running_sessions)

    # Build a JSON array of running sessions with pids
    local running_json="[]"
    if [[ -n "$running" ]]; then
        local rows=""
        while IFS= read -r session; do
            [[ -z "$session" ]] && continue
            local id="${session#loom-}"
            local pid
            pid=$(session_pid "$session")
            rows+=$(jq -nc --arg session "$session" --arg id "$id" --arg pid "$pid" \
                '{session:$session, id:$id, pid:($pid|select(.!="")|tonumber?)}')
            rows+=$'\n'
        done <<< "$running"
        running_json=$(printf '%s' "$rows" | jq -sc '.')
    fi

    local terminals
    terminals=$(read_terminals)

    jq -nc \
        --argjson running "$running_json" \
        --argjson terminals "$terminals" \
        '
        ($running | map(.id)) as $running_ids
        | {
            running: ($terminals | map(
                . as $t
                | ($running[] | select(.id == $t.id)) as $r
                | {
                    id: $t.id,
                    name: ($t.name // $t.id),
                    role: ($t.roleConfig.roleFile // null),
                    session: $r.session,
                    pid: $r.pid,
                    status: "running"
                  }
              )),
            stopped: ($terminals | map(select(.id as $id | ($running_ids | index($id)) | not))
                | map({
                    id: .id,
                    name: (.name // .id),
                    role: (.roleConfig.roleFile // null),
                    status: "stopped"
                  })),
            unmanaged: ($running | map(select(.id as $rid
                | ($terminals | map(.id) | index($rid)) | not))
                | map({session: .session, id: .id, pid: .pid, status: "unmanaged"}))
          }'
}

# ---- Human-readable output ----------------------------------------------
emit_human() {
    local running running_ids=()
    running=$(get_running_sessions)

    echo -e "${BOLD}Loom Agent Pool${NC}"
    echo ""
    echo -e "  Workspace: ${CYAN}$REPO_ROOT${NC}"
    if [[ -f "$CONFIG_FILE" ]]; then
        echo -e "  Config:    ${CYAN}$CONFIG_FILE${NC}"
    else
        echo -e "  Config:    ${GRAY}(none — $CONFIG_FILE not found)${NC}"
    fi
    echo -e "  Socket:    ${CYAN}tmux -L $TMUX_SOCKET${NC}"
    echo ""

    if ! command -v tmux &>/dev/null; then
        echo -e "${YELLOW}tmux is not installed — cannot inspect the agent pool.${NC}"
        return 0
    fi

    # Collect running ids
    if [[ -n "$running" ]]; then
        while IFS= read -r session; do
            [[ -z "$session" ]] && continue
            running_ids+=("${session#loom-}")
        done <<< "$running"
    fi

    # Read config terminals for cross-referencing
    local terminals
    terminals=$(read_terminals)
    local terminal_count
    terminal_count=$(echo "$terminals" | jq 'length' 2>/dev/null || echo 0)

    # Running agents section
    if [[ ${#running_ids[@]} -eq 0 ]]; then
        echo -e "${YELLOW}No agents running.${NC}"
    else
        echo -e "${GREEN}Running agents (${#running_ids[@]}):${NC}"
        echo ""
        local session id name role pid
        while IFS= read -r session; do
            [[ -z "$session" ]] && continue
            id="${session#loom-}"
            pid=$(session_pid "$session")
            name=""
            role=""
            if [[ "$terminal_count" -gt 0 ]]; then
                name=$(echo "$terminals" | jq -r --arg id "$id" \
                    '.[] | select(.id == $id) | (.name // .id)' 2>/dev/null | head -1)
                role=$(echo "$terminals" | jq -r --arg id "$id" \
                    '.[] | select(.id == $id) | (.roleConfig.roleFile // "")' 2>/dev/null | head -1)
            fi
            if [[ -n "$name" ]]; then
                echo -e "  ${GREEN}●${NC} ${BOLD}$id${NC} ($name)"
            else
                echo -e "  ${GREEN}●${NC} ${BOLD}$id${NC} ${GRAY}(not in config — unmanaged)${NC}"
            fi
            echo -e "      session: ${CYAN}$session${NC}   pid: ${CYAN}${pid:-unknown}${NC}"
            [[ -n "$role" ]] && echo -e "      role:    ${CYAN}$role${NC}"
        done <<< "$running"
    fi

    # Configured-but-not-running section
    if [[ "$terminal_count" -gt 0 ]]; then
        local stopped
        stopped=$(echo "$terminals" | jq -r \
            --argjson running "$(printf '%s\n' "${running_ids[@]:-}" | jq -R . | jq -sc 'map(select(. != ""))')" \
            '.[] | select(.id as $id | ($running | index($id)) | not)
                | "\(.id)\t\((.name // .id))\t\((.roleConfig.roleFile // ""))"' 2>/dev/null || true)
        if [[ -n "$stopped" ]]; then
            echo ""
            echo -e "${YELLOW}Configured but not running:${NC}"
            echo ""
            while IFS=$'\t' read -r sid sname srole; do
                [[ -z "$sid" ]] && continue
                echo -e "  ${GRAY}○${NC} ${BOLD}$sid${NC} ($sname)${srole:+   role: $srole}"
            done <<< "$stopped"
        fi
    fi

    echo ""
}

# Main
main() {
    local json=false
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --help|-h)
                show_help
                exit 0
                ;;
            --json)
                json=true
                shift
                ;;
            -*)
                echo -e "${RED}Error: Unknown option '$1'${NC}" >&2
                echo "Use 'loom status --help' for usage" >&2
                exit 1
                ;;
            *)
                echo -e "${RED}Error: Unexpected argument '$1'${NC}" >&2
                exit 1
                ;;
        esac
    done

    if [[ "$json" == "true" ]]; then
        emit_json
    else
        emit_human
    fi
    exit 0
}

main "$@"
