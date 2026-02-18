"""Tests for DoctorPhase, focused on run_test_fix behavior."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.shepherd.config import ShepherdConfig
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.phases import DoctorPhase, PhaseResult, PhaseStatus
from loom_tools.shepherd.phases.doctor import DoctorDiagnostics


@pytest.fixture
def mock_context() -> MagicMock:
    """Create a mock ShepherdContext for doctor tests."""
    ctx = MagicMock(spec=ShepherdContext)
    ctx.config = ShepherdConfig(issue=42)
    ctx.repo_root = Path("/fake/repo")
    ctx.scripts_dir = Path("/fake/repo/.loom/scripts")
    ctx.worktree_path = Path("/fake/repo/.loom/worktrees/issue-42")
    ctx.pr_number = 100
    ctx.label_cache = MagicMock()
    ctx.check_shutdown.return_value = False
    return ctx


@pytest.fixture
def test_failure_data() -> dict:
    """Sample test failure data for run_test_fix."""
    return {
        "test_output_tail": "FAILED test_foo - AssertionError",
        "test_summary": "1 failed, 5 passed",
        "test_command": "pnpm check:ci",
        "changed_files": ["src/main.py"],
    }


class TestRunTestFixTimeout:
    """Test that run_test_fix uses the shorter doctor_test_fix_timeout."""

    @patch("loom_tools.shepherd.phases.doctor.run_phase_with_retry")
    def test_uses_test_fix_timeout(
        self, mock_run: MagicMock, mock_context: MagicMock, test_failure_data: dict
    ) -> None:
        """run_test_fix should use doctor_test_fix_timeout, not doctor_timeout."""
        mock_context.config.doctor_timeout = 3600
        mock_context.config.doctor_test_fix_timeout = 600
        mock_run.return_value = 0

        phase = DoctorPhase()
        with patch.object(phase, "_get_commit_count", return_value=0):
            with patch.object(
                phase,
                "_diagnose_doctor_outcome",
                return_value=DoctorDiagnostics(commits_made=1),
            ):
                with patch.object(
                    phase, "_write_test_failure_context", return_value=None
                ):
                    phase.run_test_fix(mock_context, test_failure_data)

        # Verify the timeout passed to run_phase_with_retry
        _, kwargs = mock_run.call_args
        assert kwargs["timeout"] == 600

    @patch("loom_tools.shepherd.phases.doctor.run_phase_with_retry")
    def test_respects_env_override(
        self, mock_run: MagicMock, mock_context: MagicMock, test_failure_data: dict
    ) -> None:
        """Custom doctor_test_fix_timeout should be respected."""
        mock_context.config.doctor_test_fix_timeout = 120
        mock_run.return_value = 0

        phase = DoctorPhase()
        with patch.object(phase, "_get_commit_count", return_value=0):
            with patch.object(
                phase,
                "_diagnose_doctor_outcome",
                return_value=DoctorDiagnostics(commits_made=0),
            ):
                with patch.object(
                    phase, "_write_test_failure_context", return_value=None
                ):
                    phase.run_test_fix(mock_context, test_failure_data)

        _, kwargs = mock_run.call_args
        assert kwargs["timeout"] == 120


class TestRunTestFixStuckRecovery:
    """Test stuck-but-committed recovery in run_test_fix."""

    @patch("loom_tools.shepherd.phases.doctor.run_phase_with_retry")
    def test_stuck_with_commits_returns_success(
        self, mock_run: MagicMock, mock_context: MagicMock, test_failure_data: dict
    ) -> None:
        """When test-fix is stuck (exit 4) but made commits, treat as success."""
        mock_run.return_value = 4  # stuck exit code

        phase = DoctorPhase()
        with patch.object(phase, "_get_commit_count", return_value=5):
            with patch.object(
                phase,
                "_diagnose_doctor_outcome",
                return_value=DoctorDiagnostics(commits_made=2),
            ):
                with patch.object(
                    phase, "_write_test_failure_context", return_value=None
                ):
                    result = phase.run_test_fix(mock_context, test_failure_data)

        assert result.status == PhaseStatus.SUCCESS
        assert "hung after commit" in result.message
        assert result.data["commits_made"] == 2

    @patch("loom_tools.shepherd.phases.doctor.run_phase_with_retry")
    def test_stuck_with_commits_reports_milestone(
        self, mock_run: MagicMock, mock_context: MagicMock, test_failure_data: dict
    ) -> None:
        """Stuck-but-committed recovery should report a heartbeat milestone."""
        mock_run.return_value = 4

        phase = DoctorPhase()
        with patch.object(phase, "_get_commit_count", return_value=5):
            with patch.object(
                phase,
                "_diagnose_doctor_outcome",
                return_value=DoctorDiagnostics(commits_made=1),
            ):
                with patch.object(
                    phase, "_write_test_failure_context", return_value=None
                ):
                    phase.run_test_fix(mock_context, test_failure_data)

        mock_context.report_milestone.assert_any_call(
            "heartbeat",
            action="doctor-test-fix stuck but committed fix, treating as success",
        )

    @patch("loom_tools.shepherd.phases.doctor.run_phase_with_retry")
    def test_stuck_without_commits_returns_stuck(
        self, mock_run: MagicMock, mock_context: MagicMock, test_failure_data: dict
    ) -> None:
        """When test-fix is stuck (exit 4) with no commits, return STUCK."""
        mock_run.return_value = 4

        phase = DoctorPhase()
        with patch.object(phase, "_get_commit_count", return_value=5):
            with patch.object(
                phase,
                "_diagnose_doctor_outcome",
                return_value=DoctorDiagnostics(commits_made=0),
            ):
                with patch.object(
                    phase, "_write_test_failure_context", return_value=None
                ):
                    result = phase.run_test_fix(mock_context, test_failure_data)

        assert result.status == PhaseStatus.STUCK
        assert "stuck during test-fix" in result.message


class TestFullDoctorRunUnchanged:
    """Verify the full doctor run() still uses doctor_timeout (not test-fix timeout)."""

    @patch("loom_tools.shepherd.phases.doctor.run_phase_with_retry")
    def test_full_doctor_uses_doctor_timeout(
        self, mock_run: MagicMock, mock_context: MagicMock
    ) -> None:
        """Full doctor run() should use doctor_timeout, not doctor_test_fix_timeout."""
        mock_context.config.doctor_timeout = 3600
        mock_context.config.doctor_test_fix_timeout = 600
        mock_run.return_value = 0

        phase = DoctorPhase()
        with patch.object(phase, "_get_commit_count", return_value=0):
            with patch.object(
                phase,
                "_diagnose_doctor_outcome",
                return_value=DoctorDiagnostics(commits_made=0),
            ):
                with patch.object(phase, "validate", return_value=True):
                    with patch.object(
                        phase, "_write_judge_feedback_context", return_value=None
                    ):
                        phase.run(mock_context)

        _, kwargs = mock_run.call_args
        assert kwargs["timeout"] == 3600


class TestJudgeFeedbackContext:
    """Tests for _write_judge_feedback_context and its integration into run()."""

    @patch("loom_tools.shepherd.phases.doctor.run_phase_with_retry")
    def test_run_passes_context_flag_when_context_file_written(
        self, mock_run: MagicMock, mock_context: MagicMock
    ) -> None:
        """When context file is written, run() should pass --context flag to doctor."""
        mock_run.return_value = 0
        context_path = Path("/fake/repo/.loom/worktrees/issue-42/.loom-judge-feedback.json")

        phase = DoctorPhase()
        with patch.object(phase, "_get_commit_count", return_value=0):
            with patch.object(
                phase,
                "_diagnose_doctor_outcome",
                return_value=DoctorDiagnostics(commits_made=0),
            ):
                with patch.object(phase, "validate", return_value=True):
                    with patch.object(
                        phase,
                        "_write_judge_feedback_context",
                        return_value=context_path,
                    ):
                        phase.run(mock_context)

        _, kwargs = mock_run.call_args
        assert kwargs["args"] == f"100 --context {context_path}"

    @patch("loom_tools.shepherd.phases.doctor.run_phase_with_retry")
    def test_run_passes_only_pr_number_when_no_context_file(
        self, mock_run: MagicMock, mock_context: MagicMock
    ) -> None:
        """When context file cannot be written, run() should pass just the PR number."""
        mock_run.return_value = 0

        phase = DoctorPhase()
        with patch.object(phase, "_get_commit_count", return_value=0):
            with patch.object(
                phase,
                "_diagnose_doctor_outcome",
                return_value=DoctorDiagnostics(commits_made=0),
            ):
                with patch.object(phase, "validate", return_value=True):
                    with patch.object(
                        phase, "_write_judge_feedback_context", return_value=None
                    ):
                        phase.run(mock_context)

        _, kwargs = mock_run.call_args
        assert kwargs["args"] == "100"

    @patch("loom_tools.shepherd.phases.doctor.subprocess.run")
    def test_write_judge_feedback_context_success(
        self, mock_subprocess: MagicMock, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """_write_judge_feedback_context writes a JSON context file on success."""
        import json

        mock_context.worktree_path = tmp_path
        mock_subprocess.return_value = MagicMock(
            returncode=0,
            stdout='[{"body": "Fix the exit code at line 42", "author": "judge-bot", "created_at": "2026-01-01T00:00:00Z"}]',
        )

        phase = DoctorPhase()
        result = phase._write_judge_feedback_context(mock_context)

        assert result is not None
        assert result == tmp_path / ".loom-judge-feedback.json"
        assert result.exists()

        data = json.loads(result.read_text())
        assert data["pr_number"] == 100
        assert data["issue"] == 42
        assert data["context_type"] == "judge_feedback"
        assert len(data["judge_comments"]) == 1
        assert data["judge_comments"][0]["body"] == "Fix the exit code at line 42"

    @patch("loom_tools.shepherd.phases.doctor.subprocess.run")
    def test_write_judge_feedback_context_returns_none_on_gh_failure(
        self, mock_subprocess: MagicMock, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """_write_judge_feedback_context returns None when gh command fails."""
        mock_context.worktree_path = tmp_path
        mock_subprocess.return_value = MagicMock(returncode=1, stdout="")

        phase = DoctorPhase()
        result = phase._write_judge_feedback_context(mock_context)

        assert result is None

    @patch("loom_tools.shepherd.phases.doctor.subprocess.run")
    def test_write_judge_feedback_context_returns_none_on_empty_comments(
        self, mock_subprocess: MagicMock, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """_write_judge_feedback_context returns None when PR has no comments."""
        mock_context.worktree_path = tmp_path
        mock_subprocess.return_value = MagicMock(returncode=0, stdout="[]")

        phase = DoctorPhase()
        result = phase._write_judge_feedback_context(mock_context)

        assert result is None

    def test_write_judge_feedback_context_returns_none_when_no_worktree(
        self, mock_context: MagicMock
    ) -> None:
        """_write_judge_feedback_context returns None when worktree_path is not set."""
        mock_context.worktree_path = None

        phase = DoctorPhase()
        result = phase._write_judge_feedback_context(mock_context)

        assert result is None
