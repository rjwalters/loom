"""Query and display auto-recovery statistics.

This module provides a CLI (``loom-recovery-stats``) to analyze recovery events
logged by the shepherd phase validation. It helps diagnose when builders are
not completing their workflow normally.

Usage::

    loom-recovery-stats                     # Summary for past week
    loom-recovery-stats --period today      # Today's events only
    loom-recovery-stats --period month      # Past month
    loom-recovery-stats --period all        # All recorded events
    loom-recovery-stats --json              # JSON output
    loom-recovery-stats --verbose           # Show individual events

Recovery Types:
    - commit_and_pr: Worktree had uncommitted changes, recovery committed and created PR
    - pr_only: Worktree had unpushed commits, recovery pushed and created PR
    - add_label: PR existed but was missing loom:review-requested label
"""

from __future__ import annotations

import argparse
import json
import sys
from collections import Counter
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Sequence

from loom_tools.common.paths import LoomPaths
from loom_tools.common.repo import find_repo_root
from loom_tools.common.time_utils import parse_iso_timestamp


@dataclass
class RecoveryEvent:
    """A single recovery event."""

    timestamp: datetime
    issue: int
    recovery_type: str
    reason: str
    elapsed_seconds: int | None = None
    worktree_had_changes: bool = False
    commits_recovered: int = 0
    pr_number: int | None = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> RecoveryEvent | None:
        """Parse a recovery event from JSON dict. Returns None if invalid."""
        try:
            ts_str = data.get("timestamp", "")
            ts = parse_iso_timestamp(ts_str) if ts_str else None
            if ts is None:
                return None

            return cls(
                timestamp=ts,
                issue=int(data.get("issue", 0)),
                recovery_type=data.get("recovery_type", "unknown"),
                reason=data.get("reason", "unknown"),
                elapsed_seconds=data.get("elapsed_seconds"),
                worktree_had_changes=bool(data.get("worktree_had_changes", False)),
                commits_recovered=int(data.get("commits_recovered", 0)),
                pr_number=data.get("pr_number"),
            )
        except (ValueError, TypeError):
            return None


@dataclass
class RecoveryStats:
    """Aggregated recovery statistics."""

    period_start: datetime
    period_end: datetime
    total_events: int = 0
    by_type: dict[str, int] = field(default_factory=dict)
    by_reason: dict[str, int] = field(default_factory=dict)
    by_day: dict[str, int] = field(default_factory=dict)
    events: list[RecoveryEvent] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        """Convert to JSON-serializable dict."""
        return {
            "period": {
                "start": self.period_start.strftime("%Y-%m-%dT%H:%M:%SZ"),
                "end": self.period_end.strftime("%Y-%m-%dT%H:%M:%SZ"),
            },
            "summary": {
                "total_recovery_events": self.total_events,
                "by_type": self.by_type,
                "by_reason": self.by_reason,
            },
            "by_day": self.by_day,
            "events": [
                {
                    "timestamp": e.timestamp.strftime("%Y-%m-%dT%H:%M:%SZ"),
                    "issue": e.issue,
                    "recovery_type": e.recovery_type,
                    "reason": e.reason,
                    "pr_number": e.pr_number,
                }
                for e in self.events
            ],
        }


def load_recovery_events(repo_root: Path) -> list[RecoveryEvent]:
    """Load all recovery events from the metrics file."""
    paths = LoomPaths(repo_root)
    events_file = paths.recovery_events_file

    if not events_file.is_file():
        return []

    try:
        with open(events_file) as f:
            data = json.load(f)

        if not isinstance(data, list):
            return []

        events: list[RecoveryEvent] = []
        for item in data:
            event = RecoveryEvent.from_dict(item)
            if event:
                events.append(event)

        return events
    except (json.JSONDecodeError, OSError):
        return []


def compute_period_range(period: str) -> tuple[datetime, datetime]:
    """Compute start and end times for the given period."""
    now = datetime.now(timezone.utc)
    end = now

    if period == "today":
        start = now.replace(hour=0, minute=0, second=0, microsecond=0)
    elif period == "week":
        start = now - timedelta(days=7)
    elif period == "month":
        start = now - timedelta(days=30)
    elif period == "all":
        # Go back 10 years - effectively all time
        start = now - timedelta(days=3650)
    else:
        # Default to week
        start = now - timedelta(days=7)

    return start, end


def compute_stats(
    events: list[RecoveryEvent],
    period: str,
) -> RecoveryStats:
    """Compute statistics for the given period."""
    start, end = compute_period_range(period)

    # Filter events to period
    filtered = [e for e in events if start <= e.timestamp <= end]

    # Count by type
    by_type = Counter(e.recovery_type for e in filtered)

    # Count by reason
    by_reason = Counter(e.reason for e in filtered)

    # Count by day
    by_day: dict[str, int] = {}
    for e in filtered:
        day = e.timestamp.strftime("%Y-%m-%d")
        by_day[day] = by_day.get(day, 0) + 1

    # Sort by_day by date
    by_day = dict(sorted(by_day.items()))

    return RecoveryStats(
        period_start=start,
        period_end=end,
        total_events=len(filtered),
        by_type=dict(by_type),
        by_reason=dict(by_reason),
        by_day=by_day,
        events=sorted(filtered, key=lambda e: e.timestamp, reverse=True),
    )


def format_text_output(stats: RecoveryStats, verbose: bool = False) -> str:
    """Format stats as human-readable text."""
    lines: list[str] = []

    # Header
    lines.append("=" * 60)
    lines.append("RECOVERY STATISTICS")
    lines.append("=" * 60)
    lines.append(
        f"Period: {stats.period_start.strftime('%Y-%m-%d %H:%M')} to "
        f"{stats.period_end.strftime('%Y-%m-%d %H:%M')} UTC"
    )
    lines.append("")

    # Summary
    lines.append(f"Total recovery events: {stats.total_events}")
    lines.append("")

    # By type
    if stats.by_type:
        lines.append("By Recovery Type:")
        for rtype, count in sorted(stats.by_type.items(), key=lambda x: -x[1]):
            pct = (count / stats.total_events * 100) if stats.total_events > 0 else 0
            lines.append(f"  {rtype:20s}: {count:4d} ({pct:5.1f}%)")
        lines.append("")

    # By reason
    if stats.by_reason:
        lines.append("By Reason:")
        for reason, count in sorted(stats.by_reason.items(), key=lambda x: -x[1]):
            pct = (count / stats.total_events * 100) if stats.total_events > 0 else 0
            lines.append(f"  {reason:20s}: {count:4d} ({pct:5.1f}%)")
        lines.append("")

    # By day (last 7 days only in non-verbose mode)
    if stats.by_day:
        lines.append("By Day:")
        days_to_show = list(stats.by_day.items())
        if not verbose and len(days_to_show) > 7:
            days_to_show = days_to_show[-7:]
            lines.append("  (showing last 7 days, use --verbose for all)")
        for day, count in days_to_show:
            lines.append(f"  {day}: {count}")
        lines.append("")

    # Individual events (verbose only)
    if verbose and stats.events:
        lines.append("-" * 60)
        lines.append("Recent Events (newest first):")
        lines.append("-" * 60)
        for e in stats.events[:50]:  # Limit to 50 most recent
            pr_info = f" -> PR #{e.pr_number}" if e.pr_number else ""
            lines.append(
                f"  {e.timestamp.strftime('%Y-%m-%d %H:%M')} "
                f"Issue #{e.issue}: {e.recovery_type} ({e.reason}){pr_info}"
            )

    return "\n".join(lines)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

_HELP_TEXT = """\
loom-recovery-stats - Query auto-recovery statistics

USAGE:
    loom-recovery-stats                     Summary for past week
    loom-recovery-stats --period today      Today's events only
    loom-recovery-stats --period month      Past 30 days
    loom-recovery-stats --period all        All recorded events
    loom-recovery-stats --json              Output as JSON
    loom-recovery-stats --verbose           Show individual events

DESCRIPTION:
    Analyzes recovery events logged when the shepherd validation
    phase auto-recovers builder work (uncommitted changes, missing
    PR labels, etc.).

    High recovery rates (>5%) may indicate builder reliability issues.

RECOVERY TYPES:
    commit_and_pr   Worktree had uncommitted changes; recovery committed
                    the changes and created a PR
    pr_only         Worktree had unpushed commits; recovery pushed and
                    created a PR
    add_label       PR existed but was missing loom:review-requested label

DATA FILE:
    Events are stored in .loom/metrics/recovery-events.json
"""


def _parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="loom-recovery-stats",
        description="Query and display auto-recovery statistics",
        add_help=False,
    )
    parser.add_argument(
        "--period", "-p",
        choices=["today", "week", "month", "all"],
        default="week",
        help="Time period to analyze (default: week)",
    )
    parser.add_argument(
        "--json", "-j",
        action="store_true",
        dest="json_output",
        help="Output as JSON",
    )
    parser.add_argument(
        "--verbose", "-v",
        action="store_true",
        help="Show individual events",
    )
    parser.add_argument(
        "--help", "-h",
        action="store_true",
        help="Show this help message",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> None:
    """CLI entry point."""
    args = _parse_args(argv)

    if args.help:
        print(_HELP_TEXT)
        sys.exit(0)

    repo_root = find_repo_root()
    events = load_recovery_events(repo_root)
    stats = compute_stats(events, args.period)

    if args.json_output:
        print(json.dumps(stats.to_dict(), indent=2))
    else:
        print(format_text_output(stats, verbose=args.verbose))


if __name__ == "__main__":
    main()
