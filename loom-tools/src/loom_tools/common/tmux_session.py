"""Shared tmux session management for the loom server.

Consolidates tmux interaction patterns used across agent_wait.py,
agent_monitor.py, and other modules that manage loom tmux sessions.
"""

from __future__ import annotations

import subprocess
import time


# tmux configuration (must match agent-spawn.sh)
TMUX_SOCKET = "loom"
SESSION_PREFIX = "loom-"

# Claude Code shows this in the status bar when actively processing
PROCESSING_INDICATORS = "esc to interrupt"


class TmuxSession:
    """Manages a single tmux session on the loom server."""

    def __init__(self, name: str, server_name: str = TMUX_SOCKET) -> None:
        self.name = name
        self.server_name = server_name

    def _run(self, *args: str) -> subprocess.CompletedProcess[str]:
        """Run a tmux command on this session's server."""
        return subprocess.run(
            ["tmux", "-L", self.server_name, *args],
            capture_output=True,
            text=True,
            check=False,
        )

    def exists(self) -> bool:
        """Check if this tmux session exists."""
        try:
            result = self._run("has-session", "-t", self.name)
            return result.returncode == 0
        except Exception:
            return False

    def capture_pane(self) -> str:
        """Capture the current visible content of the tmux pane."""
        try:
            result = self._run("capture-pane", "-t", self.name, "-p")
            return result.stdout if result.returncode == 0 else ""
        except Exception:
            return ""

    def capture_scrollback(self, lines: int = 200) -> str:
        """Capture scrollback history from the tmux pane.

        Args:
            lines: Number of scrollback lines to capture (default 200).

        Returns:
            The captured scrollback text, or empty string on failure.
        """
        try:
            result = self._run(
                "capture-pane", "-t", self.name, "-p", "-S", f"-{lines}"
            )
            return result.stdout if result.returncode == 0 else ""
        except Exception:
            return ""

    def send_keys(self, keys: str, *extra: str) -> bool:
        """Send keys to this tmux session.

        Extra arguments are passed directly to tmux send-keys (e.g. "C-m" for Enter).
        """
        try:
            cmd = ["send-keys", "-t", self.name, keys, *extra]
            result = self._run(*cmd)
            return result.returncode == 0
        except Exception:
            return False

    def kill(self) -> bool:
        """Kill this tmux session."""
        try:
            self._run("kill-session", "-t", self.name)
            return True
        except Exception:
            return False

    def get_shell_pid(self) -> str | None:
        """Get the shell PID for this session's first pane.

        Returns None if the session doesn't exist or PID can't be determined.
        """
        try:
            result = self._run("list-panes", "-t", self.name, "-F", "#{pane_pid}")
            if result.returncode == 0 and result.stdout.strip():
                return result.stdout.strip().split("\n")[0]
        except Exception:
            pass
        return None

    def get_session_age(self) -> int:
        """Get the age of this tmux session in seconds since creation.

        Returns -1 if the session doesn't exist or age can't be determined.
        """
        try:
            result = self._run(
                "display-message", "-t", self.name, "-p", "#{session_created}"
            )
            if result.returncode != 0 or not result.stdout.strip():
                return -1
            created_at = int(result.stdout.strip())
            if created_at == 0:
                return -1
            return int(time.time()) - created_at
        except Exception:
            return -1
