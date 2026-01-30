#!/usr/bin/env python3
"""Health monitoring and alerting for Loom daemon.

This module provides proactive health monitoring for the Loom daemon by:
- Tracking throughput, latency, and error metrics over time
- Computing a composite health score (0-100)
- Generating alerts when metrics cross thresholds
- Maintaining historical data for trend analysis (24-hour retention)

The health system is designed to:
- Detect degradation patterns before they become critical
- Enable extended unattended autonomous operation
- Integrate with existing daemon-state.json and daemon-snapshot.sh

Health metrics are stored in .loom/health-metrics.json
Alerts are stored in .loom/alerts.json

Usage:
    health-check.py                    # Display health summary
    health-check.py --json             # Output health status as JSON
    health-check.py --collect          # Collect and store health metrics
    health-check.py --history [hours]  # Show metric history (default: 1 hour)
    health-check.py --alerts           # Show current alerts
    health-check.py --alerts --json    # Show alerts as JSON
    health-check.py --acknowledge <id> # Acknowledge an alert
    health-check.py --clear-alerts     # Clear all alerts
    health-check.py --help             # Show help
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_json_file, write_json_file
from loom_tools.common.time_utils import parse_iso_timestamp
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

# Configuration defaults (can be overridden via environment)
RETENTION_HOURS = int(os.environ.get("LOOM_HEALTH_RETENTION_HOURS", "24"))
THROUGHPUT_DECLINE_THRESHOLD = int(
    os.environ.get("LOOM_THROUGHPUT_DECLINE_THRESHOLD", "50")
)
QUEUE_GROWTH_THRESHOLD = int(os.environ.get("LOOM_QUEUE_GROWTH_THRESHOLD", "5"))
STUCK_AGENT_THRESHOLD = int(os.environ.get("LOOM_STUCK_AGENT_THRESHOLD", "10"))
ERROR_RATE_THRESHOLD = int(os.environ.get("LOOM_ERROR_RATE_THRESHOLD", "20"))

# ANSI colors (disabled if stdout is not a terminal)
if sys.stdout.isatty():
    RED = "\033[0;31m"
    GREEN = "\033[0;32m"
    YELLOW = "\033[1;33m"
    BLUE = "\033[0;34m"
    CYAN = "\033[0;36m"
    GRAY = "\033[0;90m"
    BOLD = "\033[1m"
    NC = "\033[0m"
else:
    RED = GREEN = YELLOW = BLUE = CYAN = GRAY = BOLD = NC = ""


def get_timestamp() -> str:
    """Get current timestamp in ISO format."""
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def timestamp_to_epoch(timestamp: str) -> int:
    """Convert ISO timestamp to epoch seconds."""
    if not timestamp or timestamp == "null":
        return 0
    try:
        dt = parse_iso_timestamp(timestamp)
        return int(dt.timestamp())
    except (ValueError, OSError):
        return 0


def init_health_metrics(repo_root: Path) -> HealthMetrics:
    """Initialize health metrics file if it doesn't exist."""
    metrics_file = repo_root / ".loom" / "health-metrics.json"
    if metrics_file.exists():
        data = read_json_file(metrics_file)
        if isinstance(data, dict):
            return HealthMetrics.from_dict(data)

    timestamp = get_timestamp()
    metrics = HealthMetrics(
        initialized_at=timestamp,
        retention_hours=RETENTION_HOURS,
        metrics=[],
        health_score=100,
        health_status="excellent",
        last_updated=timestamp,
    )
    write_json_file(metrics_file, metrics.to_dict())
    return metrics


def init_alerts(repo_root: Path) -> AlertsFile:
    """Initialize alerts file if it doesn't exist."""
    alerts_file = repo_root / ".loom" / "alerts.json"
    if alerts_file.exists():
        data = read_json_file(alerts_file)
        if isinstance(data, dict):
            return AlertsFile.from_dict(data)

    timestamp = get_timestamp()
    alerts = AlertsFile(
        initialized_at=timestamp,
        alerts=[],
        acknowledged=[],
    )
    write_json_file(alerts_file, alerts.to_dict())
    return alerts


def collect_current_metrics(repo_root: Path) -> MetricEntry:
    """Collect current metrics from daemon state and snapshot."""
    timestamp = get_timestamp()

    # Get snapshot data (if daemon-snapshot.sh exists)
    snapshot: dict[str, Any] = {}
    snapshot_script = repo_root / ".loom" / "scripts" / "daemon-snapshot.sh"
    if snapshot_script.exists() and os.access(snapshot_script, os.X_OK):
        try:
            result = subprocess.run(
                [str(snapshot_script)],
                capture_output=True,
                text=True,
                timeout=30,
                cwd=repo_root,
            )
            if result.returncode == 0 and result.stdout.strip():
                snapshot = json.loads(result.stdout)
        except (subprocess.SubprocessError, json.JSONDecodeError):
            pass

    # Extract metrics from snapshot
    computed = snapshot.get("computed", {})
    ready_count = computed.get("total_ready", 0)
    building_count = computed.get("total_building", 0)
    review_count = computed.get("prs_awaiting_review", 0)
    changes_count = computed.get("prs_needing_fixes", 0)
    merge_count = computed.get("prs_ready_to_merge", 0)

    # Get shepherd status
    active_shepherds = computed.get("active_shepherds", 0)
    stale_heartbeats = computed.get("stale_heartbeat_count", 0)

    # Get pipeline health from snapshot
    pipeline_health = snapshot.get("pipeline_health", {})
    pipeline_health_status = pipeline_health.get("status", "healthy")
    blocked_count = computed.get("total_blocked", 0)
    retryable_count = pipeline_health.get("retryable_count", 0)
    permanent_blocked_count = pipeline_health.get("permanent_blocked_count", 0)

    # Get systematic failure status
    sys_failure = snapshot.get("systematic_failure", {})
    systematic_failure_active = sys_failure.get("active", False)

    # Get daemon metrics if available
    session_percent = 0.0
    iteration_count = 0
    avg_duration = 0.0
    success_rate = 100.0
    consecutive_failures = 0

    daemon_metrics_file = repo_root / ".loom" / "daemon-metrics.json"
    if daemon_metrics_file.exists():
        try:
            daemon_metrics_data = read_json_file(daemon_metrics_file)
            if isinstance(daemon_metrics_data, dict):
                session_percent = float(
                    daemon_metrics_data.get("session_percent", 0)
                )
                iteration_count = int(
                    daemon_metrics_data.get("total_iterations", 0)
                )
                avg_duration = float(
                    daemon_metrics_data.get("average_iteration_seconds", 0)
                )
                health_data = daemon_metrics_data.get("health", {})
                consecutive_failures = int(
                    health_data.get("consecutive_failures", 0)
                )

                successful = int(
                    daemon_metrics_data.get("successful_iterations", 0)
                )
                if iteration_count > 0:
                    success_rate = (successful * 100) / iteration_count
        except (ValueError, TypeError):
            pass

    # Get usage data from snapshot (may override daemon_metrics)
    usage = snapshot.get("usage", {})
    if "session_percent" in usage:
        session_percent = float(usage["session_percent"])

    # Calculate throughput from daemon state (completed issues in last hour)
    issues_per_hour = 0.0
    prs_per_hour = 0.0

    daemon_state_file = repo_root / ".loom" / "daemon-state.json"
    if daemon_state_file.exists():
        try:
            state_data = read_json_file(daemon_state_file)
            if isinstance(state_data, dict):
                started_at = state_data.get("started_at", "")
                completed_count = len(state_data.get("completed_issues", []))
                prs_merged = state_data.get("total_prs_merged", 0)

                if started_at:
                    started_epoch = timestamp_to_epoch(started_at)
                    now_epoch = int(time.time())
                    hours_running = (now_epoch - started_epoch) / 3600

                    if hours_running > 0:
                        issues_per_hour = completed_count / hours_running
                        prs_per_hour = prs_merged / hours_running
                    elif hours_running == 0 and completed_count > 0:
                        # Less than an hour, use actual counts
                        issues_per_hour = float(completed_count)
                        prs_per_hour = float(prs_merged)
        except (ValueError, TypeError):
            pass

    return MetricEntry(
        timestamp=timestamp,
        throughput=ThroughputMetric(
            issues_per_hour=issues_per_hour,
            prs_per_hour=prs_per_hour,
        ),
        latency=LatencyMetric(avg_iteration_seconds=avg_duration),
        queue_depths=QueueDepths(
            ready=ready_count,
            building=building_count,
            review_requested=review_count,
            changes_requested=changes_count,
            ready_to_merge=merge_count,
        ),
        error_rates=ErrorRates(
            consecutive_failures=consecutive_failures,
            success_rate=success_rate,
            stuck_agents=stale_heartbeats,
        ),
        resource_usage=ResourceUsage(
            active_shepherds=active_shepherds,
            session_percent=session_percent,
        ),
        pipeline_health=PipelineHealthMetric(
            status=pipeline_health_status,
            blocked_count=blocked_count,
            retryable_count=retryable_count,
            permanent_blocked_count=permanent_blocked_count,
            systematic_failure_active=systematic_failure_active,
        ),
    )


def calculate_health_score(metrics: HealthMetrics) -> int:
    """Calculate health score from recent metrics.

    Returns 0-100 based on 7 factors:
    - Factor 1: Error rate (0-25 points deduction)
    - Factor 2: Consecutive failures (0-15 points deduction)
    - Factor 3: Stuck agents (0-20 points deduction)
    - Factor 4: Queue growth (0-15 points deduction)
    - Factor 5: Resource usage (0-15 points deduction)
    - Factor 6: Throughput decline (0-15 points deduction)
    - Factor 7: Pipeline stall (0-20 points deduction)
    - Factor 8: Systematic failure (0-15 points deduction)
    """
    # Start with perfect score
    score = 100

    if not metrics.metrics:
        return score

    latest = metrics.metrics[-1]

    # Factor 1: Error rate (0-25 points deduction)
    success_rate = latest.error_rates.success_rate
    if success_rate < 50:
        score -= 25
    elif success_rate < 70:
        score -= 15
    elif success_rate < 90:
        score -= 5

    # Factor 2: Consecutive failures (0-15 points deduction)
    consecutive_failures = latest.error_rates.consecutive_failures
    if consecutive_failures >= 5:
        score -= 15
    elif consecutive_failures >= 3:
        score -= 10
    elif consecutive_failures >= 1:
        score -= 5

    # Factor 3: Stuck agents (0-20 points deduction)
    stuck_agents = latest.error_rates.stuck_agents
    if stuck_agents >= 3:
        score -= 20
    elif stuck_agents >= 2:
        score -= 15
    elif stuck_agents >= 1:
        score -= 10

    # Factor 4: Queue growth (0-15 points deduction)
    current_ready = latest.queue_depths.ready
    prev_ready = 0
    if len(metrics.metrics) >= 2:
        prev_ready = metrics.metrics[-2].queue_depths.ready

    queue_growth = current_ready - prev_ready
    if queue_growth >= QUEUE_GROWTH_THRESHOLD:
        score -= 15
    elif queue_growth >= 3:
        score -= 10
    elif queue_growth >= 1:
        score -= 5

    # Factor 5: Resource usage (0-15 points deduction)
    session_percent = latest.resource_usage.session_percent
    if session_percent >= 95:
        score -= 15
    elif session_percent >= 90:
        score -= 10
    elif session_percent >= 80:
        score -= 5

    # Factor 6: Throughput decline (0-15 points deduction)
    current_throughput = latest.throughput.issues_per_hour
    prev_throughput = 0.0
    if len(metrics.metrics) >= 2:
        prev_throughput = metrics.metrics[-2].throughput.issues_per_hour

    if prev_throughput > 0 and current_throughput < prev_throughput:
        decline_percent = int(
            ((prev_throughput - current_throughput) * 100) / prev_throughput
        )
        if decline_percent >= THROUGHPUT_DECLINE_THRESHOLD:
            score -= 15
        elif decline_percent >= 30:
            score -= 10
        elif decline_percent >= 10:
            score -= 5

    # Factor 7: Pipeline stall (0-20 points deduction)
    pipeline_status = latest.pipeline_health.status
    if pipeline_status == "stalled":
        score -= 20
    elif pipeline_status == "degraded":
        score -= 10

    # Factor 8: Systematic failure (0-15 points deduction)
    if latest.pipeline_health.systematic_failure_active:
        score -= 15

    # Ensure score is within bounds
    return max(0, min(100, score))


def get_health_status(score: int) -> str:
    """Get health status from score."""
    if score >= 90:
        return "excellent"
    elif score >= 70:
        return "good"
    elif score >= 50:
        return "fair"
    elif score >= 30:
        return "warning"
    else:
        return "critical"


def generate_alerts(metrics: HealthMetrics) -> list[Alert]:
    """Generate alerts based on current metrics."""
    if not metrics.metrics:
        return []

    timestamp = get_timestamp()
    alerts: list[Alert] = []
    latest = metrics.metrics[-1]
    epoch = int(time.time())

    # Check for stuck agents
    stuck_agents = latest.error_rates.stuck_agents
    if stuck_agents >= 1:
        severity = "critical" if stuck_agents >= 3 else "warning"
        alert_id = f"alert-stuck-{epoch}"
        alerts.append(
            Alert(
                id=alert_id,
                type="stuck_agents",
                severity=severity,
                message=f"{stuck_agents} agent(s) with stale heartbeats",
                timestamp=timestamp,
                acknowledged=False,
                context={"stuck_count": stuck_agents},
            )
        )

    # Check for consecutive failures
    consecutive_failures = latest.error_rates.consecutive_failures
    if consecutive_failures >= 3:
        severity = "critical" if consecutive_failures >= 5 else "warning"
        alert_id = f"alert-failures-{epoch}"
        alerts.append(
            Alert(
                id=alert_id,
                type="high_error_rate",
                severity=severity,
                message=f"{consecutive_failures} consecutive iteration failures",
                timestamp=timestamp,
                acknowledged=False,
                context={"consecutive_failures": consecutive_failures},
            )
        )

    # Check for resource exhaustion
    session_percent = latest.resource_usage.session_percent
    if session_percent >= 90:
        severity = "critical" if session_percent >= 97 else "warning"
        alert_id = f"alert-resource-{epoch}"
        alerts.append(
            Alert(
                id=alert_id,
                type="resource_exhaustion",
                severity=severity,
                message=f"Session budget at {session_percent:.0f}%",
                timestamp=timestamp,
                acknowledged=False,
                context={"session_percent": session_percent},
            )
        )

    # Check for pipeline stall
    pipeline_status = latest.pipeline_health.status
    if pipeline_status == "stalled":
        blocked_count = latest.pipeline_health.blocked_count
        retryable_count = latest.pipeline_health.retryable_count
        permanent_count = latest.pipeline_health.permanent_blocked_count
        severity = "critical" if retryable_count == 0 else "warning"
        alert_id = f"alert-pipeline-stall-{epoch}"
        alerts.append(
            Alert(
                id=alert_id,
                type="pipeline_stall",
                severity=severity,
                message=(
                    f"Pipeline stalled: {blocked_count} blocked "
                    f"({retryable_count} retryable, {permanent_count} permanent)"
                ),
                timestamp=timestamp,
                acknowledged=False,
                context={
                    "blocked_count": blocked_count,
                    "retryable_count": retryable_count,
                    "permanent_blocked_count": permanent_count,
                },
            )
        )

    # Check for systematic failure
    if latest.pipeline_health.systematic_failure_active:
        alert_id = f"alert-systematic-failure-{epoch}"
        alerts.append(
            Alert(
                id=alert_id,
                type="systematic_failure",
                severity="critical",
                message="Systematic failure detected - shepherd spawning paused",
                timestamp=timestamp,
                acknowledged=False,
                context={},
            )
        )

    # Check for queue growth
    current_ready = latest.queue_depths.ready
    prev_ready = 0
    if len(metrics.metrics) >= 2:
        prev_ready = metrics.metrics[-2].queue_depths.ready
    queue_growth = current_ready - prev_ready

    if queue_growth >= QUEUE_GROWTH_THRESHOLD:
        alert_id = f"alert-queue-{epoch}"
        alerts.append(
            Alert(
                id=alert_id,
                type="queue_growth",
                severity="warning",
                message=f"Ready queue grew by {queue_growth} (now {current_ready})",
                timestamp=timestamp,
                acknowledged=False,
                context={"growth": queue_growth, "current": current_ready},
            )
        )

    return alerts


def prune_old_metrics(metrics: HealthMetrics, retention_hours: int) -> HealthMetrics:
    """Remove metrics older than retention window."""
    cutoff_epoch = int(time.time()) - (retention_hours * 3600)
    pruned_entries = [
        m
        for m in metrics.metrics
        if timestamp_to_epoch(m.timestamp) > cutoff_epoch
    ]
    metrics.metrics = pruned_entries
    return metrics


def collect_metrics(repo_root: Path) -> None:
    """Collect metrics and update health status."""
    metrics = init_health_metrics(repo_root)
    alerts_file = init_alerts(repo_root)

    timestamp = get_timestamp()

    # Collect current metrics
    current_metric = collect_current_metrics(repo_root)

    # Add new metric
    metrics.metrics.append(current_metric)

    # Prune old metrics
    metrics = prune_old_metrics(metrics, RETENTION_HOURS)

    # Calculate health score
    health_score = calculate_health_score(metrics)
    health_status = get_health_status(health_score)

    # Update metrics
    metrics.health_score = health_score
    metrics.health_status = health_status
    metrics.last_updated = timestamp

    # Write metrics
    metrics_path = repo_root / ".loom" / "health-metrics.json"
    write_json_file(metrics_path, metrics.to_dict())

    # Generate and store alerts
    new_alerts = generate_alerts(metrics)

    if new_alerts:
        alerts_file.alerts.extend(new_alerts)
        # Keep only last 100 alerts
        alerts_file.alerts = alerts_file.alerts[-100:]
        alerts_path = repo_root / ".loom" / "alerts.json"
        write_json_file(alerts_path, alerts_file.to_dict())

    print(f"Metrics collected. Health score: {health_score} ({health_status})")


def show_health_status(repo_root: Path, json_output: bool = False) -> None:
    """Show health status."""
    metrics = init_health_metrics(repo_root)
    alerts_file = init_alerts(repo_root)

    health_score = metrics.health_score
    health_status = metrics.health_status
    last_updated = metrics.last_updated
    metric_count = len(metrics.metrics)

    # Get latest metrics for display
    latest = metrics.metrics[-1] if metrics.metrics else None

    # Get alert counts
    unack_count = sum(1 for a in alerts_file.alerts if not a.acknowledged)
    total_alerts = len(alerts_file.alerts)

    if json_output:
        output: dict[str, Any] = {
            "health_score": health_score,
            "health_status": health_status,
            "last_updated": last_updated,
            "metric_count": metric_count,
            "unacknowledged_alerts": unack_count,
            "total_alerts": total_alerts,
            "latest_metrics": latest.to_dict() if latest else {},
            "metrics_history": [m.to_dict() for m in metrics.metrics],
        }
        print(json.dumps(output, indent=2))
        return

    print()
    print(f"{BOLD}{CYAN}======================================================================={NC}")
    print(f"{BOLD}{CYAN}  LOOM HEALTH STATUS{NC}")
    print(f"{BOLD}{CYAN}======================================================================={NC}")
    print()

    # Health score with color
    score_color = GREEN
    if health_score < 30:
        score_color = RED
    elif health_score < 70:
        score_color = YELLOW

    print(f"  {BOLD}Health Score:{NC} {score_color}{health_score}/100{NC} ({health_status})")
    print(f"  {BOLD}Last Updated:{NC} {last_updated}")
    print(f"  {BOLD}Metrics Stored:{NC} {metric_count} samples")
    print()

    # Alert summary
    if unack_count > 0:
        print(f"  {BOLD}Alerts:{NC} {RED}{unack_count} unacknowledged{NC} ({total_alerts} total)")
    else:
        print(f"  {BOLD}Alerts:{NC} {GREEN}No unacknowledged alerts{NC} ({total_alerts} total)")
    print()

    # Latest metrics
    if latest:
        print(f"  {BOLD}Current Metrics:{NC}")

        issues_per_hour = latest.throughput.issues_per_hour
        prs_per_hour = latest.throughput.prs_per_hour
        print(f"    Throughput: {issues_per_hour:.0f} issues/hr, {prs_per_hour:.0f} PRs/hr")

        ready = latest.queue_depths.ready
        building = latest.queue_depths.building
        review = latest.queue_depths.review_requested
        print(f"    Queue Depths: ready={ready}, building={building}, review={review}")

        success_rate = latest.error_rates.success_rate
        consecutive_failures = latest.error_rates.consecutive_failures
        stuck = latest.error_rates.stuck_agents
        print(f"    Error Rates: {success_rate:.0f}% success, {consecutive_failures} failures, {stuck} stuck")

        active_shepherds = latest.resource_usage.active_shepherds
        session_percent = latest.resource_usage.session_percent
        print(f"    Resources: {active_shepherds} shepherds, {session_percent:.0f}% session")
    else:
        print(f"  {GRAY}No metrics collected yet. Run: health-check.py --collect{NC}")
    print()

    print(f"{BOLD}{CYAN}======================================================================={NC}")
    print()


def show_alerts(repo_root: Path, json_output: bool = False) -> None:
    """Show alerts."""
    alerts_file = init_alerts(repo_root)

    if json_output:
        print(json.dumps(alerts_file.to_dict(), indent=2))
        return

    unack_alerts = [a for a in alerts_file.alerts if not a.acknowledged]
    unack_count = len(unack_alerts)

    print()
    print(f"{BOLD}{CYAN}======================================================================={NC}")
    print(f"{BOLD}{CYAN}  LOOM ALERTS{NC}")
    print(f"{BOLD}{CYAN}======================================================================={NC}")
    print()

    if unack_count == 0:
        print(f"  {GREEN}No unacknowledged alerts{NC}")
    else:
        print(f"  {YELLOW}{unack_count} unacknowledged alert(s):{NC}")
        print()

        for alert in unack_alerts:
            print(f"    [{alert.severity}] {alert.type}: {alert.message}")
            print(f"      ID: {alert.id}")
            print(f"      Time: {alert.timestamp}")
            print()

    print(f"{BOLD}{CYAN}======================================================================={NC}")
    print()


def acknowledge_alert(repo_root: Path, alert_id: str) -> None:
    """Acknowledge an alert."""
    alerts_file = init_alerts(repo_root)

    # Find the alert
    found = False
    for alert in alerts_file.alerts:
        if alert.id == alert_id:
            alert.acknowledged = True
            alert.acknowledged_at = get_timestamp()
            found = True
            break

    if not found:
        print(f"{RED}Alert not found: {alert_id}{NC}", file=sys.stderr)
        sys.exit(1)

    alerts_path = repo_root / ".loom" / "alerts.json"
    write_json_file(alerts_path, alerts_file.to_dict())
    print(f"{GREEN}Alert acknowledged: {alert_id}{NC}")


def clear_alerts(repo_root: Path) -> None:
    """Clear all alerts."""
    timestamp = get_timestamp()
    alerts_file = AlertsFile(
        initialized_at=timestamp,
        alerts=[],
        acknowledged=[],
    )
    alerts_path = repo_root / ".loom" / "alerts.json"
    write_json_file(alerts_path, alerts_file.to_dict())
    print(f"{GREEN}All alerts cleared{NC}")


def show_history(
    repo_root: Path, hours: int = 1, json_output: bool = False
) -> None:
    """Show metric history."""
    metrics = init_health_metrics(repo_root)

    cutoff_epoch = int(time.time()) - (hours * 3600)
    filtered_metrics = [
        m
        for m in metrics.metrics
        if timestamp_to_epoch(m.timestamp) > cutoff_epoch
    ]

    if json_output:
        output = metrics.to_dict()
        output["metrics"] = [m.to_dict() for m in filtered_metrics]
        print(json.dumps(output, indent=2))
        return

    count = len(filtered_metrics)

    print()
    print(f"{BOLD}Metric History (last {hours} hour(s), {count} samples):{NC}")
    print()

    for m in filtered_metrics:
        # Match shell script output format
        ready = m.queue_depths.ready
        building = m.queue_depths.building
        stuck = m.error_rates.stuck_agents
        print(
            f"{m.timestamp}: score=?, ready={ready}, building={building}, stuck={stuck}"
        )
    print()


def show_help() -> None:
    """Show help message."""
    print(f"""{BOLD}health-check.py - Proactive Health Monitoring for Loom{NC}

{YELLOW}USAGE:{NC}
    health-check.py                    Display health summary
    health-check.py --json             Output health status as JSON
    health-check.py --collect          Collect and store health metrics
    health-check.py --history [hours]  Show metric history (default: 1 hour)
    health-check.py --alerts           Show current alerts
    health-check.py --acknowledge <id> Acknowledge an alert
    health-check.py --clear-alerts     Clear all alerts
    health-check.py --help             Show this help

{YELLOW}HEALTH SCORE:{NC}
    The health score (0-100) is computed from:
    - Throughput trend (declining = lower score)
    - Queue depth trend (growing = lower score)
    - Error rate (increasing = lower score)
    - Resource availability (near limits = lower score)

    Score ranges:
      90-100: {GREEN}Excellent{NC} - System operating optimally
      70-89:  {GREEN}Good{NC} - Normal operation, minor issues
      50-69:  {YELLOW}Fair{NC} - Some degradation detected
      30-49:  {YELLOW}Warning{NC} - Significant issues, attention needed
      0-29:   {RED}Critical{NC} - Immediate intervention required

{YELLOW}ALERT TYPES:{NC}
    {CYAN}throughput_decline{NC}    Throughput dropped significantly
    {CYAN}stuck_agents{NC}          Agents without recent heartbeats
    {CYAN}queue_growth{NC}          Ready queue growing without progress
    {CYAN}high_error_rate{NC}       Error rate exceeds threshold
    {CYAN}resource_exhaustion{NC}   Session budget or capacity limits

{YELLOW}ALERT SEVERITY:{NC}
    {GRAY}info{NC}       Metric changed significantly (informational)
    {YELLOW}warning{NC}    Metric approaching threshold
    {RED}critical{NC}   Metric exceeded threshold, intervention needed

{YELLOW}ENVIRONMENT VARIABLES:{NC}
    LOOM_HEALTH_RETENTION_HOURS      Metric retention (default: 24)
    LOOM_THROUGHPUT_DECLINE_THRESHOLD  Throughput decline % (default: 50)
    LOOM_QUEUE_GROWTH_THRESHOLD      Queue growth count (default: 5)
    LOOM_STUCK_AGENT_THRESHOLD       Stuck minutes (default: 10)
    LOOM_ERROR_RATE_THRESHOLD        Error rate % (default: 20)

{YELLOW}FILES:{NC}
    .loom/health-metrics.json   Historical health metrics
    .loom/alerts.json           Active and acknowledged alerts
    .loom/daemon-state.json     Current daemon state
    .loom/daemon-metrics.json   Daemon iteration metrics

{YELLOW}EXAMPLES:{NC}
    # Check current health status
    health-check.py

    # Collect metrics (called by daemon iteration)
    health-check.py --collect

    # View alerts in JSON format
    health-check.py --alerts --json

    # Acknowledge a specific alert
    health-check.py --acknowledge alert-12345

    # View 4-hour metric history
    health-check.py --history 4
""")


def main() -> None:
    """Main entry point."""
    parser = argparse.ArgumentParser(
        description="Health monitoring for Loom daemon",
        add_help=False,
    )
    parser.add_argument("--json", action="store_true", help="Output as JSON")
    parser.add_argument("--collect", action="store_true", help="Collect metrics")
    parser.add_argument("--alerts", action="store_true", help="Show alerts")
    parser.add_argument("--acknowledge", metavar="ID", help="Acknowledge an alert")
    parser.add_argument("--clear-alerts", action="store_true", help="Clear all alerts")
    parser.add_argument(
        "--history",
        nargs="?",
        const=1,
        type=int,
        metavar="HOURS",
        help="Show metric history",
    )
    parser.add_argument("--help", "-h", action="store_true", help="Show help")

    args = parser.parse_args()

    try:
        repo_root = find_repo_root()
    except FileNotFoundError as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)

    if args.help:
        show_help()
        return

    if args.collect:
        collect_metrics(repo_root)
        return

    if args.alerts:
        show_alerts(repo_root, args.json)
        return

    if args.acknowledge:
        acknowledge_alert(repo_root, args.acknowledge)
        return

    if args.clear_alerts:
        clear_alerts(repo_root)
        return

    if args.history is not None:
        show_history(repo_root, args.history, args.json)
        return

    # Default: show health status
    show_health_status(repo_root, args.json)


if __name__ == "__main__":
    main()
