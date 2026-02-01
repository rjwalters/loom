"""CLI for managing baseline health status.

Used by the Auditor role to report main branch test health, and by
shepherds/operators to query the current status.

Usage:
    # Report main as healthy
    loom-baseline-health report --status healthy

    # Report main as failing with details
    loom-baseline-health report --status failing \\
        --test "test_cli_wrapper_health" \\
        --issue "#2042"

    # Check current status (for scripting)
    loom-baseline-health check
    # Exit codes: 0=healthy, 1=failing, 2=unknown/stale

    # Show current status (human-readable)
    loom-baseline-health show
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from datetime import datetime, timezone

from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_baseline_health, write_baseline_health
from loom_tools.models.baseline_health import BaselineHealth, FailingTest


def _get_main_head() -> str:
    """Get current HEAD commit hash."""
    try:
        result = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode == 0:
            return result.stdout.strip()
    except OSError:
        pass
    return ""


def _cmd_report(args: argparse.Namespace) -> int:
    """Report baseline health status."""
    repo_root = find_repo_root()

    failing_tests: list[FailingTest] = []
    if args.test:
        for name in args.test:
            failing_tests.append(FailingTest(name=name))

    health = BaselineHealth(
        status=args.status,
        checked_at=datetime.now(timezone.utc).isoformat(),
        main_commit=_get_main_head(),
        failing_tests=failing_tests,
        issue_tracking=args.issue or "",
        cache_ttl_minutes=args.ttl,
    )

    write_baseline_health(repo_root, health)

    print(f"Baseline health: {health.status}", file=sys.stderr)
    if failing_tests:
        for t in failing_tests:
            print(f"  - {t.name}", file=sys.stderr)
    if health.issue_tracking:
        print(f"Tracking: {health.issue_tracking}", file=sys.stderr)

    return 0


def _cmd_check(args: argparse.Namespace) -> int:
    """Check baseline health (for scripting).

    Exit codes:
        0: healthy
        1: failing
        2: unknown or stale
    """
    repo_root = find_repo_root()
    health = read_baseline_health(repo_root)

    if health.status == "healthy":
        return 0
    elif health.status == "failing":
        return 1
    else:
        return 2


def _cmd_show(args: argparse.Namespace) -> int:
    """Show current baseline health status."""
    repo_root = find_repo_root()
    health = read_baseline_health(repo_root)

    print(f"Status: {health.status}")
    if health.checked_at:
        print(f"Checked at: {health.checked_at}")
    if health.main_commit:
        print(f"Main commit: {health.main_commit[:12]}")
    if health.failing_tests:
        print("Failing tests:")
        for t in health.failing_tests:
            msg = f"  - {t.name}"
            if t.ecosystem:
                msg += f" ({t.ecosystem})"
            print(msg)
    if health.issue_tracking:
        print(f"Tracking: {health.issue_tracking}")
    print(f"Cache TTL: {health.cache_ttl_minutes}min")

    return 0


def main(argv: list[str] | None = None) -> int:
    """Entry point for loom-baseline-health CLI."""
    parser = argparse.ArgumentParser(
        prog="loom-baseline-health",
        description="Manage baseline health status for shepherd pre-flight checks",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    # report subcommand
    report_parser = subparsers.add_parser(
        "report", help="Report baseline health status"
    )
    report_parser.add_argument(
        "--status",
        required=True,
        choices=["healthy", "failing", "unknown"],
        help="Baseline health status",
    )
    report_parser.add_argument(
        "--test",
        action="append",
        help="Name of a failing test (can be repeated)",
    )
    report_parser.add_argument(
        "--issue",
        help="Issue tracking the failure (e.g., '#2042')",
    )
    report_parser.add_argument(
        "--ttl",
        type=int,
        default=15,
        help="Cache TTL in minutes (default: 15)",
    )

    # check subcommand
    subparsers.add_parser("check", help="Check baseline health (exit code)")

    # show subcommand
    subparsers.add_parser("show", help="Show current baseline health")

    args = parser.parse_args(argv)

    commands = {
        "report": _cmd_report,
        "check": _cmd_check,
        "show": _cmd_show,
    }

    return commands[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
