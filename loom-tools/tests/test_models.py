"""Tests for model from_dict / to_dict round-trips using fixture data."""

from __future__ import annotations

import json
import pathlib

import pytest

from loom_tools.models.daemon_state import (
    DaemonState,
    PipelineState,
    ShepherdEntry,
    SupportRoleEntry,
    Warning,
)
from loom_tools.models.health import (
    Alert,
    AlertsFile,
    HealthMetrics,
    MetricEntry,
    PipelineHealthMetric,
)
from loom_tools.models.progress import Milestone, ShepherdProgress
from loom_tools.models.stuck import (
    StuckDetection,
    StuckHistory,
    StuckHistoryEntry,
    StuckMetrics,
    StuckThresholds,
)

FIXTURES = pathlib.Path(__file__).parent / "fixtures"


def _load(name: str) -> dict:
    return json.loads((FIXTURES / name).read_text())


# -- DaemonState -----------------------------------------------------------


class TestDaemonState:
    @pytest.fixture()
    def raw(self) -> dict:
        return _load("daemon-state.json")

    def test_from_dict(self, raw: dict) -> None:
        ds = DaemonState.from_dict(raw)
        assert ds.iteration == 4
        assert ds.running is False
        assert ds.force_mode is True
        assert ds.execution_mode == "direct"
        assert len(ds.shepherds) == 3
        assert ds.shepherds["shepherd-2"].status == "working"
        assert ds.shepherds["shepherd-2"].issue == 1354
        assert ds.shepherds["shepherd-1"].idle_reason == "task_not_found"
        assert len(ds.support_roles) == 5
        assert ds.support_roles["guide"].status == "running"

    def test_pipeline_state(self, raw: dict) -> None:
        ds = DaemonState.from_dict(raw)
        ps = ds.pipeline_state
        assert len(ps.ready) == 3
        assert len(ps.building) == 3
        assert ps.last_updated == "2026-01-27T04:26:10Z"

    def test_round_trip(self, raw: dict) -> None:
        ds = DaemonState.from_dict(raw)
        out = ds.to_dict()
        ds2 = DaemonState.from_dict(out)
        assert ds2.iteration == ds.iteration
        assert ds2.running == ds.running
        assert len(ds2.shepherds) == len(ds.shepherds)
        assert ds2.shepherds["shepherd-2"].issue == ds.shepherds["shepherd-2"].issue

    def test_empty_dict(self) -> None:
        ds = DaemonState.from_dict({})
        assert ds.iteration == 0
        assert ds.running is False
        assert ds.shepherds == {}


class TestShepherdEntry:
    def test_idle_entry(self) -> None:
        entry = ShepherdEntry.from_dict({
            "status": "idle",
            "issue": None,
            "idle_since": "2026-01-27T04:29:30Z",
            "idle_reason": "task_not_found",
        })
        assert entry.status == "idle"
        assert entry.issue is None
        assert entry.idle_reason == "task_not_found"

    def test_to_dict_drops_none(self) -> None:
        entry = ShepherdEntry(status="working", issue=42, task_id="abc")
        d = entry.to_dict()
        assert "idle_since" not in d
        assert "idle_reason" not in d
        assert d["issue"] == 42


class TestSupportRoleEntry:
    def test_round_trip(self) -> None:
        data = {"status": "running", "task_id": "22", "started": "2026-01-27T04:18:07Z"}
        entry = SupportRoleEntry.from_dict(data)
        out = entry.to_dict()
        assert out["status"] == "running"
        assert out["task_id"] == "22"
        assert "last_completed" not in out


class TestWarning:
    def test_round_trip(self) -> None:
        data = {
            "time": "2026-01-23T11:10:00Z",
            "type": "blocked_pr",
            "severity": "warning",
            "message": "PR #1059 has merge conflicts",
            "context": {"pr_number": 1059},
            "acknowledged": False,
        }
        w = Warning.from_dict(data)
        assert w.type == "blocked_pr"
        out = w.to_dict()
        assert out == data


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


# -- ShepherdProgress ------------------------------------------------------


class TestShepherdProgress:
    @pytest.fixture()
    def raw(self) -> dict:
        return _load("progress.json")

    def test_from_dict(self, raw: dict) -> None:
        sp = ShepherdProgress.from_dict(raw)
        assert sp.task_id == "00d085c"
        assert sp.issue == 1618
        assert sp.mode == "force-merge"
        assert sp.status == "completed"
        assert len(sp.milestones) == 7

    def test_milestones(self, raw: dict) -> None:
        sp = ShepherdProgress.from_dict(raw)
        assert sp.milestones[0].event == "started"
        assert sp.milestones[0].data["issue"] == 1618
        assert sp.milestones[-1].event == "completed"
        assert sp.milestones[-1].data["pr_merged"] is True

    def test_round_trip(self, raw: dict) -> None:
        sp = ShepherdProgress.from_dict(raw)
        out = sp.to_dict()
        sp2 = ShepherdProgress.from_dict(out)
        assert sp2.task_id == sp.task_id
        assert len(sp2.milestones) == len(sp.milestones)

    def test_empty_dict(self) -> None:
        sp = ShepherdProgress.from_dict({})
        assert sp.task_id == ""
        assert sp.milestones == []


class TestMilestone:
    def test_round_trip(self) -> None:
        data = {
            "event": "pr_created",
            "timestamp": "2026-01-30T16:50:07Z",
            "data": {"pr_number": 1631},
        }
        m = Milestone.from_dict(data)
        assert m.to_dict() == data


# -- Stuck Detection -------------------------------------------------------


class TestStuckHistory:
    @pytest.fixture()
    def raw(self) -> dict:
        return _load("stuck-history.json")

    def test_from_dict(self, raw: dict) -> None:
        sh = StuckHistory.from_dict(raw)
        assert sh.created_at == "2026-01-24T18:21:48Z"
        assert len(sh.entries) == 2

    def test_entry_detection(self, raw: dict) -> None:
        sh = StuckHistory.from_dict(raw)
        entry = sh.entries[0]
        det = entry.detection
        assert det.agent_id == "shepherd-1"
        assert det.issue == 1075
        assert det.stuck is True
        assert det.severity == "elevated"
        assert det.suggested_intervention == "clarify"
        assert "no_progress:759s" in det.indicators

    def test_stuck_metrics(self, raw: dict) -> None:
        sh = StuckHistory.from_dict(raw)
        metrics = sh.entries[0].detection.metrics
        assert metrics.idle_seconds == 759
        assert metrics.error_count == 59

    def test_stuck_thresholds(self, raw: dict) -> None:
        sh = StuckHistory.from_dict(raw)
        t = sh.entries[0].detection.thresholds
        assert t.idle == 600
        assert t.working == 1800
        assert t.error_spike == 5

    def test_round_trip(self, raw: dict) -> None:
        sh = StuckHistory.from_dict(raw)
        out = sh.to_dict()
        sh2 = StuckHistory.from_dict(out)
        assert len(sh2.entries) == len(sh.entries)
        assert sh2.entries[0].detection.agent_id == sh.entries[0].detection.agent_id


class TestStuckDetection:
    def test_to_dict_drops_none_issue(self) -> None:
        det = StuckDetection(agent_id="s-1", status="idle", stuck=False)
        d = det.to_dict()
        assert "issue" not in d

    def test_to_dict_includes_issue(self) -> None:
        det = StuckDetection(agent_id="s-1", issue=42, status="working", stuck=True)
        d = det.to_dict()
        assert d["issue"] == 42


class TestStuckThresholds:
    def test_defaults(self) -> None:
        t = StuckThresholds()
        assert t.idle == 600
        assert t.working == 1800
        assert t.loop == 3
        assert t.error_spike == 5
        assert t.heartbeat_stale == 120
