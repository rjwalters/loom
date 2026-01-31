"""Proactive health monitoring and alerting for Loom daemon.

Replaces the former ``health-check.sh`` (1,008 LOC) with a Python module that
reuses ``snapshot.build_snapshot()`` for data collection and the existing
``HealthMetrics`` / ``AlertsFile`` models for persistence.

This module is a **proactive monitoring** system (different from the diagnostic
``health_check.py``):

- Time-series metrics collection (throughput, latency, error rates)
- Composite health score computation (0-100)
- Alert generation and management (with acknowledgement)
- 24-hour historical data retention for trend analysis
- JSON metrics storage in ``.loom/health-metrics.json`` and ``.loom/alerts.json``

Usage::

    loom-health-monitor                    # Display health summary
    loom-health-monitor --json             # Output health status as JSON
    loom-health-monitor --collect          # Collect and store health metrics
    loom-health-monitor --alerts           # Show current alerts
    loom-health-monitor --acknowledge <id> # Acknowledge an alert
    loom-health-monitor --clear-alerts     # Clear all alerts
    loom-health-monitor --history [hours]  # Show metric history
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from datetime import datetime, timezone
from typing import Any, Sequence

from loom_tools.common.config import env_int
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import (
    read_alerts,
    read_daemon_state,
    read_health_metrics,
    read_json_file,
    write_json_file,
)
from loom_tools.common.time_utils import (
    format_duration,
    now_utc,
    parse_iso_timestamp,
)
from loom_tools.models.daemon_state import DaemonState
from loom_tools.models.health import (
    Alert,
    AlertsFile,
    ErrorRates,
    HealthMetrics,
    LatencyMetric,
    MetricEntry,
    PipelineHealthMetric,
    QueueDepths,
    ResourceUsage,
    ThroughputMetric,
)

# ---------------------------------------------------------------------------
# Configuration (env-overridable thresholds)
# ---------------------------------------------------------------------------

RETENTION_HOURS = env_int("LOOM_HEALTH_RETENTION_HOURS", 24)
THROUGHPUT_DECLINE_THRESHOLD = env_int("LOOM_THROUGHPUT_DECLINE_THRESHOLD", 50)
QUEUE_GROWTH_THRESHOLD = env_int("LOOM_QUEUE_GROWTH_THRESHOLD", 5)
STUCK_AGENT_THRESHOLD = env_int("LOOM_STUCK_AGENT_THRESHOLD", 10)
ERROR_RATE_THRESHOLD = env_int("LOOM_ERROR_RATE_THRESHOLD", 20)

# Maximum alerts to retain
MAX_ALERTS = 100

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
# Metric collection
# ---------------------------------------------------------------------------


def collect_current_metrics(
    repo_root: Any,
    *,
    _now: datetime | None = None,
) -> MetricEntry:
    """Collect current metrics from daemon state and snapshot data.

    Uses ``snapshot.build_snapshot()`` for authoritative pipeline data,
    and ``daemon-metrics.json`` for iteration-level statistics.
    """
    import pathlib

    repo_root = pathlib.Path(repo_root)
    now = _now or now_utc()
    timestamp = now.strftime("%Y-%m-%dT%H:%M:%SZ")

    # Get snapshot data (reuses the same logic as loom-status)
    snapshot_data: dict[str, Any] = {}
    try:
        from loom_tools.snapshot import build_snapshot

        snapshot_data = build_snapshot(repo_root=repo_root)
    except Exception:
        pass

    computed = snapshot_data.get("computed", {})
    pipeline_health = snapshot_data.get("pipeline_health", {})
    systematic_failure = snapshot_data.get("systematic_failure", {})
    usage = snapshot_data.get("usage", {})

    # Queue depths from snapshot
    queue_depths = QueueDepths(
        ready=computed.get("total_ready", 0),
        building=computed.get("total_building", 0),
        review_requested=computed.get("prs_awaiting_review", 0),
        changes_requested=computed.get("prs_needing_fixes", 0),
        ready_to_merge=computed.get("prs_ready_to_merge", 0),
    )

    # Shepherd and stale heartbeat data
    active_shepherds = computed.get("active_shepherds", 0)
    stale_heartbeats = computed.get("stale_heartbeat_count", 0)

    # Pipeline health
    pipeline_status = pipeline_health.get("status", "healthy")
    blocked_count = computed.get("total_blocked", 0)
    retryable_count = pipeline_health.get("retryable_count", 0)
    permanent_blocked_count = pipeline_health.get("permanent_blocked_count", 0)
    sys_failure_active = systematic_failure.get("active", False)

    # Daemon metrics from daemon-metrics.json
    daemon_metrics_path = repo_root / ".loom" / "daemon-metrics.json"
    daemon_metrics = read_json_file(daemon_metrics_path)
    if isinstance(daemon_metrics, list):
        daemon_metrics = {}

    session_percent = usage.get(
        "session_percent", daemon_metrics.get("session_percent", 0)
    )
    iteration_count = daemon_metrics.get("total_iterations", 0)
    avg_duration = daemon_metrics.get("average_iteration_seconds", 0)
    consecutive_failures = daemon_metrics.get("health", {}).get(
        "consecutive_failures", 0
    )
    successful = daemon_metrics.get("successful_iterations", 0)
    success_rate: float = 100.0
    if iteration_count > 0:
        success_rate = (successful * 100.0) / iteration_count

    # Throughput from daemon state
    daemon_state = read_daemon_state(repo_root)
    issues_per_hour: float = 0.0
    prs_per_hour: float = 0.0

    if daemon_state.started_at:
        try:
            started_dt = parse_iso_timestamp(daemon_state.started_at)
            hours_running = (now - started_dt).total_seconds() / 3600
            completed_count = len(daemon_state.completed_issues)
            prs_merged = daemon_state.total_prs_merged

            if hours_running > 0:
                issues_per_hour = completed_count / hours_running
                prs_per_hour = prs_merged / hours_running
        except (ValueError, OSError):
            pass

    return MetricEntry(
        timestamp=timestamp,
        throughput=ThroughputMetric(
            issues_per_hour=round(issues_per_hour, 2),
            prs_per_hour=round(prs_per_hour, 2),
        ),
        latency=LatencyMetric(avg_iteration_seconds=avg_duration),
        queue_depths=queue_depths,
        error_rates=ErrorRates(
            consecutive_failures=consecutive_failures,
            success_rate=round(success_rate, 1),
            stuck_agents=stale_heartbeats,
        ),
        resource_usage=ResourceUsage(
            active_shepherds=active_shepherds,
            session_percent=session_percent,
        ),
        pipeline_health=PipelineHealthMetric(
            status=pipeline_status,
            blocked_count=blocked_count,
            retryable_count=retryable_count,
            permanent_blocked_count=permanent_blocked_count,
            systematic_failure_active=sys_failure_active,
        ),
    )


# ---------------------------------------------------------------------------
# Health score computation
# ---------------------------------------------------------------------------


def calculate_health_score(
    metrics: HealthMetrics,
    *,
    throughput_decline_threshold: int = THROUGHPUT_DECLINE_THRESHOLD,
    queue_growth_threshold: int = QUEUE_GROWTH_THRESHOLD,
) -> int:
    """Calculate composite health score (0-100) from recent metrics.

    Scoring factors (max deduction):
    - Error rate: 0-25 points
    - Consecutive failures: 0-15 points
    - Stuck agents: 0-20 points
    - Queue growth: 0-15 points
    - Resource usage: 0-15 points
    - Throughput decline: 0-15 points
    - Pipeline stall: 0-20 points
    - Systematic failure: 0-15 points
    """
    if not metrics.metrics:
        return 100

    latest = metrics.metrics[-1]
    score = 100

    # Factor 1: Error rate (0-25 points)
    sr = latest.error_rates.success_rate
    if sr < 50:
        score -= 25
    elif sr < 70:
        score -= 15
    elif sr < 90:
        score -= 5

    # Factor 2: Consecutive failures (0-15 points)
    cf = latest.error_rates.consecutive_failures
    if cf >= 5:
        score -= 15
    elif cf >= 3:
        score -= 10
    elif cf >= 1:
        score -= 5

    # Factor 3: Stuck agents (0-20 points)
    stuck = latest.error_rates.stuck_agents
    if stuck >= 3:
        score -= 20
    elif stuck >= 2:
        score -= 15
    elif stuck >= 1:
        score -= 10

    # Factor 4: Queue growth (0-15 points)
    if len(metrics.metrics) >= 2:
        prev = metrics.metrics[-2]
        growth = latest.queue_depths.ready - prev.queue_depths.ready
        if growth >= queue_growth_threshold:
            score -= 15
        elif growth >= 3:
            score -= 10
        elif growth >= 1:
            score -= 5

    # Factor 5: Resource usage (0-15 points)
    sp = latest.resource_usage.session_percent
    if sp >= 95:
        score -= 15
    elif sp >= 90:
        score -= 10
    elif sp >= 80:
        score -= 5

    # Factor 6: Throughput decline (0-15 points)
    if len(metrics.metrics) >= 2:
        prev = metrics.metrics[-2]
        prev_throughput = prev.throughput.issues_per_hour
        cur_throughput = latest.throughput.issues_per_hour
        if prev_throughput > 0 and cur_throughput < prev_throughput:
            decline_pct = ((prev_throughput - cur_throughput) * 100) / prev_throughput
            if decline_pct >= throughput_decline_threshold:
                score -= 15
            elif decline_pct >= 30:
                score -= 10
            elif decline_pct >= 10:
                score -= 5

    # Factor 7: Pipeline stall (0-20 points)
    if latest.pipeline_health.status == "stalled":
        score -= 20
    elif latest.pipeline_health.status == "degraded":
        score -= 10

    # Factor 8: Systematic failure (0-15 points)
    if latest.pipeline_health.systematic_failure_active:
        score -= 15

    return max(0, min(100, score))


def get_health_status(score: int) -> str:
    """Map numeric score to status label."""
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
# Alert generation
# ---------------------------------------------------------------------------


def generate_alerts(
    metrics: HealthMetrics,
    *,
    _now: datetime | None = None,
) -> list[Alert]:
    """Generate alerts based on current metrics."""
    if not metrics.metrics:
        return []

    now = _now or now_utc()
    timestamp = now.strftime("%Y-%m-%dT%H:%M:%SZ")
    epoch = int(now.timestamp())
    latest = metrics.metrics[-1]
    alerts: list[Alert] = []

    # Stuck agents
    stuck = latest.error_rates.stuck_agents
    if stuck >= 1:
        severity = "critical" if stuck >= 3 else "warning"
        alerts.append(Alert(
            id=f"alert-stuck-{epoch}",
            type="stuck_agents",
            severity=severity,
            message=f"{stuck} agent(s) with stale heartbeats",
            timestamp=timestamp,
            context={"stuck_count": stuck},
        ))

    # Consecutive failures
    cf = latest.error_rates.consecutive_failures
    if cf >= 3:
        severity = "critical" if cf >= 5 else "warning"
        alerts.append(Alert(
            id=f"alert-failures-{epoch}",
            type="high_error_rate",
            severity=severity,
            message=f"{cf} consecutive iteration failures",
            timestamp=timestamp,
            context={"consecutive_failures": cf},
        ))

    # Resource exhaustion
    sp = latest.resource_usage.session_percent
    if sp >= 90:
        severity = "critical" if sp >= 97 else "warning"
        alerts.append(Alert(
            id=f"alert-resource-{epoch}",
            type="resource_exhaustion",
            severity=severity,
            message=f"Session budget at {sp}%",
            timestamp=timestamp,
            context={"session_percent": sp},
        ))

    # Pipeline stall
    if latest.pipeline_health.status == "stalled":
        blocked = latest.pipeline_health.blocked_count
        retryable = latest.pipeline_health.retryable_count
        permanent = latest.pipeline_health.permanent_blocked_count
        severity = "critical" if retryable == 0 else "warning"
        alerts.append(Alert(
            id=f"alert-pipeline-stall-{epoch}",
            type="pipeline_stall",
            severity=severity,
            message=(
                f"Pipeline stalled: {blocked} blocked "
                f"({retryable} retryable, {permanent} permanent)"
            ),
            timestamp=timestamp,
            context={
                "blocked_count": blocked,
                "retryable_count": retryable,
                "permanent_blocked_count": permanent,
            },
        ))

    # Systematic failure
    if latest.pipeline_health.systematic_failure_active:
        alerts.append(Alert(
            id=f"alert-systematic-failure-{epoch}",
            type="systematic_failure",
            severity="critical",
            message="Systematic failure detected - shepherd spawning paused",
            timestamp=timestamp,
        ))

    # Queue growth
    if len(metrics.metrics) >= 2:
        prev = metrics.metrics[-2]
        growth = latest.queue_depths.ready - prev.queue_depths.ready
        if growth >= QUEUE_GROWTH_THRESHOLD:
            alerts.append(Alert(
                id=f"alert-queue-{epoch}",
                type="queue_growth",
                severity="warning",
                message=f"Ready queue grew by {growth} (now {latest.queue_depths.ready})",
                timestamp=timestamp,
                context={"growth": growth, "current": latest.queue_depths.ready},
            ))

    return alerts


# ---------------------------------------------------------------------------
# Core operations
# ---------------------------------------------------------------------------


def collect_metrics(repo_root: Any, *, _now: datetime | None = None) -> str:
    """Collect metrics, update health score, generate alerts. Return summary."""
    import pathlib

    repo_root = pathlib.Path(repo_root)
    now = _now or now_utc()
    timestamp = now.strftime("%Y-%m-%dT%H:%M:%SZ")

    # Collect current metric
    entry = collect_current_metrics(repo_root, _now=now)

    # Read existing metrics
    health = read_health_metrics(repo_root)
    if not health.initialized_at:
        health.initialized_at = timestamp
        health.retention_hours = RETENTION_HOURS

    # Append new metric
    health.metrics.append(entry)

    # Prune old metrics
    cutoff = now.timestamp() - (RETENTION_HOURS * 3600)
    health.metrics = [
        m
        for m in health.metrics
        if _metric_epoch(m) > cutoff
    ]

    # Calculate health score
    score = calculate_health_score(health)
    status = get_health_status(score)
    health.health_score = score
    health.health_status = status
    health.last_updated = timestamp

    # Write metrics
    metrics_path = repo_root / ".loom" / "health-metrics.json"
    write_json_file(metrics_path, health.to_dict())

    # Generate and store alerts
    new_alerts = generate_alerts(health, _now=now)
    if new_alerts:
        alerts_file = read_alerts(repo_root)
        if not alerts_file.initialized_at:
            alerts_file.initialized_at = timestamp
        alerts_file.alerts.extend(new_alerts)
        # Keep only last MAX_ALERTS
        alerts_file.alerts = alerts_file.alerts[-MAX_ALERTS:]
        alerts_path = repo_root / ".loom" / "alerts.json"
        write_json_file(alerts_path, alerts_file.to_dict())

    return f"Metrics collected. Health score: {score} ({status})"


def _metric_epoch(entry: MetricEntry) -> float:
    """Convert metric timestamp to epoch seconds."""
    if not entry.timestamp:
        return 0.0
    try:
        dt = parse_iso_timestamp(entry.timestamp)
        return dt.timestamp()
    except (ValueError, OSError):
        return 0.0


def acknowledge_alert(repo_root: Any, alert_id: str) -> str:
    """Mark an alert as acknowledged. Returns status message."""
    import pathlib

    repo_root = pathlib.Path(repo_root)
    alerts_file = read_alerts(repo_root)

    found = False
    now = now_utc()
    timestamp = now.strftime("%Y-%m-%dT%H:%M:%SZ")

    for alert in alerts_file.alerts:
        if alert.id == alert_id:
            alert.acknowledged = True
            alert.acknowledged_at = timestamp
            found = True
            break

    if not found:
        return f"Alert not found: {alert_id}"

    alerts_path = repo_root / ".loom" / "alerts.json"
    write_json_file(alerts_path, alerts_file.to_dict())
    return f"Alert acknowledged: {alert_id}"


def clear_alerts(repo_root: Any) -> str:
    """Clear all alerts. Returns status message."""
    import pathlib

    repo_root = pathlib.Path(repo_root)
    now = now_utc()
    timestamp = now.strftime("%Y-%m-%dT%H:%M:%SZ")

    alerts_file = AlertsFile(initialized_at=timestamp)
    alerts_path = repo_root / ".loom" / "alerts.json"
    write_json_file(alerts_path, alerts_file.to_dict())
    return "All alerts cleared"


# ---------------------------------------------------------------------------
# Display functions
# ---------------------------------------------------------------------------


def format_health_json(repo_root: Any) -> str:
    """Format health status as JSON."""
    import pathlib

    repo_root = pathlib.Path(repo_root)
    health = read_health_metrics(repo_root)
    alerts_file = read_alerts(repo_root)

    unack_count = sum(1 for a in alerts_file.alerts if not a.acknowledged)

    latest: dict[str, Any] = {}
    if health.metrics:
        latest = health.metrics[-1].to_dict()

    output = {
        "health_score": health.health_score,
        "health_status": health.health_status,
        "last_updated": health.last_updated,
        "metric_count": len(health.metrics),
        "unacknowledged_alerts": unack_count,
        "total_alerts": len(alerts_file.alerts),
        "latest_metrics": latest,
        "metrics_history": [m.to_dict() for m in health.metrics],
    }
    return json.dumps(output, indent=2)


def format_health_human(repo_root: Any) -> str:
    """Format health status for human display."""
    import pathlib

    repo_root = pathlib.Path(repo_root)
    health = read_health_metrics(repo_root)
    alerts_file = read_alerts(repo_root)
    c = _Colors(use_color=_use_color())

    score = health.health_score
    status = health.health_status
    last_updated = health.last_updated or "never"
    metric_count = len(health.metrics)
    unack_count = sum(1 for a in alerts_file.alerts if not a.acknowledged)
    total_alerts = len(alerts_file.alerts)

    lines: list[str] = []
    lines.append("")
    lines.append(
        f"{c.bold}{c.cyan}======================================================================={c.reset}"
    )
    lines.append(
        f"{c.bold}{c.cyan}  LOOM HEALTH STATUS{c.reset}"
    )
    lines.append(
        f"{c.bold}{c.cyan}======================================================================={c.reset}"
    )
    lines.append("")

    # Score with color
    if score < 30:
        score_color = c.red
    elif score < 70:
        score_color = c.yellow
    else:
        score_color = c.green

    lines.append(
        f"  {c.bold}Health Score:{c.reset} {score_color}{score}/100{c.reset} ({status})"
    )
    lines.append(f"  {c.bold}Last Updated:{c.reset} {last_updated}")
    lines.append(f"  {c.bold}Metrics Stored:{c.reset} {metric_count} samples")
    lines.append("")

    # Alert summary
    if unack_count > 0:
        lines.append(
            f"  {c.bold}Alerts:{c.reset} {c.red}{unack_count} unacknowledged{c.reset} ({total_alerts} total)"
        )
    else:
        lines.append(
            f"  {c.bold}Alerts:{c.reset} {c.green}No unacknowledged alerts{c.reset} ({total_alerts} total)"
        )
    lines.append("")

    # Latest metrics
    if health.metrics:
        latest = health.metrics[-1]
        lines.append(f"  {c.bold}Current Metrics:{c.reset}")
        lines.append(
            f"    Throughput: {latest.throughput.issues_per_hour} issues/hr, "
            f"{latest.throughput.prs_per_hour} PRs/hr"
        )
        lines.append(
            f"    Queue Depths: ready={latest.queue_depths.ready}, "
            f"building={latest.queue_depths.building}, "
            f"review={latest.queue_depths.review_requested}"
        )
        lines.append(
            f"    Error Rates: {latest.error_rates.success_rate}% success, "
            f"{latest.error_rates.consecutive_failures} failures, "
            f"{latest.error_rates.stuck_agents} stuck"
        )
        lines.append(
            f"    Resources: {latest.resource_usage.active_shepherds} shepherds, "
            f"{latest.resource_usage.session_percent}% session"
        )
    else:
        lines.append(
            f"  {c.gray}No metrics collected yet. Run: loom-health-monitor --collect{c.reset}"
        )
    lines.append("")
    lines.append(
        f"{c.bold}{c.cyan}======================================================================={c.reset}"
    )
    lines.append("")
    return "\n".join(lines)


def format_alerts_json(repo_root: Any) -> str:
    """Format alerts as JSON."""
    import pathlib

    repo_root = pathlib.Path(repo_root)
    alerts_file = read_alerts(repo_root)
    return json.dumps(alerts_file.to_dict(), indent=2)


def format_alerts_human(repo_root: Any) -> str:
    """Format alerts for human display."""
    import pathlib

    repo_root = pathlib.Path(repo_root)
    alerts_file = read_alerts(repo_root)
    c = _Colors(use_color=_use_color())

    unack = [a for a in alerts_file.alerts if not a.acknowledged]

    lines: list[str] = []
    lines.append("")
    lines.append(
        f"{c.bold}{c.cyan}======================================================================={c.reset}"
    )
    lines.append(f"{c.bold}{c.cyan}  LOOM ALERTS{c.reset}")
    lines.append(
        f"{c.bold}{c.cyan}======================================================================={c.reset}"
    )
    lines.append("")

    if not unack:
        lines.append(f"  {c.green}No unacknowledged alerts{c.reset}")
    else:
        lines.append(f"  {c.yellow}{len(unack)} unacknowledged alert(s):{c.reset}")
        lines.append("")
        for a in unack:
            lines.append(f"    [{a.severity}] {a.type}: {a.message}")
            lines.append(f"      ID: {a.id}")
            lines.append(f"      Time: {a.timestamp}")
            lines.append("")

    lines.append(
        f"{c.bold}{c.cyan}======================================================================={c.reset}"
    )
    lines.append("")
    return "\n".join(lines)


def format_history_json(repo_root: Any, hours: int = 1) -> str:
    """Format metric history as JSON."""
    import pathlib

    repo_root = pathlib.Path(repo_root)
    health = read_health_metrics(repo_root)

    now = now_utc()
    cutoff = now.timestamp() - (hours * 3600)
    filtered = [m for m in health.metrics if _metric_epoch(m) > cutoff]

    output = {
        "initialized_at": health.initialized_at,
        "retention_hours": health.retention_hours,
        "metrics": [m.to_dict() for m in filtered],
        "health_score": health.health_score,
        "health_status": health.health_status,
        "last_updated": health.last_updated,
    }
    return json.dumps(output, indent=2)


def format_history_human(repo_root: Any, hours: int = 1) -> str:
    """Format metric history for human display."""
    import pathlib

    repo_root = pathlib.Path(repo_root)
    health = read_health_metrics(repo_root)
    c = _Colors(use_color=_use_color())

    now = now_utc()
    cutoff = now.timestamp() - (hours * 3600)
    filtered = [m for m in health.metrics if _metric_epoch(m) > cutoff]

    lines: list[str] = []
    lines.append("")
    lines.append(
        f"{c.bold}Metric History (last {hours} hour(s), {len(filtered)} samples):{c.reset}"
    )
    lines.append("")

    for m in filtered:
        lines.append(
            f"{m.timestamp}: "
            f"ready={m.queue_depths.ready}, "
            f"building={m.queue_depths.building}, "
            f"stuck={m.error_rates.stuck_agents}"
        )

    lines.append("")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    """Main entry point for the health monitor CLI."""
    parser = argparse.ArgumentParser(
        description="Proactive health monitoring for Loom daemon",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Commands:
    (default)              Display health summary
    --collect              Collect and store health metrics
    --alerts               Show current alerts
    --acknowledge <id>     Acknowledge an alert
    --clear-alerts         Clear all alerts
    --history [hours]      Show metric history (default: 1 hour)

Environment Variables:
    LOOM_HEALTH_RETENTION_HOURS       Metric retention period (default: 24)
    LOOM_THROUGHPUT_DECLINE_THRESHOLD Throughput decline % (default: 50)
    LOOM_QUEUE_GROWTH_THRESHOLD       Queue growth count (default: 5)
    LOOM_STUCK_AGENT_THRESHOLD        Stuck minutes (default: 10)
    LOOM_ERROR_RATE_THRESHOLD         Error rate % (default: 20)
""",
    )
    parser.add_argument("--json", action="store_true", help="Output as JSON")
    parser.add_argument(
        "--collect", action="store_true", help="Collect and store health metrics"
    )
    parser.add_argument("--alerts", action="store_true", help="Show current alerts")
    parser.add_argument("--acknowledge", metavar="ID", help="Acknowledge an alert")
    parser.add_argument(
        "--clear-alerts", action="store_true", help="Clear all alerts"
    )
    parser.add_argument(
        "--history",
        nargs="?",
        const=1,
        type=int,
        metavar="HOURS",
        help="Show metric history (default: 1 hour)",
    )

    args = parser.parse_args(argv)

    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        print("Error: Not in a git repository with .loom directory", file=sys.stderr)
        return 1

    if args.collect:
        msg = collect_metrics(repo_root)
        print(msg)
        return 0

    if args.acknowledge:
        msg = acknowledge_alert(repo_root, args.acknowledge)
        print(msg)
        return 0 if "acknowledged" in msg.lower() else 1

    if args.clear_alerts:
        msg = clear_alerts(repo_root)
        print(msg)
        return 0

    if args.alerts:
        if args.json:
            print(format_alerts_json(repo_root))
        else:
            print(format_alerts_human(repo_root))
        return 0

    if args.history is not None:
        hours = args.history
        if args.json:
            print(format_history_json(repo_root, hours))
        else:
            print(format_history_human(repo_root, hours))
        return 0

    # Default: show health status
    if args.json:
        print(format_health_json(repo_root))
    else:
        print(format_health_human(repo_root))
    return 0


if __name__ == "__main__":
    sys.exit(main())
