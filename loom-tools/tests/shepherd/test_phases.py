"""Tests for phase runners."""

from __future__ import annotations

import json
import logging
import subprocess
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.shepherd.config import ExecutionMode, Phase, ShepherdConfig
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.phases import (
    ApprovalPhase,
    BuilderPhase,
    CuratorPhase,
    JudgePhase,
    MergePhase,
    PhaseStatus,
)
from loom_tools.shepherd.phases.base import _print_heartbeat, _read_heartbeats


@pytest.fixture
def mock_context() -> MagicMock:
    """Create a mock ShepherdContext."""
    ctx = MagicMock(spec=ShepherdContext)
    ctx.config = ShepherdConfig(issue=42)
    ctx.repo_root = Path("/fake/repo")
    ctx.scripts_dir = Path("/fake/repo/.loom/scripts")
    ctx.worktree_path = Path("/fake/repo/.loom/worktrees/issue-42")
    ctx.pr_number = None
    ctx.label_cache = MagicMock()
    return ctx


class TestCuratorPhase:
    """Test CuratorPhase."""

    def test_should_skip_when_from_builder(self, mock_context: MagicMock) -> None:
        """Curator should be skipped when --from builder."""
        mock_context.config = ShepherdConfig(issue=42, start_from=Phase.BUILDER)
        curator = CuratorPhase()
        skip, reason = curator.should_skip(mock_context)
        assert skip is True
        assert "skipped via --from" in reason

    def test_should_skip_when_already_curated(self, mock_context: MagicMock) -> None:
        """Curator should be skipped when issue already has loom:curated."""
        mock_context.has_issue_label.return_value = True
        curator = CuratorPhase()
        skip, reason = curator.should_skip(mock_context)
        assert skip is True
        assert "already curated" in reason

    def test_should_not_skip_when_not_curated(self, mock_context: MagicMock) -> None:
        """Curator should not be skipped when issue doesn't have loom:curated."""
        mock_context.has_issue_label.return_value = False
        curator = CuratorPhase()
        skip, reason = curator.should_skip(mock_context)
        assert skip is False


class TestApprovalPhase:
    """Test ApprovalPhase."""

    def test_never_skips(self, mock_context: MagicMock) -> None:
        """Approval phase should never skip."""
        approval = ApprovalPhase()
        skip, reason = approval.should_skip(mock_context)
        assert skip is False

    def test_returns_success_when_already_approved(self, mock_context: MagicMock) -> None:
        """Should return success when issue already has loom:issue."""
        mock_context.check_shutdown.return_value = False
        mock_context.has_issue_label.return_value = True

        approval = ApprovalPhase()
        result = approval.run(mock_context)

        assert result.status == PhaseStatus.SUCCESS
        assert "already approved" in result.message

    def test_auto_approves_in_force_mode(self, mock_context: MagicMock) -> None:
        """Should auto-approve in force mode."""
        mock_context.config = ShepherdConfig(issue=42, mode=ExecutionMode.FORCE_MERGE)
        mock_context.check_shutdown.return_value = False
        mock_context.has_issue_label.return_value = False

        approval = ApprovalPhase()

        with patch("loom_tools.shepherd.phases.approval.add_issue_label"):
            result = approval.run(mock_context)

        assert result.status == PhaseStatus.SUCCESS
        assert "auto-approved" in result.message

    def test_returns_shutdown_on_signal(self, mock_context: MagicMock) -> None:
        """Should return shutdown status when shutdown signal detected."""
        mock_context.check_shutdown.return_value = True

        approval = ApprovalPhase()
        result = approval.run(mock_context)

        assert result.status == PhaseStatus.SHUTDOWN


class TestBuilderPhase:
    """Test BuilderPhase."""

    def test_should_skip_when_pr_exists(self, mock_context: MagicMock) -> None:
        """Builder should be skipped when PR already exists."""
        builder = BuilderPhase()

        with patch("loom_tools.shepherd.phases.builder.get_pr_for_issue", return_value=100):
            skip, reason = builder.should_skip(mock_context)

        assert skip is True
        assert "PR #100" in reason
        assert mock_context.pr_number == 100

    def test_should_not_skip_when_no_pr(self, mock_context: MagicMock) -> None:
        """Builder should not be skipped when no PR exists."""
        builder = BuilderPhase()

        with patch("loom_tools.shepherd.phases.builder.get_pr_for_issue", return_value=None):
            skip, reason = builder.should_skip(mock_context)

        assert skip is False


class TestJudgePhase:
    """Test JudgePhase."""

    def test_should_skip_when_from_merge_and_approved(self, mock_context: MagicMock) -> None:
        """Judge should be skipped when --from merge and PR is approved."""
        mock_context.config = ShepherdConfig(issue=42, start_from=Phase.MERGE)
        mock_context.pr_number = 100
        mock_context.has_pr_label.return_value = True  # loom:pr

        judge = JudgePhase()
        skip, reason = judge.should_skip(mock_context)

        assert skip is True
        assert "skipped via --from" in reason

    def test_should_not_skip_when_from_merge_but_not_approved(
        self, mock_context: MagicMock
    ) -> None:
        """Judge should not be skipped when --from merge but PR not approved."""
        mock_context.config = ShepherdConfig(issue=42, start_from=Phase.MERGE)
        mock_context.pr_number = 100
        mock_context.has_pr_label.return_value = False  # no loom:pr

        judge = JudgePhase()
        skip, reason = judge.should_skip(mock_context)

        assert skip is False


class TestMergePhase:
    """Test MergePhase."""

    def test_never_skips(self, mock_context: MagicMock) -> None:
        """Merge phase should never skip via --from."""
        merge = MergePhase()
        skip, reason = merge.should_skip(mock_context)
        assert skip is False

    def test_returns_success_with_awaiting_merge_in_default_mode(
        self, mock_context: MagicMock
    ) -> None:
        """Should return success with awaiting_merge in default mode."""
        mock_context.check_shutdown.return_value = False
        mock_context.pr_number = 100

        merge = MergePhase()
        result = merge.run(mock_context)

        assert result.status == PhaseStatus.SUCCESS
        assert result.data.get("awaiting_merge") is True

    def test_auto_merges_in_force_mode(self, mock_context: MagicMock) -> None:
        """Should auto-merge in force mode."""
        mock_context.config = ShepherdConfig(issue=42, mode=ExecutionMode.FORCE_MERGE)
        mock_context.check_shutdown.return_value = False
        mock_context.pr_number = 100
        mock_context.run_script.return_value = MagicMock(returncode=0)

        merge = MergePhase()
        result = merge.run(mock_context)

        assert result.status == PhaseStatus.SUCCESS
        assert result.data.get("merged") is True
        mock_context.run_script.assert_called()

    def test_returns_failure_when_no_pr(self, mock_context: MagicMock) -> None:
        """Should return failure when no PR number."""
        mock_context.pr_number = None

        merge = MergePhase()
        result = merge.run(mock_context)

        assert result.status == PhaseStatus.FAILED


class TestPhaseStatus:
    """Test PhaseStatus enum and PhaseResult."""

    def test_success_is_success(self) -> None:
        """SUCCESS status should be success."""
        from loom_tools.shepherd.phases.base import PhaseResult

        result = PhaseResult(status=PhaseStatus.SUCCESS)
        assert result.is_success is True
        assert result.is_shutdown is False

    def test_skipped_is_success(self) -> None:
        """SKIPPED status should be success."""
        from loom_tools.shepherd.phases.base import PhaseResult

        result = PhaseResult(status=PhaseStatus.SKIPPED)
        assert result.is_success is True

    def test_failed_is_not_success(self) -> None:
        """FAILED status should not be success."""
        from loom_tools.shepherd.phases.base import PhaseResult

        result = PhaseResult(status=PhaseStatus.FAILED)
        assert result.is_success is False

    def test_shutdown_is_shutdown(self) -> None:
        """SHUTDOWN status should be shutdown."""
        from loom_tools.shepherd.phases.base import PhaseResult

        result = PhaseResult(status=PhaseStatus.SHUTDOWN)
        assert result.is_shutdown is True
        assert result.is_success is False


class TestStaleBranchDetection:
    """Test stale remote branch detection in ShepherdContext."""

    def test_warns_when_stale_branch_exists(
        self, mock_context: MagicMock, caplog: pytest.LogCaptureFixture
    ) -> None:
        """Should log warning when remote branch feature/issue-N exists."""
        ls_remote_output = "abc123\trefs/heads/feature/issue-42\n"
        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=ls_remote_output, stderr=""
        )
        with patch("loom_tools.shepherd.context.subprocess.run", return_value=completed):
            with caplog.at_level(logging.WARNING, logger="loom_tools.shepherd.context"):
                ShepherdContext._check_stale_branch(mock_context, 42)

        assert any("Stale branch feature/issue-42" in r.message for r in caplog.records)

    def test_no_warning_when_no_branch(
        self, mock_context: MagicMock, caplog: pytest.LogCaptureFixture
    ) -> None:
        """Should not warn when no remote branch exists."""
        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout="", stderr=""
        )
        with patch("loom_tools.shepherd.context.subprocess.run", return_value=completed):
            with caplog.at_level(logging.WARNING, logger="loom_tools.shepherd.context"):
                ShepherdContext._check_stale_branch(mock_context, 42)

        assert not any("Stale branch" in r.message for r in caplog.records)

    def test_no_warning_on_git_failure(
        self, mock_context: MagicMock, caplog: pytest.LogCaptureFixture
    ) -> None:
        """Should silently continue when git ls-remote fails."""
        completed = subprocess.CompletedProcess(
            args=[], returncode=128, stdout="", stderr="fatal: error"
        )
        with patch("loom_tools.shepherd.context.subprocess.run", return_value=completed):
            with caplog.at_level(logging.WARNING, logger="loom_tools.shepherd.context"):
                ShepherdContext._check_stale_branch(mock_context, 42)

        assert not any("Stale branch" in r.message for r in caplog.records)

    def test_no_warning_on_os_error(
        self, mock_context: MagicMock, caplog: pytest.LogCaptureFixture
    ) -> None:
        """Should silently continue when git is not available."""
        with patch(
            "loom_tools.shepherd.context.subprocess.run",
            side_effect=OSError("No such file"),
        ):
            with caplog.at_level(logging.WARNING, logger="loom_tools.shepherd.context"):
                ShepherdContext._check_stale_branch(mock_context, 42)

        assert not any("Stale branch" in r.message for r in caplog.records)


class TestReadHeartbeats:
    """Test _read_heartbeats helper."""

    def test_extracts_heartbeat_milestones(self, tmp_path: Path) -> None:
        """Should return only heartbeat milestones from a progress file."""
        progress = {
            "task_id": "abc123",
            "milestones": [
                {"event": "started", "timestamp": "t0", "data": {"issue": 42}},
                {
                    "event": "heartbeat",
                    "timestamp": "t1",
                    "data": {"action": "builder running (1m elapsed)"},
                },
                {"event": "phase_entered", "timestamp": "t2", "data": {"phase": "judge"}},
                {
                    "event": "heartbeat",
                    "timestamp": "t3",
                    "data": {"action": "builder running (2m elapsed)"},
                },
            ],
        }
        f = tmp_path / "shepherd-abc123.json"
        f.write_text(json.dumps(progress))

        result = _read_heartbeats(f)

        assert len(result) == 2
        assert result[0]["data"]["action"] == "builder running (1m elapsed)"
        assert result[1]["data"]["action"] == "builder running (2m elapsed)"

    def test_returns_empty_for_missing_file(self, tmp_path: Path) -> None:
        """Should return empty list when file does not exist."""
        f = tmp_path / "nonexistent.json"
        assert _read_heartbeats(f) == []

    def test_returns_empty_for_invalid_json(self, tmp_path: Path) -> None:
        """Should return empty list when file has invalid JSON."""
        f = tmp_path / "bad.json"
        f.write_text("not json")
        assert _read_heartbeats(f) == []

    def test_returns_empty_for_no_milestones(self, tmp_path: Path) -> None:
        """Should return empty list when there are no milestones."""
        f = tmp_path / "empty.json"
        f.write_text(json.dumps({"task_id": "abc", "milestones": []}))
        assert _read_heartbeats(f) == []

    def test_returns_empty_when_no_heartbeats(self, tmp_path: Path) -> None:
        """Should return empty list when milestones exist but none are heartbeats."""
        progress = {
            "milestones": [
                {"event": "started", "timestamp": "t0", "data": {}},
                {"event": "phase_entered", "timestamp": "t1", "data": {"phase": "builder"}},
            ],
        }
        f = tmp_path / "no-hb.json"
        f.write_text(json.dumps(progress))
        assert _read_heartbeats(f) == []


class TestPrintHeartbeat:
    """Test _print_heartbeat output formatting."""

    def test_prints_to_stderr(self, capsys: pytest.CaptureFixture[str]) -> None:
        """Should print heartbeat with dim ANSI formatting to stderr."""
        _print_heartbeat("builder running (1m elapsed)")
        captured = capsys.readouterr()
        assert captured.out == ""
        assert "builder running (1m elapsed)" in captured.err
        assert "\u27f3" in captured.err  # looping arrow symbol

    def test_includes_timestamp(self, capsys: pytest.CaptureFixture[str]) -> None:
        """Should include HH:MM:SS timestamp in output."""
        _print_heartbeat("test action")
        captured = capsys.readouterr()
        # Timestamp is in [HH:MM:SS] format
        assert "[" in captured.err
        assert "]" in captured.err

    def test_uses_dim_ansi(self, capsys: pytest.CaptureFixture[str]) -> None:
        """Should use dim ANSI escape code, not cyan."""
        _print_heartbeat("test")
        captured = capsys.readouterr()
        assert "\033[2m" in captured.err  # dim
        assert "\033[0m" in captured.err  # reset
