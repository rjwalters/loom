"""Main daemon event loop."""

from __future__ import annotations

import os
import shutil
import signal
import subprocess
import sys
import time
from typing import Any

from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.state import read_json_file, write_json_file
from loom_tools.common.time_utils import now_utc
from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.daemon_v2.exit_codes import DaemonExitCode
from loom_tools.daemon_v2.iteration import run_iteration
from loom_tools.daemon_v2.signals import (
    check_existing_pid,
    check_session_conflict,
    check_stop_signal,
    cleanup_on_exit,
    clear_stop_signal,
    write_pid_file,
)
from loom_tools.daemon_cleanup import handle_daemon_shutdown, handle_daemon_startup, load_config


def run(ctx: DaemonContext) -> int:
    """Run the daemon main loop.

    Returns an exit code from DaemonExitCode.
    """
    # 1. Check for existing daemon instance
    is_running, existing_pid = check_existing_pid(ctx)
    if is_running:
        log_error(f"Daemon loop already running (PID: {existing_pid})")
        log_info("Use --status to check status or stop the existing daemon first")
        return DaemonExitCode.SESSION_CONFLICT

    # 2. Run pre-flight checks
    preflight_errors = _run_preflight_checks(ctx)
    if preflight_errors:
        for err in preflight_errors:
            log_error(err)
        return DaemonExitCode.STARTUP_FAILED

    # 3. Write PID file
    write_pid_file(ctx)

    # 4. Setup signal handlers
    def signal_handler(signum: int, frame: Any) -> None:
        ctx.running = False

    signal.signal(signal.SIGINT, signal_handler)
    signal.signal(signal.SIGTERM, signal_handler)

    try:
        # 5. Rotate existing state file
        _rotate_state_file(ctx)

        # 6. Initialize state and metrics files
        _init_state_file(ctx)
        _init_metrics_file(ctx)

        # 7. Clear any existing stop signal
        clear_stop_signal(ctx)

        # 8. Print startup header
        _print_header(ctx)

        # 9. Run startup cleanup
        log_info("Running startup cleanup...")
        cleanup_config = load_config()
        handle_daemon_startup(ctx.repo_root, cleanup_config)

        # 10. Main loop
        while ctx.running:
            ctx.iteration += 1

            # Check for stop signal
            if check_stop_signal(ctx):
                log_info(f"Iteration {ctx.iteration}: SHUTDOWN_SIGNAL detected")
                break

            # Check for session conflict
            if check_session_conflict(ctx):
                log_warning("Yielding to other daemon instance. Exiting.")
                break

            # Run iteration
            log_info(f"Iteration {ctx.iteration}: Starting...")
            start_time = time.time()
            result = run_iteration(ctx)
            duration = int(time.time() - start_time)

            # Log result
            log_info(f"Iteration {ctx.iteration}: {result.summary} ({duration}s)")

            # Update metrics
            _update_metrics(ctx, result.status, duration, result.summary)

            # Check for shutdown in result
            if result.status == "shutdown" or "SHUTDOWN" in result.summary:
                break

            # Check stop signal again before sleeping
            if check_stop_signal(ctx):
                log_info("SHUTDOWN_SIGNAL detected after iteration")
                break

            # Sleep before next iteration
            log_info(f"Sleeping {ctx.config.poll_interval}s until next iteration...")
            time.sleep(ctx.config.poll_interval)

        # 11. Run shutdown cleanup
        log_info("Running shutdown cleanup...")
        handle_daemon_shutdown(ctx.repo_root, cleanup_config)

        log_success("Daemon loop completed gracefully")
        return DaemonExitCode.SUCCESS

    except Exception as e:
        log_error(f"Daemon error: {e}")
        return DaemonExitCode.ERROR

    finally:
        cleanup_on_exit(ctx)


def _run_preflight_checks(ctx: DaemonContext) -> list[str]:
    """Run pre-flight dependency checks.

    Returns a list of error messages. Empty list means all checks passed.
    """
    failures: list[str] = []

    # Check 1: claude CLI available
    if not shutil.which("claude"):
        failures.append("Error: 'claude' CLI not found in PATH")
        failures.append("Install Claude Code CLI: https://claude.ai/code")

    # Check 2: loom_tools module importable
    try:
        result = subprocess.run(
            [sys.executable, "-c", "import loom_tools"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode != 0:
            failures.append("Error: 'loom_tools' Python module not importable")
            failures.append(f"Run: pip install -e {ctx.repo_root / 'loom-tools'}")
    except (subprocess.TimeoutExpired, FileNotFoundError):
        failures.append("Error: Failed to verify loom_tools module")

    # Check 3: gh CLI available and authenticated
    if not shutil.which("gh"):
        failures.append("Error: 'gh' CLI not found in PATH")
        failures.append("Install GitHub CLI: https://cli.github.com/")
    else:
        try:
            result = subprocess.run(
                ["gh", "auth", "status"],
                capture_output=True,
                text=True,
                timeout=10,
            )
            if result.returncode != 0:
                failures.append("Error: 'gh' CLI not authenticated")
                failures.append("Run: gh auth login")
        except (subprocess.TimeoutExpired, FileNotFoundError):
            failures.append("Warning: Could not verify gh authentication")

    # Check 4: tmux available
    if not shutil.which("tmux"):
        failures.append("Error: 'tmux' not found in PATH")
        failures.append("Install tmux: brew install tmux (macOS)")

    return failures


def _rotate_state_file(ctx: DaemonContext) -> None:
    """Rotate existing state file if present."""
    if not ctx.state_file.exists():
        return

    log_info("Rotating previous daemon state...")

    # Try shell script first
    rotate_script = ctx.repo_root / ".loom" / "scripts" / "rotate-daemon-state.sh"
    if rotate_script.exists():
        try:
            result = subprocess.run(
                [str(rotate_script)],
                capture_output=True,
                timeout=30,
                cwd=ctx.repo_root,
            )
            if result.returncode == 0:
                log_info("State rotation complete (shell)")
                return
        except (subprocess.TimeoutExpired, Exception):
            pass

    # Fallback to Python-native rotation
    _rotate_state_python(ctx)


def _rotate_state_python(ctx: DaemonContext) -> None:
    """Python-native state rotation fallback."""
    loom_dir = ctx.repo_root / ".loom"
    max_archived = int(os.environ.get("LOOM_MAX_ARCHIVED_SESSIONS", "10"))

    try:
        data = read_json_file(ctx.state_file)
    except Exception:
        log_warning("State file unreadable, skipping rotation")
        return

    if not isinstance(data, dict):
        return

    # Skip if file has no useful data
    file_size = ctx.state_file.stat().st_size
    if file_size < 50:
        log_info(f"State file too small ({file_size} bytes), skipping rotation")
        return

    iteration = data.get("iteration", 0)
    has_shepherds = any(
        isinstance(v, dict) and v.get("issue") is not None
        for v in data.get("shepherds", {}).values()
    )
    has_completed = len(data.get("completed_issues", []))

    if iteration == 0 and not has_shepherds and has_completed == 0:
        log_info("State file has no useful data, skipping rotation")
        return

    # Find next session number
    session_num = 0
    while (loom_dir / f"{session_num:02d}-daemon-state.json").exists():
        session_num += 1
        if session_num >= 100:
            session_num = 0
            break

    # Prune old sessions
    archives = sorted(loom_dir.glob("[0-9][0-9]-daemon-state.json"))
    to_delete = len(archives) - max_archived + 1
    if to_delete > 0:
        for archive in archives[:to_delete]:
            archive.unlink(missing_ok=True)
            log_info(f"Pruned old archive: {archive.name}")

    # Add session summary before archiving
    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
    data["session_summary"] = {
        "session_id": session_num,
        "archived_at": timestamp,
        "issues_completed": has_completed,
        "prs_merged": data.get("total_prs_merged", 0),
        "total_iterations": iteration,
    }
    write_json_file(ctx.state_file, data)

    # Rename to archive
    archive_name = f"{session_num:02d}-daemon-state.json"
    archive_path = loom_dir / archive_name
    ctx.state_file.rename(archive_path)
    log_info(f"Archived: daemon-state.json -> {archive_name}")


def _init_state_file(ctx: DaemonContext) -> None:
    """Initialize or update the daemon state file."""
    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

    if ctx.state_file.exists():
        try:
            data = read_json_file(ctx.state_file)
            if isinstance(data, dict):
                data["force_mode"] = ctx.config.force_mode
                data["started_at"] = timestamp
                data["running"] = True
                data["iteration"] = 0
                data["daemon_session_id"] = ctx.session_id
                data["execution_mode"] = "direct"
                write_json_file(ctx.state_file, data)
                return
        except Exception:
            pass

    # Create fresh state file
    data = {
        "started_at": timestamp,
        "last_poll": None,
        "running": True,
        "iteration": 0,
        "force_mode": ctx.config.force_mode,
        "execution_mode": "direct",
        "daemon_session_id": ctx.session_id,
        "shepherds": {},
        "support_roles": {},
        "completed_issues": [],
        "total_prs_merged": 0,
        "systematic_failure": {
            "active": False,
            "pattern": "",
            "count": 0,
            "probe_count": 0,
        },
        "blocked_issue_retries": {},
        "recent_failures": [],
    }
    ctx.state_file.parent.mkdir(parents=True, exist_ok=True)
    write_json_file(ctx.state_file, data)


def _init_metrics_file(ctx: DaemonContext) -> None:
    """Initialize the metrics file."""
    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
    data = {
        "session_start": timestamp,
        "total_iterations": 0,
        "successful_iterations": 0,
        "failed_iterations": 0,
        "timeout_iterations": 0,
        "iteration_durations": [],
        "average_iteration_seconds": 0,
        "last_iteration": None,
        "health": {
            "status": "healthy",
            "consecutive_failures": 0,
            "last_success": None,
        },
    }
    write_json_file(ctx.metrics_file, data)


def _update_metrics(ctx: DaemonContext, status: str, duration: int, summary: str) -> None:
    """Update the metrics file after an iteration."""
    try:
        data = read_json_file(ctx.metrics_file)
        if not isinstance(data, dict):
            data = {}
    except Exception:
        data = {}

    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

    data["total_iterations"] = data.get("total_iterations", 0) + 1
    data["last_iteration"] = {
        "timestamp": timestamp,
        "duration_seconds": duration,
        "status": status,
        "summary": summary,
    }

    if status == "success":
        data["successful_iterations"] = data.get("successful_iterations", 0) + 1
        health = data.get("health", {})
        health["consecutive_failures"] = 0
        health["last_success"] = timestamp
        health["status"] = "healthy"
        data["health"] = health
    else:
        data["failed_iterations"] = data.get("failed_iterations", 0) + 1
        health = data.get("health", {})
        health["consecutive_failures"] = health.get("consecutive_failures", 0) + 1
        if health["consecutive_failures"] >= 3:
            health["status"] = "unhealthy"
        data["health"] = health

    # Update rolling average (keep last 100 durations)
    durations = data.get("iteration_durations", [])
    durations = (durations + [duration])[-100:]
    data["iteration_durations"] = durations
    if durations:
        data["average_iteration_seconds"] = sum(durations) // len(durations)

    write_json_file(ctx.metrics_file, data)


def _print_header(ctx: DaemonContext) -> None:
    """Print startup header."""
    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

    log_info("")
    log_info("=" * 67)
    log_info("  LOOM DAEMON - PYTHON IMPLEMENTATION")
    log_info("=" * 67)
    log_info(f"  Started: {timestamp}")
    log_info(f"  PID: {os.getpid()}")
    log_info(f"  Session ID: {ctx.session_id}")
    log_info(f"  Mode: {ctx.config.mode_display()}")
    log_info(f"  Poll interval: {ctx.config.poll_interval}s")
    log_info(f"  Max shepherds: {ctx.config.max_shepherds}")
    log_info(f"  PID file: {ctx.pid_file}")
    log_info(f"  State file: {ctx.state_file}")
    log_info(f"  Stop signal: {ctx.stop_signal}")
    log_info("=" * 67)
    log_info("")
