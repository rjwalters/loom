"""Tests for the stuck_detection module (post-#3392 spawn-loop port).

These tests exercise the runner against ``.loom/spawn-loop-state.json`` as
the input source. The legacy daemon-state and progress-file paths have been
retired (see #3392 AC).
"""

from __future__ import annotations

import json
import pathlib
import time
from unittest.mock import patch

import pytest

from loom_tools.models.stuck import StuckDetection, StuckMetrics, StuckThresholds
from loom_tools.stuck_detection import (
    SWEEP_AGENT_PREFIX,
    AgentState,
    DetectionResult,
    ErrorSpikeDetector,
    ExtendedWorkDetector,
    IdleTimeoutDetector,
    LoopDetector,
    StaleHeartbeatDetector,
    StuckDetectionConfig,
    StuckDetectionRunner,
    _agent_id_for_issue,
    _issue_from_agent_id,
)
from loom_tools.stuck_formatting import (
    format_agent_json,
    format_check_human,
    format_check_json,
    format_intervention_summary,
    format_status_human,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _write_spawn_loop_state(
    repo: pathlib.Path,
    running: list[dict] | None = None,
    started_at: str = "2026-06-02T18:00:00Z",
) -> pathlib.Path:
    """Materialize a synthetic ``.loom/spawn-loop-state.json`` for tests."""
    loom_dir = repo / ".loom"
    loom_dir.mkdir(exist_ok=True)
    state = {
        "started_at": started_at,
        "running": running or [],
    }
    path = loom_dir / "spawn-loop-state.json"
    path.write_text(json.dumps(state))
    return path


# ---------------------------------------------------------------------------
# Agent ID helpers
# ---------------------------------------------------------------------------


class TestAgentIDHelpers:
    def test_agent_id_for_issue(self) -> None:
        assert _agent_id_for_issue(42) == "sweep-42"
        assert _agent_id_for_issue(0) == "sweep-0"

    def test_issue_from_agent_id_valid(self) -> None:
        assert _issue_from_agent_id("sweep-42") == 42
        assert _issue_from_agent_id("sweep-0") == 0

    def test_issue_from_agent_id_rejects_non_sweep(self) -> None:
        # Daemon-style shepherd IDs are no longer recognized.
        assert _issue_from_agent_id("shepherd-1") is None
        assert _issue_from_agent_id("random") is None

    def test_issue_from_agent_id_rejects_garbage_suffix(self) -> None:
        assert _issue_from_agent_id("sweep-abc") is None
        assert _issue_from_agent_id("sweep-") is None

    def test_prefix_constant(self) -> None:
        assert SWEEP_AGENT_PREFIX == "sweep-"


# ---------------------------------------------------------------------------
# Detection result + detectors
# ---------------------------------------------------------------------------


class TestDetectionResult:
    def test_default_values(self) -> None:
        result = DetectionResult()
        assert result.detected is False
        assert result.indicator is None
        assert result.severity == "none"
        assert result.suggested_intervention == "none"

    def test_detected_result(self) -> None:
        result = DetectionResult(
            detected=True,
            indicator="test:123",
            severity="warning",
            suggested_intervention="alert",
        )
        assert result.detected is True
        assert result.indicator == "test:123"


class TestIdleTimeoutDetector:
    def test_no_output_file(self) -> None:
        detector = IdleTimeoutDetector()
        agent_state = AgentState(agent_id="sweep-1")
        result = detector.detect(agent_state, StuckThresholds())
        assert result.detected is False

    def test_missing_output_file(self) -> None:
        detector = IdleTimeoutDetector()
        agent_state = AgentState(agent_id="sweep-1", output_file="/nonexistent/file.txt")
        result = detector.detect(agent_state, StuckThresholds())
        assert result.detected is False

    def test_recent_output_file(self, tmp_path: pathlib.Path) -> None:
        output_file = tmp_path / "output.txt"
        output_file.write_text("some output")
        detector = IdleTimeoutDetector()
        agent_state = AgentState(agent_id="sweep-1", output_file=str(output_file))
        result = detector.detect(agent_state, StuckThresholds(idle=600))
        assert result.detected is False

    def test_stale_output_file(self, tmp_path: pathlib.Path) -> None:
        output_file = tmp_path / "output.txt"
        output_file.write_text("some output")
        import os

        old_time = time.time() - 700  # More than 600s threshold
        os.utime(output_file, (old_time, old_time))

        detector = IdleTimeoutDetector()
        agent_state = AgentState(agent_id="sweep-1", output_file=str(output_file))
        result = detector.detect(agent_state, StuckThresholds(idle=600))
        assert result.detected is True
        assert "no_progress" in (result.indicator or "")
        assert result.severity == "warning"


class TestStaleHeartbeatDetector:
    def test_no_heartbeat(self) -> None:
        detector = StaleHeartbeatDetector()
        agent_state = AgentState(agent_id="sweep-1")
        result = detector.detect(agent_state, StuckThresholds())
        assert result.detected is False

    def test_fresh_heartbeat(self) -> None:
        from loom_tools.common.time_utils import now_utc

        detector = StaleHeartbeatDetector()
        agent_state = AgentState(
            agent_id="sweep-1",
            heartbeat=now_utc().strftime("%Y-%m-%dT%H:%M:%SZ"),
        )
        result = detector.detect(agent_state, StuckThresholds(heartbeat_stale=120))
        assert result.detected is False

    def test_stale_heartbeat(self) -> None:
        from datetime import datetime, timedelta, timezone

        detector = StaleHeartbeatDetector()
        old_time = datetime.now(timezone.utc) - timedelta(seconds=200)
        agent_state = AgentState(
            agent_id="sweep-1",
            heartbeat=old_time.strftime("%Y-%m-%dT%H:%M:%SZ"),
        )
        result = detector.detect(agent_state, StuckThresholds(heartbeat_stale=120))
        assert result.detected is True
        assert "stale_heartbeat" in (result.indicator or "")


class TestExtendedWorkDetector:
    def test_no_started_time(self) -> None:
        detector = ExtendedWorkDetector()
        agent_state = AgentState(agent_id="sweep-1", issue=42)
        result = detector.detect(agent_state, StuckThresholds())
        assert result.detected is False

    def test_recent_start(self) -> None:
        from loom_tools.common.time_utils import now_utc

        detector = ExtendedWorkDetector()
        agent_state = AgentState(
            agent_id="sweep-1",
            issue=42,
            started=now_utc().strftime("%Y-%m-%dT%H:%M:%SZ"),
        )
        result = detector.detect(agent_state, StuckThresholds(working=1800))
        assert result.detected is False

    def test_extended_work_no_pr(self) -> None:
        from datetime import datetime, timedelta, timezone

        old_time = datetime.now(timezone.utc) - timedelta(seconds=2000)
        with patch.object(ExtendedWorkDetector, "_check_pr_exists", return_value=False):
            detector = ExtendedWorkDetector()
            agent_state = AgentState(
                agent_id="sweep-42",
                issue=42,
                started=old_time.strftime("%Y-%m-%dT%H:%M:%SZ"),
            )
            result = detector.detect(agent_state, StuckThresholds(working=1800))
            assert result.detected is True
            assert "extended_work" in (result.indicator or "")
            assert result.severity == "elevated"

    def test_extended_work_with_pr(self) -> None:
        from datetime import datetime, timedelta, timezone

        old_time = datetime.now(timezone.utc) - timedelta(seconds=2000)
        with patch.object(ExtendedWorkDetector, "_check_pr_exists", return_value=True):
            detector = ExtendedWorkDetector()
            agent_state = AgentState(
                agent_id="sweep-42",
                issue=42,
                started=old_time.strftime("%Y-%m-%dT%H:%M:%SZ"),
            )
            result = detector.detect(agent_state, StuckThresholds(working=1800))
            assert result.detected is False


class TestLoopDetector:
    def test_no_output_file(self) -> None:
        detector = LoopDetector()
        agent_state = AgentState(agent_id="sweep-1")
        result = detector.detect(agent_state, StuckThresholds())
        assert result.detected is False

    def test_no_errors(self, tmp_path: pathlib.Path) -> None:
        output_file = tmp_path / "output.txt"
        output_file.write_text("normal output\nno problems here\n")
        detector = LoopDetector()
        agent_state = AgentState(agent_id="sweep-1", output_file=str(output_file))
        result = detector.detect(agent_state, StuckThresholds(loop=3))
        assert result.detected is False

    def test_looping_errors(self, tmp_path: pathlib.Path) -> None:
        output_file = tmp_path / "output.txt"
        errors = "Error: Something failed\n" * 5
        output_file.write_text(errors)
        detector = LoopDetector()
        agent_state = AgentState(agent_id="sweep-1", output_file=str(output_file))
        result = detector.detect(agent_state, StuckThresholds(loop=3))
        assert result.detected is True
        assert "looping" in (result.indicator or "")
        assert result.severity == "critical"

    def test_count_repeated_errors(self) -> None:
        detector = LoopDetector()
        lines = [
            "Error: Connection failed",
            "Error: Connection failed",
            "Error: Connection failed",
            "Info: Retrying...",
            "Error: Connection failed",
        ]
        assert detector._count_repeated_errors(lines) == 4


class TestErrorSpikeDetector:
    def test_no_output_file(self) -> None:
        detector = ErrorSpikeDetector()
        agent_state = AgentState(agent_id="sweep-1")
        result = detector.detect(agent_state, StuckThresholds())
        assert result.detected is False

    def test_few_errors(self, tmp_path: pathlib.Path) -> None:
        output_file = tmp_path / "output.txt"
        output_file.write_text("Error: one\nError: two\n")
        detector = ErrorSpikeDetector()
        agent_state = AgentState(agent_id="sweep-1", output_file=str(output_file))
        result = detector.detect(agent_state, StuckThresholds(error_spike=5))
        assert result.detected is False

    def test_error_spike(self, tmp_path: pathlib.Path) -> None:
        output_file = tmp_path / "output.txt"
        errors = "Error: Something went wrong\n" * 10
        output_file.write_text(errors)
        detector = ErrorSpikeDetector()
        agent_state = AgentState(agent_id="sweep-1", output_file=str(output_file))
        result = detector.detect(agent_state, StuckThresholds(error_spike=5))
        assert result.detected is True
        assert "error_spike" in (result.indicator or "")
        assert result.severity == "elevated"


# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------


class TestStuckDetectionConfig:
    def test_default_values(self) -> None:
        config = StuckDetectionConfig()
        assert config.idle_threshold == 600
        assert config.working_threshold == 1800
        assert config.loop_threshold == 3
        assert config.error_spike_threshold == 5
        assert config.intervention_mode == "escalate"

    def test_from_dict(self) -> None:
        data = {
            "idle_threshold": 900,
            "working_threshold": 2400,
            "loop_threshold": 5,
            "error_spike_threshold": 10,
            "intervention_mode": "pause",
        }
        config = StuckDetectionConfig.from_dict(data)
        assert config.idle_threshold == 900
        assert config.working_threshold == 2400
        assert config.intervention_mode == "pause"

    def test_to_dict(self) -> None:
        config = StuckDetectionConfig(idle_threshold=900)
        data = config.to_dict()
        assert data["idle_threshold"] == 900
        assert "intervention_mode" in data
        # Retired field — confirm not surfaced anywhere in the schema.
        assert "no_worktree_threshold" not in data

    def test_to_thresholds(self) -> None:
        config = StuckDetectionConfig(idle_threshold=900, working_threshold=2400)
        thresholds = config.to_thresholds()
        assert thresholds.idle == 900
        assert thresholds.working == 2400


# ---------------------------------------------------------------------------
# Runner — spawn-loop integration
# ---------------------------------------------------------------------------


class TestStuckDetectionRunner:
    @pytest.fixture
    def mock_repo(self, tmp_path: pathlib.Path) -> pathlib.Path:
        """Repo with a single fresh-running spawn-loop task (#42)."""
        from loom_tools.common.time_utils import now_utc

        now_str = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
        _write_spawn_loop_state(
            tmp_path,
            running=[
                {
                    "issue": 42,
                    "pid": 12345,
                    "started_at": now_str,
                    "token": "agent-test",
                    "last_heartbeat": now_str,
                }
            ],
        )
        return tmp_path

    def test_load_default_config(self, mock_repo: pathlib.Path) -> None:
        runner = StuckDetectionRunner(mock_repo)
        assert runner.config.idle_threshold == 600
        assert runner.config.intervention_mode == "escalate"

    def test_load_custom_config(self, mock_repo: pathlib.Path) -> None:
        config_data = {"idle_threshold": 900, "intervention_mode": "pause"}
        (mock_repo / ".loom" / "stuck-config.json").write_text(json.dumps(config_data))

        runner = StuckDetectionRunner(mock_repo)
        assert runner.config.idle_threshold == 900
        assert runner.config.intervention_mode == "pause"

    def test_save_config(self, mock_repo: pathlib.Path) -> None:
        runner = StuckDetectionRunner(mock_repo)
        runner.config.idle_threshold = 1200
        runner.save_config()

        config_path = mock_repo / ".loom" / "stuck-config.json"
        saved = json.loads(config_path.read_text())
        assert saved["idle_threshold"] == 1200

    def test_check_unknown_agent_returns_status_unknown(
        self, mock_repo: pathlib.Path
    ) -> None:
        runner = StuckDetectionRunner(mock_repo)
        result = runner.check_agent("sweep-999")
        assert result.status == "unknown"
        assert result.stuck is False

    def test_check_invalid_agent_id_returns_unknown(
        self, mock_repo: pathlib.Path
    ) -> None:
        runner = StuckDetectionRunner(mock_repo)
        result = runner.check_agent("shepherd-1")  # daemon-era ID, no longer recognized
        assert result.status == "unknown"

    def test_check_healthy_agent_not_stuck(self, mock_repo: pathlib.Path) -> None:
        runner = StuckDetectionRunner(mock_repo)
        detection = runner.check_agent("sweep-42")
        assert detection.agent_id == "sweep-42"
        assert detection.issue == 42
        assert detection.status == "working"
        assert detection.stuck is False
        assert detection.metrics is not None
        assert detection.metrics.current_phase == "sweep"

    def test_check_all_empty_state_returns_empty(self, tmp_path: pathlib.Path) -> None:
        _write_spawn_loop_state(tmp_path, running=[])
        runner = StuckDetectionRunner(tmp_path)
        results, stuck = runner.check_all()
        assert results == []
        assert stuck == []

    def test_check_all_missing_state_file_returns_empty(
        self, tmp_path: pathlib.Path
    ) -> None:
        (tmp_path / ".loom").mkdir()
        # No spawn-loop-state.json on disk.
        runner = StuckDetectionRunner(tmp_path)
        results, stuck = runner.check_all()
        assert results == []
        assert stuck == []

    def test_check_all_reports_stale_heartbeat(self, tmp_path: pathlib.Path) -> None:
        """End-to-end: a task with a stale heartbeat is flagged as stuck."""
        from datetime import datetime, timedelta, timezone

        stale = (datetime.now(timezone.utc) - timedelta(seconds=900)).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        )
        start = (datetime.now(timezone.utc) - timedelta(seconds=1000)).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        )
        _write_spawn_loop_state(
            tmp_path,
            running=[
                {
                    "issue": 77,
                    "pid": 99999,
                    "started_at": start,
                    "token": "agent-x",
                    "last_heartbeat": stale,
                }
            ],
        )
        # Disable intervention writes so the test stays hermetic.
        config = StuckDetectionConfig(intervention_mode="none")
        runner = StuckDetectionRunner(tmp_path, config=config)

        results, stuck = runner.check_all()
        assert len(results) == 1
        assert stuck == ["sweep-77"]
        det = results[0]
        assert det.stuck is True
        # The runner reports the staleness via the idle-threshold path; idle was
        # 600 default and heartbeat_age was ~900 — both gates fire.
        joined_indicators = ",".join(det.indicators)
        assert "stale_heartbeat" in joined_indicators

    def test_check_all_records_history_on_stuck(self, tmp_path: pathlib.Path) -> None:
        """When a stuck agent is detected, an entry is appended to history."""
        from datetime import datetime, timedelta, timezone

        stale = (datetime.now(timezone.utc) - timedelta(seconds=900)).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        )
        _write_spawn_loop_state(
            tmp_path,
            running=[
                {
                    "issue": 88,
                    "pid": 99998,
                    "started_at": stale,
                    "token": "agent-y",
                    "last_heartbeat": stale,
                }
            ],
        )
        config = StuckDetectionConfig(intervention_mode="alert")
        runner = StuckDetectionRunner(tmp_path, config=config)
        results, stuck = runner.check_all()
        assert stuck == ["sweep-88"]
        history_file = tmp_path / ".loom" / "stuck-history.json"
        assert history_file.exists()
        history = json.loads(history_file.read_text())
        assert len(history["entries"]) == 1
        assert history["entries"][0]["detection"]["agent_id"] == "sweep-88"

    def test_runner_has_no_missing_milestone_detector(
        self, mock_repo: pathlib.Path
    ) -> None:
        """Regression: the milestone detector was retired in #3392."""
        runner = StuckDetectionRunner(mock_repo)
        detector_class_names = {type(d).__name__ for d in runner.detectors}
        assert "MissingMilestoneDetector" not in detector_class_names
        assert "NoWorktreeDetector" not in detector_class_names


# ---------------------------------------------------------------------------
# Formatters (compatibility surface)
# ---------------------------------------------------------------------------


class TestFormatFunctions:
    def test_format_check_json(self) -> None:
        results = [
            StuckDetection(
                agent_id="sweep-42",
                issue=42,
                status="working",
                stuck=True,
                severity="warning",
                indicators=["no_progress:600s"],
            ),
        ]
        config = StuckDetectionConfig()
        output = format_check_json(results, ["sweep-42"], config)
        data = json.loads(output)
        assert data["stuck_count"] == 1
        assert "sweep-42" in data["stuck_agents"]
        assert len(data["results"]) == 1

    def test_format_check_human(self) -> None:
        results = [
            StuckDetection(
                agent_id="sweep-42",
                issue=42,
                status="working",
                stuck=True,
                severity="warning",
                indicators=["no_progress:600s"],
            ),
        ]
        config = StuckDetectionConfig()
        output = format_check_human(results, ["sweep-42"], config)
        assert "STUCK AGENT DETECTION" in output
        assert "sweep-42" in output
        assert "no_progress:600s" in output

    def test_format_agent_json(self) -> None:
        detection = StuckDetection(
            agent_id="sweep-42",
            issue=42,
            status="working",
            stuck=True,
            indicators=["stale_heartbeat:900s"],
        )
        output = format_agent_json(detection)
        data = json.loads(output)
        assert data["agent_id"] == "sweep-42"
        # Field still present in output for shell-consumer compatibility,
        # even though the detector that populated it is retired.
        assert "missing_milestones" in data
        # No milestone in indicators -> empty list.
        assert data["missing_milestones"] == []

    def test_format_status_human(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        config = StuckDetectionConfig()
        output = format_status_human(tmp_path, config)
        assert "STUCK DETECTION STATUS" in output
        assert "Configuration:" in output
        assert "Active Interventions:" in output

    def test_format_intervention_summary_alert_uses_sweep_log_path(
        self, tmp_path: pathlib.Path
    ) -> None:
        loom_dir = tmp_path / ".loom"
        detection = StuckDetection(
            agent_id="sweep-42",
            issue=42,
            stuck=True,
            severity="warning",
            suggested_intervention="alert",
            indicators=["stale_heartbeat:600s"],
        )
        output = format_intervention_summary(detection, "2026-06-02T18:00:00Z", loom_dir)
        # The post-#3392 alert hint points at the spawn-loop log, NOT
        # daemon-state.json::shepherds[].output_file.
        assert "sweep-issue-42.log" in output
        assert "daemon-state.json" not in output

    def test_format_intervention_summary_pause(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        detection = StuckDetection(
            agent_id="sweep-99",
            issue=99,
            stuck=True,
            severity="critical",
            suggested_intervention="pause",
            indicators=["looping:5x"],
        )
        output = format_intervention_summary(detection, "2026-06-02T18:00:00Z", loom_dir)
        assert "paused automatically" in output
        assert "signal.sh clear sweep-99" in output

    def test_format_intervention_summary_no_issue(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        detection = StuckDetection(
            agent_id="sweep-999",
            stuck=True,
            severity="warning",
            suggested_intervention="escalate",
            indicators=["stale_heartbeat:300s"],
        )
        output = format_intervention_summary(detection, "2026-06-02T18:00:00Z", loom_dir)
        assert "Issue:       none" in output
        assert "ESCALATION" in output


class TestSeverityEscalation:
    def test_extended_work_severity(self) -> None:
        from datetime import datetime, timedelta, timezone

        old_time = datetime.now(timezone.utc) - timedelta(seconds=2000)
        thresholds = StuckThresholds(idle=600, working=1800)
        agent_state = AgentState(
            agent_id="sweep-42",
            issue=42,
            started=old_time.strftime("%Y-%m-%dT%H:%M:%SZ"),
        )
        with patch.object(ExtendedWorkDetector, "_check_pr_exists", return_value=False):
            detector = ExtendedWorkDetector()
            result = detector.detect(agent_state, thresholds)
            assert result.severity == "elevated"

    def test_looping_always_critical(self, tmp_path: pathlib.Path) -> None:
        output_file = tmp_path / "output.txt"
        output_file.write_text("Error: loop detected\n" * 10)
        detector = LoopDetector()
        agent_state = AgentState(agent_id="sweep-1", output_file=str(output_file))
        result = detector.detect(agent_state, StuckThresholds(loop=3))
        assert result.severity == "critical"
        assert result.suggested_intervention == "pause"


class TestJSONOutputCompatibility:
    """Verify JSON output shape is preserved for existing shell consumers."""

    def test_single_agent_json_format(self) -> None:
        detection = StuckDetection(
            agent_id="sweep-123",
            issue=123,
            status="working",
            stuck=True,
            severity="warning",
            suggested_intervention="alert",
            indicators=["stale_heartbeat:600s"],
            metrics=StuckMetrics(
                idle_seconds=600,
                heartbeat_age=600,
                working_seconds=1200,
                loop_count=0,
                error_count=0,
                current_phase="sweep",
            ),
            thresholds=StuckThresholds(
                idle=600, working=1800, loop=3, error_spike=5, heartbeat_stale=120
            ),
            checked_at="2026-06-02T18:00:00Z",
        )
        output = format_agent_json(detection)
        data = json.loads(output)
        assert data["agent_id"] == "sweep-123"
        assert data["issue"] == 123
        assert data["status"] == "working"
        assert data["stuck"] is True
        assert data["severity"] == "warning"
        assert data["suggested_intervention"] == "alert"
        assert data["indicators"] == ["stale_heartbeat:600s"]
        assert data["metrics"]["idle_seconds"] == 600
        assert data["metrics"]["current_phase"] == "sweep"
        assert data["thresholds"]["idle"] == 600
        assert "missing_milestones" in data

    def test_check_all_json_format(self) -> None:
        results = [
            StuckDetection(
                agent_id="sweep-123", issue=123, stuck=True, severity="warning"
            ),
            StuckDetection(agent_id="sweep-456", status="idle", stuck=False),
        ]
        config = StuckDetectionConfig()
        output = format_check_json(results, ["sweep-123"], config)
        data = json.loads(output)
        assert "checked_at" in data
        assert data["total_checked"] == 2
        assert data["stuck_count"] == 1
        assert data["stuck_agents"] == ["sweep-123"]
        assert len(data["results"]) == 2
        assert "config" in data
        assert data["config"]["idle_threshold"] == 600


class TestHistoryTracking:
    """History file management is unchanged by the port."""

    def test_history_append(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()

        history_data = {"created_at": "2026-06-02T18:00:00Z", "entries": []}
        history_path = loom_dir / "stuck-history.json"
        history_path.write_text(json.dumps(history_data))

        _write_spawn_loop_state(tmp_path, running=[])

        runner = StuckDetectionRunner(tmp_path)
        detection = StuckDetection(agent_id="sweep-42", issue=42, stuck=True)
        runner._record_detection(detection)

        updated = json.loads(history_path.read_text())
        assert len(updated["entries"]) == 1
        assert updated["entries"][0]["detection"]["agent_id"] == "sweep-42"

    def test_history_max_entries(self, tmp_path: pathlib.Path) -> None:
        from loom_tools.stuck_detection import MAX_HISTORY_ENTRIES

        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()

        entries = [
            {
                "detected_at": "2026-06-02T18:00:00Z",
                "detection": {"agent_id": f"sweep-{i}", "stuck": True},
            }
            for i in range(MAX_HISTORY_ENTRIES)
        ]
        history_data = {"created_at": "2026-06-02T18:00:00Z", "entries": entries}
        history_path = loom_dir / "stuck-history.json"
        history_path.write_text(json.dumps(history_data))

        _write_spawn_loop_state(tmp_path, running=[])

        runner = StuckDetectionRunner(tmp_path)
        detection = StuckDetection(agent_id="sweep-new", issue=42, stuck=True)
        runner._record_detection(detection)

        updated = json.loads(history_path.read_text())
        assert len(updated["entries"]) == MAX_HISTORY_ENTRIES
        assert updated["entries"][-1]["detection"]["agent_id"] == "sweep-new"
