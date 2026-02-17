"""Spawn Claude Code CLI agents in tmux sessions.

This module provides the atomic building block for tmux-based agent management,
enabling persistent, inspectable, and interactive Claude Code agents that can
be spawned programmatically.

Replaces the shell script agent-spawn.sh with full feature parity.

Features:
- Creates tmux sessions with predictable names (loom-<name>)
- Uses shared tmux socket (-L loom) for unified session visibility
- Captures all output to .loom/logs/<session-name>.log
- Integrates with signal files for graceful shutdown
- Wraps Claude CLI with claude-wrapper.sh for resilience
- Supports git worktrees for isolated development
- On-demand ephemeral worker spawning with optional wait

Exit codes:
    0 - Success
    1 - Error
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone

from loom_tools.common.claude_config import setup_agent_config_dir
from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.repo import find_repo_root

# tmux configuration (must match agent_monitor.py)
TMUX_SOCKET = "loom"
SESSION_PREFIX = "loom-"

# Default thresholds (overridable via environment)
DEFAULT_STUCK_THRESHOLD = 300  # 5 minutes
DEFAULT_VERIFY_TIMEOUT = 10  # seconds

# Patterns in log output that indicate transient API errors
# (agent is waiting for "try again" input, not actually stuck on a logic problem)
API_ERROR_PATTERNS = (
    "500 Internal Server Error",
    "Rate limit exceeded",
    "rate_limit",
    "overloaded",
    "temporarily unavailable",
    "503 Service",
    "502 Bad Gateway",
    "Connection refused",
    "ECONNREFUSED",
    "ETIMEDOUT",
    "ECONNRESET",
    "NetworkError",
    "network error",
    "socket hang up",
    "No messages returned",
)


@dataclass
class SpawnResult:
    """Result of a spawn operation."""

    status: str  # "spawned", "exists", "error"
    name: str
    session: str = ""
    on_demand: bool = False
    log: str = ""
    error: str = ""

    def to_dict(self) -> dict:
        d: dict = {"status": self.status, "name": self.name}
        if self.session:
            d["session"] = self.session
        if self.status == "spawned" or self.status == "exists":
            d["on_demand"] = self.on_demand
        if self.log:
            d["log"] = self.log
        if self.error:
            d["error"] = self.error
        return d


@dataclass
class SpawnConfig:
    """Configuration for agent spawning."""

    role: str = ""
    name: str = ""
    args: str = ""
    worktree: str = ""
    on_demand: bool = False
    fresh: bool = False
    do_wait: bool = False
    wait_timeout: int = 3600
    json_output: bool = False
    check_name: str = ""
    do_list: bool = False

    stuck_threshold: int = field(
        default_factory=lambda: int(
            os.environ.get("LOOM_STUCK_SESSION_THRESHOLD", str(DEFAULT_STUCK_THRESHOLD))
        )
    )
    verify_timeout: int = field(
        default_factory=lambda: int(
            os.environ.get("LOOM_SPAWN_VERIFY_TIMEOUT", str(DEFAULT_VERIFY_TIMEOUT))
        )
    )


# ---------------------------------------------------------------------------
# tmux helpers
# ---------------------------------------------------------------------------


def _tmux(*args: str, check: bool = False) -> subprocess.CompletedProcess[str]:
    """Run a tmux command on the loom socket."""
    cmd = ["tmux", "-L", TMUX_SOCKET, *args]
    return subprocess.run(
        cmd, capture_output=True, text=True, check=check, timeout=10
    )


def session_exists(name: str) -> bool:
    """Check if a tmux session exists."""
    session_name = f"{SESSION_PREFIX}{name}"
    try:
        result = _tmux("has-session", "-t", session_name)
        return result.returncode == 0
    except (subprocess.TimeoutExpired, FileNotFoundError):
        return False


def session_is_alive(name: str) -> bool:
    """Check if session exists and has at least one window."""
    session_name = f"{SESSION_PREFIX}{name}"
    try:
        result = _tmux("list-windows", "-t", session_name)
        if result.returncode != 0:
            return False
        lines = [l for l in result.stdout.strip().splitlines() if l.strip()]
        return len(lines) > 0
    except (subprocess.TimeoutExpired, FileNotFoundError):
        return False


def cleanup_dead_session(name: str) -> None:
    """Kill a dead tmux session."""
    session_name = f"{SESSION_PREFIX}{name}"
    log_info(f"Cleaning up dead session: {session_name}")
    try:
        _tmux("kill-session", "-t", session_name)
    except (subprocess.TimeoutExpired, FileNotFoundError):
        pass


def _capture_session_output(name: str, session_name: str) -> None:
    """Best-effort capture of terminal scrollback before killing a session.

    Writes captured output to .loom/logs/<session>-killed-<timestamp>.log.
    Failures are logged but never prevent the kill from proceeding.
    """
    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_warning("Cannot capture session output: not in a git repository")
        return

    try:
        from loom_tools.common.tmux_session import TmuxSession

        session = TmuxSession(session_name)
        output = session.capture_scrollback(lines=200)

        if not output or not output.strip():
            log_info(f"No scrollback content to capture for {session_name}")
            return

        log_dir = repo_root / ".loom" / "logs"
        log_dir.mkdir(parents=True, exist_ok=True)

        timestamp = datetime.now().strftime("%Y%m%d-%H%M%S")
        kill_log = log_dir / f"{session_name}-killed-{timestamp}.log"
        kill_log.write_text(output)

        log_info(f"Captured session output to {kill_log}")
    except Exception as exc:
        log_warning(f"Failed to capture session output: {exc}")


def capture_tmux_output(name: str, lines: int = 200) -> str:
    """Capture the current tmux pane output for a session.

    Uses ``tmux capture-pane`` to read the visible buffer content.
    Returns the captured output as a string, or empty string on failure.
    """
    session_name = f"{SESSION_PREFIX}{name}"
    try:
        result = _tmux("capture-pane", "-t", session_name, "-p", "-S", f"-{lines}")
        if result.returncode == 0:
            return result.stdout
    except (subprocess.TimeoutExpired, FileNotFoundError):
        pass
    return ""


def kill_stuck_session(name: str) -> None:
    """Kill a stuck session with graceful then forced shutdown."""
    session_name = f"{SESSION_PREFIX}{name}"
    log_warning(f"Killing stuck session: {session_name}")

    # Capture scrollback before any shutdown attempts (best-effort)
    try:
        _capture_session_output(name, session_name)
    except Exception as exc:
        log_warning(f"Failed to capture session output: {exc}")

    # Attempt graceful shutdown first
    try:
        _tmux("send-keys", "-t", session_name, "C-c")
        time.sleep(1)
    except (subprocess.TimeoutExpired, FileNotFoundError):
        pass

    # Force kill
    try:
        _tmux("kill-session", "-t", session_name)
    except (subprocess.TimeoutExpired, FileNotFoundError):
        pass

    log_success(f"Stuck session killed: {session_name}")


def list_sessions() -> None:
    """List all loom-agent tmux sessions."""
    try:
        result = _tmux("list-sessions")
        if result.returncode == 0 and result.stdout.strip():
            print(result.stdout.strip())
        else:
            log_info("No active loom-agent sessions")
    except (subprocess.TimeoutExpired, FileNotFoundError):
        log_info("No active loom-agent sessions (tmux not available)")


def _get_pane_pid(session_name: str) -> str | None:
    """Get the shell PID from a tmux session pane."""
    try:
        result = _tmux("list-panes", "-t", session_name, "-F", "#{pane_pid}")
        if result.returncode == 0 and result.stdout.strip():
            return result.stdout.strip().splitlines()[0].strip()
    except (subprocess.TimeoutExpired, FileNotFoundError):
        pass
    return None


def _is_claude_running(shell_pid: str) -> bool:
    """Check if a claude process is running as a child or grandchild of shell_pid."""
    # Direct child: shell -> claude or shell -> claude-wrapper
    try:
        result = subprocess.run(
            ["pgrep", "-P", shell_pid, "-f", "claude"],
            capture_output=True, text=True, check=False, timeout=5,
        )
        if result.returncode == 0:
            return True
    except (subprocess.TimeoutExpired, FileNotFoundError):
        return False

    # Grandchild: shell -> claude-wrapper -> claude
    try:
        result = subprocess.run(
            ["pgrep", "-P", shell_pid],
            capture_output=True, text=True, check=False, timeout=5,
        )
        if result.returncode != 0:
            return False
        children = result.stdout.strip().splitlines()
        for child in children:
            child = child.strip()
            if not child:
                continue
            try:
                gc_result = subprocess.run(
                    ["pgrep", "-P", child, "-f", "claude"],
                    capture_output=True, text=True, check=False, timeout=5,
                )
                if gc_result.returncode == 0:
                    return True
            except (subprocess.TimeoutExpired, FileNotFoundError):
                continue
    except (subprocess.TimeoutExpired, FileNotFoundError):
        pass
    return False


# ---------------------------------------------------------------------------
# Validation helpers
# ---------------------------------------------------------------------------


def check_tmux() -> bool:
    """Validate that tmux is installed and has a recent enough version."""
    if not shutil.which("tmux"):
        log_error("tmux is not installed")
        log_info("Install with: brew install tmux (macOS) or apt-get install tmux (Linux)")
        return False

    try:
        result = subprocess.run(
            ["tmux", "-V"], capture_output=True, text=True, check=False, timeout=5
        )
        version_str = result.stdout.strip()
        # Extract version number (e.g. "tmux 3.4" -> "3.4")
        import re

        match = re.search(r"(\d+)\.(\d+)", version_str)
        if match:
            major, minor = int(match.group(1)), int(match.group(2))
            if major < 1 or (major == 1 and minor < 8):
                log_warning(
                    f"tmux version {major}.{minor} may not support all features "
                    "(recommend >= 1.8)"
                )
    except (subprocess.TimeoutExpired, FileNotFoundError):
        pass

    return True


def check_claude_cli() -> bool:
    """Validate that the Claude CLI is available."""
    if not shutil.which("claude"):
        log_error("Claude CLI not found in PATH")
        log_info("Install with: npm install -g @anthropic-ai/claude-code")
        return False
    return True


def validate_role(role: str, repo_root: pathlib.Path) -> bool:
    """Validate that a role definition file exists."""
    # Check .loom/roles/<role>.md first (may be symlink)
    role_file = repo_root / ".loom" / "roles" / f"{role}.md"
    if role_file.is_file() or role_file.is_symlink():
        return True

    # Check .claude/commands/<role>.md as fallback
    role_file = repo_root / ".claude" / "commands" / f"{role}.md"
    if role_file.is_file():
        return True

    log_error(f"Role not found: {role}")
    log_info(f"Expected at: {repo_root}/.loom/roles/{role}.md")
    log_info(f"         or: {repo_root}/.claude/commands/{role}.md")
    log_info("")
    log_info("Available roles:")
    roles_dir = repo_root / ".loom" / "roles"
    if roles_dir.is_dir():
        for f in sorted(roles_dir.glob("*.md")):
            name = f.stem
            if name != "README" and (f.is_file() or f.is_symlink()):
                log_info(f"  - {name}")
    return False


def validate_worktree(worktree_path: pathlib.Path) -> bool:
    """Validate that a worktree path exists and is a git repository."""
    if not worktree_path.is_dir():
        log_error(f"Worktree path does not exist: {worktree_path}")
        return False

    try:
        result = subprocess.run(
            ["git", "-C", str(worktree_path), "rev-parse", "--git-dir"],
            capture_output=True, text=True, check=False, timeout=5,
        )
        if result.returncode != 0:
            log_error(f"Not a valid git repository: {worktree_path}")
            return False
    except (subprocess.TimeoutExpired, FileNotFoundError):
        log_error(f"Could not verify git repository: {worktree_path}")
        return False

    return True


# ---------------------------------------------------------------------------
# Signal checking
# ---------------------------------------------------------------------------


def check_stop_signals(name: str, repo_root: pathlib.Path) -> bool:
    """Check for stop signals before spawning. Returns True if a signal blocks spawning."""
    # Global stop signal
    if (repo_root / ".loom" / "stop-daemon").exists():
        log_warning("Global stop signal exists (.loom/stop-daemon) - not spawning")
        return True

    # Shepherd-specific stop signal
    if name.startswith("shepherd-") and (repo_root / ".loom" / "stop-shepherds").exists():
        log_warning("Shepherd stop signal exists (.loom/stop-shepherds) - not spawning")
        return True

    # Per-agent stop signal
    if (repo_root / ".loom" / "signals" / f"stop-{name}").exists():
        log_warning(f"Agent stop signal exists (.loom/signals/stop-{name}) - not spawning")
        return True

    return False


# ---------------------------------------------------------------------------
# Stuck detection
# ---------------------------------------------------------------------------


def check_log_for_api_errors(log_file: pathlib.Path, tail_lines: int = 50) -> str | None:
    """Check the tail of a log file for API error patterns.

    Returns the matched pattern if found, None otherwise.
    """
    if not log_file.is_file():
        return None

    try:
        content = log_file.read_text()
        # Only check the last portion to avoid matching old errors
        lines = content.splitlines()
        tail = "\n".join(lines[-tail_lines:])
        for pattern in API_ERROR_PATTERNS:
            if pattern.lower() in tail.lower():
                return pattern
    except (OSError, UnicodeDecodeError):
        pass

    return None


def session_is_stuck(name: str, repo_root: pathlib.Path, threshold: int) -> bool:
    """Check if an existing session is stuck (idle with no claude activity).

    Returns True if stuck, False if healthy.
    """
    session_name = f"{SESSION_PREFIX}{name}"
    log_file = repo_root / ".loom" / "logs" / f"{session_name}.log"

    # Check 1: Is claude actually running in this session?
    shell_pid = _get_pane_pid(session_name)
    if not shell_pid:
        log_warning("Session has no shell PID - considered stuck")
        return True

    if not _is_claude_running(shell_pid):
        log_warning("No claude process found in session - considered stuck")
        return True

    # Check 2: Has the log file been written to recently?
    if log_file.is_file():
        try:
            log_mtime = log_file.stat().st_mtime
            idle_seconds = int(time.time() - log_mtime)

            if idle_seconds >= threshold:
                log_warning(
                    f"Session log idle for {idle_seconds}s (threshold: {threshold}s)"
                )

                # Check 3: Look for progress milestones as a secondary signal
                progress_dir = repo_root / ".loom" / "progress"
                if progress_dir.is_dir():
                    now = time.time()
                    for pfile in progress_dir.glob("shepherd-*.json"):
                        try:
                            pfile_age = int(now - pfile.stat().st_mtime)
                            if pfile_age < threshold:
                                log_info(
                                    "Recent progress milestone found - "
                                    "session may still be active"
                                )
                                return False  # Not stuck
                        except OSError:
                            continue

                # Check 4: Look for API error patterns in log
                api_error = check_log_for_api_errors(log_file)
                if api_error:
                    log_warning(
                        f"API error pattern detected in log: {api_error} "
                        f"- session likely waiting for 'try again' input"
                    )

                return True  # Stuck
        except OSError:
            pass

    # Session appears healthy
    return False


# ---------------------------------------------------------------------------
# Core spawn logic
# ---------------------------------------------------------------------------


def spawn_agent(
    role: str,
    name: str,
    args: str,
    worktree: str,
    repo_root: pathlib.Path,
    verify_timeout: int = DEFAULT_VERIFY_TIMEOUT,
) -> SpawnResult:
    """Spawn a Claude Code agent in a tmux session.

    Returns a SpawnResult indicating success or failure.
    """
    session_name = f"{SESSION_PREFIX}{name}"
    log_dir = repo_root / ".loom" / "logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    log_file = log_dir / f"{session_name}.log"

    # Determine working directory
    if worktree:
        working_dir = pathlib.Path(worktree)
        if not working_dir.is_absolute():
            working_dir = repo_root / worktree
    else:
        working_dir = repo_root

    # Rotate previous log file
    if log_file.is_file():
        timestamp = datetime.now().strftime("%Y%m%d-%H%M%S")
        rotated = log_file.with_suffix(f".{timestamp}.log")
        try:
            log_file.rename(rotated)
            log_info("Rotated previous log file")
        except OSError:
            pass

    # Write log header
    started = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    header = (
        f"# Loom Agent Log\n"
        f"# Session: {session_name}\n"
        f"# Role: {role}\n"
        f"# Args: {args}\n"
        f"# Working Directory: {working_dir}\n"
        f"# Started: {started}\n"
        f"# ---\n"
    )
    log_file.write_text(header)

    log_info(f"Creating tmux session: {session_name}")
    log_info(f"Working directory: {working_dir}")
    log_info(f"Log file: {log_file}")

    # Create new detached session
    result = _tmux("new-session", "-d", "-s", session_name, "-c", str(working_dir))
    if result.returncode != 0:
        log_error(f"Failed to create tmux session: {session_name}")
        return SpawnResult(status="error", name=name, error="session_create_failed")

    # Set up output capture via pipe-pane with Python log filter
    # The Python filter handles ANSI stripping, CR processing,
    # blank line suppression, and duplicate line collapsing.
    # Falls back to basic sed stripping if Python filter is unavailable.
    strip_ansi_cmd = (
        f"python3 -u -m loom_tools.log_filter >> '{log_file}' 2>/dev/null "
        f"|| sed -l -E 's/\\x1b\\[[?0-9;]*[a-zA-Z]//g; "
        f"s/\\x1b\\][^\\x07]*\\x07//g' "
        f">> '{log_file}' 2>/dev/null "
        f"|| sed -u -E 's/\\x1b\\[[?0-9;]*[a-zA-Z]//g; "
        f"s/\\x1b\\][^\\x07]*\\x07//g' "
        f">> '{log_file}'"
    )
    pipe_result = _tmux("pipe-pane", "-t", session_name, strip_ansi_cmd)
    if pipe_result.returncode != 0:
        log_warning("Failed to set up output capture (continuing anyway)")

    # Set environment variables for the session
    _tmux("set-environment", "-t", session_name, "LOOM_TERMINAL_ID", name)
    _tmux("set-environment", "-t", session_name, "LOOM_WORKSPACE", str(working_dir))
    _tmux("set-environment", "-t", session_name, "LOOM_ROLE", role)
    # Unset CLAUDECODE to prevent nested session guard from blocking subprocess
    _tmux("set-environment", "-t", session_name, "-u", "CLAUDECODE")

    # Create per-agent CLAUDE_CONFIG_DIR for session isolation
    config_dir = setup_agent_config_dir(name, repo_root)
    _tmux("set-environment", "-t", session_name, "CLAUDE_CONFIG_DIR", str(config_dir))
    _tmux("set-environment", "-t", session_name, "TMPDIR", str(config_dir / "tmp"))

    # Set PYTHONPATH so pytest in worktrees resolves imports from the worktree's
    # source instead of the main repo's editable install (see issue #2358)
    worktree_src = working_dir / "loom-tools" / "src"
    pythonpath_prefix = ""
    if worktree_src.is_dir():
        existing = os.environ.get("PYTHONPATH", "")
        pythonpath_val = f"{worktree_src}:{existing}" if existing else str(worktree_src)
        _tmux("set-environment", "-t", session_name, "PYTHONPATH", pythonpath_val)
        pythonpath_prefix = f"PYTHONPATH='{pythonpath_val}' "

    # Pin git operations to the worktree so that absolute paths cannot
    # accidentally resolve to the main repo (see issue #2418).
    # Also set LOOM_WORKTREE_PATH so the PreToolUse hook can block
    # Edit/Write calls outside the worktree (see issue #2441).
    if worktree and working_dir != repo_root:
        git_file = working_dir / ".git"
        if git_file.exists():
            _tmux(
                "set-environment", "-t", session_name,
                "GIT_WORK_TREE", str(working_dir),
            )
            _tmux(
                "set-environment", "-t", session_name,
                "GIT_DIR", str(git_file),
            )
        _tmux(
            "set-environment", "-t", session_name,
            "LOOM_WORKTREE_PATH", str(working_dir),
        )

    # Build the role slash command
    role_cmd = f"/{role}"
    if args:
        role_cmd = f"{role_cmd} {args}"

    # Worktree path prefix for the command line (makes LOOM_WORKTREE_PATH
    # available to Claude Code's PreToolUse hooks — see issue #2441).
    worktree_prefix = ""
    if worktree and working_dir != repo_root:
        worktree_prefix = f"LOOM_WORKTREE_PATH='{working_dir}' "

    # Propagate LOOM_MAX_RETRIES if the caller set it (e.g. shepherd
    # sets it to 1 to prevent double-retry with run_phase_with_retry).
    # See issue #2516.
    max_retries_prefix = ""
    max_retries_val = os.environ.get("LOOM_MAX_RETRIES")
    if max_retries_val is not None:
        max_retries_prefix = f"LOOM_MAX_RETRIES='{max_retries_val}' "

    # Build the Claude CLI command
    wrapper_script = repo_root / ".loom" / "scripts" / "claude-wrapper.sh"
    if wrapper_script.is_file() and os.access(wrapper_script, os.X_OK):
        claude_cmd = (
            f"{pythonpath_prefix}{worktree_prefix}{max_retries_prefix}"
            f"LOOM_TERMINAL_ID='{name}' LOOM_WORKSPACE='{working_dir}' "
            f"CLAUDE_CONFIG_DIR='{config_dir}' TMPDIR='{config_dir / 'tmp'}' "
            f"'{wrapper_script}' --dangerously-skip-permissions \"{role_cmd}\""
        )
    else:
        claude_cmd = f'claude --dangerously-skip-permissions "{role_cmd}"'
        log_warning("claude-wrapper.sh not found, using claude directly (no retry logic)")

    # Send the command to the session
    log_info(f"Starting Claude CLI with command: {role_cmd}")
    _tmux("send-keys", "-t", session_name, claude_cmd, "C-m")

    # Verify spawn succeeded by checking process existence
    log_info(f"Verifying Claude process started (up to {verify_timeout}s)...")
    elapsed = 0

    while elapsed < verify_timeout:
        # Check session still exists
        try:
            r = _tmux("has-session", "-t", session_name)
            if r.returncode != 0:
                log_error(f"tmux session disappeared: {session_name}")
                return SpawnResult(
                    status="error", name=name, error="session_disappeared"
                )
        except (subprocess.TimeoutExpired, FileNotFoundError):
            log_error(f"tmux session disappeared: {session_name}")
            return SpawnResult(status="error", name=name, error="session_disappeared")

        # Check for claude process
        shell_pid = _get_pane_pid(session_name)
        if shell_pid and _is_claude_running(shell_pid):
            log_info(f"Claude process detected after {elapsed}s")
            break

        time.sleep(1)
        elapsed += 1

    if elapsed >= verify_timeout:
        log_error(f"Claude process not detected within {verify_timeout}s")
        log_error(f"Session: {session_name}")
        log_error("The tmux session exists but no claude process is running.")
        log_error(f"Check: tmux -L {TMUX_SOCKET} attach -t {session_name}")
        return SpawnResult(status="error", name=name, error="process_not_detected")

    log_success("Agent spawned successfully")
    log_info("")
    log_info(f"Session: {session_name}")
    log_info(f"Attach:  tmux -L {TMUX_SOCKET} attach -t {session_name}")
    log_info(f"Logs:    tail -f {log_file}")
    log_info(f"Stop:    ./.loom/scripts/signal.sh stop {name}")

    return SpawnResult(
        status="spawned",
        name=name,
        session=session_name,
        log=str(log_file),
    )


# ---------------------------------------------------------------------------
# Main entry point
# ---------------------------------------------------------------------------


def run(config: SpawnConfig) -> int:
    """Execute the agent spawn logic. Returns an exit code."""

    # Handle --list
    if config.do_list:
        list_sessions()
        return 0

    # Handle --check
    if config.check_name:
        if session_exists(config.check_name):
            log_success(f"Session exists: {SESSION_PREFIX}{config.check_name}")
            return 0
        else:
            log_info(f"Session does not exist: {SESSION_PREFIX}{config.check_name}")
            return 1

    # Validate required parameters for spawn
    if not config.role:
        log_error("Missing required parameter: --role")
        log_info("Run 'loom-agent-spawn --help' for usage")
        return 1

    if not config.name:
        log_error("Missing required parameter: --name")
        log_info("Run 'loom-agent-spawn --help' for usage")
        return 1

    # Find repository root
    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository")
        return 1

    # Run validations
    if not check_tmux():
        return 1

    if not check_claude_cli():
        return 1

    if not validate_role(config.role, repo_root):
        return 1

    if config.worktree:
        abs_worktree = pathlib.Path(config.worktree)
        if not abs_worktree.is_absolute():
            abs_worktree = repo_root / config.worktree
        if not validate_worktree(abs_worktree):
            return 1

    # Check for stop signals before spawning
    if check_stop_signals(config.name, repo_root):
        return 1

    # Handle idempotency - check if session already exists
    if session_exists(config.name):
        session_name = f"{SESSION_PREFIX}{config.name}"
        if config.fresh:
            log_info(
                f"Fresh session requested - killing existing session: {session_name}"
            )
            kill_stuck_session(config.name)
        elif session_is_alive(config.name):
            log_info(f"Checking health of existing session: {session_name}")
            if session_is_stuck(config.name, repo_root, config.stuck_threshold):
                log_warning(
                    f"Session is stuck (idle > {config.stuck_threshold}s with no progress)"
                )
                log_info("Recovering: killing stuck session and restarting fresh")
                kill_stuck_session(config.name)
            else:
                log_success(
                    f"Session already exists and is healthy: {session_name}"
                )
                log_info(
                    f"Attach:  tmux -L {TMUX_SOCKET} attach -t {session_name}"
                )
                return 0
        else:
            cleanup_dead_session(config.name)

    # Spawn the agent
    spawn_result = spawn_agent(
        role=config.role,
        name=config.name,
        args=config.args,
        worktree=config.worktree,
        repo_root=repo_root,
        verify_timeout=config.verify_timeout,
    )

    if spawn_result.status == "error":
        if config.json_output:
            print(json.dumps(spawn_result.to_dict()))
        return 1

    # Mark as on-demand (ephemeral)
    if config.on_demand:
        spawn_result.on_demand = True
        _tmux(
            "set-environment",
            "-t",
            spawn_result.session,
            "LOOM_ON_DEMAND",
            "true",
        )

    if config.json_output and not config.do_wait:
        print(json.dumps(spawn_result.to_dict()))

    # Wait for completion if requested
    if config.do_wait:
        wait_script = repo_root / ".loom" / "scripts" / "agent-wait.sh"
        if not wait_script.is_file():
            log_error(f"agent-wait.sh not found at {wait_script}")
            return 1

        wait_args = [str(wait_script), config.name, "--timeout", str(config.wait_timeout)]
        if config.json_output:
            wait_args.append("--json")

        try:
            result = subprocess.run(wait_args, check=False)
            return result.returncode
        except FileNotFoundError:
            log_error(f"Could not execute: {wait_script}")
            return 1

    return 0


def main() -> None:
    """CLI entry point."""
    parser = argparse.ArgumentParser(
        description="Spawn Claude Code CLI agents in tmux sessions",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
examples:
  loom-agent-spawn --role shepherd --args "42 --merge" --name shepherd-1
  loom-agent-spawn --role builder --args "42" --name builder-1 --worktree .loom/worktrees/issue-42
  loom-agent-spawn --role builder --name builder-issue-42 --args "42" --on-demand --wait --timeout 1800
  loom-agent-spawn --check shepherd-1
  loom-agent-spawn --list

environment:
  LOOM_SPAWN_VERIFY_TIMEOUT     Timeout for process verification in seconds (default: 10)
  LOOM_STUCK_SESSION_THRESHOLD  Seconds before idle session is considered stuck (default: 300)
""",
    )

    # Spawn parameters
    parser.add_argument("--role", help="Role name (shepherd, builder, judge, etc.)")
    parser.add_argument(
        "--name", help="Session identifier (used in tmux session name: loom-<name>)"
    )
    parser.add_argument("--args", default="", help="Arguments to pass to the role slash command")
    parser.add_argument(
        "--worktree", help="Path to git worktree (agent runs in isolated worktree)"
    )
    parser.add_argument(
        "--on-demand",
        action="store_true",
        help="Mark session as ephemeral (for agent-destroy.sh cleanup)",
    )
    parser.add_argument(
        "--fresh",
        action="store_true",
        help="Force new session even if one already exists (kills stuck sessions)",
    )
    parser.add_argument(
        "--wait",
        action="store_true",
        help="Block until agent completes (requires agent-wait.sh)",
    )
    parser.add_argument(
        "--timeout", type=int, default=3600, help="Timeout for --wait (default: 3600)"
    )
    parser.add_argument("--json", action="store_true", help="Output spawn result as JSON")

    # Mode-specific parameters
    parser.add_argument(
        "--check", metavar="NAME", help="Check if session exists (exit 0 if yes, 1 if no)"
    )
    parser.add_argument(
        "--list", action="store_true", help="List all active loom-agent sessions"
    )

    args = parser.parse_args()

    config = SpawnConfig(
        role=args.role or "",
        name=args.name or "",
        args=args.args,
        worktree=args.worktree or "",
        on_demand=args.on_demand,
        fresh=args.fresh,
        do_wait=args.wait,
        wait_timeout=args.timeout,
        json_output=args.json,
        check_name=args.check or "",
        do_list=args.list,
    )

    sys.exit(run(config))


if __name__ == "__main__":
    main()
