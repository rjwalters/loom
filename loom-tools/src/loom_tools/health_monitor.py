"""Forge-derived health monitor for the spawn-loop orchestrator (Phase 3.1.8).

Originally a daemon-state.json + health-metrics.json consumer that maintained a
24h time-series of throughput/latency/error metrics. After the shepherd/daemon
deprecation (#3372, tracker #3378), most of the inputs no longer exist:

- ``health-metrics.json`` and ``alerts.json`` retire (no time-series storage).
- ``daemon-metrics.json`` (iteration counts, success rate, avg duration) is
  daemon-brain-internal — no producer once daemon_v2 is deleted.
- ``daemon-state.json`` lifetime counters (``completed_issues``,
  ``total_prs_merged``) require persistent shepherd memory the spawn loop
  intentionally does not keep.

This port chooses a **simplified, point-in-time composite score** computed from
forge queries (`gh issue list` / `gh pr list` via `snapshot.collect_pipeline_data`)
plus `.loom/spawn-loop-state.json`. No history, no persistent alerts, no
acknowledgement state. The CLI returns a single snapshot per invocation — same
shape as `loom-status`, narrower focus.

See issue #3397 for the recipe-decision discussion.

Usage::

    loom-health-monitor              # Display health summary
    loom-health-monitor --json       # Output health status as JSON
    loom-health-monitor --alerts     # Show live alerts (computed from snapshot)

The retired flags ``--collect``, ``--acknowledge``, ``--clear-alerts``, and
``--history`` print a one-line deprecation note and exit non-zero so operators
don't silently rely on them. They can be removed in a follow-up.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import sys
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any

from loom_tools.common.config import env_int
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_spawn_loop_state
from loom_tools.common.time_utils import now_utc, parse_iso_timestamp
from loom_tools.models.spawn_loop_state import SpawnLoopState

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

# Heartbeat threshold matches loom-stuck-detection (#3411).
STUCK_HEARTBEAT_SECONDS = env_int("LOOM_STUCK_HEARTBEAT_SECONDS", 120)

# Score-deduction thresholds (env-overridable).
READY_QUEUE_HIGH = env_int("LOOM_HEALTH_READY_HIGH", 20)
READY_QUEUE_MEDIUM = env_int("LOOM_HEALTH_READY_MEDIUM", 10)
REVIEW_BACKLOG_HIGH = env_int("LOOM_HEALTH_REVIEW_HIGH", 10)
REVIEW_BACKLOG_MEDIUM = env_int("LOOM_HEALTH_REVIEW_MEDIUM", 5)
MERGE_CONFLICT_HIGH = env_int("LOOM_HEALTH_MERGE_CONFLICT_HIGH", 5)
MERGE_CONFLICT_MEDIUM = env_int("LOOM_HEALTH_MERGE_CONFLICT_MEDIUM", 3)

# ---------------------------------------------------------------------------
# ANSI color support
# ---------------------------------------------------------------------------

_RED = "\033[0;31m"
_GREEN = "\033[0;32m"
_YELLOW = "\033[1;33m"
_BLUE = "\033[0;34m"
_CYAN = "\033[0;36m"
_GRAY = "\033[0;90m"
_BOLD = "\033[1m"
_RESET = "\033[0m"


class _Colors:
    """Color palette that respects TTY detection."""

    def __init__(self, *, use_color: bool = True) -> None:
        if use_color:
            self.red, self.green, self.yellow = _RED, _GREEN, _YELLOW
            self.blue, self.cyan, self.gray = _BLUE, _CYAN, _GRAY
            self.bold, self.reset = _BOLD, _RESET
        else:
            self.red = self.green = self.yellow = ""
            self.blue = self.cyan = self.gray = ""
            self.bold = self.reset = ""


def _use_color() -> bool:
    try:
        return os.isatty(sys.stdout.fileno())
    except (OSError, ValueError, AttributeError):
        return False


# ---------------------------------------------------------------------------
# Snapshot data shape (the inputs to the composite score)
# ---------------------------------------------------------------------------


@dataclass
class HealthSnapshot:
    """Point-in-time inputs derived from forge + spawn-loop-state.

    Fields default to zero/empty so the score gracefully reports "100/100"
    in an idle, healthy workspace.
    """

    timestamp: str = ""
    # Issue queues (forge)
    ready_count: int = 0
    building_count: int = 0
    blocked_count: int = 0
    # PR queues (forge)
    review_requested_count: int = 0
    changes_requested_count: int = 0
    ready_to_merge_count: int = 0
    merge_conflict_count: int = 0
    # Spawn loop
    spawn_loop_present: bool = False
    running_tasks: int = 0
    stuck_tasks: int = 0  # tasks with stale (>= STUCK_HEARTBEAT_SECONDS) heartbeat
    # Diagnostic: building issues that no running task claims (orphan drift).
    orphan_building: int = 0


@dataclass
class Alert:
    """Live alert. Computed each invocation; not persisted."""

    type: str
    severity: str  # "info" | "warning" | "critical"
    message: str
    context: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "type": self.type,
            "severity": self.severity,
            "message": self.message,
            "context": self.context,
        }


# ---------------------------------------------------------------------------
# Data collection
# ---------------------------------------------------------------------------


def _count_stuck_tasks(
    spawn_loop_state: SpawnLoopState,
    *,
    now: datetime,
    threshold_seconds: int = STUCK_HEARTBEAT_SECONDS,
) -> int:
    """Count spawn-loop tasks whose heartbeat is older than the threshold.

    A task with no ``last_heartbeat`` field is counted as stuck only if its
    ``started_at`` is older than the threshold (otherwise it's a fresh spawn
    that hasn't received its first heartbeat yet).
    """
    stuck = 0
    for task in spawn_loop_state.running:
        anchor = task.last_heartbeat or task.started_at
        if not anchor:
            # No timestamp at all — can't classify. Don't count as stuck.
            continue
        try:
            ts = parse_iso_timestamp(anchor)
        except (ValueError, OSError):
            continue
        age = (now - ts).total_seconds()
        if age >= threshold_seconds:
            stuck += 1
    return stuck


def collect_snapshot(
    repo_root: pathlib.Path,
    *,
    _now: datetime | None = None,
    _pipeline_data: dict[str, Any] | None = None,
    _spawn_loop_state: SpawnLoopState | None = None,
) -> HealthSnapshot:
    """Build a HealthSnapshot from forge queries + spawn-loop-state.

    Pipeline data is fetched via :func:`snapshot.collect_pipeline_data` —
    the same parallel ``gh`` orchestrator used by ``loom-status``. Spawn-loop
    state is read from ``.loom/spawn-loop-state.json``.

    ``_pipeline_data`` and ``_spawn_loop_state`` are test-injection seams.
    """
    now = _now or now_utc()
    timestamp = now.strftime("%Y-%m-%dT%H:%M:%SZ")

    # Forge pipeline (10 parallel gh queries).
    if _pipeline_data is None:
        from loom_tools.snapshot import collect_pipeline_data

        try:
            pipeline = collect_pipeline_data(repo_root, ci_health_check_enabled=False)
        except Exception:
            pipeline = {}
    else:
        pipeline = _pipeline_data

    ready = pipeline.get("ready_issues", []) or []
    building = pipeline.get("building_issues", []) or []
    blocked = pipeline.get("blocked_issues", []) or []
    review_requested = pipeline.get("review_requested", []) or []
    changes_requested = pipeline.get("changes_requested", []) or []
    ready_to_merge = pipeline.get("ready_to_merge", []) or []

    # Merge-conflict subset of ready_to_merge — approved-but-blocked PRs.
    merge_conflict_count = sum(
        1
        for pr in ready_to_merge
        if any(
            (lbl.get("name") if isinstance(lbl, dict) else None) == "loom:merge-conflict"
            for lbl in pr.get("labels", []) or []
        )
    )

    # Spawn-loop state.
    sls = (
        _spawn_loop_state
        if _spawn_loop_state is not None
        else read_spawn_loop_state(repo_root)
    )
    running_issues = {task.issue for task in sls.running}
    running_tasks = len(sls.running)
    stuck = _count_stuck_tasks(sls, now=now)

    # Orphan-drift: loom:building issues that no running task is working.
    # Only meaningful when spawn loop is present (otherwise we don't know
    # which tasks "should" be running).
    orphan_building = 0
    if sls.present and running_tasks > 0:
        building_numbers = {
            int(item.get("number") or 0) for item in building if item.get("number")
        }
        orphan_building = len(building_numbers - running_issues)

    return HealthSnapshot(
        timestamp=timestamp,
        ready_count=len(ready),
        building_count=len(building),
        blocked_count=len(blocked),
        review_requested_count=len(review_requested),
        changes_requested_count=len(changes_requested),
        ready_to_merge_count=len(ready_to_merge),
        merge_conflict_count=merge_conflict_count,
        spawn_loop_present=sls.present,
        running_tasks=running_tasks,
        stuck_tasks=stuck,
        orphan_building=orphan_building,
    )


# ---------------------------------------------------------------------------
# Composite-score recipe (see PR body for rationale)
# ---------------------------------------------------------------------------


def calculate_health_score(snapshot: HealthSnapshot) -> int:
    """Compute a composite health score in [0, 100].

    Recipe (5 factors, max deduction 80; floor at 0):

    1. **Stuck tasks** (0-20): spawn-loop children with stale heartbeats.
       1: -10, 2: -15, >=3: -20.
    2. **Orphan building** (0-15): ``loom:building`` issues with no running
       task. 1: -5, 2: -10, >=3: -15.
    3. **Ready queue depth** (0-15): backlog absolute. Deducts when issues are
       waiting and nothing is running. >= READY_QUEUE_HIGH: -15,
       >= READY_QUEUE_MEDIUM: -10, >=1 (when running==0): -5.
    4. **Review backlog** (0-15): review-requested + changes-requested PRs.
       >= REVIEW_BACKLOG_HIGH: -15, >= REVIEW_BACKLOG_MEDIUM: -10, >=1: -3.
    5. **Merge-conflict backlog** (0-15): approved PRs that can't merge.
       >= MERGE_CONFLICT_HIGH: -15, >= MERGE_CONFLICT_MEDIUM: -10, >=1: -5.

    Total max deduction = 80, so a fully degraded system bottoms out at 20.
    The floor at 0 is symbolic — the recipe shouldn't reach it.
    """
    score = 100

    # Factor 1: Stuck tasks
    if snapshot.stuck_tasks >= 3:
        score -= 20
    elif snapshot.stuck_tasks == 2:
        score -= 15
    elif snapshot.stuck_tasks == 1:
        score -= 10

    # Factor 2: Orphan building (drift between loom:building and spawn loop)
    if snapshot.orphan_building >= 3:
        score -= 15
    elif snapshot.orphan_building == 2:
        score -= 10
    elif snapshot.orphan_building == 1:
        score -= 5

    # Factor 3: Ready queue depth
    if snapshot.ready_count >= READY_QUEUE_HIGH:
        score -= 15
    elif snapshot.ready_count >= READY_QUEUE_MEDIUM:
        score -= 10
    elif snapshot.ready_count >= 1 and snapshot.running_tasks == 0:
        # Issues piling up with no one working = mild penalty even at low N.
        score -= 5

    # Factor 4: Review backlog
    review_total = snapshot.review_requested_count + snapshot.changes_requested_count
    if review_total >= REVIEW_BACKLOG_HIGH:
        score -= 15
    elif review_total >= REVIEW_BACKLOG_MEDIUM:
        score -= 10
    elif review_total >= 1:
        score -= 3

    # Factor 5: Merge-conflict backlog
    if snapshot.merge_conflict_count >= MERGE_CONFLICT_HIGH:
        score -= 15
    elif snapshot.merge_conflict_count >= MERGE_CONFLICT_MEDIUM:
        score -= 10
    elif snapshot.merge_conflict_count >= 1:
        score -= 5

    return max(0, min(100, score))


def get_health_status(score: int) -> str:
    """Map numeric score to status label. Thresholds unchanged from v1."""
    if score >= 90:
        return "excellent"
    if score >= 70:
        return "good"
    if score >= 50:
        return "fair"
    if score >= 30:
        return "warning"
    return "critical"


# ---------------------------------------------------------------------------
# Alerts (live, not persisted)
# ---------------------------------------------------------------------------


def generate_alerts(snapshot: HealthSnapshot) -> list[Alert]:
    """Generate alerts from the current snapshot.

    Unlike the daemon-state version, alerts are recomputed on every CLI
    invocation. There is no acknowledgement state.
    """
    alerts: list[Alert] = []

    # Stuck tasks
    if snapshot.stuck_tasks >= 1:
        severity = "critical" if snapshot.stuck_tasks >= 3 else "warning"
        alerts.append(
            Alert(
                type="stuck_tasks",
                severity=severity,
                message=f"{snapshot.stuck_tasks} spawn-loop task(s) with stale heartbeats",
                context={
                    "stuck_count": snapshot.stuck_tasks,
                    "threshold_seconds": STUCK_HEARTBEAT_SECONDS,
                },
            )
        )

    # Orphan building
    if snapshot.orphan_building >= 1:
        severity = "warning" if snapshot.orphan_building < 3 else "critical"
        alerts.append(
            Alert(
                type="orphan_building",
                severity=severity,
                message=(
                    f"{snapshot.orphan_building} loom:building issue(s) "
                    "with no running spawn-loop task"
                ),
                context={"orphan_count": snapshot.orphan_building},
            )
        )

    # Pipeline stall: ready issues pending AND no running tasks AND no
    # ready-to-merge PRs. This is the spawn-loop equivalent of the old
    # "pipeline stalled" alert.
    if (
        snapshot.ready_count >= 1
        and snapshot.running_tasks == 0
        and snapshot.ready_to_merge_count == 0
    ):
        alerts.append(
            Alert(
                type="pipeline_stall",
                severity="warning",
                message=(
                    f"{snapshot.ready_count} ready issue(s) and no spawn-loop "
                    "task(s) running — is the loop alive?"
                ),
                context={"ready_count": snapshot.ready_count},
            )
        )

    # Review backlog
    review_total = snapshot.review_requested_count + snapshot.changes_requested_count
    if review_total >= REVIEW_BACKLOG_MEDIUM:
        severity = "critical" if review_total >= REVIEW_BACKLOG_HIGH else "warning"
        alerts.append(
            Alert(
                type="review_backlog",
                severity=severity,
                message=(
                    f"{review_total} PR(s) awaiting review/fixes "
                    f"({snapshot.review_requested_count} review-requested, "
                    f"{snapshot.changes_requested_count} changes-requested)"
                ),
                context={
                    "review_requested": snapshot.review_requested_count,
                    "changes_requested": snapshot.changes_requested_count,
                },
            )
        )

    # Merge-conflict backlog
    if snapshot.merge_conflict_count >= MERGE_CONFLICT_MEDIUM:
        severity = (
            "critical"
            if snapshot.merge_conflict_count >= MERGE_CONFLICT_HIGH
            else "warning"
        )
        alerts.append(
            Alert(
                type="merge_conflict_backlog",
                severity=severity,
                message=(
                    f"{snapshot.merge_conflict_count} approved PR(s) blocked "
                    "on merge conflicts"
                ),
                context={"merge_conflict_count": snapshot.merge_conflict_count},
            )
        )

    return alerts


# ---------------------------------------------------------------------------
# Display
# ---------------------------------------------------------------------------


def format_health_json(snapshot: HealthSnapshot) -> str:
    score = calculate_health_score(snapshot)
    status = get_health_status(score)
    alerts = generate_alerts(snapshot)
    output = {
        "timestamp": snapshot.timestamp,
        "health_score": score,
        "health_status": status,
        "snapshot": {
            "ready_count": snapshot.ready_count,
            "building_count": snapshot.building_count,
            "blocked_count": snapshot.blocked_count,
            "review_requested_count": snapshot.review_requested_count,
            "changes_requested_count": snapshot.changes_requested_count,
            "ready_to_merge_count": snapshot.ready_to_merge_count,
            "merge_conflict_count": snapshot.merge_conflict_count,
            "spawn_loop_present": snapshot.spawn_loop_present,
            "running_tasks": snapshot.running_tasks,
            "stuck_tasks": snapshot.stuck_tasks,
            "orphan_building": snapshot.orphan_building,
        },
        "alerts": [a.to_dict() for a in alerts],
    }
    return json.dumps(output, indent=2)


def format_health_human(snapshot: HealthSnapshot) -> str:
    score = calculate_health_score(snapshot)
    status = get_health_status(score)
    alerts = generate_alerts(snapshot)
    c = _Colors(use_color=_use_color())

    if score < 30:
        score_color = c.red
    elif score < 70:
        score_color = c.yellow
    elif score < 90:
        score_color = c.cyan
    else:
        score_color = c.green

    lines: list[str] = []
    lines.append("")
    lines.append(
        f"{c.bold}{c.cyan}======================================================================={c.reset}"
    )
    lines.append(f"{c.bold}{c.cyan}  LOOM HEALTH STATUS{c.reset}")
    lines.append(
        f"{c.bold}{c.cyan}======================================================================={c.reset}"
    )
    lines.append("")
    lines.append(
        f"  {c.bold}Health Score:{c.reset} {score_color}{score}/100{c.reset} ({status})"
    )
    lines.append(f"  {c.bold}Timestamp:{c.reset}    {snapshot.timestamp}")
    lines.append("")

    # Spawn loop section
    sl_status = (
        f"{c.green}present{c.reset}"
        if snapshot.spawn_loop_present
        else f"{c.gray}absent{c.reset}"
    )
    lines.append(f"  {c.bold}Spawn Loop:{c.reset}     {sl_status}")
    lines.append(
        f"  {c.bold}Running Tasks:{c.reset}  {snapshot.running_tasks}"
    )
    stuck_color = c.red if snapshot.stuck_tasks else c.green
    lines.append(
        f"  {c.bold}Stuck Tasks:{c.reset}    {stuck_color}{snapshot.stuck_tasks}{c.reset}"
    )
    if snapshot.orphan_building:
        lines.append(
            f"  {c.bold}Orphan Building:{c.reset} {c.yellow}{snapshot.orphan_building}{c.reset} "
            f"({c.gray}loom:building w/o running task{c.reset})"
        )
    lines.append("")

    # Pipeline section
    lines.append(f"  {c.bold}Issue Queues:{c.reset}")
    lines.append(
        f"    ready={snapshot.ready_count}, building={snapshot.building_count}, "
        f"blocked={snapshot.blocked_count}"
    )
    lines.append(f"  {c.bold}PR Queues:{c.reset}")
    lines.append(
        f"    review-requested={snapshot.review_requested_count}, "
        f"changes-requested={snapshot.changes_requested_count}, "
        f"ready-to-merge={snapshot.ready_to_merge_count}"
    )
    if snapshot.merge_conflict_count:
        lines.append(
            f"    {c.yellow}merge-conflict={snapshot.merge_conflict_count}{c.reset} "
            f"({c.gray}approved PRs blocked{c.reset})"
        )
    lines.append("")

    # Alerts section
    if alerts:
        lines.append(f"  {c.bold}Active Alerts:{c.reset} {c.yellow}{len(alerts)}{c.reset}")
        for a in alerts:
            sev_color = c.red if a.severity == "critical" else c.yellow
            lines.append(f"    [{sev_color}{a.severity}{c.reset}] {a.type}: {a.message}")
    else:
        lines.append(f"  {c.bold}Active Alerts:{c.reset} {c.green}none{c.reset}")
    lines.append("")
    lines.append(
        f"{c.bold}{c.cyan}======================================================================={c.reset}"
    )
    lines.append("")
    return "\n".join(lines)


def format_alerts_json(snapshot: HealthSnapshot) -> str:
    return json.dumps(
        {"alerts": [a.to_dict() for a in generate_alerts(snapshot)]},
        indent=2,
    )


def format_alerts_human(snapshot: HealthSnapshot) -> str:
    alerts = generate_alerts(snapshot)
    c = _Colors(use_color=_use_color())
    lines: list[str] = [
        "",
        f"{c.bold}{c.cyan}======================================================================={c.reset}",
        f"{c.bold}{c.cyan}  LOOM ALERTS (live){c.reset}",
        f"{c.bold}{c.cyan}======================================================================={c.reset}",
        "",
    ]
    if not alerts:
        lines.append(f"  {c.green}No active alerts{c.reset}")
    else:
        lines.append(f"  {c.yellow}{len(alerts)} active alert(s):{c.reset}")
        lines.append("")
        for a in alerts:
            sev_color = c.red if a.severity == "critical" else c.yellow
            lines.append(f"    [{sev_color}{a.severity}{c.reset}] {a.type}: {a.message}")
    lines.append("")
    lines.append(
        f"{c.bold}{c.cyan}======================================================================={c.reset}"
    )
    lines.append("")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


_RETIRED_FLAGS_MSG = (
    "loom-health-monitor: --collect, --history, --acknowledge, and --clear-alerts "
    "were retired in the Phase 3 port (issue #3397). The monitor is now a "
    "point-in-time snapshot — no persistent metrics, no acknowledgement state."
)


def main(argv: list[str] | None = None) -> int:
    """Main entry point for the health monitor CLI."""
    parser = argparse.ArgumentParser(
        description="Forge-derived health monitor for the spawn-loop orchestrator",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Commands:
    (default)              Display health summary
    --json                 Output as JSON
    --alerts               Show live alerts only

Environment Variables:
    LOOM_STUCK_HEARTBEAT_SECONDS    Stuck-task threshold (default: 120)
    LOOM_HEALTH_READY_HIGH          Ready queue HIGH threshold  (default: 20)
    LOOM_HEALTH_READY_MEDIUM        Ready queue MEDIUM threshold (default: 10)
    LOOM_HEALTH_REVIEW_HIGH         Review backlog HIGH threshold  (default: 10)
    LOOM_HEALTH_REVIEW_MEDIUM       Review backlog MEDIUM threshold (default: 5)
    LOOM_HEALTH_MERGE_CONFLICT_HIGH    Merge-conflict HIGH (default: 5)
    LOOM_HEALTH_MERGE_CONFLICT_MEDIUM  Merge-conflict MEDIUM (default: 3)
""",
    )
    parser.add_argument("--json", action="store_true", help="Output as JSON")
    parser.add_argument("--alerts", action="store_true", help="Show active alerts only")
    # Retired flags — accepted so existing operator scripts get a clear error
    # rather than an argparse "unrecognized argument" abort.
    parser.add_argument("--collect", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--acknowledge", metavar="ID", help=argparse.SUPPRESS)
    parser.add_argument("--clear-alerts", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument(
        "--history", nargs="?", const=1, type=int, metavar="HOURS", help=argparse.SUPPRESS
    )

    args = parser.parse_args(argv)

    if (
        args.collect
        or args.acknowledge
        or args.clear_alerts
        or args.history is not None
    ):
        print(_RETIRED_FLAGS_MSG, file=sys.stderr)
        return 2

    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        print("Error: Not in a git repository with .loom directory", file=sys.stderr)
        return 1

    snapshot = collect_snapshot(repo_root)

    if args.alerts:
        if args.json:
            print(format_alerts_json(snapshot))
        else:
            print(format_alerts_human(snapshot))
        return 0

    if args.json:
        print(format_health_json(snapshot))
    else:
        print(format_health_human(snapshot))
    return 0


if __name__ == "__main__":
    sys.exit(main())
