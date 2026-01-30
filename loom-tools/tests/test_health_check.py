"""Tests for health_check module - scoring algorithm and alert generation."""

from __future__ import annotations

import json
import time
from pathlib import Path
from unittest.mock import patch

import pytest

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
from loom_tools.health_check import (
    calculate_health_score,
    generate_alerts,
    get_health_status,
    prune_old_metrics,
    timestamp_to_epoch,
    QUEUE_GROWTH_THRESHOLD,
    THROUGHPUT_DECLINE_THRESHOLD,
)


# -- Helper functions ----------------------------------------------------------


def make_metric(
    success_rate: float = 100.0,
    consecutive_failures: int = 0,
    stuck_agents: int = 0,
    ready: int = 0,
    session_percent: float = 0.0,
    issues_per_hour: float = 0.0,
    pipeline_status: str = "healthy",
    systematic_failure: bool = False,
    retryable_count: int = 0,
    blocked_count: int = 0,
    timestamp: str = "",
) -> MetricEntry:
    """Create a MetricEntry with specific values for testing."""
    if not timestamp:
        timestamp = f"2026-01-30T10:{int(time.time()) % 60:02d}:00Z"

    return MetricEntry(
        timestamp=timestamp,
        throughput=ThroughputMetric(issues_per_hour=issues_per_hour),
        latency=LatencyMetric(),
        queue_depths=QueueDepths(ready=ready),
        error_rates=ErrorRates(
            success_rate=success_rate,
            consecutive_failures=consecutive_failures,
            stuck_agents=stuck_agents,
        ),
        resource_usage=ResourceUsage(session_percent=session_percent),
        pipeline_health=PipelineHealthMetric(
            status=pipeline_status,
            systematic_failure_active=systematic_failure,
            retryable_count=retryable_count,
            blocked_count=blocked_count,
        ),
    )


def make_metrics(*entries: MetricEntry) -> HealthMetrics:
    """Create HealthMetrics with given entries."""
    return HealthMetrics(
        initialized_at="2026-01-30T10:00:00Z",
        retention_hours=24,
        metrics=list(entries),
        health_score=100,
        health_status="excellent",
        last_updated="2026-01-30T10:00:00Z",
    )


# -- Health Score Tests --------------------------------------------------------


class TestHealthScoreEmptyMetrics:
    """Test scoring with no metrics."""

    def test_empty_metrics_returns_100(self) -> None:
        metrics = make_metrics()
        assert calculate_health_score(metrics) == 100


class TestHealthScoreErrorRate:
    """Test Factor 1: Error rate scoring."""

    def test_success_rate_100_no_deduction(self) -> None:
        m = make_metric(success_rate=100)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 100

    def test_success_rate_90_no_deduction(self) -> None:
        m = make_metric(success_rate=90)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 100

    def test_success_rate_89_deducts_5(self) -> None:
        m = make_metric(success_rate=89)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 95

    def test_success_rate_70_deducts_5(self) -> None:
        m = make_metric(success_rate=70)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 95

    def test_success_rate_69_deducts_15(self) -> None:
        m = make_metric(success_rate=69)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 85

    def test_success_rate_50_deducts_15(self) -> None:
        m = make_metric(success_rate=50)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 85

    def test_success_rate_49_deducts_25(self) -> None:
        m = make_metric(success_rate=49)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 75


class TestHealthScoreConsecutiveFailures:
    """Test Factor 2: Consecutive failures scoring."""

    def test_no_failures_no_deduction(self) -> None:
        m = make_metric(consecutive_failures=0)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 100

    def test_1_failure_deducts_5(self) -> None:
        m = make_metric(consecutive_failures=1)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 95

    def test_2_failures_deducts_5(self) -> None:
        m = make_metric(consecutive_failures=2)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 95

    def test_3_failures_deducts_10(self) -> None:
        m = make_metric(consecutive_failures=3)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 90

    def test_4_failures_deducts_10(self) -> None:
        m = make_metric(consecutive_failures=4)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 90

    def test_5_failures_deducts_15(self) -> None:
        m = make_metric(consecutive_failures=5)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 85

    def test_10_failures_deducts_15(self) -> None:
        m = make_metric(consecutive_failures=10)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 85


class TestHealthScoreStuckAgents:
    """Test Factor 3: Stuck agents scoring."""

    def test_no_stuck_agents_no_deduction(self) -> None:
        m = make_metric(stuck_agents=0)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 100

    def test_1_stuck_agent_deducts_10(self) -> None:
        m = make_metric(stuck_agents=1)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 90

    def test_2_stuck_agents_deducts_15(self) -> None:
        m = make_metric(stuck_agents=2)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 85

    def test_3_stuck_agents_deducts_20(self) -> None:
        m = make_metric(stuck_agents=3)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 80

    def test_5_stuck_agents_deducts_20(self) -> None:
        m = make_metric(stuck_agents=5)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 80


class TestHealthScoreQueueGrowth:
    """Test Factor 4: Queue growth scoring."""

    def test_no_growth_no_deduction(self) -> None:
        m1 = make_metric(ready=5, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(ready=5, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 100

    def test_growth_of_1_deducts_5(self) -> None:
        m1 = make_metric(ready=5, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(ready=6, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 95

    def test_growth_of_2_deducts_5(self) -> None:
        m1 = make_metric(ready=5, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(ready=7, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 95

    def test_growth_of_3_deducts_10(self) -> None:
        m1 = make_metric(ready=5, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(ready=8, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 90

    def test_growth_of_4_deducts_10(self) -> None:
        m1 = make_metric(ready=5, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(ready=9, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 90

    def test_growth_at_threshold_deducts_15(self) -> None:
        m1 = make_metric(ready=0, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(ready=QUEUE_GROWTH_THRESHOLD, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 85

    def test_growth_above_threshold_deducts_15(self) -> None:
        m1 = make_metric(ready=0, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(ready=10, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 85

    def test_queue_shrink_no_deduction(self) -> None:
        m1 = make_metric(ready=10, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(ready=5, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 100


class TestHealthScoreResourceUsage:
    """Test Factor 5: Resource usage scoring."""

    def test_session_0_no_deduction(self) -> None:
        m = make_metric(session_percent=0)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 100

    def test_session_79_no_deduction(self) -> None:
        m = make_metric(session_percent=79)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 100

    def test_session_80_deducts_5(self) -> None:
        m = make_metric(session_percent=80)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 95

    def test_session_89_deducts_5(self) -> None:
        m = make_metric(session_percent=89)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 95

    def test_session_90_deducts_10(self) -> None:
        m = make_metric(session_percent=90)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 90

    def test_session_94_deducts_10(self) -> None:
        m = make_metric(session_percent=94)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 90

    def test_session_95_deducts_15(self) -> None:
        m = make_metric(session_percent=95)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 85

    def test_session_100_deducts_15(self) -> None:
        m = make_metric(session_percent=100)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 85


class TestHealthScoreThroughputDecline:
    """Test Factor 6: Throughput decline scoring."""

    def test_no_decline_no_deduction(self) -> None:
        m1 = make_metric(issues_per_hour=10, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(issues_per_hour=10, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 100

    def test_throughput_increase_no_deduction(self) -> None:
        m1 = make_metric(issues_per_hour=10, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(issues_per_hour=15, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 100

    def test_decline_9_percent_no_deduction(self) -> None:
        m1 = make_metric(issues_per_hour=100, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(issues_per_hour=91, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 100

    def test_decline_10_percent_deducts_5(self) -> None:
        m1 = make_metric(issues_per_hour=100, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(issues_per_hour=90, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 95

    def test_decline_29_percent_deducts_5(self) -> None:
        m1 = make_metric(issues_per_hour=100, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(issues_per_hour=71, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 95

    def test_decline_30_percent_deducts_10(self) -> None:
        m1 = make_metric(issues_per_hour=100, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(issues_per_hour=70, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 90

    def test_decline_49_percent_deducts_10(self) -> None:
        m1 = make_metric(issues_per_hour=100, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(issues_per_hour=51, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 90

    def test_decline_at_threshold_deducts_15(self) -> None:
        m1 = make_metric(issues_per_hour=100, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(issues_per_hour=50, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 85

    def test_decline_100_percent_deducts_15(self) -> None:
        m1 = make_metric(issues_per_hour=100, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(issues_per_hour=0, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 85

    def test_zero_prev_throughput_no_deduction(self) -> None:
        m1 = make_metric(issues_per_hour=0, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(issues_per_hour=0, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 100


class TestHealthScorePipelineStall:
    """Test Factor 7: Pipeline stall scoring."""

    def test_healthy_status_no_deduction(self) -> None:
        m = make_metric(pipeline_status="healthy")
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 100

    def test_degraded_status_deducts_10(self) -> None:
        m = make_metric(pipeline_status="degraded")
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 90

    def test_stalled_status_deducts_20(self) -> None:
        m = make_metric(pipeline_status="stalled")
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 80


class TestHealthScoreSystematicFailure:
    """Test Factor 8: Systematic failure scoring."""

    def test_no_systematic_failure_no_deduction(self) -> None:
        m = make_metric(systematic_failure=False)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 100

    def test_systematic_failure_active_deducts_15(self) -> None:
        m = make_metric(systematic_failure=True)
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 85


class TestHealthScoreCombined:
    """Test combined scoring with multiple factors."""

    def test_all_factors_maximum_deduction(self) -> None:
        # success_rate < 50: -25
        # consecutive_failures >= 5: -15
        # stuck_agents >= 3: -20
        # queue_growth >= 5: -15
        # session_percent >= 95: -15
        # throughput_decline >= 50%: -15
        # pipeline stalled: -20
        # systematic_failure: -15
        # Total: -140, but capped at 0
        m1 = make_metric(
            issues_per_hour=100,
            ready=0,
            timestamp="2026-01-30T10:00:00Z",
        )
        m2 = make_metric(
            success_rate=40,
            consecutive_failures=10,
            stuck_agents=5,
            ready=10,
            session_percent=100,
            issues_per_hour=0,
            pipeline_status="stalled",
            systematic_failure=True,
            timestamp="2026-01-30T10:01:00Z",
        )
        metrics = make_metrics(m1, m2)
        assert calculate_health_score(metrics) == 0

    def test_moderate_issues(self) -> None:
        # success_rate 80: -5
        # consecutive_failures 2: -5
        # stuck_agents 1: -10
        # no queue growth
        # session_percent 85: -5
        # Total: -25
        m = make_metric(
            success_rate=80,
            consecutive_failures=2,
            stuck_agents=1,
            session_percent=85,
        )
        metrics = make_metrics(m)
        assert calculate_health_score(metrics) == 75


# -- Health Status Tests -------------------------------------------------------


class TestGetHealthStatus:
    """Test get_health_status function."""

    def test_excellent(self) -> None:
        assert get_health_status(100) == "excellent"
        assert get_health_status(95) == "excellent"
        assert get_health_status(90) == "excellent"

    def test_good(self) -> None:
        assert get_health_status(89) == "good"
        assert get_health_status(80) == "good"
        assert get_health_status(70) == "good"

    def test_fair(self) -> None:
        assert get_health_status(69) == "fair"
        assert get_health_status(60) == "fair"
        assert get_health_status(50) == "fair"

    def test_warning(self) -> None:
        assert get_health_status(49) == "warning"
        assert get_health_status(40) == "warning"
        assert get_health_status(30) == "warning"

    def test_critical(self) -> None:
        assert get_health_status(29) == "critical"
        assert get_health_status(10) == "critical"
        assert get_health_status(0) == "critical"


# -- Alert Generation Tests ----------------------------------------------------


class TestAlertGenerationStuckAgents:
    """Test stuck_agents alert generation."""

    def test_no_stuck_agents_no_alert(self) -> None:
        m = make_metric(stuck_agents=0)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        stuck_alerts = [a for a in alerts if a.type == "stuck_agents"]
        assert len(stuck_alerts) == 0

    def test_1_stuck_agent_warning_alert(self) -> None:
        m = make_metric(stuck_agents=1)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        stuck_alerts = [a for a in alerts if a.type == "stuck_agents"]
        assert len(stuck_alerts) == 1
        assert stuck_alerts[0].severity == "warning"
        assert "1 agent(s)" in stuck_alerts[0].message

    def test_2_stuck_agents_warning_alert(self) -> None:
        m = make_metric(stuck_agents=2)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        stuck_alerts = [a for a in alerts if a.type == "stuck_agents"]
        assert len(stuck_alerts) == 1
        assert stuck_alerts[0].severity == "warning"

    def test_3_stuck_agents_critical_alert(self) -> None:
        m = make_metric(stuck_agents=3)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        stuck_alerts = [a for a in alerts if a.type == "stuck_agents"]
        assert len(stuck_alerts) == 1
        assert stuck_alerts[0].severity == "critical"

    def test_5_stuck_agents_critical_alert(self) -> None:
        m = make_metric(stuck_agents=5)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        stuck_alerts = [a for a in alerts if a.type == "stuck_agents"]
        assert len(stuck_alerts) == 1
        assert stuck_alerts[0].severity == "critical"


class TestAlertGenerationHighErrorRate:
    """Test high_error_rate alert generation."""

    def test_no_failures_no_alert(self) -> None:
        m = make_metric(consecutive_failures=0)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        error_alerts = [a for a in alerts if a.type == "high_error_rate"]
        assert len(error_alerts) == 0

    def test_2_failures_no_alert(self) -> None:
        m = make_metric(consecutive_failures=2)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        error_alerts = [a for a in alerts if a.type == "high_error_rate"]
        assert len(error_alerts) == 0

    def test_3_failures_warning_alert(self) -> None:
        m = make_metric(consecutive_failures=3)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        error_alerts = [a for a in alerts if a.type == "high_error_rate"]
        assert len(error_alerts) == 1
        assert error_alerts[0].severity == "warning"
        assert "3 consecutive" in error_alerts[0].message

    def test_4_failures_warning_alert(self) -> None:
        m = make_metric(consecutive_failures=4)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        error_alerts = [a for a in alerts if a.type == "high_error_rate"]
        assert len(error_alerts) == 1
        assert error_alerts[0].severity == "warning"

    def test_5_failures_critical_alert(self) -> None:
        m = make_metric(consecutive_failures=5)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        error_alerts = [a for a in alerts if a.type == "high_error_rate"]
        assert len(error_alerts) == 1
        assert error_alerts[0].severity == "critical"


class TestAlertGenerationResourceExhaustion:
    """Test resource_exhaustion alert generation."""

    def test_session_89_no_alert(self) -> None:
        m = make_metric(session_percent=89)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        resource_alerts = [a for a in alerts if a.type == "resource_exhaustion"]
        assert len(resource_alerts) == 0

    def test_session_90_warning_alert(self) -> None:
        m = make_metric(session_percent=90)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        resource_alerts = [a for a in alerts if a.type == "resource_exhaustion"]
        assert len(resource_alerts) == 1
        assert resource_alerts[0].severity == "warning"
        assert "90%" in resource_alerts[0].message

    def test_session_96_warning_alert(self) -> None:
        m = make_metric(session_percent=96)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        resource_alerts = [a for a in alerts if a.type == "resource_exhaustion"]
        assert len(resource_alerts) == 1
        assert resource_alerts[0].severity == "warning"

    def test_session_97_critical_alert(self) -> None:
        m = make_metric(session_percent=97)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        resource_alerts = [a for a in alerts if a.type == "resource_exhaustion"]
        assert len(resource_alerts) == 1
        assert resource_alerts[0].severity == "critical"

    def test_session_100_critical_alert(self) -> None:
        m = make_metric(session_percent=100)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        resource_alerts = [a for a in alerts if a.type == "resource_exhaustion"]
        assert len(resource_alerts) == 1
        assert resource_alerts[0].severity == "critical"


class TestAlertGenerationPipelineStall:
    """Test pipeline_stall alert generation."""

    def test_healthy_no_alert(self) -> None:
        m = make_metric(pipeline_status="healthy")
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        stall_alerts = [a for a in alerts if a.type == "pipeline_stall"]
        assert len(stall_alerts) == 0

    def test_degraded_no_alert(self) -> None:
        m = make_metric(pipeline_status="degraded")
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        stall_alerts = [a for a in alerts if a.type == "pipeline_stall"]
        assert len(stall_alerts) == 0

    def test_stalled_with_retryable_warning_alert(self) -> None:
        m = make_metric(pipeline_status="stalled", retryable_count=1)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        stall_alerts = [a for a in alerts if a.type == "pipeline_stall"]
        assert len(stall_alerts) == 1
        assert stall_alerts[0].severity == "warning"
        assert "Pipeline stalled" in stall_alerts[0].message

    def test_stalled_without_retryable_critical_alert(self) -> None:
        m = make_metric(pipeline_status="stalled", retryable_count=0)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        stall_alerts = [a for a in alerts if a.type == "pipeline_stall"]
        assert len(stall_alerts) == 1
        assert stall_alerts[0].severity == "critical"


class TestAlertGenerationSystematicFailure:
    """Test systematic_failure alert generation."""

    def test_no_systematic_failure_no_alert(self) -> None:
        m = make_metric(systematic_failure=False)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        sys_alerts = [a for a in alerts if a.type == "systematic_failure"]
        assert len(sys_alerts) == 0

    def test_systematic_failure_active_critical_alert(self) -> None:
        m = make_metric(systematic_failure=True)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        sys_alerts = [a for a in alerts if a.type == "systematic_failure"]
        assert len(sys_alerts) == 1
        assert sys_alerts[0].severity == "critical"
        assert "Systematic failure detected" in sys_alerts[0].message


class TestAlertGenerationQueueGrowth:
    """Test queue_growth alert generation."""

    def test_no_prev_metric_growth_from_zero(self) -> None:
        # With only one metric, prev_ready defaults to 0
        # So ready=10 means growth of 10, which triggers an alert
        m = make_metric(ready=10)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        queue_alerts = [a for a in alerts if a.type == "queue_growth"]
        assert len(queue_alerts) == 1  # Growth from 0 to 10

    def test_no_prev_metric_below_threshold_no_alert(self) -> None:
        # Ready=4 means growth of 4 from 0, which is below threshold of 5
        m = make_metric(ready=4)
        metrics = make_metrics(m)
        alerts = generate_alerts(metrics)
        queue_alerts = [a for a in alerts if a.type == "queue_growth"]
        assert len(queue_alerts) == 0

    def test_no_growth_no_alert(self) -> None:
        m1 = make_metric(ready=5, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(ready=5, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        alerts = generate_alerts(metrics)
        queue_alerts = [a for a in alerts if a.type == "queue_growth"]
        assert len(queue_alerts) == 0

    def test_growth_below_threshold_no_alert(self) -> None:
        m1 = make_metric(ready=5, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(ready=9, timestamp="2026-01-30T10:01:00Z")  # +4
        metrics = make_metrics(m1, m2)
        alerts = generate_alerts(metrics)
        queue_alerts = [a for a in alerts if a.type == "queue_growth"]
        assert len(queue_alerts) == 0

    def test_growth_at_threshold_warning_alert(self) -> None:
        m1 = make_metric(ready=0, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(
            ready=QUEUE_GROWTH_THRESHOLD, timestamp="2026-01-30T10:01:00Z"
        )
        metrics = make_metrics(m1, m2)
        alerts = generate_alerts(metrics)
        queue_alerts = [a for a in alerts if a.type == "queue_growth"]
        assert len(queue_alerts) == 1
        assert queue_alerts[0].severity == "warning"

    def test_growth_above_threshold_warning_alert(self) -> None:
        m1 = make_metric(ready=0, timestamp="2026-01-30T10:00:00Z")
        m2 = make_metric(ready=10, timestamp="2026-01-30T10:01:00Z")
        metrics = make_metrics(m1, m2)
        alerts = generate_alerts(metrics)
        queue_alerts = [a for a in alerts if a.type == "queue_growth"]
        assert len(queue_alerts) == 1
        assert queue_alerts[0].severity == "warning"
        assert "grew by 10" in queue_alerts[0].message


class TestAlertGenerationEmpty:
    """Test alert generation with empty metrics."""

    def test_empty_metrics_no_alerts(self) -> None:
        metrics = make_metrics()
        alerts = generate_alerts(metrics)
        assert alerts == []


# -- Metric Pruning Tests ------------------------------------------------------


class TestPruneOldMetrics:
    """Test prune_old_metrics function."""

    def test_no_metrics_no_change(self) -> None:
        metrics = make_metrics()
        pruned = prune_old_metrics(metrics, retention_hours=24)
        assert len(pruned.metrics) == 0

    def test_recent_metrics_not_pruned(self) -> None:
        now = int(time.time())
        recent_ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now - 3600))
        m = make_metric(timestamp=recent_ts)
        metrics = make_metrics(m)
        pruned = prune_old_metrics(metrics, retention_hours=24)
        assert len(pruned.metrics) == 1

    def test_old_metrics_pruned(self) -> None:
        now = int(time.time())
        # Metric from 25 hours ago
        old_ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now - 25 * 3600))
        m = make_metric(timestamp=old_ts)
        metrics = make_metrics(m)
        pruned = prune_old_metrics(metrics, retention_hours=24)
        assert len(pruned.metrics) == 0

    def test_mixed_metrics_partial_prune(self) -> None:
        now = int(time.time())
        old_ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now - 25 * 3600))
        recent_ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now - 1 * 3600))
        m1 = make_metric(timestamp=old_ts)
        m2 = make_metric(timestamp=recent_ts)
        metrics = make_metrics(m1, m2)
        pruned = prune_old_metrics(metrics, retention_hours=24)
        assert len(pruned.metrics) == 1
        assert pruned.metrics[0].timestamp == recent_ts


# -- Timestamp Utility Tests ---------------------------------------------------


class TestTimestampToEpoch:
    """Test timestamp_to_epoch function."""

    def test_valid_timestamp(self) -> None:
        epoch = timestamp_to_epoch("2026-01-30T10:00:00Z")
        assert epoch > 0

    def test_empty_string(self) -> None:
        assert timestamp_to_epoch("") == 0

    def test_null_string(self) -> None:
        assert timestamp_to_epoch("null") == 0

    def test_invalid_timestamp(self) -> None:
        assert timestamp_to_epoch("not-a-timestamp") == 0


# -- PipelineHealthMetric Tests ------------------------------------------------


class TestPipelineHealthMetric:
    """Test PipelineHealthMetric model."""

    def test_default_values(self) -> None:
        phm = PipelineHealthMetric()
        assert phm.status == "healthy"
        assert phm.blocked_count == 0
        assert phm.retryable_count == 0
        assert phm.permanent_blocked_count == 0
        assert phm.systematic_failure_active is False

    def test_from_dict(self) -> None:
        data = {
            "status": "stalled",
            "blocked_count": 5,
            "retryable_count": 2,
            "permanent_blocked_count": 3,
            "systematic_failure_active": True,
        }
        phm = PipelineHealthMetric.from_dict(data)
        assert phm.status == "stalled"
        assert phm.blocked_count == 5
        assert phm.retryable_count == 2
        assert phm.permanent_blocked_count == 3
        assert phm.systematic_failure_active is True

    def test_to_dict(self) -> None:
        phm = PipelineHealthMetric(
            status="degraded",
            blocked_count=3,
            retryable_count=1,
            permanent_blocked_count=2,
            systematic_failure_active=False,
        )
        d = phm.to_dict()
        assert d["status"] == "degraded"
        assert d["blocked_count"] == 3
        assert d["retryable_count"] == 1
        assert d["permanent_blocked_count"] == 2
        assert d["systematic_failure_active"] is False

    def test_round_trip(self) -> None:
        original = PipelineHealthMetric(
            status="stalled",
            blocked_count=10,
            retryable_count=5,
            permanent_blocked_count=5,
            systematic_failure_active=True,
        )
        d = original.to_dict()
        restored = PipelineHealthMetric.from_dict(d)
        assert restored.status == original.status
        assert restored.blocked_count == original.blocked_count
        assert restored.retryable_count == original.retryable_count
        assert restored.permanent_blocked_count == original.permanent_blocked_count
        assert restored.systematic_failure_active == original.systematic_failure_active

    def test_from_empty_dict_uses_defaults(self) -> None:
        phm = PipelineHealthMetric.from_dict({})
        assert phm.status == "healthy"
        assert phm.blocked_count == 0
        assert phm.systematic_failure_active is False
