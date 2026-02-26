"""CLI entry point for the daemon."""

from __future__ import annotations

import argparse
import os
import sys
from typing import Sequence

from loom_tools.common.logging import log_error, log_info, log_success
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_json_file
from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.daemon_v2.exit_codes import DaemonExitCode
from loom_tools.daemon_v2.loop import run


def show_status(repo_root) -> int:
    """Show daemon status and exit."""
    pid_file = repo_root / ".loom" / "daemon-loop.pid"
    state_file = repo_root / ".loom" / "daemon-state.json"

    if pid_file.exists():
        try:
            pid = int(pid_file.read_text().strip())
            os.kill(pid, 0)  # Check if process exists
            print(f"Daemon loop running (PID: {pid})")

            if state_file.exists():
                data = read_json_file(state_file)
                if isinstance(data, dict):
                    session_id = data.get("daemon_session_id", "unknown")
                    iteration = data.get("iteration", 0)
                    force_mode = data.get("force_mode", False)
                    auto_build = data.get("auto_build", False)
                    timeout_at = data.get("timeout_at")
                    print(f"  Session ID: {session_id}")
                    print(f"  Iteration: {iteration}")
                    print(f"  Force mode: {force_mode}")
                    print(f"  Auto-build: {auto_build}")
                    if timeout_at:
                        print(f"  Timeout at: {timeout_at}")
            return 0
        except ProcessLookupError:
            print("Daemon loop not running (stale PID file)")
            pid_file.unlink(missing_ok=True)
            return 1
        except ValueError:
            print("Daemon loop not running (invalid PID file)")
            pid_file.unlink(missing_ok=True)
            return 1

    print("Daemon loop not running")
    return 1


def show_health(repo_root) -> int:
    """Show daemon health status and exit."""
    metrics_file = repo_root / ".loom" / "daemon-metrics.json"
    pid_file = repo_root / ".loom" / "daemon-loop.pid"

    if not metrics_file.exists():
        print("Daemon: not running (no metrics file)")
        return 1

    # Check running status
    running_status = "stopped"
    if pid_file.exists():
        try:
            pid = int(pid_file.read_text().strip())
            os.kill(pid, 0)
            running_status = f"running (PID: {pid})"
        except (ProcessLookupError, ValueError):
            pass

    # Load metrics
    data = read_json_file(metrics_file)
    if not isinstance(data, dict):
        print("Daemon: metrics file invalid")
        return 1

    health = data.get("health", {})
    health_status = health.get("status", "unknown")
    total_iterations = data.get("total_iterations", 0)
    consecutive_failures = health.get("consecutive_failures", 0)
    avg_duration = data.get("average_iteration_seconds", 0)
    last_iteration = data.get("last_iteration", {})
    last_status = last_iteration.get("status", "none") if last_iteration else "none"
    last_duration = last_iteration.get("duration_seconds", 0) if last_iteration else 0

    # Calculate success rate
    if total_iterations > 0:
        successful = data.get("successful_iterations", 0)
        success_rate = (successful * 100) // total_iterations
    else:
        success_rate = "n/a"

    # Format health display
    health_display = health_status
    if health_status == "unhealthy":
        health_display = f"{health_status} ({consecutive_failures} consecutive failures)"

    print(f"Daemon: {running_status}")
    print(f"Health: {health_display}")
    print(f"Iterations: {total_iterations} ({success_rate}% success)")
    print(f"Avg duration: {avg_duration}s")
    print(f"Last iteration: {last_status} ({last_duration}s)")

    # Show health monitoring metrics if available
    health_metrics = repo_root / ".loom" / "health-metrics.json"
    if health_metrics.exists():
        hm_data = read_json_file(health_metrics)
        if isinstance(hm_data, dict):
            health_score = hm_data.get("health_score", "?")
            health_monitor_status = hm_data.get("health_status", "?")
            print(f"Health score: {health_score}/100 ({health_monitor_status})")

    if health_status == "unhealthy":
        return 2
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    """Main entry point for the daemon CLI."""
    parser = argparse.ArgumentParser(
        prog="loom-daemon",
        description="Loom orchestration daemon - autonomous development orchestrator",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Environment Variables:
    LOOM_TIMEOUT_MIN            Stop daemon after N minutes (default: 0 = no timeout)
    LOOM_POLL_INTERVAL          Seconds between iterations (default: 120)
    LOOM_MAX_SHEPHERDS          Maximum concurrent shepherds (default: 10)
    LOOM_ISSUE_THRESHOLD        Trigger work generation when issues < this (default: 3)
    LOOM_AUTO_BUILD             Enable shepherd auto-spawning (default: false)
    LOOM_ARCHITECT_COOLDOWN     Seconds between architect triggers (default: 1800)
    LOOM_HERMIT_COOLDOWN        Seconds between hermit triggers (default: 1800)
    LOOM_GUIDE_INTERVAL         Guide respawn interval (default: 900)
    LOOM_CHAMPION_INTERVAL      Champion respawn interval (default: 600)
    LOOM_DOCTOR_INTERVAL        Doctor respawn interval (default: 300)
    LOOM_AUDITOR_INTERVAL       Auditor respawn interval (default: 600)
    LOOM_JUDGE_INTERVAL         Judge respawn interval (default: 300)

To stop the daemon gracefully:
    touch .loom/stop-daemon

Examples:
    loom-daemon                 # Start in support-only mode (no auto-spawn)
    loom-daemon --auto-build    # Auto-spawn shepherds from loom:issue queue
    loom-daemon --force         # Force mode (auto-promote, auto-merge, auto-build)
    loom-daemon -t 180          # Run for 3 hours then gracefully stop
    loom-daemon --merge -t 60   # Merge mode for 1 hour
    loom-daemon --status        # Check if daemon is running
    loom-daemon --health        # Show daemon health
""",
    )

    parser.add_argument(
        "--auto-build", "-a",
        action="store_true",
        help="Enable automatic shepherd spawning from loom:issue queue",
    )
    parser.add_argument(
        "--force", "-f",
        action="store_true",
        help="Enable force mode for aggressive autonomous development (implies --auto-build)",
    )
    parser.add_argument(
        "--merge", "-m",
        action="store_true",
        help="Alias for --force (for CLI parity with /loom --merge)",
    )
    parser.add_argument(
        "--debug", "-d",
        action="store_true",
        help="Enable debug mode for verbose logging",
    )
    parser.add_argument(
        "--timeout-min", "-t",
        type=int,
        default=0,
        metavar="N",
        help="Stop daemon after N minutes (0 = no timeout)",
    )
    parser.add_argument(
        "--status",
        action="store_true",
        help="Check if daemon loop is running",
    )
    parser.add_argument(
        "--health",
        action="store_true",
        help="Show daemon health status and exit",
    )

    args = parser.parse_args(argv)

    # Find repo root
    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        log_info("Run this command from a Loom-enabled repository root")
        return DaemonExitCode.STARTUP_FAILED

    # Handle status/health flags
    if args.status:
        return show_status(repo_root)

    if args.health:
        return show_health(repo_root)

    # Create config (--merge is alias for --force; --force/--merge imply --auto-build)
    force_mode = args.force or args.merge
    auto_build = getattr(args, "auto_build", False)
    config = DaemonConfig.from_env(
        force_mode=force_mode,
        auto_build=auto_build,
        debug_mode=args.debug,
        timeout_min=args.timeout_min,
    )

    # Create context and run
    ctx = DaemonContext(
        config=config,
        repo_root=repo_root,
    )

    return run(ctx)


if __name__ == "__main__":
    sys.exit(main())
