"""Per-agent CLAUDE_CONFIG_DIR isolation.

Creates isolated Claude Code config directories for each agent so concurrent
agents don't fight over sessions, lock files, and temp directories in the
shared ~/.claude/ directory.

Each agent gets .loom/claude-config/{agent-name}/ with:
- Symlinks to shared read-only config (settings.json, config.json, etc.)
- Fresh empty directories for mutable per-session state
"""

from __future__ import annotations

import hashlib
import logging
import shutil
import subprocess
from pathlib import Path

from loom_tools.common.paths import LoomPaths

log = logging.getLogger(__name__)

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


def _ensure_onboarding_complete(state_path: Path) -> None:
    """Ensure .claude.json has the fields required to skip the onboarding wizard.

    Claude Code requires both ``hasCompletedOnboarding = true`` and a truthy
    ``theme`` value to bypass the first-run wizard.  If the state file is
    missing, dangling (broken symlink), or doesn't contain these fields, we
    replace it with a minimal standalone file so agents never hit the wizard.
    """
    import json

    try:
        if state_path.exists():
            data = json.loads(state_path.read_text())
            if data.get("hasCompletedOnboarding") is True and data.get("theme"):
                return  # Already has the required fields
    except (json.JSONDecodeError, OSError):
        pass

    # Remove whatever is there (dangling symlink, corrupt file, etc.)
    try:
        state_path.unlink()
    except FileNotFoundError:
        pass

    state_path.write_text(json.dumps({
        "hasCompletedOnboarding": True,
        "theme": "dark",
    }))
    log.debug("Wrote fallback .claude.json with onboarding-complete state")


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


def _keychain_service_name(config_dir: Path) -> str:
    """Build the keychain service name Claude Code uses for a given config dir.

    Claude Code v2.1.42+ appends a SHA-256 hash of the resolved config dir
    path to the keychain service name when CLAUDE_CONFIG_DIR is set:

        "Claude Code-credentials"              (default, no override)
        "Claude Code-credentials-<8hex>"       (with CLAUDE_CONFIG_DIR)

    This means credentials stored under the default name are invisible when
    the config dir is overridden. We must clone them to the hashed name.
    """
    h = hashlib.sha256(str(config_dir).encode()).hexdigest()[:8]
    return f"Claude Code-credentials-{h}"


def _clone_keychain_credentials(config_dir: Path) -> bool:
    """Clone macOS Keychain credentials to the hashed service name.

    Reads the OAuth credential from the default ``Claude Code-credentials``
    keychain entry and writes it to the per-config-dir hashed entry so that
    ``claude auth status`` returns ``loggedIn: true`` when
    ``CLAUDE_CONFIG_DIR`` is overridden.

    No-op on non-macOS or when the default credential doesn't exist.

    Returns:
        True if credentials were cloned, False otherwise.
    """
    import platform

    if platform.system() != "Darwin":
        return False

    import getpass

    account = getpass.getuser()
    target_service = _keychain_service_name(config_dir)

    # Check if the target already has a credential
    check = subprocess.run(
        ["security", "find-generic-password", "-a", account, "-s", target_service],
        capture_output=True,
        text=True,
    )
    if check.returncode == 0:
        return False  # Already exists

    # Read the default credential
    read = subprocess.run(
        ["security", "find-generic-password", "-a", account, "-w",
         "-s", "Claude Code-credentials"],
        capture_output=True,
        text=True,
    )
    if read.returncode != 0 or not read.stdout.strip():
        log.debug("No default Claude Code keychain credential found")
        return False

    cred = read.stdout.strip()
    cred_hex = cred.encode().hex()

    # Write to the hashed service name
    write = subprocess.run(
        ["security", "add-generic-password", "-U",
         "-a", account, "-s", target_service, "-X", cred_hex],
        capture_output=True,
        text=True,
    )
    if write.returncode != 0:
        log.warning("Failed to clone keychain credential: %s", write.stderr.strip())
        return False

    log.debug("Cloned keychain credential to %s", target_service)
    return True


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

    # Fallback: ensure the state file has onboarding-complete fields.
    # If the symlink wasn't created (source missing), is dangling, or the
    # target doesn't contain the required fields, write a standalone file.
    _ensure_onboarding_complete(state_dst)

    # Symlink shared directories
    for dirname in _SHARED_CONFIG_DIRS:
        src = home_claude / dirname
        dst = config_dir / dirname
        if src.exists() and not dst.exists():
            dst.symlink_to(src)

    # Create mutable directories
    for dirname in _MUTABLE_DIRS:
        (config_dir / dirname).mkdir(exist_ok=True)

    # Clone macOS Keychain credentials to the per-config-dir service name.
    # Claude Code v2.1.42+ hashes the config dir path into the keychain
    # service name, so credentials stored under the default name are
    # invisible when CLAUDE_CONFIG_DIR is overridden.
    _clone_keychain_credentials(config_dir)

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
