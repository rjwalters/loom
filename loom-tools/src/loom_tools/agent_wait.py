"""Synchronous agent completion detection.

Replaces agent-wait.sh with direct completion checking:
- Claude process exit (shell idle)
- /exit command in log file
- Idle prompt detection (task finished, waiting for input)

This is the core synchronous building block. AgentMonitor (agent_monitor.py)
wraps this with async signal handling, stuck detection, and recovery.

Exit codes:
  0 - Agent completed
  1 - Timeout reached
  2 - Session not found or error
"""

from __future__ import annotations

import json
import re
import subprocess
import time
from dataclasses import dataclass

from loom_tools.common.logging import log_info, log_success, log_warning
from loom_tools.common.repo import find_repo_root

# tmux configuration (must match agent-spawn.sh)
TMUX_SOCKET = "loom"
SESSION_PREFIX = "loom-"

# Defaults matching agent-wait.sh
DEFAULT_TIMEOUT = 3600
DEFAULT_POLL_INTERVAL = 5
DEFAULT_MIN_IDLE_ELAPSED = 10

# Consecutive idle observations required before declaring completion
IDLE_PROMPT_CONFIRM_COUNT = 2

# Claude Code shows this in the status bar when actively processing
PROCESSING_INDICATORS = "esc to interrupt"


@dataclass
class WaitConfig:
    """Configuration for synchronous agent wait."""

    name: str
    timeout: int = DEFAULT_TIMEOUT
    poll_interval: int = DEFAULT_POLL_INTERVAL
    min_idle_elapsed: int = DEFAULT_MIN_IDLE_ELAPSED
    json_output: bool = False


@dataclass
class WaitResult:
    """Result from synchronous agent wait."""

    status: str  # "completed", "timeout", "not_found", "error"
    name: str
    elapsed: int = 0
    reason: str = ""
    error: str = ""

    def to_dict(self) -> dict:
        d: dict = {"status": self.status, "name": self.name}
        if self.elapsed:
            d["elapsed"] = self.elapsed
        if self.reason:
            d["reason"] = self.reason
        if self.error:
            d["error"] = self.error
        if self.status == "timeout":
            d["timeout"] = self.elapsed  # match bash output format
        return d


def _tmux_run(*args: str) -> subprocess.CompletedProcess:
    """Run a tmux command on the loom socket."""
    return subprocess.run(
        ["tmux", "-L", TMUX_SOCKET, *args],
        capture_output=True,
        text=True,
        check=False,
    )


def session_exists(session_name: str) -> bool:
    """Check if a tmux session exists."""
    try:
        result = _tmux_run("has-session", "-t", session_name)
        return result.returncode == 0
    except Exception:
        return False


def get_session_age(session_name: str) -> int:
    """Get the age of a tmux session in seconds since creation.

    Returns -1 if the session doesn't exist or age can't be determined.
    """
    try:
        result = _tmux_run(
            "display-message", "-t", session_name, "-p", "#{session_created}"
        )
        if result.returncode != 0 or not result.stdout.strip():
            return -1
        created_at = int(result.stdout.strip())
        if created_at == 0:
            return -1
        return int(time.time()) - created_at
    except Exception:
        return -1


def get_session_shell_pid(session_name: str) -> str:
    """Get the shell PID for a tmux session's first pane."""
    try:
        result = _tmux_run("list-panes", "-t", session_name, "-F", "#{pane_pid}")
        if result.returncode == 0 and result.stdout.strip():
            return result.stdout.strip().split("\n")[0]
    except Exception:
        pass
    return ""


def claude_is_running(shell_pid: str) -> bool:
    """Check if a claude process is running under the given shell PID.

    Checks both direct children and grandchildren (for wrapper scripts).
    """
    try:
        # Check direct children
        result = subprocess.run(
            ["pgrep", "-P", shell_pid, "-f", "claude"],
            capture_output=True,
            check=False,
        )
        if result.returncode == 0:
            return True

        # Check grandchildren (claude-wrapper.sh -> claude)
        children_result = subprocess.run(
            ["pgrep", "-P", shell_pid],
            capture_output=True,
            text=True,
            check=False,
        )
        if children_result.returncode == 0:
            for child_pid in children_result.stdout.strip().split("\n"):
                child_pid = child_pid.strip()
                if not child_pid:
                    continue
                grandchild_result = subprocess.run(
                    ["pgrep", "-P", child_pid, "-f", "claude"],
                    capture_output=True,
                    check=False,
                )
                if grandchild_result.returncode == 0:
                    return True
    except Exception:
        pass
    return False


def capture_pane(session_name: str) -> str:
    """Capture the current visible content of a tmux pane."""
    try:
        result = _tmux_run("capture-pane", "-t", session_name, "-p")
        return result.stdout if result.returncode == 0 else ""
    except Exception:
        return ""


def check_exit_command(session_name: str, repo_root: str) -> bool:
    """Check for /exit command in the agent's log file."""
    import pathlib

    log_file = pathlib.Path(repo_root) / ".loom" / "logs" / f"{session_name}.log"
    if not log_file.exists():
        return False

    try:
        with open(log_file) as f:
            lines = f.readlines()
        recent_log = "".join(lines[-100:])
        if not recent_log:
            return False
        return bool(re.search(r"(^|\s+|❯\s*|>\s*)/exit\s*$", recent_log, re.MULTILINE))
    except Exception:
        return False


def check_idle_prompt(session_name: str) -> bool:
    """Check if Claude is sitting at an idle prompt (task completed, waiting for input).

    Detects when a support role has finished its work but the Claude CLI
    remains at an interactive prompt. The tmux pane will show the prompt
    character at the end of the visible content with no processing indicators.
    """
    pane_content = capture_pane(session_name)
    if not pane_content:
        return False

    # If processing indicators are present, Claude is still working
    if PROCESSING_INDICATORS in pane_content:
        return False

    # Check for idle prompt: last non-empty lines should contain just the prompt character
    last_lines = [
        line for line in pane_content.split("\n") if line.strip()
    ]
    if not last_lines:
        return False

    # Check the last few non-empty lines for the idle prompt pattern
    for line in last_lines[-5:]:
        if re.match(r"^\s*❯\s*$", line):
            return True

    return False


def handle_exit_detection(
    session_name: str, name: str, elapsed: int, json_output: bool
) -> WaitResult:
    """Handle /exit detection: send /exit to prompt and destroy session."""
    if not json_output:
        log_info(
            f"/exit detected in output - sending /exit to prompt "
            f"and terminating '{session_name}'"
        )

    # Send /exit to the tmux prompt as backup
    try:
        _tmux_run("send-keys", "-t", session_name, "/exit", "C-m")
    except Exception:
        pass

    time.sleep(1)

    # Destroy the tmux session
    try:
        _tmux_run("kill-session", "-t", session_name)
    except Exception:
        pass

    if not json_output:
        log_success(f"Agent '{name}' completed (explicit /exit after {elapsed}s)")

    return WaitResult(
        status="completed", name=name, reason="explicit_exit", elapsed=elapsed
    )


def wait_for_agent(config: WaitConfig) -> WaitResult:
    """Wait for a Claude agent to complete.

    Monitors a tmux session for agent completion by checking:
    1. Session existence (may have been destroyed)
    2. /exit command in log file
    3. Shell PID presence
    4. Claude process in process tree
    5. Idle prompt detection (with confirmation count guard)
    """
    session_name = f"{SESSION_PREFIX}{config.name}"
    repo_root = str(find_repo_root())

    # Check session exists
    if not session_exists(session_name):
        if not config.json_output:
            log_warning(f"Session not found: {session_name}")
        return WaitResult(
            status="not_found", name=config.name, error=f"session {session_name}"
        )

    # Get initial shell PID
    shell_pid = get_session_shell_pid(session_name)
    if not shell_pid:
        if not config.json_output:
            log_warning(f"Could not find shell PID for session: {session_name}")
        return WaitResult(
            status="error", name=config.name, error="could not find shell PID"
        )

    if not config.json_output:
        log_info(
            f"Waiting for agent '{config.name}' to complete "
            f"(timeout: {config.timeout}s, poll: {config.poll_interval}s)"
        )
        log_info(f"Session: {session_name}, Shell PID: {shell_pid}")

    start_time = time.time()
    idle_prompt_count = 0

    while True:
        elapsed = int(time.time() - start_time)

        # Check if session still exists (may have been destroyed)
        if not session_exists(session_name):
            if not config.json_output:
                log_success(
                    f"Agent '{config.name}' completed "
                    f"(session destroyed after {elapsed}s)"
                )
            return WaitResult(
                status="completed",
                name=config.name,
                reason="session_destroyed",
                elapsed=elapsed,
            )

        # Check for /exit command in log file
        if check_exit_command(session_name, repo_root):
            return handle_exit_detection(
                session_name, config.name, elapsed, config.json_output
            )

        # Re-fetch shell PID in case pane was recreated
        shell_pid = get_session_shell_pid(session_name)
        if not shell_pid:
            if not config.json_output:
                log_success(
                    f"Agent '{config.name}' completed "
                    f"(no shell process after {elapsed}s)"
                )
            return WaitResult(
                status="completed",
                name=config.name,
                reason="no_shell",
                elapsed=elapsed,
            )

        # Check if claude is still running
        if not claude_is_running(shell_pid):
            if not config.json_output:
                log_success(
                    f"Agent '{config.name}' completed "
                    f"(claude exited after {elapsed}s)"
                )
            return WaitResult(
                status="completed",
                name=config.name,
                reason="claude_exited",
                elapsed=elapsed,
            )

        # Idle prompt detection with guards against false positives
        elapsed = int(time.time() - start_time)

        if config.timeout == 0:
            # Non-blocking mode: single check with session-age guard.
            # Even with --timeout 0, check the tmux session's age to avoid
            # false positives on freshly created/restarted sessions (issue #1792).
            session_age = get_session_age(session_name)
            if session_age >= 0 and session_age < config.min_idle_elapsed:
                if not config.json_output:
                    log_info(
                        f"Session '{session_name}' is only {session_age}s old "
                        f"(< {config.min_idle_elapsed}s) - skipping idle check"
                    )
                # Fall through to timeout check below
            elif check_idle_prompt(session_name):
                if not config.json_output:
                    log_success(
                        f"Agent '{config.name}' completed "
                        f"(idle at prompt after {elapsed}s)"
                    )
                return WaitResult(
                    status="completed",
                    name=config.name,
                    reason="idle_prompt",
                    elapsed=elapsed,
                )
        elif elapsed >= config.min_idle_elapsed:
            if check_idle_prompt(session_name):
                idle_prompt_count += 1
                if idle_prompt_count >= IDLE_PROMPT_CONFIRM_COUNT:
                    if not config.json_output:
                        log_success(
                            f"Agent '{config.name}' completed "
                            f"(idle at prompt after {elapsed}s)"
                        )
                    return WaitResult(
                        status="completed",
                        name=config.name,
                        reason="idle_prompt",
                        elapsed=elapsed,
                    )
            else:
                idle_prompt_count = 0

        # Check timeout
        elapsed = int(time.time() - start_time)
        if elapsed >= config.timeout:
            if not config.json_output:
                log_warning(
                    f"Timeout waiting for agent '{config.name}' after {elapsed}s"
                )
            return WaitResult(
                status="timeout", name=config.name, elapsed=elapsed
            )

        time.sleep(config.poll_interval)


def main() -> None:
    """CLI entry point for synchronous agent wait."""
    import argparse
    import sys

    parser = argparse.ArgumentParser(
        description="Wait for a tmux Claude agent to complete"
    )
    parser.add_argument("name", help="Agent session name")
    parser.add_argument(
        "--timeout",
        type=int,
        default=DEFAULT_TIMEOUT,
        help=f"Maximum time to wait (default: {DEFAULT_TIMEOUT})",
    )
    parser.add_argument(
        "--poll-interval",
        type=int,
        default=DEFAULT_POLL_INTERVAL,
        help=f"Time between checks (default: {DEFAULT_POLL_INTERVAL})",
    )
    parser.add_argument(
        "--min-idle-elapsed",
        type=int,
        default=DEFAULT_MIN_IDLE_ELAPSED,
        help=f"Minimum seconds before idle prompt detection (default: {DEFAULT_MIN_IDLE_ELAPSED})",
    )
    parser.add_argument("--json", action="store_true", help="Output result as JSON")

    args = parser.parse_args()

    config = WaitConfig(
        name=args.name,
        timeout=args.timeout,
        poll_interval=args.poll_interval,
        min_idle_elapsed=args.min_idle_elapsed,
        json_output=args.json,
    )

    result = wait_for_agent(config)

    if args.json:
        print(json.dumps(result.to_dict()))

    # Exit codes matching agent-wait.sh
    exit_codes = {
        "completed": 0,
        "timeout": 1,
        "not_found": 2,
        "error": 2,
    }
    sys.exit(exit_codes.get(result.status, 2))


if __name__ == "__main__":
    main()
