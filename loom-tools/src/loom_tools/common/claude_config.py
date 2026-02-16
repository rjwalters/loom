"""Per-agent CLAUDE_CONFIG_DIR isolation.

Creates isolated Claude Code config directories for each agent so concurrent
agents don't fight over sessions, lock files, and temp directories in the
shared ~/.claude/ directory.

Each agent gets .loom/claude-config/{agent-name}/ with:
- Symlinks to shared read-only config (settings.json, config.json, etc.)
- Fresh empty directories for mutable per-session state
"""

from __future__ import annotations

import shutil
from pathlib import Path

from loom_tools.common.paths import LoomPaths

# Shared config files to symlink from ~/.claude/ (read-only)
_SHARED_CONFIG_FILES = [
    "settings.json",
    "config.json",
    "mcp.json",
    ".mcp.json",
]

# Shared directories to symlink from ~/.claude/ (read-only caches)
_SHARED_CONFIG_DIRS = [
    "statsig",
]

# Mutable directories that each agent needs its own copy of
_MUTABLE_DIRS = [
    "projects",
    "todos",
    "debug",
    "file-history",
    "session-env",
    "tasks",
    "plans",
    "shell-snapshots",
    "tmp",
]


def _resolve_state_file() -> Path:
    """Resolve the Claude Code state file path.

    Claude Code stores onboarding state (hasCompletedOnboarding, theme, etc.)
    in a state file. The resolution order is:

    1. ~/.claude/.config.json  (if it exists)
    2. ~/.claude.json          (fallback, most common)

    When CLAUDE_CONFIG_DIR is overridden (as we do for per-agent isolation),
    Claude looks for .claude.json inside that directory. We must symlink
    the resolved source file so agents inherit the onboarding-complete state.

    Returns:
        Path to the state file (may not exist on a fresh system).
    """
    home_claude = Path.home() / ".claude"
    preferred = home_claude / ".config.json"
    if preferred.exists():
        return preferred
    return Path.home() / ".claude.json"


def setup_agent_config_dir(agent_name: str, repo_root: Path) -> Path:
    """Create an isolated CLAUDE_CONFIG_DIR for an agent.

    Creates .loom/claude-config/{agent_name}/ with symlinks to shared
    read-only config from ~/.claude/ and fresh directories for mutable state.

    Idempotent — safe to call multiple times.

    Args:
        agent_name: The agent name (e.g., "builder-1", "shepherd-2").
        repo_root: Repository root path.

    Returns:
        Path to the created config directory.
    """
    paths = LoomPaths(repo_root)
    config_dir = paths.agent_claude_config_dir(agent_name)
    config_dir.mkdir(parents=True, exist_ok=True)

    home_claude = Path.home() / ".claude"

    # Symlink shared config files from ~/.claude/
    for filename in _SHARED_CONFIG_FILES:
        src = home_claude / filename
        dst = config_dir / filename
        if src.exists() and not dst.exists():
            dst.symlink_to(src)

    # Symlink Claude Code state file (onboarding completion, theme, etc.).
    # The state file lives at ~/.claude.json (or ~/.claude/.config.json),
    # NOT inside ~/.claude/. When CLAUDE_CONFIG_DIR is overridden, Claude
    # looks for $CLAUDE_CONFIG_DIR/.claude.json. Without this symlink,
    # every agent session hits the first-run onboarding wizard.
    state_src = _resolve_state_file()
    state_dst = config_dir / ".claude.json"
    if state_src.exists() and not state_dst.exists():
        state_dst.symlink_to(state_src)

    # Symlink shared directories
    for dirname in _SHARED_CONFIG_DIRS:
        src = home_claude / dirname
        dst = config_dir / dirname
        if src.exists() and not dst.exists():
            dst.symlink_to(src)

    # Create mutable directories
    for dirname in _MUTABLE_DIRS:
        (config_dir / dirname).mkdir(exist_ok=True)

    return config_dir


def cleanup_agent_config_dir(agent_name: str, repo_root: Path) -> bool:
    """Remove one agent's config directory.

    Args:
        agent_name: The agent name.
        repo_root: Repository root path.

    Returns:
        True if a directory was removed, False if it didn't exist.
    """
    paths = LoomPaths(repo_root)
    config_dir = paths.agent_claude_config_dir(agent_name)
    if config_dir.is_dir():
        shutil.rmtree(config_dir)
        return True
    return False


def cleanup_all_agent_config_dirs(repo_root: Path) -> int:
    """Remove all per-agent config directories.

    Args:
        repo_root: Repository root path.

    Returns:
        Number of directories removed.
    """
    paths = LoomPaths(repo_root)
    base_dir = paths.claude_config_base_dir
    if not base_dir.is_dir():
        return 0
    count = 0
    for child in base_dir.iterdir():
        if child.is_dir():
            shutil.rmtree(child)
            count += 1
    return count
