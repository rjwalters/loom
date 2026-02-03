"""Signal handling for daemon shutdown and session management."""

from __future__ import annotations

import os
import pathlib
from typing import TYPE_CHECKING

from loom_tools.common.logging import log_info, log_warning
from loom_tools.common.state import read_json_file

if TYPE_CHECKING:
    from loom_tools.daemon_v2.context import DaemonContext


def check_stop_signal(ctx: DaemonContext) -> bool:
    """Check if the stop signal file exists.

    Returns True if the daemon should stop.
    """
    return ctx.stop_signal.exists()


def check_session_conflict(ctx: DaemonContext) -> bool:
    """Check if another daemon has taken over the state file.

    Returns True if there is a session conflict (another daemon is running).
    """
    if not ctx.state_file.exists():
        return False

    try:
        data = read_json_file(ctx.state_file)
        if isinstance(data, list):
            return False

        file_session_id = data.get("daemon_session_id")
        if file_session_id and file_session_id != ctx.session_id:
            log_warning("SESSION CONFLICT: Another daemon has taken over the state file")
            log_warning(f"  Our session:    {ctx.session_id}")
            log_warning(f"  File session:   {file_session_id}")
            log_warning("  Yielding to the other daemon instance.")
            return True
    except Exception:
        pass

    return False


def check_existing_pid(ctx: DaemonContext) -> tuple[bool, int | None]:
    """Check if another daemon process is running.

    Returns (is_running, pid) where is_running is True if another daemon
    is running and pid is its process ID.
    """
    if not ctx.pid_file.exists():
        return False, None

    try:
        existing_pid = int(ctx.pid_file.read_text().strip())
        # Check if process is running
        os.kill(existing_pid, 0)
        return True, existing_pid
    except ProcessLookupError:
        # Process is not running, clean up stale PID file
        log_info("Removing stale PID file")
        ctx.pid_file.unlink(missing_ok=True)
        return False, None
    except ValueError:
        # Invalid PID file
        ctx.pid_file.unlink(missing_ok=True)
        return False, None


def write_pid_file(ctx: DaemonContext) -> None:
    """Write the current process ID to the PID file."""
    ctx.pid_file.parent.mkdir(parents=True, exist_ok=True)
    ctx.pid_file.write_text(str(os.getpid()))


def clear_stop_signal(ctx: DaemonContext) -> None:
    """Clear the stop signal file if it exists."""
    ctx.stop_signal.unlink(missing_ok=True)


def cleanup_on_exit(ctx: DaemonContext) -> None:
    """Clean up signal files on daemon exit."""
    try:
        ctx.stop_signal.unlink(missing_ok=True)
    except Exception:
        pass

    try:
        ctx.pid_file.unlink(missing_ok=True)
    except Exception:
        pass
