#!/usr/bin/env bash
# FROZEN COPY of worktree.sh's shared-artifact symlink provisioning as it stood
# immediately before #8195 slice 4 ported it to Rust.
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# It exists so `tests/worktree_link_differential.rs` can keep comparing the
# Rust against the exact implementation it replaced, forever, rather than only
# at the moment of the port. Reading the block out of the live worktree.sh
# stopped being possible the instant that file started delegating, and reading
# it from git history would pin the test to a moving ref.
#
# WHAT IS FROZEN AND WHAT IS NOT
#
# The four link families, the info/exclude helper and the `find` invocation are
# byte-for-byte the retired code, variable names and all. Two things are NOT
# frozen, and the test's comparison is scoped accordingly:
#
#   * `print_info`/`print_success`/`print_warning` are re-declared here rather
#     than copied from the live script's colour block. They are the retired
#     definitions (`echo -e` + the same escape sequences) — if the live script
#     ever changes them, this fixture keeps modelling the RETIRED shell, which
#     is what a differential must compare against.
#   * `loom_resolve_config` is sourced from the LIVE `lib/config-resolver.sh`.
#     That library was not retired by this port and still has its own Rust twin
#     (`config_resolver.rs`); freezing a second copy of it here would test the
#     copy rather than the pairing. The harness passes
#     LOOM_CONFIG_DEFAULTS_FILE="" to both sides so the machine-level tier
#     cannot make them disagree for a reason that is not about this code.
#
# DO NOT "fix" anything here. Its value is being a faithful record of the
# retired behaviour, including the behaviour that is arguably wrong (the
# `jq`-availability gate on the whole linkPaths family; `ln -s` attempted
# against a dangling destination symlink). If the Rust should diverge from
# this, that is a deliberate behaviour change that belongs in its own issue,
# and this file should be left alone while the test's expectation is updated
# with a comment saying why.
#
# Usage:  worktree-link-retired.sh <main-workspace-dir> <abs-worktree-path> <scripts-lib-dir>

set -e

MAIN_WORKSPACE_ARG="$1"
ABS_WORKTREE_PATH="$2"
LIB_DIR="$3"

# shellcheck source=/dev/null
source "$LIB_DIR/config-resolver.sh"

# The retired colour block. Kept identical so a message-text comparison is a
# comparison of the messages and not of the decoration around them.
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

# shellcheck disable=SC2329  # part of the frozen surface; the block never errors
print_error() {
    echo -e "${RED}ERROR: $1${NC}" >&2
}

print_success() {
    echo -e "${GREEN}✓ $1${NC}"
}

print_info() {
    echo -e "${BLUE}ℹ $1${NC}"
}

print_warning() {
    echo -e "${YELLOW}⚠ $1${NC}"
}

# In the live script this was `$JSON_OUTPUT`, and every message below was
# wrapped in `if [[ "$JSON_OUTPUT" != "true" ]]`. The differential exercises
# the human-readable mode, which is the only one that produces comparable
# output at all: under --json the retired block printed nothing.
JSON_OUTPUT=false

# ===========================================================================
# BEGIN frozen copy — defaults/scripts/worktree.sh, the block between
# `# Resolve the info/exclude path that applies to this worktree.` and
# `# Run project-specific post-worktree hook if it exists`.
# ===========================================================================

    # Resolve the info/exclude path that applies to this worktree. Running
    # `git rev-parse --git-path info/exclude` from inside the worktree returns
    # the correct file for whatever git layout is in play (info/exclude is a
    # common-dir path, so worktrees inherit the main repo's .git/info/exclude;
    # asking git rather than hardcoding a path keeps us correct across layouts).
    WORKTREE_INFO_EXCLUDE=$(cd "$ABS_WORKTREE_PATH" 2>/dev/null \
        && git rev-parse --git-path info/exclude 2>/dev/null)
    if [[ -n "$WORKTREE_INFO_EXCLUDE" && "$WORKTREE_INFO_EXCLUDE" != /* ]]; then
        # git rev-parse may return a path relative to the worktree cwd; anchor it.
        WORKTREE_INFO_EXCLUDE="$ABS_WORKTREE_PATH/$WORKTREE_INFO_EXCLUDE"
    fi

    # Idempotently append a path to the worktree's info/exclude. Safe to call
    # repeatedly (grep -qxF guards against duplicate lines) and best-effort
    # (a missing exclude file just means git tracked the ignore elsewhere).
    _append_worktree_exclude() {
        local entry="$1"
        if [[ -z "$WORKTREE_INFO_EXCLUDE" ]]; then
            return 0
        fi
        mkdir -p "$(dirname "$WORKTREE_INFO_EXCLUDE")" 2>/dev/null || true
        grep -qxF "$entry" "$WORKTREE_INFO_EXCLUDE" 2>/dev/null \
            || echo "$entry" >> "$WORKTREE_INFO_EXCLUDE" 2>/dev/null || true
    }

    # Symlink node_modules from main workspace if available
    # This avoids expensive pnpm install on every worktree (30-60s savings)
    MAIN_WORKSPACE_DIR="$MAIN_WORKSPACE_ARG"
    MAIN_NODE_MODULES="$MAIN_WORKSPACE_DIR/node_modules"
    WORKTREE_NODE_MODULES="$ABS_WORKTREE_PATH/node_modules"
    WORKTREE_PACKAGE_JSON="$ABS_WORKTREE_PATH/package.json"

    if [[ -d "$MAIN_NODE_MODULES" && -f "$WORKTREE_PACKAGE_JSON" && ! -e "$WORKTREE_NODE_MODULES" ]]; then
        if [[ "$JSON_OUTPUT" != "true" ]]; then
            print_info "Symlinking node_modules from main workspace..."
        fi

        if ln -s "$MAIN_NODE_MODULES" "$WORKTREE_NODE_MODULES" 2>/dev/null; then
            _append_worktree_exclude "node_modules"
            if [[ "$JSON_OUTPUT" != "true" ]]; then
                print_success "node_modules symlinked (skipping pnpm install)"
            fi
        else
            if [[ "$JSON_OUTPUT" != "true" ]]; then
                print_warning "Could not symlink node_modules (will install on first build)"
            fi
        fi
    fi

    # Symlink nested (per-package) node_modules for pnpm/monorepo workspaces.
    if [[ -d "$MAIN_NODE_MODULES" ]]; then
        while IFS= read -r -d '' pkg_node_modules; do
            pkg_dir="$(dirname "$pkg_node_modules")"
            rel_path="${pkg_dir#"$MAIN_WORKSPACE_DIR"/}"
            # Skip if the prefix strip did nothing (path not under main workspace).
            if [[ "$rel_path" == "$pkg_dir" ]]; then
                continue
            fi
            # Only mirror package roots (node_modules alongside a package.json).
            if [[ ! -f "$pkg_dir/package.json" ]]; then
                continue
            fi
            worktree_pkg_dir="$ABS_WORKTREE_PATH/$rel_path"
            worktree_pkg_node_modules="$worktree_pkg_dir/node_modules"
            if [[ -d "$worktree_pkg_dir" && ! -e "$worktree_pkg_node_modules" ]]; then
                if ln -s "$pkg_node_modules" "$worktree_pkg_node_modules" 2>/dev/null; then
                    _append_worktree_exclude "$rel_path/node_modules"
                    if [[ "$JSON_OUTPUT" != "true" ]]; then
                        print_success "Symlinked $rel_path/node_modules from main workspace"
                    fi
                else
                    if [[ "$JSON_OUTPUT" != "true" ]]; then
                        print_warning "Could not symlink $rel_path/node_modules"
                    fi
                fi
            fi
        done < <(find "$MAIN_WORKSPACE_DIR" -mindepth 2 -maxdepth 3 -type d \
                    -name node_modules -not -path "*/node_modules/*" -print0 2>/dev/null)
    fi

    # Symlink additional gitignored paths configured for worktree.linkPaths
    if command -v jq >/dev/null 2>&1; then
        LOOM_WORKTREE_LINKPATHS_CFG="$(loom_resolve_config "$MAIN_WORKSPACE_DIR")"
        while IFS= read -r link_path; do
            if [[ -z "$link_path" ]]; then
                continue
            fi
            link_src="$MAIN_WORKSPACE_DIR/$link_path"
            link_dst="$ABS_WORKTREE_PATH/$link_path"
            if [[ -e "$link_src" && ! -e "$link_dst" ]]; then
                mkdir -p "$(dirname "$link_dst")" 2>/dev/null || true
                if ln -s "$link_src" "$link_dst" 2>/dev/null; then
                    _append_worktree_exclude "$link_path"
                    if [[ "$JSON_OUTPUT" != "true" ]]; then
                        print_success "Symlinked $link_path from main workspace"
                    fi
                else
                    if [[ "$JSON_OUTPUT" != "true" ]]; then
                        print_warning "Could not symlink $link_path"
                    fi
                fi
            fi
        done < <(echo "$LOOM_WORKTREE_LINKPATHS_CFG" | jq -r '.worktree.linkPaths[]? // empty' 2>/dev/null)
    fi

    # Symlink .mcp.json from main workspace if available
    MAIN_MCP_JSON="$MAIN_WORKSPACE_DIR/.mcp.json"
    WORKTREE_MCP_JSON="$ABS_WORKTREE_PATH/.mcp.json"

    if [[ -f "$MAIN_MCP_JSON" && ! -e "$WORKTREE_MCP_JSON" ]]; then
        if [[ "$JSON_OUTPUT" != "true" ]]; then
            print_info "Symlinking .mcp.json from main workspace..."
        fi

        if ln -s "$MAIN_MCP_JSON" "$WORKTREE_MCP_JSON" 2>/dev/null; then
            _append_worktree_exclude ".mcp.json"
            if [[ "$JSON_OUTPUT" != "true" ]]; then
                print_success ".mcp.json symlinked"
            fi
        else
            if [[ "$JSON_OUTPUT" != "true" ]]; then
                print_warning "Could not symlink .mcp.json"
            fi
        fi
    fi

# ===========================================================================
# END frozen copy
# ===========================================================================

# Not part of the frozen block. In `worktree.sh` this code sat mid-script and
# never determined the exit status; here it is the last thing that runs, so a
# final `if` whose test is simply FALSE (the common "nothing to link" case)
# would leave $? at 1 and the harness would read a no-op as a crash.
exit 0
