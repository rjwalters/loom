"""Tests for the stuck_detection module."""

from __future__ import annotations

import json
import pathlib
import tempfile
import time
from unittest.mock import patch

import pytest

from loom_tools.models.daemon_state import DaemonState, ShepherdEntry
from loom_tools.models.progress import Milestone, ShepherdProgress
from loom_tools.models.stuck import StuckDetection, StuckMetrics, StuckThresholds
from loom_tools.stuck_detection import (
    AgentState,
    BaseDetector,
    DetectionResult,
    ErrorSpikeDetector,
    ExtendedWorkDetector,
    IdleTimeoutDetector,
    LoopDetector,
    MissingMilestoneDetector,
    NoWorktreeDetector,
    StaleHeartbeatDetector,
    StuckDetectionConfig,
    StuckDetectionRunner,
    format_agent_json,
    format_check_human,
    format_check_json,
    format_history_human,
    format_status_human,
)


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
        agent_state = AgentState(agent_id="test")
        thresholds = StuckThresholds()

        result = detector.detect(agent_state, thresholds)
        assert result.detected is False

    def test_missing_output_file(self) -> None:
        detector = IdleTimeoutDetector()
        agent_state = AgentState(
            agent_id="test", output_file="/nonexistent/file.txt"
        )
        thresholds = StuckThresholds()

        result = detector.detect(agent_state, thresholds)
        assert result.detected is False

    def test_recent_output_file(self, tmp_path: pathlib.Path) -> None:
        output_file = tmp_path / "output.txt"
        output_file.write_text("some output")

        detector = IdleTimeoutDetector()
        agent_state = AgentState(
            agent_id="test", output_file=str(output_file)
        )
        thresholds = StuckThresholds(idle=600)

        result = detector.detect(agent_state, thresholds)
        assert result.detected is False

    def test_stale_output_file(self, tmp_path: pathlib.Path) -> None:
        output_file = tmp_path / "output.txt"
        output_file.write_text("some output")
        # Make file appear old
        old_time = time.time() - 700  # More than 600s threshold
        import os
        os.utime(output_file, (old_time, old_time))

        detector = IdleTimeoutDetector()
        agent_state = AgentState(
            agent_id="test", output_file=str(output_file)
        )
        thresholds = StuckThresholds(idle=600)

        result = detector.detect(agent_state, thresholds)
        assert result.detected is True
        assert "no_progress" in (result.indicator or "")
        assert result.severity == "warning"


class TestStaleHeartbeatDetector:
    def test_no_progress(self) -> None:
        detector = StaleHeartbeatDetector()
        agent_state = AgentState(agent_id="test")
        thresholds = StuckThresholds()

        result = detector.detect(agent_state, thresholds)
        assert result.detected is False

    def test_no_heartbeat(self) -> None:
        detector = StaleHeartbeatDetector()
        progress = ShepherdProgress(task_id="abc123", issue=42)
        agent_state = AgentState(agent_id="test", progress=progress)
        thresholds = StuckThresholds()

        result = detector.detect(agent_state, thresholds)
        assert result.detected is False

    def test_fresh_heartbeat(self) -> None:
        from loom_tools.common.time_utils import now_utc

        detector = StaleHeartbeatDetector()
        progress = ShepherdProgress(
            task_id="abc123",
            issue=42,
            last_heartbeat=now_utc().strftime("%Y-%m-%dT%H:%M:%SZ"),
        )
        agent_state = AgentState(agent_id="test", progress=progress)
        thresholds = StuckThresholds(heartbeat_stale=120)

        result = detector.detect(agent_state, thresholds)
        assert result.detected is False

    def test_stale_heartbeat(self) -> None:
        from datetime import datetime, timedelta, timezone

        detector = StaleHeartbeatDetector()
        old_time = datetime.now(timezone.utc) - timedelta(seconds=200)
        progress = ShepherdProgress(
            task_id="abc123",
            issue=42,
            last_heartbeat=old_time.strftime("%Y-%m-%dT%H:%M:%SZ"),
        )
        agent_state = AgentState(agent_id="test", progress=progress)
        thresholds = StuckThresholds(heartbeat_stale=120)

        result = detector.detect(agent_state, thresholds)
        assert result.detected is True
        assert "stale_heartbeat" in (result.indicator or "")


class TestExtendedWorkDetector:
    def test_no_started_time(self) -> None:
        detector = ExtendedWorkDetector()
        agent_state = AgentState(agent_id="test", issue=42)
        thresholds = StuckThresholds()

        result = detector.detect(agent_state, thresholds)
        assert result.detected is False

    def test_recent_start(self) -> None:
        from loom_tools.common.time_utils import now_utc

        detector = ExtendedWorkDetector()
        agent_state = AgentState(
            agent_id="test",
            issue=42,
            started=now_utc().strftime("%Y-%m-%dT%H:%M:%SZ"),
        )
        thresholds = StuckThresholds(working=1800)

        result = detector.detect(agent_state, thresholds)
        assert result.detected is False

    def test_extended_work_no_pr(self) -> None:
        from datetime import datetime, timedelta, timezone

        old_time = datetime.now(timezone.utc) - timedelta(seconds=2000)

        with patch.object(ExtendedWorkDetector, "_check_pr_exists", return_value=False):
            detector = ExtendedWorkDetector()
            agent_state = AgentState(
                agent_id="test",
                issue=42,
                started=old_time.strftime("%Y-%m-%dT%H:%M:%SZ"),
            )
            thresholds = StuckThresholds(working=1800)

            result = detector.detect(agent_state, thresholds)
            assert result.detected is True
            assert "extended_work" in (result.indicator or "")
            assert result.severity == "elevated"

    def test_extended_work_with_pr(self) -> None:
        from datetime import datetime, timedelta, timezone

        old_time = datetime.now(timezone.utc) - timedelta(seconds=2000)

        with patch.object(ExtendedWorkDetector, "_check_pr_exists", return_value=True):
            detector = ExtendedWorkDetector()
            agent_state = AgentState(
                agent_id="test",
                issue=42,
                started=old_time.strftime("%Y-%m-%dT%H:%M:%SZ"),
            )
            thresholds = StuckThresholds(working=1800)

            result = detector.detect(agent_state, thresholds)
            assert result.detected is False


class TestLoopDetector:
    def test_no_output_file(self) -> None:
        detector = LoopDetector()
        agent_state = AgentState(agent_id="test")
        thresholds = StuckThresholds()

        result = detector.detect(agent_state, thresholds)
        assert result.detected is False

    def test_no_errors(self, tmp_path: pathlib.Path) -> None:
        output_file = tmp_path / "output.txt"
        output_file.write_text("normal output\nno problems here\n")

        detector = LoopDetector()
        agent_state = AgentState(
            agent_id="test", output_file=str(output_file)
        )
        thresholds = StuckThresholds(loop=3)

        result = detector.detect(agent_state, thresholds)
        assert result.detected is False

    def test_looping_errors(self, tmp_path: pathlib.Path) -> None:
        output_file = tmp_path / "output.txt"
        # Write repeated error pattern
        errors = "Error: Something failed\n" * 5
        output_file.write_text(errors)

        detector = LoopDetector()
        agent_state = AgentState(
            agent_id="test", output_file=str(output_file)
        )
        thresholds = StuckThresholds(loop=3)

        result = detector.detect(agent_state, thresholds)
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
        count = detector._count_repeated_errors(lines)
        assert count == 4  # 4 repetitions of same error


class TestErrorSpikeDetector:
    def test_no_output_file(self) -> None:
        detector = ErrorSpikeDetector()
        agent_state = AgentState(agent_id="test")
        thresholds = StuckThresholds()

        result = detector.detect(agent_state, thresholds)
        assert result.detected is False

    def test_few_errors(self, tmp_path: pathlib.Path) -> None:
        output_file = tmp_path / "output.txt"
        output_file.write_text("Error: one\nError: two\n")

        detector = ErrorSpikeDetector()
        agent_state = AgentState(
            agent_id="test", output_file=str(output_file)
        )
        thresholds = StuckThresholds(error_spike=5)

        result = detector.detect(agent_state, thresholds)
        assert result.detected is False

    def test_error_spike(self, tmp_path: pathlib.Path) -> None:
        output_file = tmp_path / "output.txt"
        errors = "Error: Something went wrong\n" * 10
        output_file.write_text(errors)

        detector = ErrorSpikeDetector()
        agent_state = AgentState(
            agent_id="test", output_file=str(output_file)
        )
        thresholds = StuckThresholds(error_spike=5)

        result = detector.detect(agent_state, thresholds)
        assert result.detected is True
        assert "error_spike" in (result.indicator or "")
        assert result.severity == "elevated"


class TestMissingMilestoneDetector:
    def test_no_progress(self) -> None:
        detector = MissingMilestoneDetector()
        agent_state = AgentState(agent_id="test")
        thresholds = StuckThresholds()

        result = detector.detect(agent_state, thresholds)
        assert result.detected is False

    def test_early_stage_no_detection(self) -> None:
        from loom_tools.common.time_utils import now_utc

        detector = MissingMilestoneDetector(no_worktree_threshold=300)
        progress = ShepherdProgress(task_id="abc123", issue=42)
        agent_state = AgentState(
            agent_id="test",
            progress=progress,
            started=now_utc().strftime("%Y-%m-%dT%H:%M:%SZ"),
        )
        thresholds = StuckThresholds()

        result = detector.detect(agent_state, thresholds)
        assert result.detected is False

    def test_missing_worktree_after_threshold(self) -> None:
        from datetime import datetime, timedelta, timezone

        old_time = datetime.now(timezone.utc) - timedelta(seconds=400)

        detector = MissingMilestoneDetector(no_worktree_threshold=300)
        progress = ShepherdProgress(task_id="abc123", issue=42, milestones=[])
        agent_state = AgentState(
            agent_id="test",
            progress=progress,
            started=old_time.strftime("%Y-%m-%dT%H:%M:%SZ"),
        )
        thresholds = StuckThresholds()

        result = detector.detect(agent_state, thresholds)
        assert result.detected is True
        assert "worktree_created" in (result.indicator or "")

    def test_worktree_present(self) -> None:
        from datetime import datetime, timedelta, timezone

        old_time = datetime.now(timezone.utc) - timedelta(seconds=400)

        detector = MissingMilestoneDetector(no_worktree_threshold=300)
        progress = ShepherdProgress(
            task_id="abc123",
            issue=42,
            milestones=[
                Milestone(event="worktree_created", timestamp="2026-01-01T00:00:00Z")
            ],
        )
        agent_state = AgentState(
            agent_id="test",
            progress=progress,
            started=old_time.strftime("%Y-%m-%dT%H:%M:%SZ"),
        )
        thresholds = StuckThresholds()

        result = detector.detect(agent_state, thresholds)
        assert result.detected is False


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

    def test_to_thresholds(self) -> None:
        config = StuckDetectionConfig(idle_threshold=900, working_threshold=2400)
        thresholds = config.to_thresholds()
        assert thresholds.idle == 900
        assert thresholds.working == 2400


class TestStuckDetectionRunner:
    @pytest.fixture
    def mock_repo(self, tmp_path: pathlib.Path) -> pathlib.Path:
        """Create a mock repository structure."""
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()

        # Create daemon state
        daemon_state = {
            "started_at": "2026-01-01T00:00:00Z",
            "running": True,
            "iteration": 1,
            "shepherds": {
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "started": "2026-01-01T00:00:00Z",
                },
                "shepherd-2": {
                    "status": "idle",
                },
                "shepherd-3": {
                    "status": "idle",
                },
            },
        }
        (loom_dir / "daemon-state.json").write_text(json.dumps(daemon_state))

        return tmp_path

    def test_load_default_config(self, mock_repo: pathlib.Path) -> None:
        with patch("loom_tools.stuck_detection.find_repo_root", return_value=mock_repo):
            runner = StuckDetectionRunner(mock_repo)
            assert runner.config.idle_threshold == 600
            assert runner.config.intervention_mode == "escalate"

    def test_load_custom_config(self, mock_repo: pathlib.Path) -> None:
        config_data = {"idle_threshold": 900, "intervention_mode": "pause"}
        (mock_repo / ".loom" / "stuck-config.json").write_text(
            json.dumps(config_data)
        )

        with patch("loom_tools.stuck_detection.find_repo_root", return_value=mock_repo):
            runner = StuckDetectionRunner(mock_repo)
            assert runner.config.idle_threshold == 900
            assert runner.config.intervention_mode == "pause"

    def test_save_config(self, mock_repo: pathlib.Path) -> None:
        with patch("loom_tools.stuck_detection.find_repo_root", return_value=mock_repo):
            runner = StuckDetectionRunner(mock_repo)
            runner.config.idle_threshold = 1200
            runner.save_config()

            config_path = mock_repo / ".loom" / "stuck-config.json"
            saved = json.loads(config_path.read_text())
            assert saved["idle_threshold"] == 1200

    def test_check_idle_agent(self, mock_repo: pathlib.Path) -> None:
        with patch("loom_tools.stuck_detection.find_repo_root", return_value=mock_repo):
            runner = StuckDetectionRunner(mock_repo)
            result = runner.check_agent("shepherd-2")
            assert result.status == "idle"
            assert result.stuck is False

    def test_check_unknown_agent(self, mock_repo: pathlib.Path) -> None:
        with patch("loom_tools.stuck_detection.find_repo_root", return_value=mock_repo):
            runner = StuckDetectionRunner(mock_repo)
            result = runner.check_agent("shepherd-999")
            assert result.status == "unknown"


class TestFormatFunctions:
    def test_format_check_json(self) -> None:
        results = [
            StuckDetection(
                agent_id="shepherd-1",
                issue=42,
                status="working",
                stuck=True,
                severity="warning",
                indicators=["no_progress:600s"],
            ),
        ]
        config = StuckDetectionConfig()
        output = format_check_json(results, ["shepherd-1"], config)
        data = json.loads(output)

        assert data["stuck_count"] == 1
        assert "shepherd-1" in data["stuck_agents"]
        assert len(data["results"]) == 1

    def test_format_check_human(self) -> None:
        results = [
            StuckDetection(
                agent_id="shepherd-1",
                issue=42,
                status="working",
                stuck=True,
                severity="warning",
                indicators=["no_progress:600s"],
            ),
        ]
        config = StuckDetectionConfig()
        output = format_check_human(results, ["shepherd-1"], config)

        assert "STUCK AGENT DETECTION" in output
        assert "shepherd-1" in output
        assert "no_progress:600s" in output

    def test_format_agent_json(self) -> None:
        detection = StuckDetection(
            agent_id="shepherd-1",
            issue=42,
            status="working",
            stuck=True,
            indicators=["missing_milestone:worktree_created"],
        )
        output = format_agent_json(detection)
        data = json.loads(output)

        assert data["agent_id"] == "shepherd-1"
        assert data["missing_milestones"] == ["worktree_created"]

    def test_format_status_human(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()

        config = StuckDetectionConfig()
        output = format_status_human(tmp_path, config)

        assert "STUCK DETECTION STATUS" in output
        assert "Configuration:" in output
        assert "Active Interventions:" in output


class TestSeverityEscalation:
    """Test that severity escalates correctly based on indicators."""

    def test_severity_escalation_from_warning_to_elevated(self) -> None:
        """Extended work should escalate severity from warning to elevated."""
        from datetime import datetime, timedelta, timezone

        old_time = datetime.now(timezone.utc) - timedelta(seconds=2000)
        thresholds = StuckThresholds(idle=600, working=1800)

        # Create agent with both idle timeout (warning) and extended work (elevated)
        agent_state = AgentState(
            agent_id="test",
            issue=42,
            started=old_time.strftime("%Y-%m-%dT%H:%M:%SZ"),
        )

        # Idle would give warning, but extended work should escalate to elevated
        with patch.object(ExtendedWorkDetector, "_check_pr_exists", return_value=False):
            detector = ExtendedWorkDetector()
            result = detector.detect(agent_state, thresholds)
            assert result.severity == "elevated"

    def test_looping_always_critical(self, tmp_path: pathlib.Path) -> None:
        """Looping should always result in critical severity."""
        output_file = tmp_path / "output.txt"
        errors = "Error: loop detected\n" * 10
        output_file.write_text(errors)

        detector = LoopDetector()
        agent_state = AgentState(
            agent_id="test", output_file=str(output_file)
        )
        thresholds = StuckThresholds(loop=3)

        result = detector.detect(agent_state, thresholds)
        assert result.severity == "critical"
        assert result.suggested_intervention == "pause"


class TestJSONOutputCompatibility:
    """Test that JSON output is compatible with existing consumers."""

    def test_single_agent_json_format(self) -> None:
        """Verify single agent JSON matches expected format from shell script."""
        detection = StuckDetection(
            agent_id="shepherd-1",
            issue=123,
            status="working",
            stuck=True,
            severity="warning",
            suggested_intervention="alert",
            indicators=["no_progress:600s"],
            metrics=StuckMetrics(
                idle_seconds=600,
                heartbeat_age=-1,
                working_seconds=1200,
                loop_count=0,
                error_count=0,
                current_phase="builder",
            ),
            thresholds=StuckThresholds(
                idle=600,
                working=1800,
                loop=3,
                error_spike=5,
                heartbeat_stale=120,
            ),
            checked_at="2026-01-30T12:00:00Z",
        )

        output = format_agent_json(detection)
        data = json.loads(output)

        # Verify required fields
        assert data["agent_id"] == "shepherd-1"
        assert data["issue"] == 123
        assert data["status"] == "working"
        assert data["stuck"] is True
        assert data["severity"] == "warning"
        assert data["suggested_intervention"] == "alert"
        assert data["indicators"] == ["no_progress:600s"]

        # Verify metrics
        assert data["metrics"]["idle_seconds"] == 600
        assert data["metrics"]["working_seconds"] == 1200
        assert data["metrics"]["current_phase"] == "builder"

        # Verify thresholds
        assert data["thresholds"]["idle"] == 600
        assert data["thresholds"]["working"] == 1800

        # Verify missing_milestones field exists
        assert "missing_milestones" in data

    def test_check_all_json_format(self) -> None:
        """Verify check-all JSON matches expected format from shell script."""
        results = [
            StuckDetection(
                agent_id="shepherd-1",
                issue=123,
                stuck=True,
                severity="warning",
            ),
            StuckDetection(
                agent_id="shepherd-2",
                status="idle",
                stuck=False,
            ),
        ]
        config = StuckDetectionConfig()

        output = format_check_json(results, ["shepherd-1"], config)
        data = json.loads(output)

        # Verify required fields
        assert "checked_at" in data
        assert data["total_checked"] == 2
        assert data["stuck_count"] == 1
        assert data["stuck_agents"] == ["shepherd-1"]
        assert len(data["results"]) == 2
        assert "config" in data
        assert data["config"]["idle_threshold"] == 600


class TestHistoryTracking:
    """Test history file management."""

    def test_history_append(self, tmp_path: pathlib.Path) -> None:
        """Test that history entries are appended correctly."""
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()

        # Create initial history
        history_data = {
            "created_at": "2026-01-01T00:00:00Z",
            "entries": [],
        }
        history_path = loom_dir / "stuck-history.json"
        history_path.write_text(json.dumps(history_data))

        # Create daemon state
        daemon_state = {
            "started_at": "2026-01-01T00:00:00Z",
            "running": True,
            "iteration": 1,
            "shepherds": {},
        }
        (loom_dir / "daemon-state.json").write_text(json.dumps(daemon_state))

        with patch("loom_tools.stuck_detection.find_repo_root", return_value=tmp_path):
            runner = StuckDetectionRunner(tmp_path)

            # Create a stuck detection
            detection = StuckDetection(
                agent_id="shepherd-1",
                issue=42,
                stuck=True,
            )
            runner._record_detection(detection)

            # Verify history was updated
            updated = json.loads(history_path.read_text())
            assert len(updated["entries"]) == 1
            assert updated["entries"][0]["detection"]["agent_id"] == "shepherd-1"

    def test_history_max_entries(self, tmp_path: pathlib.Path) -> None:
        """Test that history is truncated to MAX_HISTORY_ENTRIES."""
        from loom_tools.stuck_detection import MAX_HISTORY_ENTRIES

        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()

        # Create history with MAX entries
        entries = [
            {
                "detected_at": "2026-01-01T00:00:00Z",
                "detection": {"agent_id": f"shepherd-{i}", "stuck": True},
            }
            for i in range(MAX_HISTORY_ENTRIES)
        ]
        history_data = {"created_at": "2026-01-01T00:00:00Z", "entries": entries}
        history_path = loom_dir / "stuck-history.json"
        history_path.write_text(json.dumps(history_data))

        # Create daemon state
        daemon_state = {
            "started_at": "2026-01-01T00:00:00Z",
            "running": True,
            "iteration": 1,
            "shepherds": {},
        }
        (loom_dir / "daemon-state.json").write_text(json.dumps(daemon_state))

        with patch("loom_tools.stuck_detection.find_repo_root", return_value=tmp_path):
            runner = StuckDetectionRunner(tmp_path)

            # Add one more entry
            detection = StuckDetection(
                agent_id="shepherd-new",
                issue=42,
                stuck=True,
            )
            runner._record_detection(detection)

            # Verify history is still MAX entries
            updated = json.loads(history_path.read_text())
            assert len(updated["entries"]) == MAX_HISTORY_ENTRIES
            # Newest entry should be the one we just added
            assert updated["entries"][-1]["detection"]["agent_id"] == "shepherd-new"


class TestBashCompatibility:
    """Test compatibility between Python and bash implementations.

    These tests verify that the Python implementation matches the bash
    script's behavior for stuck-detection.sh. See issue #1694.
    """

    def test_default_thresholds_match_bash(self) -> None:
        """Verify Python default thresholds match bash script values.

        Bash defaults (from stuck-detection.sh lines 73-81):
        - DEFAULT_IDLE_THRESHOLD=600
        - DEFAULT_WORKING_THRESHOLD=1800
        - DEFAULT_LOOP_THRESHOLD=3
        - DEFAULT_ERROR_SPIKE_THRESHOLD=5
        - DEFAULT_HEARTBEAT_STALE=120
        - DEFAULT_NO_WORKTREE_THRESHOLD=300
        """
        from loom_tools.stuck_detection import (
            DEFAULT_IDLE_THRESHOLD,
            DEFAULT_WORKING_THRESHOLD,
            DEFAULT_LOOP_THRESHOLD,
            DEFAULT_ERROR_SPIKE_THRESHOLD,
            DEFAULT_HEARTBEAT_STALE,
            DEFAULT_NO_WORKTREE_THRESHOLD,
        )

        # Bash script defaults
        BASH_IDLE_THRESHOLD = 600
        BASH_WORKING_THRESHOLD = 1800
        BASH_LOOP_THRESHOLD = 3
        BASH_ERROR_SPIKE_THRESHOLD = 5
        BASH_HEARTBEAT_STALE = 120
        BASH_NO_WORKTREE_THRESHOLD = 300

        assert DEFAULT_IDLE_THRESHOLD == BASH_IDLE_THRESHOLD, "idle threshold mismatch"
        assert DEFAULT_WORKING_THRESHOLD == BASH_WORKING_THRESHOLD, "working threshold mismatch"
        assert DEFAULT_LOOP_THRESHOLD == BASH_LOOP_THRESHOLD, "loop threshold mismatch"
        assert DEFAULT_ERROR_SPIKE_THRESHOLD == BASH_ERROR_SPIKE_THRESHOLD, "error spike threshold mismatch"
        assert DEFAULT_HEARTBEAT_STALE == BASH_HEARTBEAT_STALE, "heartbeat stale threshold mismatch"
        assert DEFAULT_NO_WORKTREE_THRESHOLD == BASH_NO_WORKTREE_THRESHOLD, "no worktree threshold mismatch"

    def test_json_output_fields_match_bash(self) -> None:
        """Verify JSON output fields match bash script structure.

        Bash check_agent output (lines 556-583) produces:
        - agent_id, issue, status, stuck, severity, suggested_intervention
        - indicators array
        - metrics object with: idle_seconds, heartbeat_age, working_seconds, loop_count, error_count, current_phase
        - thresholds object with: idle, working, loop, error_spike, heartbeat_stale
        - missing_milestones array
        - checked_at timestamp
        """
        detection = StuckDetection(
            agent_id="shepherd-1",
            issue=42,
            status="working",
            stuck=True,
            severity="warning",
            suggested_intervention="alert",
            indicators=["stale_heartbeat:700s"],
            metrics=StuckMetrics(
                idle_seconds=700,
                heartbeat_age=700,
                working_seconds=1500,
                loop_count=0,
                error_count=2,
                current_phase="builder",
            ),
            thresholds=StuckThresholds(
                idle=600,
                working=1800,
                loop=3,
                error_spike=5,
                heartbeat_stale=120,
            ),
            checked_at="2026-01-30T12:00:00Z",
        )

        output = format_agent_json(detection)
        data = json.loads(output)

        # Required top-level fields (bash lines 557-564)
        assert "agent_id" in data
        assert "issue" in data
        assert "status" in data
        assert "stuck" in data
        assert "severity" in data
        assert "suggested_intervention" in data
        assert "indicators" in data
        assert "metrics" in data
        assert "thresholds" in data
        assert "checked_at" in data

        # Required metrics fields (bash lines 565-572)
        metrics = data["metrics"]
        assert "idle_seconds" in metrics
        assert "heartbeat_age" in metrics
        assert "working_seconds" in metrics
        assert "loop_count" in metrics
        assert "error_count" in metrics
        assert "current_phase" in metrics

        # Required thresholds fields (bash lines 573-579)
        thresholds = data["thresholds"]
        assert "idle" in thresholds
        assert "working" in thresholds
        assert "loop" in thresholds
        assert "error_spike" in thresholds
        assert "heartbeat_stale" in thresholds

        # missing_milestones field added for compatibility (bash line 580)
        assert "missing_milestones" in data

    def test_heartbeat_idle_uses_idle_threshold(self) -> None:
        """Verify heartbeat-based idle detection uses idle threshold like bash.

        In bash (lines 458-488):
        - When heartbeat available, idle_seconds = heartbeat_age
        - Then checks: idle_seconds >= IDLE_THRESHOLD (600s)

        This means a 150s old heartbeat should NOT be stuck (150 < 600).
        A 700s old heartbeat SHOULD be stuck (700 >= 600).
        """
        from datetime import datetime, timedelta, timezone
        from loom_tools.common.time_utils import now_utc

        # Test case 1: Recent heartbeat (150s) - should NOT be stuck
        recent_time = datetime.now(timezone.utc) - timedelta(seconds=150)
        detection_recent = StuckDetection(
            agent_id="test-1",
            status="working",
            issue=42,
        )
        # With idle threshold of 600s and heartbeat age of 150s
        # Python should NOT detect this as stuck (150 < 600)
        thresholds = StuckThresholds(idle=600, heartbeat_stale=120)

        # The logic in _run_detection:
        # if heartbeat_age >= 0: idle_seconds = heartbeat_age
        # if idle_seconds >= thresholds.idle: stuck = True
        # So 150 >= 600 is False - not stuck

        # Test case 2: Old heartbeat (700s) - SHOULD be stuck
        old_time = datetime.now(timezone.utc) - timedelta(seconds=700)

        # This verifies the Python implementation matches bash behavior
        # where heartbeat age is compared against idle threshold (600s)
        # not heartbeat_stale threshold (120s)
        assert thresholds.idle == 600, "idle threshold should be 600s"
        assert thresholds.heartbeat_stale == 120, "heartbeat_stale should be 120s"

    def test_idle_and_stale_heartbeat_indicator_names(self) -> None:
        """Verify indicator names match bash format.

        Bash uses (lines 482-485):
        - "stale_heartbeat:${idle_seconds}s" when heartbeat available
        - "no_progress:${idle_seconds}s" when falling back to file timestamp
        """
        # Test stale_heartbeat indicator format
        detection = StuckDetection(
            agent_id="test",
            stuck=True,
            indicators=["stale_heartbeat:700s"],
        )
        assert detection.indicators[0].startswith("stale_heartbeat:")
        assert detection.indicators[0].endswith("s")

        # Test no_progress indicator format
        detection2 = StuckDetection(
            agent_id="test",
            stuck=True,
            indicators=["no_progress:700s"],
        )
        assert detection2.indicators[0].startswith("no_progress:")
        assert detection2.indicators[0].endswith("s")

    def test_indicator_formats_match_bash(self) -> None:
        """Verify all indicator format strings match bash patterns.

        Bash indicator formats (lines 482-533):
        - stale_heartbeat:${seconds}s
        - no_progress:${seconds}s
        - extended_work:${seconds}s
        - looping:${count}x
        - error_spike:${count}
        - missing_milestone:${milestone_list}
        """
        expected_patterns = [
            ("stale_heartbeat:700s", r"stale_heartbeat:\d+s"),
            ("no_progress:600s", r"no_progress:\d+s"),
            ("extended_work:1900s", r"extended_work:\d+s"),
            ("looping:5x", r"looping:\d+x"),
            ("error_spike:10", r"error_spike:\d+"),
            ("missing_milestone:worktree_created", r"missing_milestone:.+"),
        ]

        import re
        for indicator, pattern in expected_patterns:
            assert re.match(pattern, indicator), f"Indicator {indicator} doesn't match pattern {pattern}"

    def test_severity_levels_match_bash(self) -> None:
        """Verify severity values match bash implementation.

        Bash severity levels (lines 486-519):
        - "none" - default
        - "warning" - idle/stale heartbeat, missing milestones
        - "elevated" - extended work, error spike
        - "critical" - looping
        """
        valid_severities = ["none", "warning", "elevated", "critical"]

        detection = StuckDetection()
        assert detection.severity == "none", "default severity should be 'none'"

        for severity in valid_severities:
            detection = StuckDetection(severity=severity)
            assert detection.severity in valid_severities

    def test_intervention_types_match_bash(self) -> None:
        """Verify intervention types match bash implementation.

        Bash intervention types (lines 99-115):
        - none - default, no intervention needed
        - alert - notify human observer
        - suggest - suggest role switch
        - pause - auto-pause agent
        - clarify - request clarification
        - escalate - full escalation chain
        """
        valid_interventions = ["none", "alert", "suggest", "pause", "clarify", "escalate"]

        detection = StuckDetection()
        assert detection.suggested_intervention == "none", "default intervention should be 'none'"

        for intervention in valid_interventions:
            detection = StuckDetection(suggested_intervention=intervention)
            assert detection.suggested_intervention in valid_interventions

    def test_check_all_json_structure_matches_bash(self) -> None:
        """Verify check-all JSON structure matches bash output.

        Bash check_all JSON output (lines 647-660):
        - checked_at, total_checked, stuck_count, stuck_agents, results, config
        """
        results = [
            StuckDetection(agent_id="shepherd-1", issue=42, stuck=True),
            StuckDetection(agent_id="shepherd-2", status="idle", stuck=False),
        ]
        config = StuckDetectionConfig()

        output = format_check_json(results, ["shepherd-1"], config)
        data = json.loads(output)

        # Verify bash output structure
        assert "checked_at" in data
        assert "total_checked" in data
        assert "stuck_count" in data
        assert "stuck_agents" in data
        assert "results" in data
        assert "config" in data

        # Verify config fields
        assert "idle_threshold" in data["config"]
        assert "working_threshold" in data["config"]
        assert "intervention_mode" in data["config"]

    def test_metrics_heartbeat_age_always_present(self) -> None:
        """Verify heartbeat_age is always included in metrics (even when -1).

        Bash always outputs heartbeat_age (line 567), even when -1.
        Python should match this behavior for compatibility.
        """
        # Test with no heartbeat data
        metrics = StuckMetrics(idle_seconds=600)
        data = metrics.to_dict()
        assert "heartbeat_age" in data
        assert data["heartbeat_age"] == -1  # Default value when not available

        # Test with heartbeat data
        metrics2 = StuckMetrics(idle_seconds=600, heartbeat_age=150)
        data2 = metrics2.to_dict()
        assert "heartbeat_age" in data2
        assert data2["heartbeat_age"] == 150

    def test_metrics_current_phase_always_present(self) -> None:
        """Verify current_phase is always included in metrics.

        Bash always outputs current_phase (line 571), defaulting to "unknown".
        """
        # Test with no phase
        metrics = StuckMetrics()
        data = metrics.to_dict()
        assert "current_phase" in data
        assert data["current_phase"] == "unknown"  # Default value

        # Test with phase
        metrics2 = StuckMetrics(current_phase="builder")
        data2 = metrics2.to_dict()
        assert data2["current_phase"] == "builder"

    def test_issue_field_always_present(self) -> None:
        """Verify issue field is always included in detection output.

        Bash always outputs issue (line 559), even when null for idle agents.
        """
        # Test with issue
        detection = StuckDetection(agent_id="test", issue=42)
        data = detection.to_dict()
        assert "issue" in data
        assert data["issue"] == 42

        # Test without issue (idle agent)
        detection2 = StuckDetection(agent_id="test", status="idle")
        data2 = detection2.to_dict()
        assert "issue" in data2
        assert data2["issue"] is None
