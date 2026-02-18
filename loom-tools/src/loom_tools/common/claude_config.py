"""Per-agent CLAUDE_CONFIG_DIR isolation.

Creates isolated Claude Code config directories for each agent so concurrent
agents don't fight over sessions, lock files, and temp directories in the
shared ~/.claude/ directory.

Each agent gets .loom/claude-config/{agent-name}/ with:
- Symlinks to shared read-only config (settings.json, config.json)
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
# Note: mcp.json / .mcp.json are intentionally excluded — they are
# project-scoped configs that Claude Code discovers from the working
# directory / git root.  Symlinking them from ~/.claude/ shadows the
# correct project-level config and causes MCP initialization failures.
#
# Note: settings.json is intentionally excluded from symlinks — it is
# copied and filtered instead to strip ``enabledPlugins`` (global MCP
# plugins).  Global plugins like rust-analyzer-lsp and swift-lsp fail
# in headless mode and cause ghost sessions.  See issue #2799.
_SHARED_CONFIG_FILES = [
    "config.json",
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


def _copy_settings_without_plugins(src: Path, dst: Path) -> bool:
    """Copy settings.json stripping the ``enabledPlugins`` key.

    Global MCP plugins (e.g. ``rust-analyzer-lsp``, ``swift-lsp``) load from
    the ``enabledPlugins`` field in ``~/.claude/settings.json``.  In headless
    agent sessions these plugins fail to initialise and can prevent Claude CLI
    from processing its input prompt, producing ghost sessions that waste
    minutes of retry time.

    This function copies *all other* settings (model, theme, permissions, etc.)
    so agent behaviour remains consistent with the user's configuration.

    Args:
        src: Path to the source settings.json (usually ``~/.claude/settings.json``).
        dst: Path to the destination (inside the agent config dir).

    Returns:
        True if the file was copied, False on any error or if *src* is missing.
    """
    import json

    if not src.is_file():
        return False

    try:
        data = json.loads(src.read_text())
    except (json.JSONDecodeError, OSError):
        log.debug("Could not read %s — skipping settings copy", src)
        return False

    if not isinstance(data, dict):
        log.debug("settings.json is not a JSON object — skipping")
        return False

    # Strip the key that triggers global plugin loading.
    data.pop("enabledPlugins", None)

    try:
        dst.write_text(json.dumps(data, indent=2) + "\n")
    except OSError as exc:
        log.debug("Failed to write filtered settings.json to %s: %s", dst, exc)
        return False

    log.debug("Copied settings.json to %s (enabledPlugins stripped)", dst)
    return True


def _ensure_onboarding_complete(state_path: Path) -> None:
    """Ensure .claude.json has the fields required to skip the onboarding wizard.

    Claude Code requires both ``hasCompletedOnboarding = true`` and a truthy
    ``theme`` value to bypass the first-run wizard.  If the state file is
    missing, dangling (broken symlink), or doesn't contain these fields, we
    merge the required fields into the existing data (preserving all other
    fields) rather than replacing the entire file.
    """
    import json

    # Required fields that must be present to skip the onboarding wizard.
    required_fields = {
        "hasCompletedOnboarding": True,
        "theme": "dark",
        "effortCalloutDismissed": True,
        "opusProMigrationComplete": True,
    }

    existing_data: dict = {}
    try:
        if state_path.exists():
            existing_data = json.loads(state_path.read_text())
            if isinstance(existing_data, dict):
                # Check if all required fields are already present and valid.
                has_onboarding = existing_data.get("hasCompletedOnboarding") is True
                has_theme = bool(existing_data.get("theme"))
                has_effort = existing_data.get("effortCalloutDismissed") is True
                has_opus = existing_data.get("opusProMigrationComplete") is True
                if has_onboarding and has_theme and has_effort and has_opus:
                    return  # All required fields present
            else:
                existing_data = {}
    except (json.JSONDecodeError, OSError):
        existing_data = {}

    # Merge: fill in only the missing required fields, preserving everything else.
    merged = {**existing_data}
    for key, default_value in required_fields.items():
        if key == "theme":
            # Only set theme if missing or empty
            if not merged.get("theme"):
                merged["theme"] = default_value
        else:
            # Only set if not already the correct value
            if merged.get(key) is not default_value:
                merged[key] = default_value

    # Remove whatever is there (dangling symlink, corrupt file, etc.)
    # so we can write a standalone file.
    try:
        state_path.unlink()
    except FileNotFoundError:
        pass

    state_path.write_text(json.dumps(merged))
    log.debug("Wrote merged .claude.json with onboarding-complete state")


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

    # Copy settings.json with enabledPlugins stripped (issue #2799).
    # Global plugins (rust-analyzer-lsp, swift-lsp, etc.) fail in headless
    # mode and cause ghost sessions.  All other settings are preserved.
    settings_dst = config_dir / "settings.json"
    if not settings_dst.exists():
        _copy_settings_without_plugins(home_claude / "settings.json", settings_dst)

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
