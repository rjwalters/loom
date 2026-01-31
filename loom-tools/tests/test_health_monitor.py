"""Tests for loom_tools.health_monitor - proactive health monitoring."""

from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import patch

import pytest

from loom_tools.health_monitor import (
    calculate_health_score,
    collect_metrics,
    generate_alerts,
    get_health_status,
    main,
    acknowledge_alert,
    clear_alerts,
)
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

NOW = datetime(2026, 1, 25, 12, 0, 0, tzinfo=timezone.utc)
TS = "2026-01-25T12:00:00Z"
TS_PREV = "2026-01-25T11:55:00Z"


def _make_entry(
    *,
    success_rate: float = 100.0,
    consecutive_failures: int = 0,
    stuck_agents: int = 0,
    ready: int = 0,
    building: int = 0,
    session_percent: float = 0.0,
    issues_per_hour: float = 1.0,
    pipeline_status: str = "healthy",
    systematic_failure: bool = False,
    timestamp: str = TS,
) -> MetricEntry:
    return MetricEntry(
        timestamp=timestamp,
        throughput=ThroughputMetric(issues_per_hour=issues_per_hour, prs_per_hour=0.5),
        latency=LatencyMetric(avg_iteration_seconds=30.0),
        queue_depths=QueueDepths(ready=ready, building=building),
        error_rates=ErrorRates(
            consecutive_failures=consecutive_failures,
            success_rate=success_rate,
            stuck_agents=stuck_agents,
        ),
        resource_usage=ResourceUsage(
            active_shepherds=2,
            session_percent=session_percent,
        ),
        pipeline_health=PipelineHealthMetric(
            status=pipeline_status,
            blocked_count=0,
            systematic_failure_active=systematic_failure,
        ),
    )


# ---------------------------------------------------------------------------
# Health score tests
# ---------------------------------------------------------------------------


class TestCalculateHealthScore:
    def test_perfect_score(self):
        metrics = HealthMetrics(metrics=[_make_entry()])
        assert calculate_health_score(metrics) == 100

    def test_empty_metrics(self):
        metrics = HealthMetrics(metrics=[])
        assert calculate_health_score(metrics) == 100

    def test_low_success_rate(self):
        metrics = HealthMetrics(metrics=[_make_entry(success_rate=40)])
        score = calculate_health_score(metrics)
        assert score == 75  # -25 for <50% success

    def test_medium_success_rate(self):
        metrics = HealthMetrics(metrics=[_make_entry(success_rate=60)])
        score = calculate_health_score(metrics)
        assert score == 85  # -15 for <70% success

    def test_consecutive_failures(self):
        metrics = HealthMetrics(metrics=[_make_entry(consecutive_failures=5)])
        score = calculate_health_score(metrics)
        assert score == 85  # -15 for >=5 failures

    def test_stuck_agents(self):
        metrics = HealthMetrics(metrics=[_make_entry(stuck_agents=3)])
        score = calculate_health_score(metrics)
        assert score == 80  # -20 for >=3 stuck

    def test_one_stuck_agent(self):
        metrics = HealthMetrics(metrics=[_make_entry(stuck_agents=1)])
        score = calculate_health_score(metrics)
        assert score == 90  # -10 for 1 stuck

    def test_queue_growth(self):
        prev = _make_entry(ready=2, timestamp=TS_PREV)
        curr = _make_entry(ready=8, timestamp=TS)
        metrics = HealthMetrics(metrics=[prev, curr])
        score = calculate_health_score(metrics)
        assert score == 85  # -15 for growth >= threshold (5)

    def test_high_resource_usage(self):
        metrics = HealthMetrics(metrics=[_make_entry(session_percent=96)])
        score = calculate_health_score(metrics)
        assert score == 85  # -15 for >=95%

    def test_throughput_decline(self):
        prev = _make_entry(issues_per_hour=10.0, timestamp=TS_PREV)
        curr = _make_entry(issues_per_hour=3.0, timestamp=TS)
        metrics = HealthMetrics(metrics=[prev, curr])
        score = calculate_health_score(metrics)
        assert score == 85  # -15 for >=50% decline (70%)

    def test_pipeline_stall(self):
        metrics = HealthMetrics(metrics=[_make_entry(pipeline_status="stalled")])
        score = calculate_health_score(metrics)
        assert score == 80  # -20 for stalled

    def test_pipeline_degraded(self):
        metrics = HealthMetrics(metrics=[_make_entry(pipeline_status="degraded")])
        score = calculate_health_score(metrics)
        assert score == 90  # -10 for degraded

    def test_systematic_failure(self):
        metrics = HealthMetrics(metrics=[_make_entry(systematic_failure=True)])
        score = calculate_health_score(metrics)
        assert score == 85  # -15 for systematic failure

    def test_multiple_factors(self):
        metrics = HealthMetrics(
            metrics=[
                _make_entry(
                    success_rate=40,  # -25
                    stuck_agents=3,  # -20
                    pipeline_status="stalled",  # -20
                )
            ]
        )
        score = calculate_health_score(metrics)
        assert score == 35

    def test_score_floor_at_zero(self):
        metrics = HealthMetrics(
            metrics=[
                _make_entry(
                    success_rate=40,  # -25
                    consecutive_failures=5,  # -15
                    stuck_agents=3,  # -20
                    session_percent=96,  # -15
                    pipeline_status="stalled",  # -20
                    systematic_failure=True,  # -15
                )
            ]
        )
        score = calculate_health_score(metrics)
        assert score == 0  # Clamped to 0


class TestGetHealthStatus:
    def test_excellent(self):
        assert get_health_status(95) == "excellent"

    def test_good(self):
        assert get_health_status(75) == "good"

    def test_fair(self):
        assert get_health_status(55) == "fair"

    def test_warning(self):
        assert get_health_status(35) == "warning"

    def test_critical(self):
        assert get_health_status(15) == "critical"

    def test_boundary_90(self):
        assert get_health_status(90) == "excellent"

    def test_boundary_70(self):
        assert get_health_status(70) == "good"

    def test_boundary_50(self):
        assert get_health_status(50) == "fair"

    def test_boundary_30(self):
        assert get_health_status(30) == "warning"


# ---------------------------------------------------------------------------
# Alert generation tests
# ---------------------------------------------------------------------------


class TestGenerateAlerts:
    def test_no_metrics(self):
        metrics = HealthMetrics(metrics=[])
        alerts = generate_alerts(metrics, _now=NOW)
        assert alerts == []

    def test_healthy_no_alerts(self):
        metrics = HealthMetrics(metrics=[_make_entry()])
        alerts = generate_alerts(metrics, _now=NOW)
        assert alerts == []

    def test_stuck_agents_warning(self):
        metrics = HealthMetrics(metrics=[_make_entry(stuck_agents=1)])
        alerts = generate_alerts(metrics, _now=NOW)
        assert len(alerts) == 1
        assert alerts[0].type == "stuck_agents"
        assert alerts[0].severity == "warning"

    def test_stuck_agents_critical(self):
        metrics = HealthMetrics(metrics=[_make_entry(stuck_agents=3)])
        alerts = generate_alerts(metrics, _now=NOW)
        assert len(alerts) == 1
        assert alerts[0].severity == "critical"

    def test_consecutive_failures(self):
        metrics = HealthMetrics(metrics=[_make_entry(consecutive_failures=3)])
        alerts = generate_alerts(metrics, _now=NOW)
        assert len(alerts) == 1
        assert alerts[0].type == "high_error_rate"
        assert alerts[0].severity == "warning"

    def test_consecutive_failures_critical(self):
        metrics = HealthMetrics(metrics=[_make_entry(consecutive_failures=5)])
        alerts = generate_alerts(metrics, _now=NOW)
        assert len(alerts) == 1
        assert alerts[0].severity == "critical"

    def test_resource_exhaustion(self):
        metrics = HealthMetrics(metrics=[_make_entry(session_percent=92)])
        alerts = generate_alerts(metrics, _now=NOW)
        assert len(alerts) == 1
        assert alerts[0].type == "resource_exhaustion"
        assert alerts[0].severity == "warning"

    def test_resource_exhaustion_critical(self):
        metrics = HealthMetrics(metrics=[_make_entry(session_percent=98)])
        alerts = generate_alerts(metrics, _now=NOW)
        assert len(alerts) == 1
        assert alerts[0].severity == "critical"

    def test_pipeline_stall(self):
        metrics = HealthMetrics(metrics=[_make_entry(pipeline_status="stalled")])
        alerts = generate_alerts(metrics, _now=NOW)
        stall_alerts = [a for a in alerts if a.type == "pipeline_stall"]
        assert len(stall_alerts) == 1

    def test_systematic_failure(self):
        metrics = HealthMetrics(metrics=[_make_entry(systematic_failure=True)])
        alerts = generate_alerts(metrics, _now=NOW)
        sys_alerts = [a for a in alerts if a.type == "systematic_failure"]
        assert len(sys_alerts) == 1
        assert sys_alerts[0].severity == "critical"

    def test_queue_growth(self):
        prev = _make_entry(ready=2, timestamp=TS_PREV)
        curr = _make_entry(ready=8, timestamp=TS)
        metrics = HealthMetrics(metrics=[prev, curr])
        alerts = generate_alerts(metrics, _now=NOW)
        queue_alerts = [a for a in alerts if a.type == "queue_growth"]
        assert len(queue_alerts) == 1

    def test_multiple_alerts(self):
        metrics = HealthMetrics(
            metrics=[_make_entry(stuck_agents=3, consecutive_failures=5)]
        )
        alerts = generate_alerts(metrics, _now=NOW)
        types = {a.type for a in alerts}
        assert "stuck_agents" in types
        assert "high_error_rate" in types


# ---------------------------------------------------------------------------
# Collect and persist tests
# ---------------------------------------------------------------------------


class TestCollectMetrics:
    def test_collect_creates_files(self, tmp_path: Path):
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        # Create minimal .git so find_repo_root works
        (tmp_path / ".git").mkdir()

        with patch(
            "loom_tools.health_monitor.find_repo_root", return_value=tmp_path
        ), patch(
            "loom_tools.health_monitor.collect_current_metrics",
            return_value=_make_entry(),
        ):
            msg = collect_metrics(tmp_path, _now=NOW)

        assert "Metrics collected" in msg
        assert (loom_dir / "health-metrics.json").exists()

    def test_collect_appends_metrics(self, tmp_path: Path):
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (tmp_path / ".git").mkdir()

        # Write existing metrics
        existing = HealthMetrics(
            initialized_at=TS_PREV,
            metrics=[_make_entry(timestamp=TS_PREV)],
        )
        metrics_path = loom_dir / "health-metrics.json"
        metrics_path.write_text(json.dumps(existing.to_dict()))

        with patch(
            "loom_tools.health_monitor.find_repo_root", return_value=tmp_path
        ), patch(
            "loom_tools.health_monitor.collect_current_metrics",
            return_value=_make_entry(timestamp=TS),
        ):
            collect_metrics(tmp_path, _now=NOW)

        data = json.loads(metrics_path.read_text())
        assert len(data["metrics"]) == 2


# ---------------------------------------------------------------------------
# Alert management tests
# ---------------------------------------------------------------------------


class TestAcknowledgeAlert:
    def test_acknowledge_existing(self, tmp_path: Path):
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (tmp_path / ".git").mkdir()

        alerts_file = AlertsFile(
            initialized_at=TS,
            alerts=[Alert(id="alert-test-1", type="stuck_agents", message="test")],
        )
        alerts_path = loom_dir / "alerts.json"
        alerts_path.write_text(json.dumps(alerts_file.to_dict()))

        msg = acknowledge_alert(tmp_path, "alert-test-1")
        assert "acknowledged" in msg.lower()

        data = json.loads(alerts_path.read_text())
        assert data["alerts"][0]["acknowledged"] is True

    def test_acknowledge_missing(self, tmp_path: Path):
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (tmp_path / ".git").mkdir()

        alerts_file = AlertsFile(initialized_at=TS)
        alerts_path = loom_dir / "alerts.json"
        alerts_path.write_text(json.dumps(alerts_file.to_dict()))

        msg = acknowledge_alert(tmp_path, "nonexistent")
        assert "not found" in msg.lower()


class TestClearAlerts:
    def test_clear(self, tmp_path: Path):
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (tmp_path / ".git").mkdir()

        alerts_file = AlertsFile(
            initialized_at=TS,
            alerts=[Alert(id="a1", type="test", message="m")],
        )
        alerts_path = loom_dir / "alerts.json"
        alerts_path.write_text(json.dumps(alerts_file.to_dict()))

        msg = clear_alerts(tmp_path)
        assert "cleared" in msg.lower()

        data = json.loads(alerts_path.read_text())
        assert len(data["alerts"]) == 0


# ---------------------------------------------------------------------------
# CLI tests
# ---------------------------------------------------------------------------


class TestCLI:
    def test_help(self, capsys):
        with pytest.raises(SystemExit) as exc:
            main(["--help"])
        assert exc.value.code == 0

    def test_collect(self, tmp_path: Path):
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (tmp_path / ".git").mkdir()

        with patch(
            "loom_tools.health_monitor.find_repo_root", return_value=tmp_path
        ), patch(
            "loom_tools.health_monitor.collect_current_metrics",
            return_value=_make_entry(),
        ):
            rc = main(["--collect"])
        assert rc == 0

    def test_json_output(self, tmp_path: Path, capsys):
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (tmp_path / ".git").mkdir()

        # Write metrics file
        health = HealthMetrics(
            initialized_at=TS,
            metrics=[_make_entry()],
            health_score=90,
            health_status="excellent",
            last_updated=TS,
        )
        (loom_dir / "health-metrics.json").write_text(json.dumps(health.to_dict()))
        (loom_dir / "alerts.json").write_text(
            json.dumps(AlertsFile(initialized_at=TS).to_dict())
        )

        with patch(
            "loom_tools.health_monitor.find_repo_root", return_value=tmp_path
        ):
            rc = main(["--json"])
        assert rc == 0
        out = capsys.readouterr().out
        data = json.loads(out)
        assert "health_score" in data

    def test_alerts_command(self, tmp_path: Path, capsys):
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (tmp_path / ".git").mkdir()
        (loom_dir / "alerts.json").write_text(
            json.dumps(AlertsFile(initialized_at=TS).to_dict())
        )

        with patch(
            "loom_tools.health_monitor.find_repo_root", return_value=tmp_path
        ):
            rc = main(["--alerts"])
        assert rc == 0

    def test_history_command(self, tmp_path: Path, capsys):
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (tmp_path / ".git").mkdir()

        health = HealthMetrics(
            initialized_at=TS,
            metrics=[_make_entry()],
            health_score=100,
            health_status="excellent",
            last_updated=TS,
        )
        (loom_dir / "health-metrics.json").write_text(json.dumps(health.to_dict()))

        with patch(
            "loom_tools.health_monitor.find_repo_root", return_value=tmp_path
        ):
            rc = main(["--history", "4"])
        assert rc == 0

    def test_acknowledge_command(self, tmp_path: Path):
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (tmp_path / ".git").mkdir()

        alerts_file = AlertsFile(
            initialized_at=TS,
            alerts=[Alert(id="alert-1", type="test", message="test")],
        )
        (loom_dir / "alerts.json").write_text(json.dumps(alerts_file.to_dict()))

        with patch(
            "loom_tools.health_monitor.find_repo_root", return_value=tmp_path
        ):
            rc = main(["--acknowledge", "alert-1"])
        assert rc == 0

    def test_clear_alerts_command(self, tmp_path: Path):
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (tmp_path / ".git").mkdir()

        alerts_file = AlertsFile(
            initialized_at=TS,
            alerts=[Alert(id="a1", type="test", message="m")],
        )
        (loom_dir / "alerts.json").write_text(json.dumps(alerts_file.to_dict()))

        with patch(
            "loom_tools.health_monitor.find_repo_root", return_value=tmp_path
        ):
            rc = main(["--clear-alerts"])
        assert rc == 0
