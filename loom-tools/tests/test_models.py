"""Tests for model from_dict / to_dict round-trips using fixture data.

Phase 3.2 (#3399): DaemonState, ShepherdEntry, SupportRoleEntry, and Warning
test classes removed — the daemon brain and its state file are deleted.
The stub daemon_state.py exists only for Phase 3.4 fallback-path cleanup.

Phase 3.3 (#3400): ShepherdProgress and Milestone test classes removed —
models/progress.py deleted with the shepherd brain.
"""

from __future__ import annotations

import json
import pathlib

import pytest

from loom_tools.models.health import (
    Alert,
    AlertsFile,
    HealthMetrics,
    MetricEntry,
    PipelineHealthMetric,
)

FIXTURES = pathlib.Path(__file__).parent / "fixtures"


def _load(name: str) -> dict:
    return json.loads((FIXTURES / name).read_text())


# -- HealthMetrics ----------------------------------------------------------


class TestHealthMetrics:
    @pytest.fixture()
    def raw(self) -> dict:
        return _load("health-metrics.json")

    def test_from_dict(self, raw: dict) -> None:
        hm = HealthMetrics.from_dict(raw)
        assert hm.health_score == 100
        assert hm.health_status == "excellent"
        assert len(hm.metrics) == 1

    def test_metric_entry(self, raw: dict) -> None:
        hm = HealthMetrics.from_dict(raw)
        m = hm.metrics[0]
        assert m.throughput.issues_per_hour == 0
        assert m.queue_depths.building == 3
        assert m.error_rates.success_rate == 100
        assert m.resource_usage.session_percent == 40.0
        assert m.pipeline_health.status == "healthy"
        assert m.pipeline_health.blocked_count == 0

    def test_round_trip(self, raw: dict) -> None:
        hm = HealthMetrics.from_dict(raw)
        out = hm.to_dict()
        hm2 = HealthMetrics.from_dict(out)
        assert hm2.health_score == hm.health_score
        assert len(hm2.metrics) == len(hm.metrics)

    def test_empty_dict(self) -> None:
        hm = HealthMetrics.from_dict({})
        assert hm.health_score == 100
        assert hm.metrics == []


class TestAlertsFile:
    @pytest.fixture()
    def raw(self) -> dict:
        return _load("alerts.json")

    def test_from_dict(self, raw: dict) -> None:
        af = AlertsFile.from_dict(raw)
        assert len(af.alerts) == 1
        assert len(af.acknowledged) == 1
        assert af.alerts[0].type == "stuck_agents"
        assert af.acknowledged[0].acknowledged is True

    def test_alert_acknowledged_at(self, raw: dict) -> None:
        af = AlertsFile.from_dict(raw)
        acked = af.acknowledged[0]
        assert acked.acknowledged_at == "2026-01-24T16:05:00Z"
        # Non-acked alert should not have acknowledged_at
        assert af.alerts[0].acknowledged_at is None

    def test_round_trip(self, raw: dict) -> None:
        af = AlertsFile.from_dict(raw)
        out = af.to_dict()
        af2 = AlertsFile.from_dict(out)
        assert len(af2.alerts) == len(af.alerts)
        assert af2.alerts[0].id == af.alerts[0].id
