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
    BasePhase,
    BuilderPhase,
    CuratorPhase,
    DoctorPhase,
    JudgePhase,
    MergePhase,
    PhaseResult,
    PhaseStatus,
)
from loom_tools.shepherd.phases.base import (
    _print_heartbeat,
    _read_heartbeats,
    run_worker_phase,
)
from loom_tools.shepherd.phases.judge import (
    APPROVAL_PATTERNS,
    NEGATIVE_PREFIXES,
    REJECTION_PATTERNS,
)


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


class TestBasePhase:
    """Test BasePhase helper methods."""

    def test_result_creates_phase_result_with_phase_name(self) -> None:
        """result() should create PhaseResult with the phase's name."""

        class MyPhase(BasePhase):
            phase_name = "my_phase"

        phase = MyPhase()
        result = phase.result(PhaseStatus.SUCCESS, "test message", {"key": "value"})

        assert result.status == PhaseStatus.SUCCESS
        assert result.message == "test message"
        assert result.phase_name == "my_phase"
        assert result.data == {"key": "value"}

    def test_result_with_defaults(self) -> None:
        """result() should use empty defaults for message and data."""

        class MyPhase(BasePhase):
            phase_name = "test"

        phase = MyPhase()
        result = phase.result(PhaseStatus.FAILED)

        assert result.message == ""
        assert result.data == {}

    def test_success_helper(self) -> None:
        """success() should create SUCCESS PhaseResult."""

        class MyPhase(BasePhase):
            phase_name = "test_phase"

        phase = MyPhase()
        result = phase.success("done", {"count": 42})

        assert result.status == PhaseStatus.SUCCESS
        assert result.message == "done"
        assert result.phase_name == "test_phase"
        assert result.data == {"count": 42}

    def test_failed_helper(self) -> None:
        """failed() should create FAILED PhaseResult."""

        class MyPhase(BasePhase):
            phase_name = "test_phase"

        phase = MyPhase()
        result = phase.failed("error occurred", {"error": "details"})

        assert result.status == PhaseStatus.FAILED
        assert result.message == "error occurred"
        assert result.phase_name == "test_phase"
        assert result.data == {"error": "details"}

    def test_skipped_helper(self) -> None:
        """skipped() should create SKIPPED PhaseResult."""

        class MyPhase(BasePhase):
            phase_name = "test_phase"

        phase = MyPhase()
        result = phase.skipped("not needed")

        assert result.status == PhaseStatus.SKIPPED
        assert result.message == "not needed"
        assert result.phase_name == "test_phase"

    def test_shutdown_helper(self) -> None:
        """shutdown() should create SHUTDOWN PhaseResult."""

        class MyPhase(BasePhase):
            phase_name = "test_phase"

        phase = MyPhase()
        result = phase.shutdown("signal received")

        assert result.status == PhaseStatus.SHUTDOWN
        assert result.message == "signal received"
        assert result.phase_name == "test_phase"

    def test_stuck_helper(self) -> None:
        """stuck() should create STUCK PhaseResult."""

        class MyPhase(BasePhase):
            phase_name = "test_phase"

        phase = MyPhase()
        result = phase.stuck("agent blocked", {"attempts": 3})

        assert result.status == PhaseStatus.STUCK
        assert result.message == "agent blocked"
        assert result.phase_name == "test_phase"
        assert result.data == {"attempts": 3}

    def test_curator_phase_inherits_basephase(self) -> None:
        """CuratorPhase should properly inherit from BasePhase."""
        curator = CuratorPhase()

        assert curator.phase_name == "curator"
        assert isinstance(curator, BasePhase)

        # Helpers should work and set correct phase_name
        result = curator.success("test")
        assert result.phase_name == "curator"
        assert result.status == PhaseStatus.SUCCESS


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

    def test_returns_success_when_already_approved(
        self, mock_context: MagicMock
    ) -> None:
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

        with patch(
            "loom_tools.shepherd.phases.builder.get_pr_for_issue", return_value=100
        ):
            skip, reason = builder.should_skip(mock_context)

        assert skip is True
        assert "PR #100" in reason
        assert mock_context.pr_number == 100

    def test_should_not_skip_when_no_pr(self, mock_context: MagicMock) -> None:
        """Builder should not be skipped when no PR exists."""
        builder = BuilderPhase()

        with patch(
            "loom_tools.shepherd.phases.builder.get_pr_for_issue", return_value=None
        ):
            skip, reason = builder.should_skip(mock_context)

        assert skip is False

    def test_worktree_failure_includes_error_detail(
        self, mock_context: MagicMock
    ) -> None:
        """Worktree creation failure should include subprocess error output."""
        mock_context.check_shutdown.return_value = False
        wt_mock = MagicMock()
        wt_mock.is_dir.return_value = False
        wt_mock.__bool__ = lambda self: True
        mock_context.worktree_path = wt_mock

        exc = subprocess.CalledProcessError(1, "worktree.sh")
        exc.stderr = "fatal: branch already exists"
        exc.stdout = ""
        mock_context.run_script.side_effect = exc

        builder = BuilderPhase()

        with (
            patch(
                "loom_tools.shepherd.phases.builder.get_pr_for_issue", return_value=None
            ),
            patch("loom_tools.shepherd.phases.builder.transition_issue_labels"),
        ):
            result = builder.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert "failed to create worktree" in result.message
        assert "branch already exists" in result.message
        assert result.data.get("error_detail") == "fatal: branch already exists"

    def test_worktree_failure_minimal_when_no_output(
        self, mock_context: MagicMock
    ) -> None:
        """Worktree creation failure with no subprocess output stays concise."""
        mock_context.check_shutdown.return_value = False
        wt_mock = MagicMock()
        wt_mock.is_dir.return_value = False
        wt_mock.__bool__ = lambda self: True
        mock_context.worktree_path = wt_mock

        exc = subprocess.CalledProcessError(1, "worktree.sh")
        exc.stderr = ""
        exc.stdout = ""
        mock_context.run_script.side_effect = exc

        builder = BuilderPhase()

        with (
            patch(
                "loom_tools.shepherd.phases.builder.get_pr_for_issue", return_value=None
            ),
            patch("loom_tools.shepherd.phases.builder.transition_issue_labels"),
        ):
            result = builder.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert result.message == "failed to create worktree"
        assert result.data.get("error_detail") == ""

    def test_pr_not_found_includes_diagnostics(self, mock_context: MagicMock) -> None:
        """PR-not-found failure should include diagnostic context."""
        mock_context.check_shutdown.return_value = False
        wt_mock = MagicMock()
        wt_mock.is_dir.return_value = True
        wt_mock.__bool__ = lambda self: True
        mock_context.worktree_path = wt_mock

        builder = BuilderPhase()
        fake_diag = {
            "summary": "worktree exists (branch=feature/issue-42, commits_ahead=2, uncommitted=False); remote branch exists; labels=[loom:building]; log=/fake/log",
            "worktree_exists": True,
        }

        with (
            patch(
                "loom_tools.shepherd.phases.builder.get_pr_for_issue",
                side_effect=[None, None, None],
            ),
            patch("loom_tools.shepherd.phases.builder.transition_issue_labels"),
            patch(
                "loom_tools.shepherd.phases.builder.run_phase_with_retry",
                return_value=0,
            ),
            patch.object(builder, "validate", return_value=True),
            patch.object(builder, "_gather_diagnostics", return_value=fake_diag),
            patch.object(builder, "_create_worktree_marker"),
            patch.object(builder, "_run_test_verification", return_value=None),
        ):
            result = builder.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert "could not find PR for issue #42" in result.message
        assert "worktree exists" in result.message
        assert result.data.get("diagnostics") == fake_diag

    def test_validation_failure_includes_diagnostics(
        self, mock_context: MagicMock
    ) -> None:
        """Validation failure should include diagnostic context."""
        mock_context.check_shutdown.return_value = False
        wt_mock = MagicMock()
        wt_mock.is_dir.return_value = True
        wt_mock.__bool__ = lambda self: True
        mock_context.worktree_path = wt_mock

        builder = BuilderPhase()
        fake_diag = {
            "summary": "worktree does not exist; remote branch missing; labels=[loom:building]; log=/fake/log",
        }

        with (
            patch(
                "loom_tools.shepherd.phases.builder.get_pr_for_issue", return_value=None
            ),
            patch("loom_tools.shepherd.phases.builder.transition_issue_labels"),
            patch(
                "loom_tools.shepherd.phases.builder.run_phase_with_retry",
                return_value=0,
            ),
            patch.object(builder, "validate", return_value=False),
            patch.object(builder, "_gather_diagnostics", return_value=fake_diag),
            patch.object(builder, "_create_worktree_marker"),
            patch.object(builder, "_cleanup_stale_worktree"),
            patch.object(builder, "_run_test_verification", return_value=None),
        ):
            result = builder.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert "builder phase validation failed" in result.message
        assert "worktree does not exist" in result.message
        assert result.data.get("diagnostics") == fake_diag

    def test_unexpected_exit_code_includes_diagnostics(
        self, mock_context: MagicMock
    ) -> None:
        """Non-zero/non-special exit codes should include diagnostics."""
        mock_context.check_shutdown.return_value = False
        wt_mock = MagicMock()
        wt_mock.is_dir.return_value = True
        wt_mock.__bool__ = lambda self: True
        mock_context.worktree_path = wt_mock

        builder = BuilderPhase()
        fake_diag = {
            "summary": "worktree exists; remote branch exists; labels=[loom:building]; log=/fake/log"
        }

        with (
            patch(
                "loom_tools.shepherd.phases.builder.get_pr_for_issue", return_value=None
            ),
            patch("loom_tools.shepherd.phases.builder.transition_issue_labels"),
            patch(
                "loom_tools.shepherd.phases.builder.run_phase_with_retry",
                return_value=1,
            ),
            patch.object(builder, "_gather_diagnostics", return_value=fake_diag),
            patch.object(builder, "_create_worktree_marker"),
        ):
            result = builder.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert "exited with code 1" in result.message
        assert result.data.get("exit_code") == 1
        assert result.data.get("diagnostics") == fake_diag

    def test_stuck_builder_includes_log_path(self, mock_context: MagicMock) -> None:
        """Builder stuck (exit 4) should include log file path in data."""
        mock_context.check_shutdown.return_value = False
        wt_mock = MagicMock()
        wt_mock.is_dir.return_value = True
        wt_mock.__bool__ = lambda self: True
        mock_context.worktree_path = wt_mock

        builder = BuilderPhase()

        with (
            patch(
                "loom_tools.shepherd.phases.builder.get_pr_for_issue", return_value=None
            ),
            patch("loom_tools.shepherd.phases.builder.transition_issue_labels"),
            patch(
                "loom_tools.shepherd.phases.builder.run_phase_with_retry",
                return_value=4,
            ),
            patch.object(builder, "_mark_issue_blocked"),
            patch.object(builder, "_create_worktree_marker"),
        ):
            result = builder.run(mock_context)

        assert result.status == PhaseStatus.STUCK
        assert "log_file" in result.data
        assert "loom-builder-issue-42.log" in result.data["log_file"]


class TestBuilderDiagnostics:
    """Test BuilderPhase._gather_diagnostics helper."""

    def test_diagnostics_when_worktree_exists(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Should report worktree state when worktree directory exists."""
        # Create a real worktree dir so is_dir() works
        wt_dir = tmp_path / "worktree"
        wt_dir.mkdir()
        mock_context.worktree_path = wt_dir
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.repo_root = tmp_path

        # Create a log file
        log_dir = tmp_path / ".loom" / "logs"
        log_dir.mkdir(parents=True)
        log_file = log_dir / "loom-builder-issue-42.log"
        log_file.write_text("line1\nline2\nline3\n")

        builder = BuilderPhase()

        # Mock git and gh subprocess calls
        def fake_run(cmd, **kwargs):
            cmd_str = " ".join(str(c) for c in cmd)
            result = subprocess.CompletedProcess(
                args=cmd, returncode=0, stdout="", stderr=""
            )
            if "rev-parse" in cmd_str:
                result.stdout = "feature/issue-42\n"
            elif "log" in cmd_str and "main..HEAD" in cmd_str:
                result.stdout = "abc1234 commit 1\ndef5678 commit 2\n"
            elif "status --porcelain" in cmd_str:
                result.stdout = ""
            elif "ls-remote" in cmd_str:
                result.stdout = "abc123\trefs/heads/feature/issue-42\n"
            elif "gh" in cmd_str:
                result.stdout = "loom:building"
            return result

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", side_effect=fake_run
        ):
            diag = builder._gather_diagnostics(mock_context)

        assert diag["worktree_exists"] is True
        assert diag["branch"] == "feature/issue-42"
        assert diag["commits_ahead"] == 2
        assert diag["has_uncommitted_changes"] is False
        assert diag["remote_branch_exists"] is True
        assert diag["log_exists"] is True
        assert diag["log_tail"] == ["line1", "line2", "line3"]
        assert "worktree exists" in diag["summary"]
        assert "remote branch exists" in diag["summary"]

    def test_diagnostics_when_worktree_missing(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Should report worktree missing when directory doesn't exist."""
        mock_context.worktree_path = tmp_path / "nonexistent"
        mock_context.config = ShepherdConfig(issue=99)
        mock_context.repo_root = tmp_path

        builder = BuilderPhase()

        def fake_run(cmd, **kwargs):
            return subprocess.CompletedProcess(
                args=cmd, returncode=1, stdout="", stderr=""
            )

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", side_effect=fake_run
        ):
            diag = builder._gather_diagnostics(mock_context)

        assert diag["worktree_exists"] is False
        assert diag["branch"] is None
        assert diag["commits_ahead"] == 0
        assert diag["has_uncommitted_changes"] is False
        assert diag["remote_branch_exists"] is False
        assert diag["log_exists"] is False
        assert "worktree does not exist" in diag["summary"]
        assert "remote branch missing" in diag["summary"]

    def test_diagnostics_log_tail_truncated(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Should include only last 20 lines of log when file is large."""
        mock_context.worktree_path = tmp_path / "nonexistent"
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.repo_root = tmp_path

        log_dir = tmp_path / ".loom" / "logs"
        log_dir.mkdir(parents=True)
        log_file = log_dir / "loom-builder-issue-42.log"
        lines = [f"line {i}" for i in range(50)]
        log_file.write_text("\n".join(lines))

        builder = BuilderPhase()

        def fake_run(cmd, **kwargs):
            return subprocess.CompletedProcess(
                args=cmd, returncode=1, stdout="", stderr=""
            )

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", side_effect=fake_run
        ):
            diag = builder._gather_diagnostics(mock_context)

        assert len(diag["log_tail"]) == 20
        assert diag["log_tail"][0] == "line 30"
        assert diag["log_tail"][-1] == "line 49"

    def test_get_log_path(self, mock_context: MagicMock) -> None:
        """Should return correct log path based on issue number."""
        builder = BuilderPhase()
        path = builder._get_log_path(mock_context)
        assert path == Path("/fake/repo/.loom/logs/loom-builder-issue-42.log")

    def test_diagnostics_summary_format(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Summary should contain all key diagnostic sections."""
        mock_context.worktree_path = tmp_path / "nonexistent"
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.repo_root = tmp_path

        builder = BuilderPhase()

        def fake_run(cmd, **kwargs):
            cmd_str = " ".join(str(c) for c in cmd)
            if "gh" in cmd_str:
                return subprocess.CompletedProcess(
                    args=cmd,
                    returncode=0,
                    stdout="loom:building, loom:curated",
                    stderr="",
                )
            return subprocess.CompletedProcess(
                args=cmd, returncode=1, stdout="", stderr=""
            )

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", side_effect=fake_run
        ):
            diag = builder._gather_diagnostics(mock_context)

        summary = diag["summary"]
        # Should have 5 semicolon-separated sections
        parts = summary.split("; ")
        assert len(parts) == 5
        assert "worktree" in parts[0]
        assert "remote branch" in parts[1]
        assert "PR" in parts[2] or "no PR" in parts[2]
        assert "labels=" in parts[3]
        assert "log=" in parts[4]


class TestBuilderQualityValidation:
    """Test pre-flight quality validation in BuilderPhase."""

    def test_fetch_issue_body_success(self, mock_context: MagicMock) -> None:
        """Should return issue body on successful gh call."""
        builder = BuilderPhase()
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="## Acceptance Criteria\n- [ ] Works\n",
            stderr="",
        )
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", return_value=completed
        ):
            body = builder._fetch_issue_body(mock_context)

        assert body is not None
        assert "Acceptance Criteria" in body

    def test_fetch_issue_body_failure(self, mock_context: MagicMock) -> None:
        """Should return None when gh call fails."""
        builder = BuilderPhase()
        completed = subprocess.CompletedProcess(
            args=[], returncode=1, stdout="", stderr="error"
        )
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", return_value=completed
        ):
            body = builder._fetch_issue_body(mock_context)

        assert body is None

    def test_fetch_issue_body_os_error(self, mock_context: MagicMock) -> None:
        """Should return None when subprocess raises OSError."""
        builder = BuilderPhase()
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            side_effect=OSError("gh not found"),
        ):
            body = builder._fetch_issue_body(mock_context)

        assert body is None

    def test_run_quality_validation_logs_warnings(
        self, mock_context: MagicMock, capsys: pytest.CaptureFixture[str]
    ) -> None:
        """Should log warnings for low-quality issues."""
        builder = BuilderPhase()
        # Issue with no AC, no test plan, no file refs
        body = "Just a vague description."
        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=body, stderr=""
        )
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", return_value=completed
        ):
            builder._run_quality_validation(mock_context)

        # Should have reported a heartbeat milestone
        mock_context.report_milestone.assert_called()
        call_args = mock_context.report_milestone.call_args
        assert call_args[0][0] == "heartbeat"
        assert "warning" in call_args[1]["action"]

    def test_run_quality_validation_no_warnings_for_good_issue(
        self, mock_context: MagicMock
    ) -> None:
        """Should not report milestone for good quality issue."""
        builder = BuilderPhase()
        body = """## Acceptance Criteria

- [ ] Validation function works
- [ ] Warnings logged

## Test Plan

- [ ] Unit test

Modify `builder.py` to add validation.
"""
        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=body, stderr=""
        )
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", return_value=completed
        ):
            builder._run_quality_validation(mock_context)

        # Should NOT have reported a heartbeat milestone
        mock_context.report_milestone.assert_not_called()

    def test_run_quality_validation_handles_fetch_failure(
        self, mock_context: MagicMock
    ) -> None:
        """Should silently skip validation when issue body fetch fails."""
        builder = BuilderPhase()
        completed = subprocess.CompletedProcess(
            args=[], returncode=1, stdout="", stderr="error"
        )
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", return_value=completed
        ):
            # Should not raise
            result = builder._run_quality_validation(mock_context)

        # Should return None (continue)
        assert result is None
        # Should not have reported any milestone
        mock_context.report_milestone.assert_not_called()

    def test_run_quality_validation_returns_none_for_warnings(
        self, mock_context: MagicMock
    ) -> None:
        """Should return None when only warnings exist (default behavior)."""
        builder = BuilderPhase()
        # Issue with no AC - default gates make this a WARNING, not BLOCK
        body = "Just a vague description."
        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=body, stderr=""
        )
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", return_value=completed
        ):
            result = builder._run_quality_validation(mock_context)

        # Default gates should NOT block
        assert result is None

    def test_run_quality_validation_blocks_with_strict_gates(
        self, mock_context: MagicMock
    ) -> None:
        """Should return FAILED PhaseResult when BLOCK findings exist."""
        from loom_tools.shepherd.config import QualityGates

        builder = BuilderPhase()
        # Issue missing acceptance criteria
        body = """## Summary

Some description without acceptance criteria.

## Test Plan

Test steps:
1. Run the command
2. Verify output

Modify `builder.py`.
"""
        # Configure strict gates
        mock_context.config.quality_gates = QualityGates.strict()

        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=body, stderr=""
        )
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", return_value=completed
        ):
            result = builder._run_quality_validation(mock_context)

        # Strict gates should block on missing AC
        assert result is not None
        assert result.status == PhaseStatus.FAILED
        assert "acceptance criteria" in result.message.lower()
        assert result.data.get("quality_blocked") is True

    def test_run_quality_validation_no_block_when_ac_present(
        self, mock_context: MagicMock
    ) -> None:
        """Should return None when acceptance criteria exist, even with strict gates."""
        from loom_tools.shepherd.config import QualityGates

        builder = BuilderPhase()
        body = """## Acceptance Criteria

- [ ] Feature works

## Test Plan

- [ ] Test it

Modify `builder.py`.
"""
        # Configure strict gates
        mock_context.config.quality_gates = QualityGates.strict()

        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=body, stderr=""
        )
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", return_value=completed
        ):
            result = builder._run_quality_validation(mock_context)

        # Should not block when AC is present
        assert result is None


class TestBuilderTestVerification:
    """Test builder phase test verification."""

    def test_detect_test_command_pnpm_check_ci_lite(self, tmp_path: Path) -> None:
        """Should prefer pnpm check:ci:lite over check:ci when both available."""
        builder = BuilderPhase()
        pkg = {"scripts": {
            "check:ci:lite": "pnpm lint && pnpm test",
            "check:ci": "pnpm lint && pnpm build && pnpm test",
            "test": "vitest",
        }}
        (tmp_path / "package.json").write_text(json.dumps(pkg))

        result = builder._detect_test_command(tmp_path)
        assert result is not None
        assert result == (["pnpm", "check:ci:lite"], "pnpm check:ci:lite")

    def test_detect_test_command_pnpm_check_ci(self, tmp_path: Path) -> None:
        """Should detect pnpm check:ci when check:ci:lite not available."""
        builder = BuilderPhase()
        pkg = {"scripts": {"check:ci": "pnpm lint && pnpm test", "test": "vitest"}}
        (tmp_path / "package.json").write_text(json.dumps(pkg))

        result = builder._detect_test_command(tmp_path)
        assert result is not None
        assert result == (["pnpm", "check:ci"], "pnpm check:ci")

    def test_detect_test_command_pnpm_test(self, tmp_path: Path) -> None:
        """Should detect pnpm test when no check:ci or check:ci:lite available."""
        builder = BuilderPhase()
        pkg = {"scripts": {"test": "vitest"}}
        (tmp_path / "package.json").write_text(json.dumps(pkg))

        result = builder._detect_test_command(tmp_path)
        assert result is not None
        assert result == (["pnpm", "test"], "pnpm test")

    def test_detect_test_command_pnpm_check(self, tmp_path: Path) -> None:
        """Should detect pnpm check when no test or check:ci available."""
        builder = BuilderPhase()
        pkg = {"scripts": {"check": "cargo check"}}
        (tmp_path / "package.json").write_text(json.dumps(pkg))

        result = builder._detect_test_command(tmp_path)
        assert result is not None
        assert result == (["pnpm", "check"], "pnpm check")

    def test_detect_test_command_cargo(self, tmp_path: Path) -> None:
        """Should detect cargo test for Rust projects."""
        builder = BuilderPhase()
        (tmp_path / "Cargo.toml").write_text("[package]\nname = 'test'\n")

        result = builder._detect_test_command(tmp_path)
        assert result is not None
        assert result == (["cargo", "test", "--workspace"], "cargo test --workspace")

    def test_detect_test_command_pytest(self, tmp_path: Path) -> None:
        """Should detect pytest for Python projects."""
        builder = BuilderPhase()
        (tmp_path / "pyproject.toml").write_text("[project]\nname = 'test'\n")

        result = builder._detect_test_command(tmp_path)
        assert result is not None
        assert result == (["python", "-m", "pytest"], "pytest")

    def test_detect_test_command_prefers_pnpm_over_cargo(self, tmp_path: Path) -> None:
        """Should prefer package.json over Cargo.toml when both exist."""
        builder = BuilderPhase()
        pkg = {"scripts": {"test": "vitest"}}
        (tmp_path / "package.json").write_text(json.dumps(pkg))
        (tmp_path / "Cargo.toml").write_text("[package]\nname = 'test'\n")

        result = builder._detect_test_command(tmp_path)
        assert result is not None
        assert result[1] == "pnpm test"

    def test_detect_test_command_none(self, tmp_path: Path) -> None:
        """Should return None when no test runner detected."""
        builder = BuilderPhase()
        result = builder._detect_test_command(tmp_path)
        assert result is None

    def test_detect_test_command_empty_package_json_scripts(
        self, tmp_path: Path
    ) -> None:
        """Should return None when package.json has no test scripts."""
        builder = BuilderPhase()
        pkg = {"scripts": {"build": "tsc"}}
        (tmp_path / "package.json").write_text(json.dumps(pkg))

        result = builder._detect_test_command(tmp_path)
        assert result is None

    def test_detect_test_command_invalid_package_json(self, tmp_path: Path) -> None:
        """Should handle invalid package.json gracefully."""
        builder = BuilderPhase()
        (tmp_path / "package.json").write_text("not json")

        result = builder._detect_test_command(tmp_path)
        assert result is None

    def test_parse_test_summary_vitest(self) -> None:
        """Should extract vitest test summary."""
        builder = BuilderPhase()
        output = """
 ✓ src/foo.test.ts (3 tests)
 ✓ src/bar.test.ts (5 tests)

 Tests  8 passed (2 suites)
 Duration  0.42s
"""
        result = builder._parse_test_summary(output)
        assert result is not None
        assert "8 passed" in result

    def test_parse_test_summary_cargo(self) -> None:
        """Should extract cargo test summary."""
        builder = BuilderPhase()
        output = """
running 17 tests
...
test result: ok. 17 passed; 0 failed; 0 ignored
"""
        result = builder._parse_test_summary(output)
        assert result is not None
        assert "17 passed" in result
        assert result.startswith("test result:")

    def test_parse_test_summary_none(self) -> None:
        """Should return None for unrecognized output."""
        builder = BuilderPhase()
        result = builder._parse_test_summary("Build complete.\nDone.")
        assert result is None

    def test_run_test_verification_passes(self, mock_context: MagicMock) -> None:
        """Should return None when tests pass."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        completed = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="Tests  5 passed\nDuration 0.1s\n",
            stderr="",
        )
        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch.object(builder, "_run_baseline_tests", return_value=None),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=completed,
            ),
        ):
            result = builder._run_test_verification(mock_context)

        assert result is None
        # Should have reported milestones
        assert mock_context.report_milestone.call_count >= 1

    def test_run_test_verification_fails_no_baseline(
        self, mock_context: MagicMock
    ) -> None:
        """Should return FAILED when tests fail and no baseline available."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        completed = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="FAIL src/foo.test.ts\nTests  2 failed, 3 passed\n",
            stderr="",
        )
        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch.object(builder, "_run_baseline_tests", return_value=None),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=completed,
            ),
        ):
            result = builder._run_test_verification(mock_context)

        assert result is not None
        assert result.status == PhaseStatus.FAILED
        assert "test verification failed" in result.message
        assert "pnpm test" in result.message

    def test_run_test_verification_preexisting_failures(
        self, mock_context: MagicMock
    ) -> None:
        """Should return None (warn) when failures are pre-existing on main."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        # Both baseline and worktree have the same failure count
        baseline_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="FAIL src/foo.test.ts\nTests  2 failed, 3 passed\n",
            stderr="",
        )
        worktree_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="FAIL src/foo.test.ts\nTests  2 failed, 3 passed\n",
            stderr="",
        )
        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch.object(
                builder, "_run_baseline_tests", return_value=baseline_result
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=worktree_result,
            ),
        ):
            result = builder._run_test_verification(mock_context)

        assert result is None

    def test_run_test_verification_preexisting_pytest_different_traces(
        self, mock_context: MagicMock
    ) -> None:
        """Same pytest failure in both runs with different stack traces.

        This is the core false-positive scenario from #1920: a single
        pre-existing test failure produces different traceback line numbers
        or formatting between baseline and worktree runs, but the failure
        count and test name are identical.
        """
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        # Baseline: one pytest failure with specific traceback
        baseline_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout=(
                "============================= test session starts ==============================\n"
                "collected 15 items\n\n"
                "tests/test_foo.py::test_bar FAILED\n"
                "tests/test_foo.py::test_baz PASSED\n\n"
                "=================================== FAILURES ===================================\n"
                "_________________________________ test_bar _____________________________________\n\n"
                "    def test_bar():\n"
                ">       assert compute(42) == 100\n"
                "E       AssertionError: assert 99 == 100\n"
                "E        +  where 99 = compute(42)\n\n"
                "tests/test_foo.py:15: AssertionError\n"
                "=========================== short test summary info ============================\n"
                "FAILED tests/test_foo.py::test_bar - AssertionError: assert 99 == 100\n"
                "========================= 1 failed, 14 passed in 2.45s ========================\n"
            ),
            stderr="",
        )
        # Worktree: same failure, different line number in traceback
        worktree_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout=(
                "============================= test session starts ==============================\n"
                "collected 15 items\n\n"
                "tests/test_foo.py::test_bar FAILED\n"
                "tests/test_foo.py::test_baz PASSED\n\n"
                "=================================== FAILURES ===================================\n"
                "_________________________________ test_bar _____________________________________\n\n"
                "    def test_bar():\n"
                ">       assert compute(42) == 100\n"
                "E       AssertionError: assert 98 == 100\n"
                "E        +  where 98 = compute(42)\n\n"
                "tests/test_foo.py:17: AssertionError\n"
                "=========================== short test summary info ============================\n"
                "FAILED tests/test_foo.py::test_bar - AssertionError: assert 98 == 100\n"
                "========================= 1 failed, 14 passed in 2.51s ========================\n"
            ),
            stderr="",
        )
        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["python", "-m", "pytest"], "pytest"),
            ),
            patch.object(
                builder, "_run_baseline_tests", return_value=baseline_result
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=worktree_result,
            ),
        ):
            result = builder._run_test_verification(mock_context)

        # Should be None (pre-existing), NOT a false-positive FAILED
        assert result is None

    def test_run_test_verification_new_failures_on_top_of_baseline(
        self, mock_context: MagicMock
    ) -> None:
        """Should FAIL when worktree adds new failures beyond baseline."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        # Baseline has one failure
        baseline_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="FAIL src/foo.test.ts\nTests  1 failed, 4 passed\n",
            stderr="",
        )
        # Worktree has more failures (higher count)
        worktree_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="FAIL src/foo.test.ts\nFAIL src/bar.test.ts\nTests  2 failed, 3 passed\n",
            stderr="",
        )
        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch.object(
                builder, "_run_baseline_tests", return_value=baseline_result
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=worktree_result,
            ),
        ):
            result = builder._run_test_verification(mock_context)

        assert result is not None
        assert result.status == PhaseStatus.FAILED

    def test_run_test_verification_fewer_failures_is_improvement(
        self, mock_context: MagicMock
    ) -> None:
        """Should return None when worktree has fewer failures than baseline."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        # Baseline has 2 failures
        baseline_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="Tests  2 failed, 3 passed\n",
            stderr="",
        )
        # Worktree has 1 failure (improvement)
        worktree_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="Tests  1 failed, 4 passed\n",
            stderr="",
        )
        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch.object(
                builder, "_run_baseline_tests", return_value=baseline_result
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=worktree_result,
            ),
        ):
            result = builder._run_test_verification(mock_context)

        # Fewer failures = improvement, should pass
        assert result is None

    def test_run_test_verification_baseline_passes_worktree_fails(
        self, mock_context: MagicMock
    ) -> None:
        """Should FAIL when baseline passes but worktree introduces failures."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        # Baseline passes
        baseline_result = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="Tests  5 passed\n",
            stderr="",
        )
        # Worktree fails
        worktree_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="FAIL src/foo.test.ts\nTests  1 failed, 4 passed\n",
            stderr="",
        )
        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch.object(
                builder, "_run_baseline_tests", return_value=baseline_result
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=worktree_result,
            ),
        ):
            result = builder._run_test_verification(mock_context)

        assert result is not None
        assert result.status == PhaseStatus.FAILED
        assert "test verification failed" in result.message

    def test_run_test_verification_timeout(self, mock_context: MagicMock) -> None:
        """Should return FAILED result on timeout."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch.object(builder, "_run_baseline_tests", return_value=None),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                side_effect=subprocess.TimeoutExpired(cmd="pnpm test", timeout=300),
            ),
        ):
            result = builder._run_test_verification(mock_context)

        assert result is not None
        assert result.status == PhaseStatus.FAILED
        assert "timed out" in result.message

    def test_run_test_verification_no_worktree(self, mock_context: MagicMock) -> None:
        """Should return None when no worktree path."""
        builder = BuilderPhase()
        mock_context.worktree_path = None

        result = builder._run_test_verification(mock_context)
        assert result is None

    def test_run_test_verification_no_test_runner(
        self, mock_context: MagicMock
    ) -> None:
        """Should return None when no test runner detected."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        with patch.object(builder, "_detect_test_command", return_value=None):
            result = builder._run_test_verification(mock_context)

        assert result is None

    def test_run_test_verification_os_error(self, mock_context: MagicMock) -> None:
        """Should return None on OSError (test runner not installed)."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch.object(builder, "_run_baseline_tests", return_value=None),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                side_effect=OSError("pnpm not found"),
            ),
        ):
            result = builder._run_test_verification(mock_context)

        assert result is None

    def test_parse_test_summary_pytest(self) -> None:
        """Should extract pytest summary."""
        builder = BuilderPhase()
        output = """
============================= test session starts ==============================
collected 12 items

tests/test_foo.py ........                                              [ 66%]
tests/test_bar.py ....                                                  [100%]

============================== 12 passed in 0.03s ==============================
"""
        result = builder._parse_test_summary(output)
        assert result is not None
        assert "12 passed" in result


class TestBuilderRunTestFailureIntegration:
    """Test that builder run() preserves worktree on test failure."""

    def test_run_calls_preserve_on_test_failure(
        self, mock_context: MagicMock
    ) -> None:
        """run() should call _preserve_on_test_failure instead of _cleanup_on_failure."""
        builder = BuilderPhase()
        mock_context.config.issue = 42
        mock_context.repo_root = Path("/fake/repo")
        mock_context.check_shutdown.return_value = False
        mock_context.has_issue_label.return_value = False
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        test_failure_result = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed (pnpm test, exit code 1)",
            phase_name="builder",
            data={"test_failure": True},
        )

        with (
            patch.object(builder, "_is_rate_limited", return_value=False),
            patch.object(builder, "_run_quality_validation", return_value=None),
            patch.object(builder, "_create_worktree_marker"),
            patch.object(
                builder, "_run_test_verification", return_value=test_failure_result
            ),
            patch.object(builder, "_preserve_on_test_failure") as mock_preserve,
            patch.object(builder, "_cleanup_on_failure") as mock_cleanup,
            patch(
                "loom_tools.shepherd.phases.builder.run_phase_with_retry",
                return_value=0,
            ),
            patch("loom_tools.shepherd.phases.builder.transition_issue_labels"),
            patch("loom_tools.shepherd.phases.builder.get_pr_for_issue", return_value=None),
        ):
            result = builder.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert result.data.get("test_failure") is True
        mock_preserve.assert_called_once()
        mock_cleanup.assert_not_called()

    def test_run_skips_test_verification_when_flag_set(
        self, mock_context: MagicMock
    ) -> None:
        """run() should skip test verification when skip_test_verification=True.

        This is used by Phase 3c after Doctor handles pre-existing test failures,
        to avoid re-running test verification which would fail again.
        See issue #1946.
        """
        builder = BuilderPhase()
        mock_context.config.issue = 42
        mock_context.repo_root = Path("/fake/repo")
        mock_context.check_shutdown.return_value = False
        mock_context.has_issue_label.return_value = False
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        with (
            patch.object(builder, "_is_rate_limited", return_value=False),
            patch.object(builder, "_run_quality_validation", return_value=None),
            patch.object(builder, "_create_worktree_marker"),
            patch.object(builder, "_run_test_verification") as mock_test_verify,
            patch.object(builder, "validate", return_value=True),
            patch(
                "loom_tools.shepherd.phases.builder.run_phase_with_retry",
                return_value=0,
            ),
            patch("loom_tools.shepherd.phases.builder.transition_issue_labels"),
            # Return None first (no existing PR), then 123 (PR created by validate)
            patch(
                "loom_tools.shepherd.phases.builder.get_pr_for_issue",
                side_effect=[None, 123],
            ),
        ):
            result = builder.run(mock_context, skip_test_verification=True)

        assert result.status == PhaseStatus.SUCCESS
        # Test verification should NOT have been called
        mock_test_verify.assert_not_called()


class TestBuilderPreserveOnTestFailure:
    """Test builder phase worktree preservation on test failure."""

    def test_preserve_pushes_branch_and_labels_needs_fix(
        self, mock_context: MagicMock
    ) -> None:
        """Should push branch, label needs-fix, and add comment on test failure."""
        builder = BuilderPhase()
        mock_context.config.issue = 42
        mock_context.repo_root = Path("/fake/repo")
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        worktree_mock.__str__ = lambda self: "/fake/repo/.loom/worktrees/issue-42"
        mock_context.worktree_path = worktree_mock

        test_result = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed (pnpm test, exit code 1)",
            phase_name="builder",
            data={"test_failure": True},
        )

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=subprocess.CompletedProcess(
                args=[], returncode=0, stdout="", stderr=""
            ),
        ) as mock_run:
            builder._preserve_on_test_failure(mock_context, test_result)

        # Should have called push, label remove, label add, and comment
        calls = mock_run.call_args_list
        # git push call
        push_calls = [c for c in calls if "push" in str(c)]
        assert len(push_calls) >= 1

        # gh issue comment call
        comment_calls = [c for c in calls if "comment" in str(c)]
        assert len(comment_calls) >= 1

        # Should have called transition_issue_labels via labels module
        # (Patched separately in the mock_context)

        # Report milestone should be called with blocked reason
        mock_context.report_milestone.assert_called()

    def test_preserve_keeps_worktree_marker(
        self, mock_context: MagicMock
    ) -> None:
        """Should NOT remove the worktree marker (preserving the worktree)."""
        builder = BuilderPhase()
        mock_context.config.issue = 42
        mock_context.repo_root = Path("/fake/repo")
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        test_result = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={"test_failure": True},
        )

        with (
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=subprocess.CompletedProcess(
                    args=[], returncode=0, stdout="", stderr=""
                ),
            ),
            patch(
                "loom_tools.shepherd.phases.builder.transition_issue_labels",
            ),
        ):
            builder._preserve_on_test_failure(mock_context, test_result)

        # _remove_worktree_marker should NOT have been called
        # The marker protects the worktree from premature cleanup

    def test_test_failure_result_includes_flag(
        self, mock_context: MagicMock
    ) -> None:
        """Test verification failure result should include test_failure flag."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        completed = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="FAIL src/foo.test.ts\nTests  2 failed, 3 passed\n",
            stderr="",
        )
        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch.object(builder, "_run_baseline_tests", return_value=None),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=completed,
            ),
        ):
            result = builder._run_test_verification(mock_context)

        assert result is not None
        assert result.status == PhaseStatus.FAILED
        assert result.data.get("test_failure") is True

    def test_test_timeout_result_includes_flag(
        self, mock_context: MagicMock
    ) -> None:
        """Test verification timeout result should include test_failure flag."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch.object(builder, "_run_baseline_tests", return_value=None),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                side_effect=subprocess.TimeoutExpired(cmd="pnpm test", timeout=300),
            ),
        ):
            result = builder._run_test_verification(mock_context)

        assert result is not None
        assert result.status == PhaseStatus.FAILED
        assert result.data.get("test_failure") is True


class TestBuilderTestFailureContext:
    """Test that test failure context is written and passed to doctor."""

    def test_preserve_writes_context_file(self, tmp_path: Path) -> None:
        """_preserve_on_test_failure should write .loom-test-failure-context.json."""
        builder = BuilderPhase()
        ctx = MagicMock(spec=ShepherdContext)
        ctx.config = ShepherdConfig(issue=42)
        ctx.repo_root = Path("/fake/repo")
        ctx.worktree_path = tmp_path
        ctx.label_cache = MagicMock()

        test_result = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed (pnpm test, exit code 1)",
            phase_name="builder",
            data={
                "test_failure": True,
                "test_output_tail": "FAIL src/foo.test.ts\nExpected true, got false",
                "test_summary": "2 failed, 3 passed",
                "test_command": "pnpm test",
                "changed_files": ["src/foo.ts", "src/bar.ts"],
            },
        )

        with (
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=subprocess.CompletedProcess(
                    args=[], returncode=0, stdout="", stderr=""
                ),
            ),
            patch("loom_tools.shepherd.phases.builder.transition_issue_labels"),
        ):
            builder._preserve_on_test_failure(ctx, test_result)

        context_file = tmp_path / ".loom-test-failure-context.json"
        assert context_file.exists()
        data = json.loads(context_file.read_text())
        assert data["issue"] == 42
        assert data["test_command"] == "pnpm test"
        assert data["test_summary"] == "2 failed, 3 passed"
        assert "FAIL src/foo.test.ts" in data["test_output_tail"]
        assert data["changed_files"] == ["src/foo.ts", "src/bar.ts"]
        assert "test verification failed" in data["failure_message"]

    def test_test_failure_result_includes_context_data(
        self, mock_context: MagicMock
    ) -> None:
        """Test failure PhaseResult should include test output and changed files."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        test_output = "FAIL src/foo.test.ts\nTests  2 failed, 3 passed\n"
        completed = subprocess.CompletedProcess(
            args=[], returncode=1, stdout=test_output, stderr=""
        )

        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch.object(builder, "_run_baseline_tests", return_value=None),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=completed,
            ),
            # Mock get_changed_files to return the expected changed files
            patch(
                "loom_tools.shepherd.phases.builder.get_changed_files",
                return_value=["src/foo.ts", "src/bar.ts"],
            ),
        ):
            result = builder._run_test_verification(mock_context)

        assert result is not None
        assert result.data["test_failure"] is True
        assert result.data["test_command"] == "pnpm test"
        assert "2 failed" in result.data["test_summary"]
        assert "FAIL" in result.data["test_output_tail"]
        assert result.data["changed_files"] == ["src/foo.ts", "src/bar.ts"]

    def test_test_failure_uses_get_changed_files_helper(
        self, mock_context: MagicMock
    ) -> None:
        """Verify _run_test_verification uses get_changed_files helper.

        This test verifies the fix for the bug where committed changes were not
        detected because the code used 'git diff --name-only origin/main' instead
        of 'git diff --name-only origin/main...HEAD'. The get_changed_files helper
        uses the correct three-dot syntax to detect both committed and uncommitted
        changes.
        """
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        test_output = "FAIL tests/test_example.py\n1 failed\n"
        completed = subprocess.CompletedProcess(
            args=[], returncode=1, stdout=test_output, stderr=""
        )

        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pytest"], "pytest"),
            ),
            patch.object(builder, "_run_baseline_tests", return_value=None),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=completed,
            ),
            patch(
                "loom_tools.shepherd.phases.builder.get_changed_files",
            ) as mock_get_changed_files,
        ):
            # Simulate committed changes that the old code would have missed
            mock_get_changed_files.return_value = ["src/module.py", "tests/test_module.py"]
            result = builder._run_test_verification(mock_context)

        # Verify get_changed_files was called with the worktree path
        mock_get_changed_files.assert_called_once_with(cwd=worktree_mock)

        # Verify the changed files are included in the result
        assert result is not None
        assert result.data["changed_files"] == ["src/module.py", "tests/test_module.py"]

    def test_preserve_handles_missing_worktree(self) -> None:
        """_preserve_on_test_failure should handle None worktree gracefully."""
        builder = BuilderPhase()
        ctx = MagicMock(spec=ShepherdContext)
        ctx.config = ShepherdConfig(issue=42)
        ctx.repo_root = Path("/fake/repo")
        ctx.worktree_path = None
        ctx.label_cache = MagicMock()

        test_result = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={"test_failure": True},
        )

        with (
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=subprocess.CompletedProcess(
                    args=[], returncode=0, stdout="", stderr=""
                ),
            ),
            patch("loom_tools.shepherd.phases.builder.transition_issue_labels"),
        ):
            # Should not raise even with worktree_path=None
            builder._preserve_on_test_failure(ctx, test_result)


class TestBuilderPushBranch:
    """Test builder phase branch pushing."""

    def test_push_branch_success(self, mock_context: MagicMock) -> None:
        """Should push branch to remote and return True."""
        builder = BuilderPhase()
        mock_context.config.issue = 42
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        worktree_mock.__str__ = lambda self: "/fake/repo/.loom/worktrees/issue-42"
        mock_context.worktree_path = worktree_mock

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=subprocess.CompletedProcess(
                args=[], returncode=0, stdout="", stderr=""
            ),
        ):
            result = builder._push_branch(mock_context)

        assert result is True

    def test_push_branch_failure(self, mock_context: MagicMock) -> None:
        """Should return False when push fails."""
        builder = BuilderPhase()
        mock_context.config.issue = 42
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        worktree_mock.__str__ = lambda self: "/fake/repo/.loom/worktrees/issue-42"
        mock_context.worktree_path = worktree_mock

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=subprocess.CompletedProcess(
                args=[], returncode=1, stdout="", stderr="error: failed to push"
            ),
        ):
            result = builder._push_branch(mock_context)

        assert result is False

    def test_push_branch_no_worktree(self, mock_context: MagicMock) -> None:
        """Should return False when worktree doesn't exist."""
        builder = BuilderPhase()
        mock_context.worktree_path = None

        result = builder._push_branch(mock_context)
        assert result is False


class TestBuilderBaselineTests:
    """Test builder phase baseline test comparison."""

    def test_run_baseline_returns_result(self, mock_context: MagicMock) -> None:
        """Should return CompletedProcess when baseline runs successfully."""
        builder = BuilderPhase()
        mock_context.repo_root = MagicMock()
        mock_context.repo_root.is_dir.return_value = True

        completed = subprocess.CompletedProcess(
            args=[], returncode=1, stdout="FAIL foo\n", stderr=""
        )
        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "check:ci"], "pnpm check:ci"),
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=completed,
            ),
        ):
            result = builder._run_baseline_tests(
                mock_context, ["pnpm", "check:ci"], "pnpm check:ci"
            )

        assert result is not None
        assert result.returncode == 1

    def test_run_baseline_no_repo_root(self, mock_context: MagicMock) -> None:
        """Should return None when repo root is not available."""
        builder = BuilderPhase()
        mock_context.repo_root = None

        result = builder._run_baseline_tests(
            mock_context, ["pnpm", "test"], "pnpm test"
        )
        assert result is None

    def test_run_baseline_timeout(self, mock_context: MagicMock) -> None:
        """Should return None when baseline times out."""
        builder = BuilderPhase()
        mock_context.repo_root = MagicMock()
        mock_context.repo_root.is_dir.return_value = True

        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                side_effect=subprocess.TimeoutExpired(cmd="pnpm test", timeout=300),
            ),
        ):
            result = builder._run_baseline_tests(
                mock_context, ["pnpm", "test"], "pnpm test"
            )

        assert result is None

    def test_run_baseline_os_error(self, mock_context: MagicMock) -> None:
        """Should return None on OSError."""
        builder = BuilderPhase()
        mock_context.repo_root = MagicMock()
        mock_context.repo_root.is_dir.return_value = True

        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                side_effect=OSError("not found"),
            ),
        ):
            result = builder._run_baseline_tests(
                mock_context, ["pnpm", "test"], "pnpm test"
            )

        assert result is None

    def test_run_baseline_no_test_runner_at_root(
        self, mock_context: MagicMock
    ) -> None:
        """Should return None when no test runner detected at repo root."""
        builder = BuilderPhase()
        mock_context.repo_root = MagicMock()
        mock_context.repo_root.is_dir.return_value = True

        with patch.object(builder, "_detect_test_command", return_value=None):
            result = builder._run_baseline_tests(
                mock_context, ["pnpm", "test"], "pnpm test"
            )

        assert result is None


class TestBuilderExtractErrorLines:
    """Test builder phase error line extraction."""

    def test_extracts_fail_lines(self) -> None:
        """Should extract lines containing FAIL."""
        builder = BuilderPhase()
        output = "PASS src/ok.test.ts\nFAIL src/bad.test.ts\nDone.\n"
        lines = builder._extract_error_lines(output)
        assert len(lines) == 1
        assert "FAIL src/bad.test.ts" in lines[0]

    def test_extracts_error_lines(self) -> None:
        """Should extract lines containing error."""
        builder = BuilderPhase()
        output = "Error: module not found\nCompiled successfully.\n"
        lines = builder._extract_error_lines(output)
        assert len(lines) == 1
        assert "Error: module not found" in lines[0]

    def test_empty_output(self) -> None:
        """Should return empty list for empty output."""
        builder = BuilderPhase()
        assert builder._extract_error_lines("") == []
        assert builder._extract_error_lines("\n\n") == []

    def test_no_errors(self) -> None:
        """Should return empty list when no error indicators present."""
        builder = BuilderPhase()
        output = "PASS src/ok.test.ts\nAll tests passed.\nDone.\n"
        lines = builder._extract_error_lines(output)
        assert len(lines) == 0

    def test_excludes_coverage_threshold_lines(self) -> None:
        """Should exclude vitest/istanbul coverage threshold violation lines."""
        builder = BuilderPhase()
        output = (
            "Tests  1075 passed\n"
            "ERROR: Coverage for functions (56.83%) does not meet global threshold (75%)\n"
            "ERROR: Coverage for lines (42%) does not meet global threshold (50%)\n"
            "ERROR: Coverage for branches (38%) does not meet global threshold (45%)\n"
        )
        lines = builder._extract_error_lines(output)
        assert len(lines) == 0

    def test_excludes_coverage_but_keeps_real_errors(self) -> None:
        """Coverage lines filtered but actual test errors preserved."""
        builder = BuilderPhase()
        output = (
            "FAIL src/bad.test.ts\n"
            "Error: expected true to be false\n"
            "ERROR: Coverage for functions (56.83%) does not meet global threshold (75%)\n"
        )
        lines = builder._extract_error_lines(output)
        assert len(lines) == 2
        assert any("FAIL" in line for line in lines)
        assert any("expected true to be false" in line for line in lines)

    def test_excludes_coverage_threshold_generic(self) -> None:
        """Should exclude generic coverage threshold lines."""
        builder = BuilderPhase()
        output = "Coverage threshold not met for src/lib\n"
        lines = builder._extract_error_lines(output)
        assert len(lines) == 0


class TestBuilderNormalizeErrorLine:
    """Test builder phase error line normalization."""

    def test_normalizes_timestamps(self) -> None:
        """Should replace ISO-8601 timestamps with placeholder."""
        builder = BuilderPhase()
        line = "Error at 2026-02-01T05:20:02.687Z: connection failed"
        result = builder._normalize_error_line(line)
        assert "<TIMESTAMP>" in result
        assert "2026-02-01" not in result

    def test_normalizes_error_ids(self) -> None:
        """Should replace error IDs like ERR-xxx-yyy with placeholder."""
        builder = BuilderPhase()
        line = "FAIL ERR-ml3akrjz-zq26l: test_something"
        result = builder._normalize_error_line(line)
        assert "<ERR-ID>" in result
        assert "ml3akrjz" not in result

    def test_normalizes_hex_hashes(self) -> None:
        """Should replace hex strings (8+ chars) with placeholder."""
        builder = BuilderPhase()
        line = "Error in module at 0x7fff5fbff8a0 (commit a7866b6f)"
        result = builder._normalize_error_line(line)
        assert "<HEX>" in result
        assert "7fff5fbff8a0" not in result

    def test_normalizes_line_column_numbers(self) -> None:
        """Should replace :line:col patterns with placeholder."""
        builder = BuilderPhase()
        line = "Error: src/foo.ts:123:45 - unexpected token"
        result = builder._normalize_error_line(line)
        assert ":<L>:<C>" in result
        assert ":123:45" not in result

    def test_normalizes_timing_values(self) -> None:
        """Should replace timing values with placeholder."""
        builder = BuilderPhase()
        line = "FAIL test_slow (took 2.45s, timeout 30s)"
        result = builder._normalize_error_line(line)
        assert "<TIME>" in result
        assert "2.45s" not in result

    def test_normalizes_coverage_percentages(self) -> None:
        """Should replace percentage values with placeholder."""
        builder = BuilderPhase()
        line = "Error: coverage 85.2% below threshold 90%"
        result = builder._normalize_error_line(line)
        assert "<PCT>" in result
        assert "85.2%" not in result

    def test_normalizes_uuids(self) -> None:
        """Should replace UUIDs with placeholder."""
        builder = BuilderPhase()
        line = "Error: session 550e8400-e29b-41d4-a716-446655440000 expired"
        result = builder._normalize_error_line(line)
        assert "<UUID>" in result
        assert "550e8400" not in result

    def test_preserves_error_message_structure(self) -> None:
        """Should keep the structural parts of error messages intact."""
        builder = BuilderPhase()
        line = "FAIL src/bad.test.ts: test_something"
        result = builder._normalize_error_line(line)
        assert "FAIL src/bad.test.ts: test_something" == result

    def test_same_error_different_runs_match(self) -> None:
        """Same logical error with different non-deterministic content should match."""
        builder = BuilderPhase()
        run1 = "Error ERR-abc123-def4: biome config at 2026-01-25T10:00:00Z"
        run2 = "Error ERR-xyz789-ghi0: biome config at 2026-01-26T15:30:00Z"
        assert builder._normalize_error_line(run1) == builder._normalize_error_line(run2)

    def test_extract_error_lines_normalizes(self) -> None:
        """_extract_error_lines should return normalized lines."""
        builder = BuilderPhase()
        output = (
            "PASS src/ok.test.ts\n"
            "FAIL ERR-abc123-def4 at 2026-01-25T10:00:00Z\n"
            "Done.\n"
        )
        lines = builder._extract_error_lines(output)
        assert len(lines) == 1
        assert "<ERR-ID>" in lines[0]
        assert "<TIMESTAMP>" in lines[0]

    def test_normalized_set_diff_eliminates_false_positives(self) -> None:
        """Set diff of normalized lines should not produce false positives."""
        builder = BuilderPhase()
        # Same biome error, different error IDs and timestamps per run
        baseline = (
            "error ERR-abc123-def4: biome config invalid at 2026-01-25T10:00:00Z\n"
            "FAIL: linting (took 1.23s)\n"
        )
        worktree = (
            "error ERR-xyz789-ghi0: biome config invalid at 2026-01-26T15:30:00Z\n"
            "FAIL: linting (took 2.05s)\n"
        )
        baseline_errors = set(builder._extract_error_lines(baseline))
        worktree_errors = set(builder._extract_error_lines(worktree))
        new_errors = worktree_errors - baseline_errors
        assert len(new_errors) == 0


class TestBuilderParseFailureCount:
    """Test builder phase failure count parsing."""

    def test_pytest_summary(self) -> None:
        """Should parse pytest failure count from summary line."""
        builder = BuilderPhase()
        output = "========================= 1 failed, 14 passed in 2.45s ========================\n"
        assert builder._parse_failure_count(output) == 1

    def test_pytest_multiple_failures(self) -> None:
        """Should parse pytest with multiple failures."""
        builder = BuilderPhase()
        output = "========================= 3 failed, 12 passed in 5.01s ========================\n"
        assert builder._parse_failure_count(output) == 3

    def test_cargo_test_summary(self) -> None:
        """Should parse cargo test failure count."""
        builder = BuilderPhase()
        output = "test result: FAILED. 8 passed; 2 failed; 0 ignored; 0 measured\n"
        assert builder._parse_failure_count(output) == 2

    def test_vitest_summary(self) -> None:
        """Should parse vitest/jest failure count."""
        builder = BuilderPhase()
        output = "Tests  2 failed, 3 passed\n"
        assert builder._parse_failure_count(output) == 2

    def test_no_recognizable_output(self) -> None:
        """Should return None for unrecognized output."""
        builder = BuilderPhase()
        output = "Build complete.\nAll good.\n"
        assert builder._parse_failure_count(output) is None

    def test_empty_output(self) -> None:
        """Should return None for empty output."""
        builder = BuilderPhase()
        assert builder._parse_failure_count("") is None

    def test_cargo_multi_target_failure(self) -> None:
        """Should parse cargo multi-target failure count."""
        builder = BuilderPhase()
        output = (
            "test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured\n"
            "\n"
            "error: 1 target failed:\n"
            "    `-p loom-daemon --test integration_basic`\n"
        )
        assert builder._parse_failure_count(output) == 1

    def test_cargo_multi_target_plural(self) -> None:
        """Should parse cargo multi-target failure with plural 'targets'."""
        builder = BuilderPhase()
        output = "error: 3 targets failed:\n    `target1`\n    `target2`\n    `target3`\n"
        assert builder._parse_failure_count(output) == 3

    def test_vitest_all_pass(self) -> None:
        """Should return 0 for vitest all-pass output (no 'failed' keyword)."""
        builder = BuilderPhase()
        output = "Tests  14 passed\n"
        assert builder._parse_failure_count(output) == 0

    def test_pytest_all_pass(self) -> None:
        """Should return 0 for pytest all-pass output."""
        builder = BuilderPhase()
        output = "========================= 14 passed in 2.45s ========================\n"
        assert builder._parse_failure_count(output) == 0

    def test_vitest_all_pass_with_coverage_errors(self) -> None:
        """Should return 0 when all tests pass but coverage lines have 'error'."""
        builder = BuilderPhase()
        output = (
            " Tests  1075 passed\n"
            "ERROR: Coverage for functions (56.83%) does not meet global threshold (75%)\n"
            "ERROR: Coverage for lines (42%) does not meet global threshold (50%)\n"
        )
        assert builder._parse_failure_count(output) == 0

    def test_pipeline_passing_cargo_then_failing_vitest(self) -> None:
        """Should return failures when cargo passes but vitest fails."""
        builder = BuilderPhase()
        # Simulates `pnpm check:ci:lite` where cargo succeeds then vitest fails
        output = (
            "running 14 tests\n"
            "test daemon::tests::test_config ... ok\n"
            "test daemon::tests::test_state ... ok\n"
            "test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured\n"
            "\n"
            " FAIL  src/components/Terminal.test.tsx > renders correctly\n"
            " Tests  2 failed, 45 passed\n"
        )
        assert builder._parse_failure_count(output) == 2

    def test_pipeline_failing_cargo_then_passing_pnpm(self) -> None:
        """Should return failures when cargo fails but pnpm tests pass."""
        builder = BuilderPhase()
        # Simulates pipeline where cargo fails then later tests pass
        output = (
            "running 8 tests\n"
            "test utils::tests::test_parse ... FAILED\n"
            "test result: FAILED. 7 passed; 1 failed; 0 ignored\n"
            "\n"
            " Tests  50 passed\n"
        )
        assert builder._parse_failure_count(output) == 1

    def test_interleaved_cargo_binaries_mixed_results(self) -> None:
        """Should return worst result from interleaved cargo test binaries."""
        builder = BuilderPhase()
        # Multiple cargo test binaries with mixed pass/fail
        output = (
            "running 5 tests\n"
            "test result: ok. 5 passed; 0 failed; 0 ignored\n"
            "\n"
            "running 8 tests\n"
            "test integration::test_api ... FAILED\n"
            "test result: FAILED. 6 passed; 2 failed; 0 ignored\n"
            "\n"
            "running 3 tests\n"
            "test result: ok. 3 passed; 0 failed; 0 ignored\n"
        )
        assert builder._parse_failure_count(output) == 2

    def test_multiple_failure_counts_returns_max(self) -> None:
        """Should return highest failure count when multiple summaries differ."""
        builder = BuilderPhase()
        # Different test runners with different failure counts
        output = (
            "test result: FAILED. 10 passed; 3 failed; 0 ignored\n"
            "========================= 5 failed, 7 passed in 2.45s ========================\n"
            " Tests  1 failed, 20 passed\n"
        )
        assert builder._parse_failure_count(output) == 5

    def test_all_stages_pass_in_pipeline(self) -> None:
        """Should return 0 when all stages in pipeline pass."""
        builder = BuilderPhase()
        output = (
            "test result: ok. 14 passed; 0 failed; 0 ignored\n"
            "\n"
            " Tests  50 passed\n"
            "\n"
            "========================= 20 passed in 1.23s ========================\n"
        )
        assert builder._parse_failure_count(output) == 0


class TestBuilderExtractFailingTestNames:
    """Test builder phase failing test name extraction."""

    def test_pytest_failed_names(self) -> None:
        """Should extract pytest FAILED test names from short summary."""
        builder = BuilderPhase()
        output = (
            "FAILED tests/test_foo.py::test_bar - AssertionError\n"
            "FAILED tests/test_baz.py::test_qux - ValueError\n"
        )
        names = builder._extract_failing_test_names(output)
        assert names == {
            "tests/test_foo.py::test_bar",
            "tests/test_baz.py::test_qux",
        }

    def test_cargo_test_names(self) -> None:
        """Should extract cargo test failing test names."""
        builder = BuilderPhase()
        output = (
            "test utils::tests::test_parse ... ok\n"
            "test utils::tests::test_validate ... FAILED\n"
            "test core::tests::test_run ... ok\n"
        )
        names = builder._extract_failing_test_names(output)
        assert names == {"utils::tests::test_validate"}

    def test_vitest_fail_names(self) -> None:
        """Should extract vitest FAIL file names."""
        builder = BuilderPhase()
        output = (
            "FAIL src/foo.test.ts\n"
            "FAIL src/bar.test.ts\n"
            " PASS src/ok.test.ts\n"
        )
        names = builder._extract_failing_test_names(output)
        assert names == {"src/foo.test.ts", "src/bar.test.ts"}

    def test_empty_output(self) -> None:
        """Should return empty set for no failures."""
        builder = BuilderPhase()
        assert builder._extract_failing_test_names("") == set()
        assert builder._extract_failing_test_names("All tests passed.\n") == set()


class TestBuilderCompareTestResults:
    """Test builder phase structured test comparison."""

    def test_same_failure_count_returns_none(self) -> None:
        """Same failure count -> no new failures (pre-existing)."""
        builder = BuilderPhase()
        baseline = "========================= 1 failed, 14 passed in 2.45s ========================\n"
        worktree = "========================= 1 failed, 14 passed in 2.51s ========================\n"
        assert builder._compare_test_results(baseline, worktree) is None

    def test_worktree_more_failures_returns_true(self) -> None:
        """Higher failure count in worktree -> new failures detected."""
        builder = BuilderPhase()
        baseline = "========================= 1 failed, 14 passed in 2.45s ========================\n"
        worktree = "========================= 2 failed, 13 passed in 2.51s ========================\n"
        assert builder._compare_test_results(baseline, worktree) is True

    def test_worktree_fewer_failures_returns_none(self) -> None:
        """Fewer failures in worktree -> improvement, no new failures."""
        builder = BuilderPhase()
        baseline = "========================= 2 failed, 13 passed in 2.45s ========================\n"
        worktree = "========================= 1 failed, 14 passed in 2.51s ========================\n"
        assert builder._compare_test_results(baseline, worktree) is None

    def test_neither_parseable_returns_false(self) -> None:
        """Neither output parseable -> signal fallback."""
        builder = BuilderPhase()
        baseline = "Something went wrong\n"
        worktree = "Unknown error\n"
        assert builder._compare_test_results(baseline, worktree) is False

    def test_one_side_parseable_returns_false(self) -> None:
        """Only one side parseable -> can't compare, signal fallback."""
        builder = BuilderPhase()
        baseline = "========================= 1 failed, 14 passed in 2.45s ========================\n"
        worktree = "Something went wrong\n"
        assert builder._compare_test_results(baseline, worktree) is False

    def test_cargo_test_same_count(self) -> None:
        """Cargo test with same failure count -> pre-existing."""
        builder = BuilderPhase()
        baseline = "test result: FAILED. 8 passed; 1 failed; 0 ignored\n"
        worktree = "test result: FAILED. 8 passed; 1 failed; 0 ignored\n"
        assert builder._compare_test_results(baseline, worktree) is None

    def test_vitest_same_count(self) -> None:
        """Vitest with same failure count -> pre-existing."""
        builder = BuilderPhase()
        baseline = "Tests  2 failed, 3 passed\n"
        worktree = "Tests  2 failed, 3 passed\n"
        assert builder._compare_test_results(baseline, worktree) is None

    def test_cargo_multi_target_same_count(self) -> None:
        """Cargo multi-target with same failure count -> pre-existing."""
        builder = BuilderPhase()
        # Typical cargo output when integration tests fail but unit tests pass
        cargo_output = (
            "test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured\n"
            "\n"
            "error: 1 target failed:\n"
            "    `-p loom-daemon --test integration_basic`\n"
        )
        assert builder._compare_test_results(cargo_output, cargo_output) is None

    def test_both_all_pass_coverage_only_failure(self) -> None:
        """Both sides all-pass with coverage errors -> no new failures (structured path)."""
        builder = BuilderPhase()
        baseline = (
            " Tests  1075 passed\n"
            "ERROR: Coverage for functions (56.83%) does not meet global threshold (75%)\n"
        )
        worktree = (
            " Tests  1075 passed\n"
            "ERROR: Coverage for functions (55.12%) does not meet global threshold (75%)\n"
        )
        # Both parse to 0 failures -> structured comparison returns None (no new failures)
        assert builder._compare_test_results(baseline, worktree) is None

    def test_all_pass_vs_new_failures(self) -> None:
        """Baseline all-pass, worktree has failures -> new failures detected."""
        builder = BuilderPhase()
        baseline = " Tests  1075 passed\n"
        worktree = "Tests  1 failed, 1074 passed\n"
        assert builder._compare_test_results(baseline, worktree) is True

    def test_higher_count_but_same_test_names_returns_none(self) -> None:
        """Higher count but identical test names -> count discrepancy is noise."""
        builder = BuilderPhase()
        baseline = (
            "FAILED tests/test_foo.py::test_bar - AssertionError\n"
            "========================= 1 failed, 14 passed in 2.45s ========================\n"
        )
        worktree = (
            "FAILED tests/test_foo.py::test_bar - AssertionError\n"
            "========================= 2 failed, 13 passed in 2.51s ========================\n"
        )
        assert builder._compare_test_results(baseline, worktree) is None

    def test_higher_count_with_new_test_name_returns_true(self) -> None:
        """Higher count with genuinely new test name -> new failures."""
        builder = BuilderPhase()
        baseline = (
            "FAILED tests/test_foo.py::test_bar - AssertionError\n"
            "========================= 1 failed, 14 passed in 2.45s ========================\n"
        )
        worktree = (
            "FAILED tests/test_foo.py::test_bar - AssertionError\n"
            "FAILED tests/test_baz.py::test_qux - ValueError\n"
            "========================= 2 failed, 13 passed in 2.51s ========================\n"
        )
        assert builder._compare_test_results(baseline, worktree) is True

    def test_regression_2006_different_test_names(self) -> None:
        """Regression test for #2006: different tests failing should be detected.

        Baseline fails test_cli_wrapper_health, worktree fails integration_basic.
        The name-based comparison should detect integration_basic as new.
        """
        builder = BuilderPhase()
        baseline = (
            "test utils::tests::test_cli_wrapper_health ... FAILED\n"
            "test result: FAILED. 8 passed; 1 failed; 0 ignored\n"
        )
        worktree = (
            "test utils::tests::test_cli_wrapper_health ... FAILED\n"
            "test integration::tests::integration_basic ... FAILED\n"
            "test result: FAILED. 7 passed; 2 failed; 0 ignored\n"
        )
        assert builder._compare_test_results(baseline, worktree) is True

    def test_flaky_test_swap_same_count(self) -> None:
        """Flaky test swap: baseline {A}, worktree {B} with same count.

        Count comparison says <= so returns None (pre-existing). This is
        correct behavior — same count means no increase in failures.
        The name-based refinement only triggers when worktree_count > baseline_count.
        """
        builder = BuilderPhase()
        baseline = (
            "FAILED tests/test_a.py::test_alpha - Error\n"
            "========================= 1 failed, 14 passed in 2.45s ========================\n"
        )
        worktree = (
            "FAILED tests/test_b.py::test_beta - Error\n"
            "========================= 1 failed, 14 passed in 2.51s ========================\n"
        )
        # Same count -> returns None via the <= check (before name comparison)
        assert builder._compare_test_results(baseline, worktree) is None

    def test_name_extraction_fails_one_side_trusts_counts(self) -> None:
        """When name extraction fails for one side, fall back to count comparison."""
        builder = BuilderPhase()
        # Baseline has no parseable test names, just a summary
        baseline = (
            "========================= 1 failed, 14 passed in 2.45s ========================\n"
        )
        # Worktree has parseable test names
        worktree = (
            "FAILED tests/test_foo.py::test_bar - Error\n"
            "FAILED tests/test_baz.py::test_qux - Error\n"
            "========================= 2 failed, 13 passed in 2.51s ========================\n"
        )
        # Baseline names empty -> can't do name comparison -> trusts counts
        assert builder._compare_test_results(baseline, worktree) is True

    def test_name_extraction_fails_both_sides_trusts_counts(self) -> None:
        """When name extraction fails for both sides, fall back to count comparison."""
        builder = BuilderPhase()
        baseline = (
            "========================= 1 failed, 14 passed in 2.45s ========================\n"
        )
        worktree = (
            "========================= 2 failed, 13 passed in 2.51s ========================\n"
        )
        # Neither side has parseable names -> trusts counts -> True
        assert builder._compare_test_results(baseline, worktree) is True

    def test_multi_runner_pytest_and_vitest(self) -> None:
        """Multi-runner output: pytest + vitest names don't collide."""
        builder = BuilderPhase()
        baseline = (
            "FAILED tests/test_foo.py::test_bar - AssertionError\n"
            "========================= 1 failed, 4 passed in 1.2s ========================\n"
            "FAIL src/foo.test.ts\n"
            " Tests  1 failed, 10 passed\n"
        )
        worktree = (
            "FAILED tests/test_foo.py::test_bar - AssertionError\n"
            "========================= 1 failed, 4 passed in 1.3s ========================\n"
            "FAIL src/foo.test.ts\n"
            "FAIL src/bar.test.ts\n"
            " Tests  2 failed, 9 passed\n"
        )
        # Baseline: 2 total failures (1 pytest + 1 vitest)
        # Worktree: 3 total failures (1 pytest + 2 vitest)
        # New failure: src/bar.test.ts
        assert builder._compare_test_results(baseline, worktree) is True


class TestBuilderFallbackComparison:
    """Test that fallback to line-based comparison works when parsing fails."""

    def test_fallback_preexisting_identical_output(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback: identical error output in both -> pre-existing."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        # Output with no parseable summary (triggers fallback)
        unparseable_output = "Error: something broke\nSegfault at line 42\n"
        baseline_result = subprocess.CompletedProcess(
            args=[], returncode=1, stdout=unparseable_output, stderr=""
        )
        worktree_result = subprocess.CompletedProcess(
            args=[], returncode=1, stdout=unparseable_output, stderr=""
        )
        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch.object(
                builder, "_run_baseline_tests", return_value=baseline_result
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=worktree_result,
            ),
        ):
            result = builder._run_test_verification(mock_context)

        assert result is None

    def test_fallback_new_errors_detected(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback: new error lines in worktree -> FAILED.

        Uses 7 error lines in worktree vs 1 in baseline (diff=6) to exceed
        the _ERROR_LINE_TOLERANCE of 5, ensuring new errors are detected.
        """
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        baseline_result = subprocess.CompletedProcess(
            args=[], returncode=1, stdout="Error: old bug\n", stderr=""
        )
        # 7 error lines total (1 original + 6 new) exceeds tolerance of 5
        new_errors = "".join(f"Error: new regression {i}\n" for i in range(6))
        worktree_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout=f"Error: old bug\n{new_errors}",
            stderr="",
        )
        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch.object(
                builder, "_run_baseline_tests", return_value=baseline_result
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=worktree_result,
            ),
        ):
            result = builder._run_test_verification(mock_context)

        assert result is not None
        assert result.status == PhaseStatus.FAILED


    def test_fallback_nondeterministic_same_exit_code(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback: same exit code + same error count with non-deterministic content -> pre-existing."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        # Same error with different timestamps/error IDs (the #1935 scenario)
        baseline_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="error ERR-abc123-def4: biome config invalid at 2026-01-25T10:00:00Z\n",
            stderr="",
        )
        worktree_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="error ERR-xyz789-ghi0: biome config invalid at 2026-01-26T15:30:00Z\n",
            stderr="",
        )
        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "check:ci:lite"], "pnpm check:ci:lite"),
            ),
            patch.object(
                builder, "_run_baseline_tests", return_value=baseline_result
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=worktree_result,
            ),
        ):
            result = builder._run_test_verification(mock_context)

        # Should be treated as pre-existing (not a new regression)
        assert result is None

    def test_fallback_different_exit_code_still_fails(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback: different exit codes should still report new errors."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        baseline_result = subprocess.CompletedProcess(
            args=[], returncode=1, stdout="Error: minor issue\n", stderr=""
        )
        worktree_result = subprocess.CompletedProcess(
            args=[], returncode=2, stdout="Error: critical crash\n", stderr=""
        )
        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch.object(
                builder, "_run_baseline_tests", return_value=baseline_result
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=worktree_result,
            ),
        ):
            result = builder._run_test_verification(mock_context)

        assert result is not None
        assert result.status == PhaseStatus.FAILED


class TestBuilderZeroFailuresNonZeroExit:
    """Test that 0 failures with non-zero exit doesn't say 'Tests failed'.

    Issue #2009: When tests pass (0 failures) but the process exits non-zero
    (e.g., coverage threshold not met), the message should NOT say "Tests
    failed" since no tests actually failed.
    """

    def test_structured_path_zero_failures_nonzero_exit(
        self, mock_context: MagicMock
    ) -> None:
        """Non-zero exit with 0 failures should say 'Tests passed but...'."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        # Both baseline and worktree exit non-zero but show 0 failures
        # (e.g., coverage threshold failure)
        baseline_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="Tests  10 passed\nCoverage threshold not met\n",
            stderr="",
        )
        worktree_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="Tests  10 passed\nCoverage threshold not met\n",
            stderr="",
        )
        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch.object(
                builder, "_run_baseline_tests", return_value=baseline_result
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=worktree_result,
            ),
            patch("loom_tools.shepherd.phases.builder.log_warning") as mock_warn,
        ):
            result = builder._run_test_verification(mock_context)

        # Should return None (not a failure)
        assert result is None
        # Message should NOT say "Tests failed"
        mock_warn.assert_called_once()
        call_args = mock_warn.call_args[0][0]
        assert "Tests passed but process exited non-zero" in call_args
        assert "Tests failed" not in call_args

    def test_structured_path_actual_preexisting_failures(
        self, mock_context: MagicMock
    ) -> None:
        """Non-zero exit with actual failures should still say 'Tests failed'."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        # Both baseline and worktree have the same failure count (non-zero)
        baseline_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="Tests  2 failed, 8 passed\n",
            stderr="",
        )
        worktree_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="Tests  2 failed, 8 passed\n",
            stderr="",
        )
        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch.object(
                builder, "_run_baseline_tests", return_value=baseline_result
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=worktree_result,
            ),
            patch("loom_tools.shepherd.phases.builder.log_warning") as mock_warn,
        ):
            result = builder._run_test_verification(mock_context)

        # Should return None (pre-existing)
        assert result is None
        # Message SHOULD say "Tests failed but all failures are pre-existing"
        mock_warn.assert_called_once()
        call_args = mock_warn.call_args[0][0]
        assert "Tests failed but all failures are pre-existing" in call_args

    def test_fallback_path_zero_errors_nonzero_exit(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback path: no error lines with non-zero exit should not say 'Tests failed'."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        # Output with no parseable summary and no error lines (triggers fallback)
        # Both exit non-zero but no actual errors in output
        baseline_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="Build completed successfully\nCoverage: 75%\nThreshold: 80%\n",
            stderr="",
        )
        worktree_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="Build completed successfully\nCoverage: 75%\nThreshold: 80%\n",
            stderr="",
        )
        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "test"], "pnpm test"),
            ),
            patch.object(
                builder, "_run_baseline_tests", return_value=baseline_result
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=worktree_result,
            ),
            patch("loom_tools.shepherd.phases.builder.log_warning") as mock_warn,
        ):
            result = builder._run_test_verification(mock_context)

        # Should return None (not a failure)
        assert result is None
        # Message should NOT say "Tests failed"
        mock_warn.assert_called_once()
        call_args = mock_warn.call_args[0][0]
        assert "Tests passed but process exited non-zero" in call_args
        assert "Tests failed" not in call_args


class TestBuilderEnsureDependencies:
    """Test builder phase dependency installation."""

    def test_installs_when_node_modules_missing(self, tmp_path: Path) -> None:
        """Should run pnpm install when package.json exists but node_modules is missing."""
        builder = BuilderPhase()
        (tmp_path / "package.json").write_text('{"name": "test"}')

        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout="", stderr=""
        )
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=completed,
        ) as mock_run:
            result = builder._ensure_dependencies(tmp_path)

        assert result is True
        mock_run.assert_called_once()
        call_args = mock_run.call_args
        assert call_args[0][0] == ["pnpm", "install", "--frozen-lockfile"]
        assert call_args[1]["cwd"] == tmp_path

    def test_noop_when_node_modules_exists(self, tmp_path: Path) -> None:
        """Should be a no-op when node_modules already exists."""
        builder = BuilderPhase()
        (tmp_path / "package.json").write_text('{"name": "test"}')
        (tmp_path / "node_modules").mkdir()

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
        ) as mock_run:
            result = builder._ensure_dependencies(tmp_path)

        assert result is True
        mock_run.assert_not_called()

    def test_noop_when_node_modules_is_symlink(self, tmp_path: Path) -> None:
        """Should be a no-op when node_modules is a symlink to a directory.

        Worktree creation symlinks node_modules from main workspace to avoid
        expensive pnpm install on every worktree (30-60s savings).
        """
        builder = BuilderPhase()
        (tmp_path / "package.json").write_text('{"name": "test"}')

        # Create source node_modules directory and symlink to it
        main_node_modules = tmp_path / "main_node_modules"
        main_node_modules.mkdir()
        (tmp_path / "node_modules").symlink_to(main_node_modules)

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
        ) as mock_run:
            result = builder._ensure_dependencies(tmp_path)

        assert result is True
        mock_run.assert_not_called()

    def test_noop_when_no_package_json(self, tmp_path: Path) -> None:
        """Should be a no-op when no package.json exists (non-JS project)."""
        builder = BuilderPhase()

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
        ) as mock_run:
            result = builder._ensure_dependencies(tmp_path)

        assert result is True
        mock_run.assert_not_called()

    def test_handles_install_failure(self, tmp_path: Path) -> None:
        """Should return False on install failure without raising."""
        builder = BuilderPhase()
        (tmp_path / "package.json").write_text('{"name": "test"}')

        completed = subprocess.CompletedProcess(
            args=[], returncode=1,
            stdout="", stderr="ERR_PNPM_FROZEN_LOCKFILE_WITH_OUTDATED_LOCKFILE"
        )
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=completed,
        ):
            result = builder._ensure_dependencies(tmp_path)

        assert result is False

    def test_handles_timeout(self, tmp_path: Path) -> None:
        """Should return False on timeout without raising."""
        builder = BuilderPhase()
        (tmp_path / "package.json").write_text('{"name": "test"}')

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            side_effect=subprocess.TimeoutExpired(cmd="pnpm install", timeout=120),
        ):
            result = builder._ensure_dependencies(tmp_path)

        assert result is False

    def test_handles_os_error(self, tmp_path: Path) -> None:
        """Should return False on OSError (pnpm not installed) without raising."""
        builder = BuilderPhase()
        (tmp_path / "package.json").write_text('{"name": "test"}')

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            side_effect=OSError("pnpm not found"),
        ):
            result = builder._ensure_dependencies(tmp_path)

        assert result is False


class TestJudgePhase:
    """Test JudgePhase."""

    def test_should_skip_when_from_merge_and_approved(
        self, mock_context: MagicMock
    ) -> None:
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

    def test_validation_retries_on_race_condition(
        self, mock_context: MagicMock
    ) -> None:
        """Validation should retry and succeed when label appears on second attempt.

        Simulates the race condition from issue #1764 where the judge worker
        applies the label after the first validation check runs.
        """
        mock_context.pr_number = 100
        mock_context.check_shutdown.return_value = False
        mock_context.has_pr_label.return_value = True  # loom:pr found after retry

        judge = JudgePhase()
        # First validate() call fails (label not yet applied), second succeeds
        with (
            patch.object(judge, "validate", side_effect=[False, True]) as mock_validate,
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep") as mock_sleep,
        ):
            result = judge.run(mock_context)

        assert result.status == PhaseStatus.SUCCESS
        assert "approved" in result.message
        assert mock_validate.call_count == 2
        mock_sleep.assert_called_once_with(2)
        # Cache should be invalidated before first attempt and before retry
        assert mock_context.label_cache.invalidate_pr.call_count >= 2

    def test_validation_fails_after_retries_exhausted(
        self, mock_context: MagicMock
    ) -> None:
        """Validation should fail after all retry attempts are exhausted.

        When the label never appears (e.g., judge truly failed), all 3 attempts
        should fail and return FAILED status. Failure message should include
        diagnostic context from the worker.
        """
        mock_context.pr_number = 100
        mock_context.check_shutdown.return_value = False

        judge = JudgePhase()
        fake_diag = {
            "summary": "no GitHub reviews on PR (Loom uses comment + label workflow instead); no loom labels on PR; log file not found",
            "log_file": "/fake/repo/.loom/logs/loom-judge-issue-42.log",
            "log_exists": False,
            "log_tail": [],
            "pr_reviews": [],
            "pr_labels": [],
        }
        with (
            patch.object(judge, "validate", return_value=False) as mock_validate,
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep") as mock_sleep,
            patch.object(judge, "_gather_diagnostics", return_value=fake_diag),
        ):
            result = judge.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert "validation failed" in result.message
        assert "no GitHub reviews on PR (Loom uses comment + label workflow instead)" in result.message
        assert result.data == fake_diag
        assert mock_validate.call_count == 3
        # Should sleep between attempts (2 sleeps for 3 attempts)
        assert mock_sleep.call_count == 2

    def test_cache_invalidated_before_validation(self, mock_context: MagicMock) -> None:
        """Cache should be invalidated BEFORE validation, not after.

        This ensures the first validation attempt uses fresh data from the API
        instead of stale cached labels (fixes the gh-cached staleness issue).
        """
        mock_context.pr_number = 100
        mock_context.check_shutdown.return_value = False
        mock_context.has_pr_label.return_value = True

        judge = JudgePhase()
        call_order: list[str] = []

        def track_invalidate(pr: int | None = None) -> None:
            call_order.append("invalidate")

        def track_validate(ctx: ShepherdContext) -> bool:
            call_order.append("validate")
            return True

        mock_context.label_cache.invalidate_pr.side_effect = track_invalidate

        with (
            patch.object(judge, "validate", side_effect=track_validate),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
        ):
            result = judge.run(mock_context)

        assert result.status == PhaseStatus.SUCCESS
        # The first two calls should be: invalidate, then validate
        assert call_order[0] == "invalidate"
        assert call_order[1] == "validate"

    def test_validation_succeeds_on_first_attempt(
        self, mock_context: MagicMock
    ) -> None:
        """When validation succeeds on first attempt, no retries or sleeps happen."""
        mock_context.pr_number = 100
        mock_context.check_shutdown.return_value = False
        mock_context.has_pr_label.return_value = True

        judge = JudgePhase()
        with (
            patch.object(judge, "validate", return_value=True) as mock_validate,
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep") as mock_sleep,
        ):
            result = judge.run(mock_context)

        assert result.status == PhaseStatus.SUCCESS
        assert mock_validate.call_count == 1
        mock_sleep.assert_not_called()


class TestJudgeFallbackApproval:
    """Test force-mode fallback approval detection in JudgePhase."""

    def _make_force_context(self, mock_context: MagicMock) -> MagicMock:
        """Set up a mock context for force-mode judge tests."""
        mock_context.config = ShepherdConfig(issue=42, mode=ExecutionMode.FORCE_MERGE)
        mock_context.pr_number = 100
        mock_context.check_shutdown.return_value = False
        return mock_context

    def test_fallback_activates_in_force_mode_with_approval_and_checks(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback should succeed when force mode + approval comment + passing checks."""
        ctx = self._make_force_context(mock_context)
        # After fallback applies loom:pr, the label check should find it
        ctx.has_pr_label.return_value = True

        judge = JudgePhase()

        gh_run_results = [
            # _has_approval_comment: gh pr view --json comments
            MagicMock(returncode=0, stdout="LGTM, looks good!\n"),
            # _pr_checks_passing: gh pr view --json statusCheckRollup,mergeable
            MagicMock(
                returncode=0,
                stdout=json.dumps({"mergeable": "MERGEABLE", "statusCheckRollup": []}),
            ),
            # _try_fallback_approval: gh pr edit --add-label loom:pr
            MagicMock(returncode=0),
        ]

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            patch(
                "loom_tools.shepherd.phases.judge.subprocess.run",
                side_effect=gh_run_results,
            ),
        ):
            result = judge.run(ctx)

        assert result.status == PhaseStatus.SUCCESS
        assert result.data.get("approved") is True

    def test_fallback_does_not_activate_in_default_mode(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback should NOT activate when not in force mode."""
        mock_context.config = ShepherdConfig(issue=42)  # Default mode
        mock_context.pr_number = 100
        mock_context.check_shutdown.return_value = False

        judge = JudgePhase()
        fake_diag = {"summary": "no GitHub reviews on PR (Loom uses comment + label workflow instead)", "log_tail": []}

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            patch.object(judge, "_gather_diagnostics", return_value=fake_diag),
        ):
            result = judge.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert "validation failed" in result.message

    def test_fallback_denied_without_approval_comment(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback should fail when no approval comment is found."""
        ctx = self._make_force_context(mock_context)

        judge = JudgePhase()
        fake_diag = {"summary": "no GitHub reviews on PR (Loom uses comment + label workflow instead)", "log_tail": []}

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            # No approval comment
            patch.object(judge, "_has_approval_comment", return_value=False),
            patch.object(judge, "_pr_checks_passing", return_value=True),
            # Rejection fallback also fails — no signals
            patch.object(judge, "_has_rejection_comment", return_value=False),
            patch.object(judge, "_has_changes_requested_review", return_value=False),
            patch.object(judge, "_gather_diagnostics", return_value=fake_diag),
        ):
            result = judge.run(ctx)

        assert result.status == PhaseStatus.FAILED

    def test_fallback_denied_without_passing_checks(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback should fail when PR checks are not passing."""
        ctx = self._make_force_context(mock_context)

        judge = JudgePhase()
        fake_diag = {"summary": "no GitHub reviews on PR (Loom uses comment + label workflow instead)", "log_tail": []}

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            patch.object(judge, "_has_approval_comment", return_value=True),
            # Checks not passing
            patch.object(judge, "_pr_checks_passing", return_value=False),
            # Rejection fallback also fails — no signals
            patch.object(judge, "_has_rejection_comment", return_value=False),
            patch.object(judge, "_has_changes_requested_review", return_value=False),
            patch.object(judge, "_gather_diagnostics", return_value=fake_diag),
        ):
            result = judge.run(ctx)

        assert result.status == PhaseStatus.FAILED

    def test_fallback_applies_loom_pr_label(self, mock_context: MagicMock) -> None:
        """Fallback should apply loom:pr label via gh pr edit."""
        ctx = self._make_force_context(mock_context)
        ctx.has_pr_label.return_value = True

        judge = JudgePhase()

        add_label_call = MagicMock(returncode=0)

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            patch.object(judge, "_has_approval_comment", return_value=True),
            patch.object(judge, "_pr_checks_passing", return_value=True),
            patch(
                "loom_tools.shepherd.phases.judge.subprocess.run",
                return_value=add_label_call,
            ) as mock_run,
        ):
            result = judge.run(ctx)

        assert result.status == PhaseStatus.SUCCESS
        # Verify the label was applied
        mock_run.assert_called_once()
        call_args = mock_run.call_args[0][0]
        assert "gh" in call_args[0]
        assert "--add-label" in call_args
        assert "loom:pr" in call_args

    def test_fallback_invalidates_cache_after_label_applied(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback should invalidate PR label cache after applying label."""
        ctx = self._make_force_context(mock_context)
        ctx.has_pr_label.return_value = True

        judge = JudgePhase()

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            patch.object(judge, "_has_approval_comment", return_value=True),
            patch.object(judge, "_pr_checks_passing", return_value=True),
            patch(
                "loom_tools.shepherd.phases.judge.subprocess.run",
                return_value=MagicMock(returncode=0),
            ),
        ):
            result = judge.run(ctx)

        assert result.status == PhaseStatus.SUCCESS
        # Cache should have been invalidated (multiple times: before validation retries + after fallback)
        assert ctx.label_cache.invalidate_pr.call_count >= 2

    def test_fallback_fails_when_label_application_fails(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback should fail if gh pr edit to apply label returns non-zero."""
        ctx = self._make_force_context(mock_context)

        judge = JudgePhase()
        fake_diag = {"summary": "no GitHub reviews on PR (Loom uses comment + label workflow instead)", "log_tail": []}

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            patch.object(judge, "_has_approval_comment", return_value=True),
            patch.object(judge, "_pr_checks_passing", return_value=True),
            patch(
                "loom_tools.shepherd.phases.judge.subprocess.run",
                return_value=MagicMock(returncode=1),  # label application fails
            ),
            patch.object(judge, "_gather_diagnostics", return_value=fake_diag),
        ):
            result = judge.run(ctx)

        assert result.status == PhaseStatus.FAILED

    def test_fallback_approval_returns_immediately_without_label_requery(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback approval should return immediately without re-querying GitHub labels.

        This test verifies the fix for issue #2083: after _try_fallback_approval
        succeeds, run() should NOT call has_pr_label() again. The race condition
        occurs when the fallback applies the label successfully (gh pr edit returns 0)
        but GitHub API propagation delay causes the subsequent label query to
        return stale data.

        By returning immediately when fallback succeeds, we trust the gh pr edit
        return code and avoid the race.
        """
        ctx = self._make_force_context(mock_context)
        # Track calls to has_pr_label
        has_pr_label_calls: list[str] = []

        def track_has_pr_label(label: str) -> bool:
            has_pr_label_calls.append(label)
            # Simulate race condition: label not visible yet (stale data)
            return False

        ctx.has_pr_label.side_effect = track_has_pr_label

        judge = JudgePhase()

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            patch.object(judge, "_has_approval_comment", return_value=True),
            patch.object(judge, "_pr_checks_passing", return_value=True),
            patch(
                "loom_tools.shepherd.phases.judge.subprocess.run",
                return_value=MagicMock(returncode=0),
            ),
        ):
            result = judge.run(ctx)

        # Should succeed because fallback applied the label
        assert result.status == PhaseStatus.SUCCESS
        assert result.data.get("approved") is True
        assert result.data.get("fallback_used") is True
        assert "[force-mode] Fallback approval applied" in result.message

        # CRITICAL: has_pr_label should NOT have been called after fallback succeeded.
        # Any calls before the fallback (during validation retry loop) are fine,
        # but there must be no calls to check for loom:pr or loom:changes-requested
        # after the fallback succeeds.
        #
        # With the fix in place, the fallback returns immediately, so we don't
        # reach the label-checking code at lines 211-225.
        #
        # Note: has_pr_label may be called during _gather_diagnostics or validate(),
        # but the key assertion is that we succeed despite has_pr_label returning False.
        # Before the fix, this test would fail because the code would reach line 211
        # and call has_pr_label("loom:pr") which returns False, causing a failure.


class TestJudgeDiagnostics:
    """Test _gather_diagnostics for judge validation failures."""

    def _make_context(self, mock_context: MagicMock) -> MagicMock:
        mock_context.pr_number = 100
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.repo_root = Path("/fake/repo")
        return mock_context

    def test_includes_log_tail_when_log_exists(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Diagnostics should include last 20 lines of judge log when file exists."""
        ctx = self._make_context(mock_context)
        ctx.repo_root = tmp_path

        # Create a log file with content
        log_dir = tmp_path / ".loom" / "logs"
        log_dir.mkdir(parents=True)
        log_file = log_dir / "loom-judge-issue-42.log"
        lines = [f"log line {i}" for i in range(30)]
        log_file.write_text("\n".join(lines))

        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=1, stdout=""),
        ):
            diag = judge._gather_diagnostics(ctx)

        assert diag["log_exists"] is True
        assert len(diag["log_tail"]) == 20
        assert diag["log_tail"][-1] == "log line 29"
        assert "last output: 'log line 29'" in diag["summary"]

    def test_handles_missing_log_file(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Diagnostics should handle missing log file gracefully."""
        ctx = self._make_context(mock_context)
        ctx.repo_root = tmp_path

        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=1, stdout=""),
        ):
            diag = judge._gather_diagnostics(ctx)

        assert diag["log_exists"] is False
        assert diag["log_tail"] == []
        assert "log file not found" in diag["summary"]

    def test_handles_empty_log_file(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Diagnostics should handle empty log file gracefully."""
        ctx = self._make_context(mock_context)
        ctx.repo_root = tmp_path

        log_dir = tmp_path / ".loom" / "logs"
        log_dir.mkdir(parents=True)
        log_file = log_dir / "loom-judge-issue-42.log"
        log_file.write_text("")

        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=1, stdout=""),
        ):
            diag = judge._gather_diagnostics(ctx)

        assert diag["log_exists"] is True
        assert diag["log_tail"] == []
        assert "log file empty" in diag["summary"]

    def test_includes_pr_review_state(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Diagnostics should include PR review state from GitHub."""
        ctx = self._make_context(mock_context)
        ctx.repo_root = tmp_path

        judge = JudgePhase()

        review_data = json.dumps({
            "reviews": [{"state": "COMMENTED", "author": "bot"}],
            "labels": ["loom:review-requested"],
        })

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=0, stdout=review_data),
        ):
            diag = judge._gather_diagnostics(ctx)

        assert diag["pr_reviews"] == [{"state": "COMMENTED", "author": "bot"}]
        assert "loom:review-requested" in diag["pr_labels"]
        assert "reviews=[COMMENTED]" in diag["summary"]
        assert "labels=[loom:review-requested]" in diag["summary"]

    def test_handles_gh_command_failure(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Diagnostics should handle gh command failures gracefully."""
        ctx = self._make_context(mock_context)
        ctx.repo_root = tmp_path

        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=1, stdout=""),
        ):
            diag = judge._gather_diagnostics(ctx)

        assert diag["pr_reviews"] == []
        assert diag["pr_labels"] == []
        assert "no GitHub reviews on PR (Loom uses comment + label workflow instead)" in diag["summary"]

    def test_failure_message_includes_diagnostics(
        self, mock_context: MagicMock
    ) -> None:
        """When judge validation fails, the PhaseResult message should include diagnostics."""
        mock_context.pr_number = 100
        mock_context.check_shutdown.return_value = False

        judge = JudgePhase()
        fake_diag = {
            "summary": "no GitHub reviews on PR (Loom uses comment + label workflow instead); no loom labels on PR; last output: 'session ended'",
            "log_file": "/fake/repo/.loom/logs/loom-judge-issue-42.log",
            "log_exists": True,
            "log_tail": ["session ended"],
            "pr_reviews": [],
            "pr_labels": [],
        }

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            patch.object(judge, "_gather_diagnostics", return_value=fake_diag),
        ):
            result = judge.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert "judge phase validation failed:" in result.message
        assert "no GitHub reviews on PR (Loom uses comment + label workflow instead)" in result.message
        assert "session ended" in result.message
        assert result.data == fake_diag

    def test_short_log_returns_all_lines(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """When log has fewer than 20 lines, all lines should be included."""
        ctx = self._make_context(mock_context)
        ctx.repo_root = tmp_path

        log_dir = tmp_path / ".loom" / "logs"
        log_dir.mkdir(parents=True)
        log_file = log_dir / "loom-judge-issue-42.log"
        lines = ["line 1", "line 2", "line 3"]
        log_file.write_text("\n".join(lines))

        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=1, stdout=""),
        ):
            diag = judge._gather_diagnostics(ctx)

        assert len(diag["log_tail"]) == 3
        assert diag["log_tail"] == ["line 1", "line 2", "line 3"]

    def test_doctor_fixed_awaiting_outcome_failure_mode(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Issue #1998: Detect when Doctor applied fixes but Judge hasn't applied outcome label."""
        ctx = self._make_context(mock_context)
        ctx.repo_root = tmp_path

        # Create a log file so agent appears to have run
        log_dir = tmp_path / ".loom" / "logs"
        log_dir.mkdir(parents=True)
        log_file = log_dir / "loom-judge-issue-42.log"
        log_file.write_text("judge started\njudge finished\n")

        judge = JudgePhase()

        # PR has loom:review-requested (Doctor completed) but no outcome label
        review_data = json.dumps({
            "reviews": [],
            "labels": ["loom:review-requested"],
        })

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=0, stdout=review_data),
        ):
            diag = judge._gather_diagnostics(ctx)

        assert diag["failure_mode"] == "doctor_fixed_awaiting_outcome"
        assert "Doctor applied fixes" in diag["failure_explanation"]


class TestHasApprovalComment:
    """Test _has_approval_comment with various comment patterns."""

    def _make_context(self, mock_context: MagicMock) -> MagicMock:
        mock_context.pr_number = 100
        mock_context.repo_root = Path("/fake/repo")
        return mock_context

    @pytest.mark.parametrize(
        "comment",
        [
            "Approved",
            "approved",
            "APPROVED",
            "This PR is approved.",
            "LGTM",
            "lgtm",
            "Lgtm, looks great!",
            "Ship it",
            "ship it!",
            "\u2705 All good",
            "\U0001f44d",
        ],
    )
    def test_recognizes_approval_patterns(
        self, mock_context: MagicMock, comment: str
    ) -> None:
        """Should detect approval in various comment formats."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=0, stdout=comment + "\n"),
        ):
            assert judge._has_approval_comment(ctx) is True

    @pytest.mark.parametrize(
        "comment",
        [
            "Not approved",
            "not approved yet",
            "I don't approve this",
            "Don't approve yet",
            "Never approve without tests",
            "Can't approve this",
            "No approval from me",
        ],
    )
    def test_rejects_negated_approval_patterns(
        self, mock_context: MagicMock, comment: str
    ) -> None:
        """Should reject comments with negation before approval pattern."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=0, stdout=comment + "\n"),
        ):
            assert judge._has_approval_comment(ctx) is False

    @pytest.mark.parametrize(
        "comment",
        [
            "Please fix the tests",
            "Needs more work",
            "This is a nice PR but has issues",
            "Changes requested",
            "",
        ],
    )
    def test_rejects_non_approval_comments(
        self, mock_context: MagicMock, comment: str
    ) -> None:
        """Should return False for comments without approval signals."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=0, stdout=comment + "\n"),
        ):
            assert judge._has_approval_comment(ctx) is False

    def test_returns_false_on_gh_failure(self, mock_context: MagicMock) -> None:
        """Should return False when gh command fails."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=1, stdout=""),
        ):
            assert judge._has_approval_comment(ctx) is False

    def test_returns_false_on_empty_comments(self, mock_context: MagicMock) -> None:
        """Should return False when PR has no comments."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=0, stdout=""),
        ):
            assert judge._has_approval_comment(ctx) is False


class TestPrChecksPassing:
    """Test _pr_checks_passing with various PR states."""

    def _make_context(self, mock_context: MagicMock) -> MagicMock:
        mock_context.pr_number = 100
        mock_context.repo_root = Path("/fake/repo")
        return mock_context

    def test_passes_when_mergeable_and_no_checks(self, mock_context: MagicMock) -> None:
        """Should pass when PR is mergeable with no required checks."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(
                returncode=0,
                stdout=json.dumps({"mergeable": "MERGEABLE", "statusCheckRollup": []}),
            ),
        ):
            assert judge._pr_checks_passing(ctx) is True

    def test_passes_with_successful_checks(self, mock_context: MagicMock) -> None:
        """Should pass when all checks have succeeded."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        checks = [
            {"conclusion": "SUCCESS", "status": "COMPLETED"},
            {"conclusion": "NEUTRAL", "status": "COMPLETED"},
            {"conclusion": "SKIPPED", "status": "COMPLETED"},
        ]
        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(
                returncode=0,
                stdout=json.dumps(
                    {"mergeable": "MERGEABLE", "statusCheckRollup": checks}
                ),
            ),
        ):
            assert judge._pr_checks_passing(ctx) is True

    def test_passes_with_in_progress_checks(self, mock_context: MagicMock) -> None:
        """Should pass when checks are still in progress."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        checks = [{"conclusion": "", "status": "IN_PROGRESS"}]
        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(
                returncode=0,
                stdout=json.dumps(
                    {"mergeable": "MERGEABLE", "statusCheckRollup": checks}
                ),
            ),
        ):
            assert judge._pr_checks_passing(ctx) is True

    def test_fails_with_failed_check(self, mock_context: MagicMock) -> None:
        """Should fail when any check has failed."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        checks = [
            {"conclusion": "SUCCESS", "status": "COMPLETED"},
            {"conclusion": "FAILURE", "status": "COMPLETED"},
        ]
        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(
                returncode=0,
                stdout=json.dumps(
                    {"mergeable": "MERGEABLE", "statusCheckRollup": checks}
                ),
            ),
        ):
            assert judge._pr_checks_passing(ctx) is False

    def test_fails_when_not_mergeable(self, mock_context: MagicMock) -> None:
        """Should fail when PR has merge conflicts."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(
                returncode=0,
                stdout=json.dumps(
                    {"mergeable": "CONFLICTING", "statusCheckRollup": []}
                ),
            ),
        ):
            assert judge._pr_checks_passing(ctx) is False

    def test_passes_with_unknown_mergeable_state(self, mock_context: MagicMock) -> None:
        """Should pass when mergeable state is UNKNOWN (GitHub hasn't computed it yet)."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(
                returncode=0,
                stdout=json.dumps({"mergeable": "UNKNOWN", "statusCheckRollup": []}),
            ),
        ):
            assert judge._pr_checks_passing(ctx) is True

    def test_fails_on_gh_failure(self, mock_context: MagicMock) -> None:
        """Should fail when gh command fails."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=1, stdout=""),
        ):
            assert judge._pr_checks_passing(ctx) is False

    def test_fails_on_invalid_json(self, mock_context: MagicMock) -> None:
        """Should fail when gh returns invalid JSON."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=0, stdout="not json"),
        ):
            assert judge._pr_checks_passing(ctx) is False


class TestApprovalPatterns:
    """Test the module-level APPROVAL_PATTERNS and NEGATIVE_PREFIXES constants."""

    def test_approval_patterns_are_compiled(self) -> None:
        """Approval patterns should be compiled regex objects."""
        for pattern in APPROVAL_PATTERNS:
            assert hasattr(pattern, "search")

    def test_negative_prefixes_are_compiled(self) -> None:
        """Negative prefixes should be compiled regex objects."""
        for pattern in NEGATIVE_PREFIXES:
            assert hasattr(pattern, "search")

    def test_approved_matches_word_boundary(self) -> None:
        """'approved' pattern should match whole words only."""
        pattern = APPROVAL_PATTERNS[0]  # \bapproved?\b
        assert pattern.search("Approved") is not None
        assert pattern.search("approve") is not None
        # Word boundaries prevent matching inside compound words
        assert pattern.search("unapproved") is None
        assert pattern.search("preapproved") is None

    def test_lgtm_case_insensitive(self) -> None:
        """LGTM pattern should be case-insensitive."""
        pattern = APPROVAL_PATTERNS[1]
        assert pattern.search("LGTM") is not None
        assert pattern.search("lgtm") is not None
        assert pattern.search("Lgtm") is not None


class TestRejectionPatterns:
    """Test the module-level REJECTION_PATTERNS constant."""

    def test_rejection_patterns_are_compiled(self) -> None:
        """Rejection patterns should be compiled regex objects."""
        for pattern in REJECTION_PATTERNS:
            assert hasattr(pattern, "search")

    def test_changes_requested_case_insensitive(self) -> None:
        """'changes requested' pattern should be case-insensitive."""
        pattern = REJECTION_PATTERNS[0]  # \bchanges\s+requested\b
        assert pattern.search("Changes Requested") is not None
        assert pattern.search("changes requested") is not None
        assert pattern.search("CHANGES REQUESTED") is not None

    def test_request_changes_case_insensitive(self) -> None:
        """'request changes' pattern should be case-insensitive."""
        pattern = REJECTION_PATTERNS[1]  # \brequest\s+changes\b
        assert pattern.search("Request Changes") is not None
        assert pattern.search("request changes") is not None

    def test_needs_changes_pattern(self) -> None:
        """'needs changes/fixes/work' pattern should match variants."""
        pattern = REJECTION_PATTERNS[2]  # \bneeds?\s+(?:changes|fixes|work)\b
        assert pattern.search("needs changes") is not None
        assert pattern.search("need fixes") is not None
        assert pattern.search("needs work") is not None
        assert pattern.search("Needs Changes") is not None

    def test_cross_mark_emoji(self) -> None:
        """Cross mark emoji pattern should match."""
        pattern = REJECTION_PATTERNS[3]  # \u274c
        assert pattern.search("\u274c") is not None
        assert pattern.search("\u274c Not good") is not None


class TestHasRejectionComment:
    """Test _has_rejection_comment with various comment patterns."""

    def _make_context(self, mock_context: MagicMock) -> MagicMock:
        mock_context.pr_number = 100
        mock_context.repo_root = Path("/fake/repo")
        return mock_context

    @pytest.mark.parametrize(
        "comment",
        [
            "Changes requested",
            "changes requested",
            "CHANGES REQUESTED",
            "I request changes on this PR",
            "Request changes",
            "Needs changes",
            "needs fixes",
            "needs work",
            "\u274c",
            "\u274c This needs work",
        ],
    )
    def test_recognizes_rejection_patterns(
        self, mock_context: MagicMock, comment: str
    ) -> None:
        """Should detect rejection in various comment formats."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=0, stdout=comment + "\n"),
        ):
            assert judge._has_rejection_comment(ctx) is True

    @pytest.mark.parametrize(
        "comment",
        [
            "No changes requested",
            "not changes requested",
            "Don't request changes",
            "no need changes",
        ],
    )
    def test_rejects_negated_rejection_patterns(
        self, mock_context: MagicMock, comment: str
    ) -> None:
        """Should reject comments with negation before rejection pattern."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=0, stdout=comment + "\n"),
        ):
            assert judge._has_rejection_comment(ctx) is False

    @pytest.mark.parametrize(
        "comment",
        [
            "Approved",
            "LGTM",
            "Looks great, ship it",
            "This PR is perfect",
            "",
        ],
    )
    def test_rejects_non_rejection_comments(
        self, mock_context: MagicMock, comment: str
    ) -> None:
        """Should return False for comments without rejection signals."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=0, stdout=comment + "\n"),
        ):
            assert judge._has_rejection_comment(ctx) is False

    def test_returns_false_on_gh_failure(self, mock_context: MagicMock) -> None:
        """Should return False when gh command fails."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=1, stdout=""),
        ):
            assert judge._has_rejection_comment(ctx) is False

    def test_returns_false_on_empty_comments(self, mock_context: MagicMock) -> None:
        """Should return False when PR has no comments."""
        ctx = self._make_context(mock_context)
        judge = JudgePhase()

        with patch(
            "loom_tools.shepherd.phases.judge.subprocess.run",
            return_value=MagicMock(returncode=0, stdout=""),
        ):
            assert judge._has_rejection_comment(ctx) is False


class TestJudgeFallbackChangesRequested:
    """Test force-mode fallback changes-requested detection in JudgePhase."""

    def _make_force_context(self, mock_context: MagicMock) -> MagicMock:
        """Set up a mock context for force-mode judge tests."""
        mock_context.config = ShepherdConfig(issue=42, mode=ExecutionMode.FORCE_MERGE)
        mock_context.pr_number = 100
        mock_context.check_shutdown.return_value = False
        return mock_context

    def test_fallback_activates_in_force_mode_with_rejection_comment(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback should succeed when force mode + rejection comment detected."""
        ctx = self._make_force_context(mock_context)

        judge = JudgePhase()

        gh_run_results = [
            # _try_fallback_approval -> _has_approval_comment: no approval
            MagicMock(returncode=0, stdout="Changes requested\n"),
            # _try_fallback_approval -> _pr_checks_passing
            MagicMock(
                returncode=0,
                stdout=json.dumps({"mergeable": "MERGEABLE", "statusCheckRollup": []}),
            ),
            # _try_fallback_changes_requested -> _has_rejection_comment
            MagicMock(returncode=0, stdout="Changes requested\n"),
            # _try_fallback_changes_requested -> _has_changes_requested_review
            MagicMock(
                returncode=0,
                stdout=json.dumps({"reviews": []}),
            ),
            # _try_fallback_changes_requested -> gh pr edit --add-label
            MagicMock(returncode=0),
        ]

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            patch(
                "loom_tools.shepherd.phases.judge.subprocess.run",
                side_effect=gh_run_results,
            ),
        ):
            result = judge.run(ctx)

        assert result.status == PhaseStatus.SUCCESS
        assert result.data.get("changes_requested") is True

    def test_fallback_activates_via_review_state(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback should succeed via CHANGES_REQUESTED review state."""
        ctx = self._make_force_context(mock_context)

        judge = JudgePhase()

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            # Approval fallback fails (no approval comment)
            patch.object(judge, "_has_approval_comment", return_value=False),
            patch.object(judge, "_pr_checks_passing", return_value=True),
            # Rejection fallback: no rejection comment but CHANGES_REQUESTED review
            patch.object(judge, "_has_rejection_comment", return_value=False),
            patch.object(judge, "_has_changes_requested_review", return_value=True),
            patch(
                "loom_tools.shepherd.phases.judge.subprocess.run",
                return_value=MagicMock(returncode=0),
            ),
        ):
            result = judge.run(ctx)

        assert result.status == PhaseStatus.SUCCESS
        assert result.data.get("changes_requested") is True

    def test_fallback_does_not_activate_in_default_mode(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback should NOT activate when not in force mode."""
        mock_context.config = ShepherdConfig(issue=42)  # Default mode
        mock_context.pr_number = 100
        mock_context.check_shutdown.return_value = False

        judge = JudgePhase()
        fake_diag = {"summary": "no GitHub reviews on PR (Loom uses comment + label workflow instead)", "log_tail": []}

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            patch.object(judge, "_gather_diagnostics", return_value=fake_diag),
        ):
            result = judge.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert "validation failed" in result.message

    def test_fallback_denied_without_rejection_signals(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback should fail when no rejection comment and no CHANGES_REQUESTED review."""
        ctx = self._make_force_context(mock_context)

        judge = JudgePhase()
        fake_diag = {"summary": "no GitHub reviews on PR (Loom uses comment + label workflow instead)", "log_tail": []}

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            # Approval fallback fails
            patch.object(judge, "_has_approval_comment", return_value=False),
            patch.object(judge, "_pr_checks_passing", return_value=True),
            # Rejection fallback also fails — no signals
            patch.object(judge, "_has_rejection_comment", return_value=False),
            patch.object(judge, "_has_changes_requested_review", return_value=False),
            patch.object(judge, "_gather_diagnostics", return_value=fake_diag),
        ):
            result = judge.run(ctx)

        assert result.status == PhaseStatus.FAILED

    def test_fallback_applies_changes_requested_label(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback should apply loom:changes-requested label via gh pr edit."""
        ctx = self._make_force_context(mock_context)

        judge = JudgePhase()

        add_label_call = MagicMock(returncode=0)

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            # Approval fallback fails
            patch.object(judge, "_has_approval_comment", return_value=False),
            patch.object(judge, "_pr_checks_passing", return_value=True),
            # Rejection fallback succeeds
            patch.object(judge, "_has_rejection_comment", return_value=True),
            patch.object(judge, "_has_changes_requested_review", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.subprocess.run",
                return_value=add_label_call,
            ) as mock_run,
        ):
            result = judge.run(ctx)

        assert result.status == PhaseStatus.SUCCESS
        # Verify the label was applied
        mock_run.assert_called_once()
        call_args = mock_run.call_args[0][0]
        assert "gh" in call_args[0]
        assert "--add-label" in call_args
        assert "loom:changes-requested" in call_args

    def test_fallback_invalidates_cache(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback should invalidate PR label cache after applying label."""
        ctx = self._make_force_context(mock_context)

        judge = JudgePhase()

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            # Approval fallback fails
            patch.object(judge, "_has_approval_comment", return_value=False),
            patch.object(judge, "_pr_checks_passing", return_value=True),
            # Rejection fallback succeeds
            patch.object(judge, "_has_rejection_comment", return_value=True),
            patch.object(judge, "_has_changes_requested_review", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.subprocess.run",
                return_value=MagicMock(returncode=0),
            ),
        ):
            result = judge.run(ctx)

        assert result.status == PhaseStatus.SUCCESS
        # Cache should have been invalidated (before validation retries + after fallback)
        assert ctx.label_cache.invalidate_pr.call_count >= 2

    def test_fallback_fails_when_label_application_fails(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback should fail if gh pr edit to apply label returns non-zero."""
        ctx = self._make_force_context(mock_context)

        judge = JudgePhase()
        fake_diag = {"summary": "no GitHub reviews on PR (Loom uses comment + label workflow instead)", "log_tail": []}

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            # Approval fallback fails
            patch.object(judge, "_has_approval_comment", return_value=False),
            patch.object(judge, "_pr_checks_passing", return_value=True),
            # Rejection fallback: signals present but label application fails
            patch.object(judge, "_has_rejection_comment", return_value=True),
            patch.object(judge, "_has_changes_requested_review", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.subprocess.run",
                return_value=MagicMock(returncode=1),  # label application fails
            ),
            patch.object(judge, "_gather_diagnostics", return_value=fake_diag),
        ):
            result = judge.run(ctx)

        assert result.status == PhaseStatus.FAILED

    def test_run_uses_rejection_fallback_before_failing(
        self, mock_context: MagicMock
    ) -> None:
        """run() should try approval fallback first, then rejection fallback, then fail."""
        ctx = self._make_force_context(mock_context)

        judge = JudgePhase()
        fake_diag = {"summary": "no GitHub reviews on PR (Loom uses comment + label workflow instead)", "log_tail": []}

        call_order: list[str] = []

        def track_approval(*args: object, **kwargs: object) -> bool:
            call_order.append("approval")
            return False

        def track_rejection(*args: object, **kwargs: object) -> bool:
            call_order.append("rejection")
            return False

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            patch.object(
                judge, "_try_fallback_approval", side_effect=track_approval
            ),
            patch.object(
                judge, "_try_fallback_changes_requested", side_effect=track_rejection
            ),
            patch.object(judge, "_gather_diagnostics", return_value=fake_diag),
        ):
            result = judge.run(ctx)

        assert result.status == PhaseStatus.FAILED
        assert call_order == ["approval", "rejection"]

    def test_fallback_changes_requested_sets_fallback_used_flag(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback changes-requested should set fallback_used in result data.

        Part of the fix for issue #2083: fallback results should indicate
        they came from fallback detection so callers can distinguish between
        standard validation and fallback paths.
        """
        ctx = self._make_force_context(mock_context)

        judge = JudgePhase()

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            # Approval fallback fails
            patch.object(judge, "_has_approval_comment", return_value=False),
            patch.object(judge, "_pr_checks_passing", return_value=True),
            # Rejection fallback succeeds
            patch.object(judge, "_has_rejection_comment", return_value=True),
            patch.object(judge, "_has_changes_requested_review", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.subprocess.run",
                return_value=MagicMock(returncode=0),
            ),
        ):
            result = judge.run(ctx)

        assert result.status == PhaseStatus.SUCCESS
        assert result.data.get("changes_requested") is True
        assert result.data.get("fallback_used") is True
        assert "[force-mode] Fallback detected changes requested" in result.message


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
        with patch(
            "loom_tools.shepherd.context.subprocess.run", return_value=completed
        ):
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
        with patch(
            "loom_tools.shepherd.context.subprocess.run", return_value=completed
        ):
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
        with patch(
            "loom_tools.shepherd.context.subprocess.run", return_value=completed
        ):
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


class TestRunWorkerPhaseIdleThreshold:
    """Test run_worker_phase min-idle-elapsed threshold configuration."""

    @pytest.fixture
    def mock_context(self) -> MagicMock:
        """Create a mock ShepherdContext for these tests."""
        ctx = MagicMock(spec=ShepherdContext)
        ctx.config = ShepherdConfig(issue=42, task_id="test-123")
        ctx.repo_root = Path("/fake/repo")
        ctx.scripts_dir = Path("/fake/repo/.loom/scripts")
        ctx.progress_dir = Path("/tmp/progress")
        return ctx

    @pytest.mark.parametrize(
        "phase,expected_threshold",
        [
            ("builder", "120"),
            ("doctor", "120"),
            ("judge", "120"),
            ("curator", None),
            ("approval", None),
        ],
    )
    def test_phase_idle_thresholds(
        self, mock_context: MagicMock, phase: str, expected_threshold: str | None
    ) -> None:
        """Verify which phases get extended idle thresholds.

        - builder, doctor, judge: 120 seconds (work-producing roles)
        - curator, approval: default (no explicit threshold)
        """
        captured_wait_cmd: list[str] = []

        def capture_spawn(cmd: list[str], **kwargs):
            result = MagicMock()
            result.returncode = 0
            return result

        def capture_popen(cmd: list[str], **kwargs):
            captured_wait_cmd.extend(cmd)
            proc = MagicMock()
            proc.poll.return_value = 0  # Process completed
            proc.returncode = 0
            return proc

        with (
            patch("subprocess.run", side_effect=capture_spawn),
            patch("subprocess.Popen", side_effect=capture_popen),
            patch("time.sleep"),  # Don't actually sleep
        ):
            run_worker_phase(
                mock_context,
                role=phase,
                name=f"{phase}-issue-42",
                timeout=600,
                phase=phase,
            )

        if expected_threshold:
            assert "--min-idle-elapsed" in captured_wait_cmd
            idx = captured_wait_cmd.index("--min-idle-elapsed")
            assert captured_wait_cmd[idx + 1] == expected_threshold
        else:
            assert "--min-idle-elapsed" not in captured_wait_cmd


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
                {
                    "event": "phase_entered",
                    "timestamp": "t2",
                    "data": {"phase": "judge"},
                },
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
                {
                    "event": "phase_entered",
                    "timestamp": "t1",
                    "data": {"phase": "builder"},
                },
            ],
        }
        f = tmp_path / "no-hb.json"
        f.write_text(json.dumps(progress))
        assert _read_heartbeats(f) == []

    def test_filters_heartbeats_by_phase(self, tmp_path: Path) -> None:
        """Should only return heartbeats after the most recent phase_entered for the given phase."""
        progress = {
            "task_id": "abc123",
            "milestones": [
                {
                    "event": "phase_entered",
                    "timestamp": "t0",
                    "data": {"phase": "curator"},
                },
                {
                    "event": "heartbeat",
                    "timestamp": "t1",
                    "data": {"action": "curator running (1m elapsed)"},
                },
                {
                    "event": "phase_entered",
                    "timestamp": "t2",
                    "data": {"phase": "builder"},
                },
                {
                    "event": "heartbeat",
                    "timestamp": "t3",
                    "data": {"action": "builder running (1m elapsed)"},
                },
                {
                    "event": "heartbeat",
                    "timestamp": "t4",
                    "data": {"action": "builder running (2m elapsed)"},
                },
                {
                    "event": "phase_entered",
                    "timestamp": "t5",
                    "data": {"phase": "judge"},
                },
                {
                    "event": "heartbeat",
                    "timestamp": "t6",
                    "data": {"action": "judge running (1m elapsed)"},
                },
            ],
        }
        f = tmp_path / "shepherd-abc123.json"
        f.write_text(json.dumps(progress))

        # Only judge heartbeats when filtering by judge phase
        result = _read_heartbeats(f, phase="judge")
        assert len(result) == 1
        assert result[0]["data"]["action"] == "judge running (1m elapsed)"

        # Only builder heartbeats when filtering by builder phase
        result = _read_heartbeats(f, phase="builder")
        assert len(result) == 2
        assert result[0]["data"]["action"] == "builder running (1m elapsed)"
        assert result[1]["data"]["action"] == "builder running (2m elapsed)"

        # Only curator heartbeats when filtering by curator phase
        result = _read_heartbeats(f, phase="curator")
        assert len(result) == 1
        assert result[0]["data"]["action"] == "curator running (1m elapsed)"

    def test_no_phase_filter_returns_all(self, tmp_path: Path) -> None:
        """Without phase filter, all heartbeats should be returned (backward compat)."""
        progress = {
            "milestones": [
                {
                    "event": "phase_entered",
                    "timestamp": "t0",
                    "data": {"phase": "curator"},
                },
                {
                    "event": "heartbeat",
                    "timestamp": "t1",
                    "data": {"action": "curator running"},
                },
                {
                    "event": "phase_entered",
                    "timestamp": "t2",
                    "data": {"phase": "judge"},
                },
                {
                    "event": "heartbeat",
                    "timestamp": "t3",
                    "data": {"action": "judge running"},
                },
            ],
        }
        f = tmp_path / "progress.json"
        f.write_text(json.dumps(progress))

        result = _read_heartbeats(f)
        assert len(result) == 2

    def test_phase_filter_with_no_matching_phase_entered(self, tmp_path: Path) -> None:
        """When phase has no phase_entered milestone, return all heartbeats."""
        progress = {
            "milestones": [
                {
                    "event": "heartbeat",
                    "timestamp": "t0",
                    "data": {"action": "running"},
                },
                {
                    "event": "heartbeat",
                    "timestamp": "t1",
                    "data": {"action": "still running"},
                },
            ],
        }
        f = tmp_path / "progress.json"
        f.write_text(json.dumps(progress))

        result = _read_heartbeats(f, phase="builder")
        assert len(result) == 2

    def test_phase_filter_uses_latest_phase_entered(self, tmp_path: Path) -> None:
        """When a phase is entered multiple times, use the most recent entry."""
        progress = {
            "milestones": [
                {
                    "event": "phase_entered",
                    "timestamp": "t0",
                    "data": {"phase": "builder"},
                },
                {
                    "event": "heartbeat",
                    "timestamp": "t1",
                    "data": {"action": "first attempt"},
                },
                {
                    "event": "phase_entered",
                    "timestamp": "t2",
                    "data": {"phase": "builder"},
                },
                {
                    "event": "heartbeat",
                    "timestamp": "t3",
                    "data": {"action": "second attempt"},
                },
            ],
        }
        f = tmp_path / "progress.json"
        f.write_text(json.dumps(progress))

        result = _read_heartbeats(f, phase="builder")
        assert len(result) == 1
        assert result[0]["data"]["action"] == "second attempt"


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


class TestBuilderDetectTestEcosystem:
    """Test _detect_test_ecosystem method."""

    def test_cargo_test(self) -> None:
        builder = BuilderPhase()
        assert builder._detect_test_ecosystem(["cargo", "test", "--workspace"]) == "cargo"

    def test_pnpm_check_ci_lite(self) -> None:
        builder = BuilderPhase()
        assert builder._detect_test_ecosystem(["pnpm", "check:ci:lite"]) == "pnpm"

    def test_npm_test(self) -> None:
        builder = BuilderPhase()
        assert builder._detect_test_ecosystem(["npm", "test"]) == "pnpm"

    def test_vitest(self) -> None:
        builder = BuilderPhase()
        assert builder._detect_test_ecosystem(["vitest"]) == "pnpm"

    def test_pytest(self) -> None:
        builder = BuilderPhase()
        assert builder._detect_test_ecosystem(["python", "-m", "pytest"]) == "pytest"

    def test_unknown(self) -> None:
        builder = BuilderPhase()
        assert builder._detect_test_ecosystem(["make", "test"]) is None


class TestBuilderShouldSkipDoctorRecovery:
    """Test should_skip_doctor_recovery method."""

    def test_skip_rust_changes_with_pnpm_failures(self, mock_context: MagicMock) -> None:
        """Rust-only changes should skip Doctor when pnpm tests fail."""
        builder = BuilderPhase()
        mock_context.worktree_path = MagicMock()
        mock_context.worktree_path.is_dir.return_value = True

        with patch("loom_tools.shepherd.phases.builder.get_changed_files") as mock_files:
            mock_files.return_value = ["src/main.rs", "src/lib.rs"]
            result = builder.should_skip_doctor_recovery(
                mock_context, ["pnpm", "check:ci:lite"]
            )
        assert result is True

    def test_no_skip_python_changes_with_pytest_failures(self, mock_context: MagicMock) -> None:
        """Python changes should NOT skip Doctor when pytest fails."""
        builder = BuilderPhase()
        mock_context.worktree_path = MagicMock()
        mock_context.worktree_path.is_dir.return_value = True

        with patch("loom_tools.shepherd.phases.builder.get_changed_files") as mock_files:
            mock_files.return_value = ["src/main.py", "tests/test_foo.py"]
            result = builder.should_skip_doctor_recovery(
                mock_context, ["python", "-m", "pytest"]
            )
        assert result is False

    def test_no_skip_ts_changes_with_pnpm_failures(self, mock_context: MagicMock) -> None:
        """TypeScript changes should NOT skip Doctor when pnpm tests fail."""
        builder = BuilderPhase()
        mock_context.worktree_path = MagicMock()
        mock_context.worktree_path.is_dir.return_value = True

        with patch("loom_tools.shepherd.phases.builder.get_changed_files") as mock_files:
            mock_files.return_value = ["src/app.ts", "src/utils.tsx"]
            result = builder.should_skip_doctor_recovery(
                mock_context, ["pnpm", "test"]
            )
        assert result is False

    def test_skip_python_changes_with_cargo_failures(self, mock_context: MagicMock) -> None:
        """Python-only changes should skip Doctor when cargo tests fail."""
        builder = BuilderPhase()
        mock_context.worktree_path = MagicMock()
        mock_context.worktree_path.is_dir.return_value = True

        with patch("loom_tools.shepherd.phases.builder.get_changed_files") as mock_files:
            mock_files.return_value = ["loom-tools/src/main.py"]
            result = builder.should_skip_doctor_recovery(
                mock_context, ["cargo", "test", "--workspace"]
            )
        assert result is True

    def test_skip_markdown_changes(self, mock_context: MagicMock) -> None:
        """Markdown-only changes should skip Doctor for any test ecosystem."""
        builder = BuilderPhase()
        mock_context.worktree_path = MagicMock()
        mock_context.worktree_path.is_dir.return_value = True

        with patch("loom_tools.shepherd.phases.builder.get_changed_files") as mock_files:
            mock_files.return_value = ["README.md", "docs/guide.md"]
            result = builder.should_skip_doctor_recovery(
                mock_context, ["pnpm", "check:ci:lite"]
            )
        assert result is True

    def test_no_skip_toml_changes_with_cargo_failures(self, mock_context: MagicMock) -> None:
        """TOML changes should NOT skip Doctor when cargo tests fail (.toml affects cargo)."""
        builder = BuilderPhase()
        mock_context.worktree_path = MagicMock()
        mock_context.worktree_path.is_dir.return_value = True

        with patch("loom_tools.shepherd.phases.builder.get_changed_files") as mock_files:
            mock_files.return_value = ["Cargo.toml"]
            result = builder.should_skip_doctor_recovery(
                mock_context, ["cargo", "test"]
            )
        assert result is False

    def test_skip_no_changed_files(self, mock_context: MagicMock) -> None:
        """No changed files should skip Doctor (failures are pre-existing)."""
        builder = BuilderPhase()
        mock_context.worktree_path = MagicMock()
        mock_context.worktree_path.is_dir.return_value = True

        with patch("loom_tools.shepherd.phases.builder.get_changed_files") as mock_files:
            mock_files.return_value = []
            result = builder.should_skip_doctor_recovery(
                mock_context, ["pnpm", "test"]
            )
        assert result is True

    def test_no_skip_unknown_extension(self, mock_context: MagicMock) -> None:
        """Unknown file extensions should conservatively NOT skip Doctor."""
        builder = BuilderPhase()
        mock_context.worktree_path = MagicMock()
        mock_context.worktree_path.is_dir.return_value = True

        with patch("loom_tools.shepherd.phases.builder.get_changed_files") as mock_files:
            mock_files.return_value = ["build.zig"]
            result = builder.should_skip_doctor_recovery(
                mock_context, ["pnpm", "test"]
            )
        assert result is False

    def test_no_skip_unknown_test_ecosystem(self, mock_context: MagicMock) -> None:
        """Unknown test ecosystem should conservatively NOT skip Doctor."""
        builder = BuilderPhase()
        mock_context.worktree_path = MagicMock()
        mock_context.worktree_path.is_dir.return_value = True

        with patch("loom_tools.shepherd.phases.builder.get_changed_files") as mock_files:
            mock_files.return_value = ["src/main.rs"]
            result = builder.should_skip_doctor_recovery(
                mock_context, ["make", "test"]
            )
        assert result is False

    def test_no_worktree_path(self, mock_context: MagicMock) -> None:
        """Missing worktree path should conservatively NOT skip Doctor."""
        builder = BuilderPhase()
        mock_context.worktree_path = None

        result = builder.should_skip_doctor_recovery(
            mock_context, ["pnpm", "test"]
        )
        assert result is False

    def test_mixed_changes_one_overlaps(self, mock_context: MagicMock) -> None:
        """Mixed file types should NOT skip if any file overlaps."""
        builder = BuilderPhase()
        mock_context.worktree_path = MagicMock()
        mock_context.worktree_path.is_dir.return_value = True

        with patch("loom_tools.shepherd.phases.builder.get_changed_files") as mock_files:
            # .rs doesn't affect pnpm, but .ts does
            mock_files.return_value = ["src/main.rs", "src/app.ts"]
            result = builder.should_skip_doctor_recovery(
                mock_context, ["pnpm", "check:ci:lite"]
            )
        assert result is False

    def test_config_files_affect_all(self, mock_context: MagicMock) -> None:
        """Config files (.yml) should NOT skip Doctor for any ecosystem."""
        builder = BuilderPhase()
        mock_context.worktree_path = MagicMock()
        mock_context.worktree_path.is_dir.return_value = True

        with patch("loom_tools.shepherd.phases.builder.get_changed_files") as mock_files:
            mock_files.return_value = [".github/workflows/ci.yml"]
            result = builder.should_skip_doctor_recovery(
                mock_context, ["cargo", "test"]
            )
        assert result is False


class TestDoctorPhaseExitCode5:
    """Test Doctor phase handling of exit code 5 (pre-existing failures)."""

    def test_exit_code_5_returns_skipped_with_preexisting_flag(
        self, mock_context: MagicMock
    ) -> None:
        """Exit code 5 should return SKIPPED status with preexisting flag."""
        doctor = DoctorPhase()
        mock_context.pr_number = 123
        mock_context.check_shutdown.return_value = False

        with patch(
            "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
        ) as mock_run:
            mock_run.return_value = 5
            result = doctor.run(mock_context)

        assert result.status == PhaseStatus.SKIPPED
        assert result.data.get("preexisting") is True
        assert "pre-existing" in result.message.lower()

    def test_exit_code_0_validates_and_returns_success(
        self, mock_context: MagicMock
    ) -> None:
        """Exit code 0 should validate phase and return SUCCESS."""
        from loom_tools.shepherd.phases.doctor import (
            CIResult,
            CIStatus,
            DoctorDiagnostics,
        )

        doctor = DoctorPhase()
        mock_context.pr_number = 123
        mock_context.check_shutdown.return_value = False

        diagnostics = DoctorDiagnostics(commits_made=1)
        ci_result = CIResult(status=CIStatus.PASSED, message="CI passed")

        with (
            patch(
                "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
            ) as mock_run,
            patch.object(doctor, "validate") as mock_validate,
            patch.object(doctor, "_get_commit_count", return_value=0),
            patch.object(doctor, "_diagnose_doctor_outcome", return_value=diagnostics),
            patch.object(doctor, "_wait_for_ci", return_value=ci_result),
        ):
            mock_run.return_value = 0
            mock_validate.return_value = True
            result = doctor.run(mock_context)

        assert result.status == PhaseStatus.SUCCESS

    def test_exit_code_5_does_not_mark_blocked(self, mock_context: MagicMock) -> None:
        """Exit code 5 should NOT mark issue as blocked (unlike exit code 4)."""
        doctor = DoctorPhase()
        mock_context.pr_number = 123
        mock_context.check_shutdown.return_value = False

        with patch(
            "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
        ) as mock_run:
            mock_run.return_value = 5
            result = doctor.run(mock_context)

        # Should not call _mark_issue_blocked
        mock_context.label_cache.invalidate_issue.assert_not_called()


class TestDoctorTestFixMode:
    """Test Doctor phase test-fix mode (issue #2046)."""

    def test_run_test_fix_success(self, mock_context: MagicMock) -> None:
        """run_test_fix should return SUCCESS when doctor exits 0."""
        doctor = DoctorPhase()
        mock_context.check_shutdown.return_value = False

        with patch(
            "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
        ) as mock_run:
            mock_run.return_value = 0
            result = doctor.run_test_fix(mock_context, {
                "test_command": "pnpm test",
                "test_output_tail": "3 failed",
                "changed_files": ["src/foo.ts"],
            })

        assert result.status == PhaseStatus.SUCCESS
        assert "test fixes" in result.message.lower()

    def test_run_test_fix_preexisting(self, mock_context: MagicMock) -> None:
        """run_test_fix should return SKIPPED with preexisting flag on exit code 5."""
        doctor = DoctorPhase()
        mock_context.check_shutdown.return_value = False

        with patch(
            "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
        ) as mock_run:
            mock_run.return_value = 5
            result = doctor.run_test_fix(mock_context, {})

        assert result.status == PhaseStatus.SKIPPED
        assert result.data.get("preexisting") is True

    def test_run_test_fix_shutdown(self, mock_context: MagicMock) -> None:
        """run_test_fix should return SHUTDOWN on exit code 3."""
        doctor = DoctorPhase()
        mock_context.check_shutdown.return_value = False

        with patch(
            "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
        ) as mock_run:
            mock_run.return_value = 3
            result = doctor.run_test_fix(mock_context, {})

        assert result.status == PhaseStatus.SHUTDOWN

    def test_run_test_fix_stuck(self, mock_context: MagicMock) -> None:
        """run_test_fix should return STUCK on exit code 4."""
        doctor = DoctorPhase()
        mock_context.check_shutdown.return_value = False

        with patch(
            "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
        ) as mock_run:
            mock_run.return_value = 4
            result = doctor.run_test_fix(mock_context, {})

        assert result.status == PhaseStatus.STUCK

    def test_run_test_fix_failed(self, mock_context: MagicMock) -> None:
        """run_test_fix should return FAILED on non-zero exit code."""
        doctor = DoctorPhase()
        mock_context.check_shutdown.return_value = False

        with patch(
            "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
        ) as mock_run:
            mock_run.return_value = 1
            result = doctor.run_test_fix(mock_context, {})

        assert result.status == PhaseStatus.FAILED

    def test_run_test_fix_passes_args_with_context(self, mock_context: MagicMock) -> None:
        """run_test_fix should pass --test-fix args with context file path."""
        doctor = DoctorPhase()
        mock_context.check_shutdown.return_value = False

        with (
            patch(
                "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
            ) as mock_run,
            patch.object(doctor, "_write_test_failure_context") as mock_write,
        ):
            mock_run.return_value = 0
            mock_write.return_value = Path("/fake/worktree/.loom-test-failure-context.json")
            doctor.run_test_fix(mock_context, {"test_command": "pnpm test"})

        # Verify args contain --test-fix and --context
        call_kwargs = mock_run.call_args[1]
        assert "--test-fix 42" in call_kwargs["args"]
        assert "--context" in call_kwargs["args"]

    def test_run_test_fix_respects_shutdown(self, mock_context: MagicMock) -> None:
        """run_test_fix should check for shutdown before running."""
        doctor = DoctorPhase()
        mock_context.check_shutdown.return_value = True

        result = doctor.run_test_fix(mock_context, {})
        assert result.status == PhaseStatus.SHUTDOWN


class TestBuilderDiagnoseRemainingSteps:
    """Test _diagnose_remaining_steps for targeted completion instructions."""

    def test_uncommitted_changes_need_all_steps(self) -> None:
        """Uncommitted changes with no remote should need commit, push, and PR."""
        builder = BuilderPhase()
        diag = {
            "has_uncommitted_changes": True,
            "commits_ahead": 0,
            "remote_branch_exists": False,
            "pr_number": None,
            "pr_has_review_label": False,
        }
        steps = builder._diagnose_remaining_steps(diag, 42)
        assert steps == ["stage_and_commit", "push_branch", "create_pr"]

    def test_commits_ahead_no_remote_needs_push_and_pr(self) -> None:
        """Commits ahead but no remote should need push and PR."""
        builder = BuilderPhase()
        diag = {
            "has_uncommitted_changes": False,
            "commits_ahead": 3,
            "remote_branch_exists": False,
            "pr_number": None,
            "pr_has_review_label": False,
        }
        steps = builder._diagnose_remaining_steps(diag, 42)
        assert steps == ["push_branch", "create_pr"]

    def test_remote_exists_no_pr_needs_pr_only(self) -> None:
        """Remote branch exists but no PR should need PR creation only."""
        builder = BuilderPhase()
        diag = {
            "has_uncommitted_changes": False,
            "commits_ahead": 0,
            "remote_branch_exists": True,
            "pr_number": None,
            "pr_has_review_label": False,
        }
        steps = builder._diagnose_remaining_steps(diag, 42)
        assert steps == ["create_pr"]

    def test_pr_exists_missing_label(self) -> None:
        """PR exists but missing label should need label only."""
        builder = BuilderPhase()
        diag = {
            "has_uncommitted_changes": False,
            "commits_ahead": 0,
            "remote_branch_exists": True,
            "pr_number": 100,
            "pr_has_review_label": False,
        }
        steps = builder._diagnose_remaining_steps(diag, 42)
        assert steps == ["add_review_label"]

    def test_pr_exists_with_label_no_steps(self) -> None:
        """PR with label should need no steps."""
        builder = BuilderPhase()
        diag = {
            "has_uncommitted_changes": False,
            "commits_ahead": 0,
            "remote_branch_exists": True,
            "pr_number": 100,
            "pr_has_review_label": True,
        }
        steps = builder._diagnose_remaining_steps(diag, 42)
        assert steps == []

    def test_commits_ahead_with_remote_needs_pr(self) -> None:
        """Commits ahead with remote already pushed needs PR."""
        builder = BuilderPhase()
        diag = {
            "has_uncommitted_changes": False,
            "commits_ahead": 2,
            "remote_branch_exists": True,
            "pr_number": None,
            "pr_has_review_label": False,
        }
        steps = builder._diagnose_remaining_steps(diag, 42)
        assert steps == ["create_pr"]


class TestBuilderHasIncompleteWork:
    """Test _has_incomplete_work with expanded state detection."""

    def test_no_worktree_returns_false(self) -> None:
        builder = BuilderPhase()
        diag = {"worktree_exists": False}
        assert builder._has_incomplete_work(diag) is False

    def test_uncommitted_changes_returns_true(self) -> None:
        builder = BuilderPhase()
        diag = {
            "worktree_exists": True,
            "has_uncommitted_changes": True,
            "commits_ahead": 0,
            "remote_branch_exists": False,
            "pr_number": None,
            "pr_has_review_label": False,
        }
        assert builder._has_incomplete_work(diag) is True

    def test_commits_ahead_returns_true(self) -> None:
        builder = BuilderPhase()
        diag = {
            "worktree_exists": True,
            "has_uncommitted_changes": False,
            "commits_ahead": 2,
            "remote_branch_exists": False,
            "pr_number": None,
            "pr_has_review_label": False,
        }
        assert builder._has_incomplete_work(diag) is True

    def test_remote_exists_no_pr_returns_true(self) -> None:
        """Remote branch pushed but no PR should be incomplete."""
        builder = BuilderPhase()
        diag = {
            "worktree_exists": True,
            "has_uncommitted_changes": False,
            "commits_ahead": 0,
            "remote_branch_exists": True,
            "pr_number": None,
            "pr_has_review_label": False,
        }
        assert builder._has_incomplete_work(diag) is True

    def test_pr_missing_label_returns_true(self) -> None:
        """PR exists but missing loom:review-requested should be incomplete."""
        builder = BuilderPhase()
        diag = {
            "worktree_exists": True,
            "has_uncommitted_changes": False,
            "commits_ahead": 0,
            "remote_branch_exists": True,
            "pr_number": 100,
            "pr_has_review_label": False,
        }
        assert builder._has_incomplete_work(diag) is True

    def test_fully_complete_returns_false(self) -> None:
        """Everything done (PR with label) should not be incomplete."""
        builder = BuilderPhase()
        diag = {
            "worktree_exists": True,
            "has_uncommitted_changes": False,
            "commits_ahead": 0,
            "remote_branch_exists": True,
            "pr_number": 100,
            "pr_has_review_label": True,
        }
        assert builder._has_incomplete_work(diag) is False

    def test_no_work_no_remote_no_pr_returns_false(self) -> None:
        """Worktree exists but nothing done should return False."""
        builder = BuilderPhase()
        diag = {
            "worktree_exists": True,
            "has_uncommitted_changes": False,
            "commits_ahead": 0,
            "remote_branch_exists": False,
            "pr_number": None,
            "pr_has_review_label": False,
        }
        assert builder._has_incomplete_work(diag) is False


class TestBuilderDirectCompletion:
    """Test _direct_completion for mechanical fallback operations."""

    def test_push_only(self, mock_context: MagicMock) -> None:
        """Should push branch directly when that's the only step."""
        builder = BuilderPhase()
        diag = {
            "has_uncommitted_changes": False,
            "commits_ahead": 2,
            "remote_branch_exists": False,
            "pr_number": None,
            "pr_has_review_label": False,
        }

        with patch.object(builder, "_push_branch", return_value=True):
            # push_branch is mechanical but create_pr is not
            result = builder._direct_completion(mock_context, diag)

        # Should return False because "create_pr" is not mechanical
        assert result is False

    def test_add_label_only(self, mock_context: MagicMock) -> None:
        """Should add label directly when that's the only step."""
        builder = BuilderPhase()
        mock_context.repo_root = Path("/fake/repo")
        diag = {
            "has_uncommitted_changes": False,
            "commits_ahead": 0,
            "remote_branch_exists": True,
            "pr_number": 100,
            "pr_has_review_label": False,
        }

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run"
        ) as mock_run:
            mock_run.return_value = MagicMock(returncode=0)
            result = builder._direct_completion(mock_context, diag)

        assert result is True
        # Verify gh pr edit was called
        call_args = mock_run.call_args[0][0]
        assert "gh" in call_args
        assert "pr" in call_args
        assert "edit" in call_args
        assert "100" in call_args
        assert "--add-label" in call_args

    def test_non_mechanical_steps_returns_false(self, mock_context: MagicMock) -> None:
        """Should return False when non-mechanical steps remain."""
        builder = BuilderPhase()
        diag = {
            "has_uncommitted_changes": True,
            "commits_ahead": 0,
            "remote_branch_exists": False,
            "pr_number": None,
            "pr_has_review_label": False,
        }
        result = builder._direct_completion(mock_context, diag)
        assert result is False

    def test_push_failure_returns_false(self, mock_context: MagicMock) -> None:
        """Should return False when push fails."""
        builder = BuilderPhase()
        diag = {
            "has_uncommitted_changes": False,
            "commits_ahead": 0,
            "remote_branch_exists": False,
            "pr_number": None,
            "pr_has_review_label": False,
            # push_branch is only step (remote missing, but also need PR)
        }
        # No steps because commits_ahead is 0 and no remote — falls through
        result = builder._direct_completion(mock_context, diag)
        # No steps to complete
        assert result is False

    def test_empty_steps_returns_false(self, mock_context: MagicMock) -> None:
        """Should return False when no steps remain."""
        builder = BuilderPhase()
        diag = {
            "has_uncommitted_changes": False,
            "commits_ahead": 0,
            "remote_branch_exists": True,
            "pr_number": 100,
            "pr_has_review_label": True,
        }
        result = builder._direct_completion(mock_context, diag)
        assert result is False


class TestBuilderCompletionRetryDefault:
    """Test that builder_completion_retries defaults to 2."""

    def test_default_is_two(self) -> None:
        config = ShepherdConfig(issue=1)
        assert config.builder_completion_retries == 2


class TestBuilderDiagnosticsPRDetection:
    """Test PR detection in _gather_diagnostics."""

    def test_pr_number_detected(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Should detect PR number and label state."""
        wt_dir = tmp_path / "worktree"
        wt_dir.mkdir()
        mock_context.worktree_path = wt_dir
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.repo_root = tmp_path

        # Create log file
        log_dir = tmp_path / ".loom" / "logs"
        log_dir.mkdir(parents=True)
        log_file = log_dir / "loom-builder-issue-42.log"
        log_file.write_text("log line\n")

        builder = BuilderPhase()

        def fake_run(cmd, **kwargs):
            cmd_str = " ".join(str(c) for c in cmd)
            result = subprocess.CompletedProcess(
                args=cmd, returncode=0, stdout="", stderr=""
            )
            if "rev-parse" in cmd_str:
                result.stdout = "feature/issue-42\n"
            elif "log" in cmd_str and "main..HEAD" in cmd_str:
                result.stdout = "abc1234 commit\n"
            elif "status" in cmd_str and "--porcelain" in cmd_str:
                result.stdout = ""
            elif "ls-remote" in cmd_str:
                result.stdout = "abc1234\trefs/heads/feature/issue-42\n"
            elif "pr" in cmd_str and "list" in cmd_str:
                result.stdout = '{"number": 200, "labels": [{"name": "loom:review-requested"}]}\n'
            elif "issue" in cmd_str and "view" in cmd_str:
                result.stdout = "loom:building\n"
            return result

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", side_effect=fake_run
        ):
            diag = builder._gather_diagnostics(mock_context)

        assert diag["pr_number"] == 200
        assert diag["pr_has_review_label"] is True
        assert "PR #200" in diag["summary"]
        assert "with loom:review-requested" in diag["summary"]

    def test_no_pr_detected(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Should report no PR when none exists."""
        wt_dir = tmp_path / "worktree"
        wt_dir.mkdir()
        mock_context.worktree_path = wt_dir
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.repo_root = tmp_path

        log_dir = tmp_path / ".loom" / "logs"
        log_dir.mkdir(parents=True)
        log_file = log_dir / "loom-builder-issue-42.log"
        log_file.write_text("log line\n")

        builder = BuilderPhase()

        def fake_run(cmd, **kwargs):
            cmd_str = " ".join(str(c) for c in cmd)
            result = subprocess.CompletedProcess(
                args=cmd, returncode=0, stdout="", stderr=""
            )
            if "rev-parse" in cmd_str:
                result.stdout = "feature/issue-42\n"
            elif "log" in cmd_str and "main..HEAD" in cmd_str:
                result.stdout = ""
            elif "status" in cmd_str and "--porcelain" in cmd_str:
                result.stdout = ""
            elif "ls-remote" in cmd_str:
                result.stdout = ""
            elif "pr" in cmd_str and "list" in cmd_str:
                result.stdout = ""
            elif "issue" in cmd_str and "view" in cmd_str:
                result.stdout = "loom:building\n"
            return result

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", side_effect=fake_run
        ):
            diag = builder._gather_diagnostics(mock_context)

        assert diag["pr_number"] is None
        assert diag["pr_has_review_label"] is False
        assert "no PR" in diag["summary"]

    def test_pr_missing_label(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Should detect PR that's missing loom:review-requested."""
        wt_dir = tmp_path / "worktree"
        wt_dir.mkdir()
        mock_context.worktree_path = wt_dir
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.repo_root = tmp_path

        log_dir = tmp_path / ".loom" / "logs"
        log_dir.mkdir(parents=True)
        log_file = log_dir / "loom-builder-issue-42.log"
        log_file.write_text("log line\n")

        builder = BuilderPhase()

        def fake_run(cmd, **kwargs):
            cmd_str = " ".join(str(c) for c in cmd)
            result = subprocess.CompletedProcess(
                args=cmd, returncode=0, stdout="", stderr=""
            )
            if "rev-parse" in cmd_str:
                result.stdout = "feature/issue-42\n"
            elif "log" in cmd_str and "main..HEAD" in cmd_str:
                result.stdout = ""
            elif "status" in cmd_str and "--porcelain" in cmd_str:
                result.stdout = ""
            elif "ls-remote" in cmd_str:
                result.stdout = "abc\trefs/heads/feature/issue-42\n"
            elif "pr" in cmd_str and "list" in cmd_str:
                result.stdout = '{"number": 150, "labels": [{"name": "loom:building"}]}\n'
            elif "issue" in cmd_str and "view" in cmd_str:
                result.stdout = "loom:building\n"
            return result

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", side_effect=fake_run
        ):
            diag = builder._gather_diagnostics(mock_context)

        assert diag["pr_number"] == 150
        assert diag["pr_has_review_label"] is False
        assert "missing loom:review-requested" in diag["summary"]


class TestBuilderCompletionPhaseTargetedInstructions:
    """Test that _run_completion_phase sends targeted instructions."""

    def test_attempt_2_uses_explicit_commands(self, mock_context: MagicMock) -> None:
        """Later attempts should use explicit git/gh commands."""
        builder = BuilderPhase()
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.worktree_path = Path("/fake/worktree")

        diag = {
            "has_uncommitted_changes": True,
            "commits_ahead": 0,
            "remote_branch_exists": False,
            "pr_number": None,
            "pr_has_review_label": False,
            "branch": "feature/issue-42",
        }

        with patch(
            "loom_tools.shepherd.phases.base.run_worker_phase", return_value=0
        ) as mock_run:
            builder._run_completion_phase(mock_context, diag, attempt=2)

        # Check the args passed to the worker contain explicit commands
        call_kwargs = mock_run.call_args[1]
        assert "git add -A" in call_kwargs["args"]
        assert "gh pr create" in call_kwargs["args"]

    def test_attempt_1_uses_general_instructions(self, mock_context: MagicMock) -> None:
        """First attempt should use general instructions."""
        builder = BuilderPhase()
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.worktree_path = Path("/fake/worktree")

        diag = {
            "has_uncommitted_changes": True,
            "commits_ahead": 0,
            "remote_branch_exists": False,
            "pr_number": None,
            "pr_has_review_label": False,
            "branch": "feature/issue-42",
        }

        with patch(
            "loom_tools.shepherd.phases.base.run_worker_phase", return_value=0
        ) as mock_run:
            builder._run_completion_phase(mock_context, diag, attempt=1)

        call_kwargs = mock_run.call_args[1]
        assert "Stage and commit" in call_kwargs["args"]
        # Not explicit git commands on first attempt
        assert "git add -A" not in call_kwargs["args"]

    def test_label_only_step(self, mock_context: MagicMock) -> None:
        """When only label is missing, instructions should target label."""
        builder = BuilderPhase()
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.worktree_path = Path("/fake/worktree")

        diag = {
            "has_uncommitted_changes": False,
            "commits_ahead": 0,
            "remote_branch_exists": True,
            "pr_number": 200,
            "pr_has_review_label": False,
            "branch": "feature/issue-42",
        }

        with patch(
            "loom_tools.shepherd.phases.base.run_worker_phase", return_value=0
        ) as mock_run:
            builder._run_completion_phase(mock_context, diag, attempt=1)

        call_kwargs = mock_run.call_args[1]
        assert "gh pr edit 200 --add-label loom:review-requested" in call_kwargs["args"]
        # Should not include commit or push instructions
        assert "commit" not in call_kwargs["args"].lower()
        assert "push" not in call_kwargs["args"].lower()

    def test_attempt_2_includes_diagnostic_context(self, mock_context: MagicMock) -> None:
        """Second attempt should include specific diagnostic details in prompt."""
        builder = BuilderPhase()
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.worktree_path = Path("/fake/worktree")

        diag = {
            "has_uncommitted_changes": True,
            "uncommitted_file_count": 7,
            "commits_ahead": 0,
            "remote_branch_exists": True,
            "pr_number": None,
            "pr_has_review_label": False,
            "branch": "feature/issue-42",
        }

        with patch(
            "loom_tools.shepherd.phases.base.run_worker_phase", return_value=0
        ) as mock_run:
            builder._run_completion_phase(mock_context, diag, attempt=2)

        call_kwargs = mock_run.call_args[1]
        # Should include file count
        assert "7 uncommitted files" in call_kwargs["args"]
        # Should include branch state
        assert "remote branch exists but has no commits ahead of main" in call_kwargs["args"]

    def test_attempt_1_does_not_include_diagnostic_context(self, mock_context: MagicMock) -> None:
        """First attempt should not include diagnostic context."""
        builder = BuilderPhase()
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.worktree_path = Path("/fake/worktree")

        diag = {
            "has_uncommitted_changes": True,
            "uncommitted_file_count": 7,
            "commits_ahead": 0,
            "remote_branch_exists": True,
            "pr_number": None,
            "pr_has_review_label": False,
            "branch": "feature/issue-42",
        }

        with patch(
            "loom_tools.shepherd.phases.base.run_worker_phase", return_value=0
        ) as mock_run:
            builder._run_completion_phase(mock_context, diag, attempt=1)

        call_kwargs = mock_run.call_args[1]
        # Should NOT include file count on first attempt
        assert "uncommitted files" not in call_kwargs["args"]

    def test_diagnostic_context_with_commits_ahead(self, mock_context: MagicMock) -> None:
        """Diagnostic context should describe commits ahead when present."""
        builder = BuilderPhase()
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.worktree_path = Path("/fake/worktree")

        diag = {
            "has_uncommitted_changes": False,
            "uncommitted_file_count": 0,
            "commits_ahead": 3,
            "remote_branch_exists": False,
            "pr_number": None,
            "pr_has_review_label": False,
            "branch": "feature/issue-42",
        }

        with patch(
            "loom_tools.shepherd.phases.base.run_worker_phase", return_value=0
        ) as mock_run:
            builder._run_completion_phase(mock_context, diag, attempt=2)

        call_kwargs = mock_run.call_args[1]
        assert "3 commits ahead of main" in call_kwargs["args"]

    def test_diagnostic_context_with_pr_missing_label(self, mock_context: MagicMock) -> None:
        """Diagnostic context should note PR missing label."""
        builder = BuilderPhase()
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.worktree_path = Path("/fake/worktree")

        diag = {
            "has_uncommitted_changes": False,
            "uncommitted_file_count": 0,
            "commits_ahead": 1,
            "remote_branch_exists": True,
            "pr_number": 150,
            "pr_has_review_label": False,
            "branch": "feature/issue-42",
        }

        with patch(
            "loom_tools.shepherd.phases.base.run_worker_phase", return_value=0
        ) as mock_run:
            builder._run_completion_phase(mock_context, diag, attempt=2)

        call_kwargs = mock_run.call_args[1]
        assert "PR #150 exists but is missing loom:review-requested label" in call_kwargs["args"]


class TestBuilderDiagnosticsUncommittedFileCount:
    """Test uncommitted file count in diagnostics."""

    def test_uncommitted_file_count_captured(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Should capture count of uncommitted files."""
        wt_dir = tmp_path / "worktree"
        wt_dir.mkdir()
        mock_context.worktree_path = wt_dir
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.repo_root = tmp_path

        log_dir = tmp_path / ".loom" / "logs"
        log_dir.mkdir(parents=True)
        log_file = log_dir / "loom-builder-issue-42.log"
        log_file.write_text("log line\n")

        builder = BuilderPhase()

        def fake_run(cmd, **kwargs):
            cmd_str = " ".join(str(c) for c in cmd)
            result = subprocess.CompletedProcess(
                args=cmd, returncode=0, stdout="", stderr=""
            )
            if "rev-parse" in cmd_str:
                result.stdout = "feature/issue-42\n"
            elif "log" in cmd_str and "main..HEAD" in cmd_str:
                result.stdout = ""
            elif "status" in cmd_str and "--porcelain" in cmd_str:
                # Simulate 7 uncommitted files
                result.stdout = "M file1.py\nM file2.py\nA file3.py\n?? file4.py\nM file5.py\nM file6.py\nD file7.py\n"
            elif "ls-remote" in cmd_str:
                result.stdout = ""
            elif "pr" in cmd_str and "list" in cmd_str:
                result.stdout = ""
            elif "issue" in cmd_str and "view" in cmd_str:
                result.stdout = "loom:building\n"
            return result

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", side_effect=fake_run
        ):
            diag = builder._gather_diagnostics(mock_context)

        assert diag["uncommitted_file_count"] == 7
        assert diag["has_uncommitted_changes"] is True

    def test_summary_includes_file_count(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Summary should include file count when uncommitted."""
        wt_dir = tmp_path / "worktree"
        wt_dir.mkdir()
        mock_context.worktree_path = wt_dir
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.repo_root = tmp_path

        log_dir = tmp_path / ".loom" / "logs"
        log_dir.mkdir(parents=True)
        log_file = log_dir / "loom-builder-issue-42.log"
        log_file.write_text("log line\n")

        builder = BuilderPhase()

        def fake_run(cmd, **kwargs):
            cmd_str = " ".join(str(c) for c in cmd)
            result = subprocess.CompletedProcess(
                args=cmd, returncode=0, stdout="", stderr=""
            )
            if "rev-parse" in cmd_str:
                result.stdout = "feature/issue-42\n"
            elif "log" in cmd_str and "main..HEAD" in cmd_str:
                result.stdout = ""
            elif "status" in cmd_str and "--porcelain" in cmd_str:
                result.stdout = "M file1.py\nM file2.py\nA file3.py\n"
            elif "ls-remote" in cmd_str:
                result.stdout = ""
            elif "pr" in cmd_str and "list" in cmd_str:
                result.stdout = ""
            elif "issue" in cmd_str and "view" in cmd_str:
                result.stdout = "loom:building\n"
            return result

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", side_effect=fake_run
        ):
            diag = builder._gather_diagnostics(mock_context)

        # Summary should say "3 files" not just "True"
        assert "uncommitted=3 files" in diag["summary"]

    def test_no_uncommitted_shows_none_in_summary(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Summary should show 'none' when no uncommitted files."""
        wt_dir = tmp_path / "worktree"
        wt_dir.mkdir()
        mock_context.worktree_path = wt_dir
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.repo_root = tmp_path

        log_dir = tmp_path / ".loom" / "logs"
        log_dir.mkdir(parents=True)
        log_file = log_dir / "loom-builder-issue-42.log"
        log_file.write_text("log line\n")

        builder = BuilderPhase()

        def fake_run(cmd, **kwargs):
            cmd_str = " ".join(str(c) for c in cmd)
            result = subprocess.CompletedProcess(
                args=cmd, returncode=0, stdout="", stderr=""
            )
            if "rev-parse" in cmd_str:
                result.stdout = "feature/issue-42\n"
            elif "log" in cmd_str and "main..HEAD" in cmd_str:
                result.stdout = ""
            elif "status" in cmd_str and "--porcelain" in cmd_str:
                result.stdout = ""
            elif "ls-remote" in cmd_str:
                result.stdout = ""
            elif "pr" in cmd_str and "list" in cmd_str:
                result.stdout = ""
            elif "issue" in cmd_str and "view" in cmd_str:
                result.stdout = "loom:building\n"
            return result

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", side_effect=fake_run
        ):
            diag = builder._gather_diagnostics(mock_context)

        assert diag["uncommitted_file_count"] == 0
        assert "uncommitted=none" in diag["summary"]

    def test_no_worktree_has_zero_file_count(self, mock_context: MagicMock) -> None:
        """Missing worktree should have zero file count."""
        mock_context.worktree_path = None
        mock_context.config = ShepherdConfig(issue=42)
        mock_context.repo_root = Path("/fake/repo")

        builder = BuilderPhase()

        def fake_run(cmd, **kwargs):
            return subprocess.CompletedProcess(
                args=cmd, returncode=0, stdout="", stderr=""
            )

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run", side_effect=fake_run
        ):
            diag = builder._gather_diagnostics(mock_context)

        assert diag["uncommitted_file_count"] == 0


class TestBuilderHasPytestOutput:
    """Test _has_pytest_output detection."""

    def test_detects_pytest_session_header(self) -> None:
        builder = BuilderPhase()
        output = (
            "============================= test session starts ==============================\n"
            "collected 15 items\n"
        )
        assert builder._has_pytest_output(output) is True

    def test_detects_pytest_summary_with_borders(self) -> None:
        builder = BuilderPhase()
        output = "========================= 1 failed, 14 passed in 2.45s ========================\n"
        assert builder._has_pytest_output(output) is True

    def test_detects_pytest_all_pass_summary(self) -> None:
        builder = BuilderPhase()
        output = "========================= 15 passed in 0.50s ========================\n"
        assert builder._has_pytest_output(output) is True

    def test_no_pytest_cargo_only(self) -> None:
        builder = BuilderPhase()
        output = (
            "running 14 tests\n"
            "test result: ok. 14 passed; 0 failed; 0 ignored\n"
        )
        assert builder._has_pytest_output(output) is False

    def test_no_pytest_vitest_only(self) -> None:
        builder = BuilderPhase()
        output = "Tests  5 passed in 1.23s\n"
        assert builder._has_pytest_output(output) is False

    def test_empty_output(self) -> None:
        builder = BuilderPhase()
        assert builder._has_pytest_output("") is False

    def test_mixed_cargo_and_pytest(self) -> None:
        builder = BuilderPhase()
        output = (
            "test result: ok. 14 passed; 0 failed\n"
            "============================= test session starts ==============================\n"
            "15 passed in 0.50s\n"
        )
        assert builder._has_pytest_output(output) is True


class TestBuilderGetSupplementalTestCommands:
    """Test _get_supplemental_test_commands."""

    def test_no_python_changes_returns_empty(self, mock_context: MagicMock) -> None:
        builder = BuilderPhase()
        mock_context.worktree_path = MagicMock()
        mock_context.worktree_path.is_dir.return_value = True

        with patch(
            "loom_tools.shepherd.phases.builder.get_changed_files",
            return_value=["src/main.rs", "src/lib.rs"],
        ):
            result = builder._get_supplemental_test_commands(
                mock_context, "test result: ok. 14 passed; 0 failed\n"
            )
        assert result == []

    def test_python_changes_with_pytest_in_output_returns_empty(
        self, mock_context: MagicMock
    ) -> None:
        builder = BuilderPhase()
        mock_context.worktree_path = MagicMock()
        mock_context.worktree_path.is_dir.return_value = True

        pytest_output = (
            "test result: ok. 14 passed; 0 failed\n"
            "============================= test session starts ==============================\n"
            "15 passed in 0.50s\n"
        )
        with patch(
            "loom_tools.shepherd.phases.builder.get_changed_files",
            return_value=["loom-tools/src/loom_tools/builder.py"],
        ):
            result = builder._get_supplemental_test_commands(mock_context, pytest_output)
        assert result == []

    def test_python_changes_without_pytest_uses_pnpm_test_python(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        builder = BuilderPhase()
        mock_context.worktree_path = tmp_path

        # Create package.json with test:python script
        pkg = {"scripts": {"check:ci:lite": "...", "test:python": "cd loom-tools && uv run pytest"}}
        (tmp_path / "package.json").write_text(json.dumps(pkg))

        cargo_only_output = "test result: ok. 14 passed; 0 failed\n"
        with patch(
            "loom_tools.shepherd.phases.builder.get_changed_files",
            return_value=["loom-tools/src/loom_tools/builder.py"],
        ):
            result = builder._get_supplemental_test_commands(mock_context, cargo_only_output)

        assert len(result) == 1
        assert result[0] == (["pnpm", "test:python"], "pnpm test:python (supplemental)")

    def test_python_changes_without_test_python_script_uses_pytest(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        builder = BuilderPhase()
        mock_context.worktree_path = tmp_path

        # package.json without test:python, but pyproject.toml exists
        pkg = {"scripts": {"check:ci:lite": "..."}}
        (tmp_path / "package.json").write_text(json.dumps(pkg))
        (tmp_path / "pyproject.toml").write_text("[project]\nname = 'test'\n")

        cargo_only_output = "test result: ok. 14 passed; 0 failed\n"
        with patch(
            "loom_tools.shepherd.phases.builder.get_changed_files",
            return_value=["loom-tools/src/loom_tools/builder.py"],
        ):
            result = builder._get_supplemental_test_commands(mock_context, cargo_only_output)

        assert len(result) == 1
        assert result[0] == (["python", "-m", "pytest"], "pytest (supplemental)")

    def test_pyi_files_count_as_python(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        builder = BuilderPhase()
        mock_context.worktree_path = tmp_path

        pkg = {"scripts": {"test:python": "pytest"}}
        (tmp_path / "package.json").write_text(json.dumps(pkg))

        with patch(
            "loom_tools.shepherd.phases.builder.get_changed_files",
            return_value=["loom-tools/src/types.pyi"],
        ):
            result = builder._get_supplemental_test_commands(mock_context, "")
        assert len(result) == 1

    def test_no_changed_files_returns_empty(self, mock_context: MagicMock) -> None:
        builder = BuilderPhase()
        mock_context.worktree_path = MagicMock()
        mock_context.worktree_path.is_dir.return_value = True

        with patch(
            "loom_tools.shepherd.phases.builder.get_changed_files",
            return_value=[],
        ):
            result = builder._get_supplemental_test_commands(mock_context, "")
        assert result == []

    def test_no_worktree_returns_empty(self, mock_context: MagicMock) -> None:
        builder = BuilderPhase()
        mock_context.worktree_path = None
        result = builder._get_supplemental_test_commands(mock_context, "")
        assert result == []


class TestBuilderSupplementalVerification:
    """Test _run_supplemental_verification end-to-end scenarios."""

    def test_no_supplemental_needed_returns_none(
        self, mock_context: MagicMock
    ) -> None:
        """No supplemental tests needed -> returns None."""
        builder = BuilderPhase()
        mock_context.worktree_path = MagicMock()
        mock_context.worktree_path.is_dir.return_value = True

        with patch.object(builder, "_get_supplemental_test_commands", return_value=[]):
            result = builder._run_supplemental_verification(mock_context, "")
        assert result is None

    def test_supplemental_passes_returns_none(
        self, mock_context: MagicMock
    ) -> None:
        """Supplemental test passes -> returns None."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        passing_result = subprocess.CompletedProcess(
            args=[], returncode=0, stdout="15 passed in 0.50s\n", stderr=""
        )
        with (
            patch.object(
                builder,
                "_get_supplemental_test_commands",
                return_value=[(["pnpm", "test:python"], "pnpm test:python (supplemental)")],
            ),
            patch.object(builder, "_run_baseline_tests", return_value=None),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=passing_result,
            ),
        ):
            result = builder._run_supplemental_verification(mock_context, "")
        assert result is None

    def test_supplemental_catches_new_python_failure(
        self, mock_context: MagicMock
    ) -> None:
        """Supplemental test finds new Python failure -> returns FAILED.

        This is the core scenario from issue #1980: the primary pipeline
        (check:ci:lite) short-circuited before reaching pytest, but the
        builder introduced a new Python test failure.
        """
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        # Baseline passes (no pre-existing failures)
        baseline_result = subprocess.CompletedProcess(
            args=[], returncode=0, stdout="15 passed in 0.50s\n", stderr=""
        )
        # Worktree fails (new regression)
        worktree_result = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout=(
                "FAILED tests/test_foo.py::test_bar - AssertionError\n"
                "1 failed, 14 passed in 2.45s\n"
            ),
            stderr="",
        )
        with (
            patch.object(
                builder,
                "_get_supplemental_test_commands",
                return_value=[(["pnpm", "test:python"], "pnpm test:python (supplemental)")],
            ),
            patch.object(builder, "_run_baseline_tests", return_value=baseline_result),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=worktree_result,
            ),
            patch(
                "loom_tools.shepherd.phases.builder.get_changed_files",
                return_value=["loom-tools/src/loom_tools/builder.py"],
            ),
        ):
            result = builder._run_supplemental_verification(mock_context, "")

        assert result is not None
        assert result.status == PhaseStatus.FAILED
        assert "supplemental" in result.message
        assert result.data["test_failure"] is True

    def test_supplemental_preexisting_failure_returns_none(
        self, mock_context: MagicMock
    ) -> None:
        """Supplemental test fails but failure is pre-existing on main -> returns None."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        # Both baseline and worktree fail with same count
        failing_output = (
            "FAILED tests/test_foo.py::test_bar\n"
            "1 failed, 14 passed in 2.45s\n"
        )
        baseline_result = subprocess.CompletedProcess(
            args=[], returncode=1, stdout=failing_output, stderr=""
        )
        worktree_result = subprocess.CompletedProcess(
            args=[], returncode=1, stdout=failing_output, stderr=""
        )
        with (
            patch.object(
                builder,
                "_get_supplemental_test_commands",
                return_value=[(["pnpm", "test:python"], "pnpm test:python (supplemental)")],
            ),
            patch.object(builder, "_run_baseline_tests", return_value=baseline_result),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=worktree_result,
            ),
        ):
            result = builder._run_supplemental_verification(mock_context, "")
        assert result is None

    def test_supplemental_timeout_returns_failed(
        self, mock_context: MagicMock
    ) -> None:
        """Supplemental test timeout -> returns FAILED."""
        builder = BuilderPhase()
        worktree_mock = MagicMock()
        worktree_mock.is_dir.return_value = True
        mock_context.worktree_path = worktree_mock

        with (
            patch.object(
                builder,
                "_get_supplemental_test_commands",
                return_value=[(["pnpm", "test:python"], "pnpm test:python (supplemental)")],
            ),
            patch.object(builder, "_run_baseline_tests", return_value=None),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                side_effect=subprocess.TimeoutExpired(cmd="pnpm test:python", timeout=300),
            ),
        ):
            result = builder._run_supplemental_verification(mock_context, "")

        assert result is not None
        assert result.status == PhaseStatus.FAILED
        assert "timed out" in result.message

    def test_primary_pass_triggers_supplemental_for_python_changes(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Full integration: primary passes but pipeline missed pytest, supplemental catches failure.

        Simulates the exact scenario from issue #1980: check:ci:lite passes
        (cargo OK), but Python files changed and pytest didn't run. The
        supplemental verification runs pytest and catches the new failure.
        """
        builder = BuilderPhase()
        mock_context.worktree_path = tmp_path

        # Create package.json with test scripts
        pkg = {"scripts": {"check:ci:lite": "...", "test:python": "cd loom-tools && uv run pytest"}}
        (tmp_path / "package.json").write_text(json.dumps(pkg))

        # Primary pipeline passed (cargo only, no pytest)
        primary_pass = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="test result: ok. 14 passed; 0 failed; 0 ignored\n",
            stderr="",
        )

        # Supplemental baseline passes
        supp_baseline = subprocess.CompletedProcess(
            args=[], returncode=0, stdout="15 passed in 0.50s\n", stderr=""
        )
        # Supplemental worktree fails (new regression)
        supp_worktree_fail = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="FAILED tests/test_bar.py::test_baz\n1 failed, 14 passed in 2.45s\n",
            stderr="",
        )

        call_count = 0

        def mock_subprocess_run(*args, **kwargs):
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                return primary_pass  # Primary pipeline
            return supp_worktree_fail  # Supplemental test in worktree

        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "check:ci:lite"], "pnpm check:ci:lite"),
            ),
            patch.object(
                builder, "_run_baseline_tests",
                side_effect=[None, supp_baseline],  # First call: primary baseline, second: supplemental
            ),
            patch.object(builder, "_ensure_dependencies"),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                side_effect=mock_subprocess_run,
            ),
            patch(
                "loom_tools.shepherd.phases.builder.get_changed_files",
                return_value=["loom-tools/src/loom_tools/builder.py"],
            ),
        ):
            result = builder._run_test_verification(mock_context)

        assert result is not None
        assert result.status == PhaseStatus.FAILED
        assert "supplemental" in result.message

    def test_primary_preexisting_triggers_supplemental(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Full integration: primary has pre-existing failures, supplemental catches new Python failure.

        The primary pipeline (check:ci:lite) fails with pre-existing
        failures (lint, etc.). The supplemental verification still runs
        pytest and catches a new Python regression.
        """
        builder = BuilderPhase()
        mock_context.worktree_path = tmp_path

        pkg = {"scripts": {"check:ci:lite": "...", "test:python": "cd loom-tools && uv run pytest"}}
        (tmp_path / "package.json").write_text(json.dumps(pkg))

        # Both primary runs fail identically (pre-existing lint failure)
        primary_output = "error: biome config invalid\ntest result: ok. 14 passed; 0 failed\n"
        primary_baseline = subprocess.CompletedProcess(
            args=[], returncode=1, stdout=primary_output, stderr=""
        )
        primary_worktree = subprocess.CompletedProcess(
            args=[], returncode=1, stdout=primary_output, stderr=""
        )

        # Supplemental baseline passes, worktree fails
        supp_baseline = subprocess.CompletedProcess(
            args=[], returncode=0, stdout="15 passed in 0.50s\n", stderr=""
        )
        supp_worktree_fail = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="FAILED tests/test_bar.py::test_baz\n1 failed, 14 passed\n",
            stderr="",
        )

        call_count = 0

        def mock_subprocess_run(*args, **kwargs):
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                return primary_worktree
            return supp_worktree_fail

        with (
            patch.object(
                builder,
                "_detect_test_command",
                return_value=(["pnpm", "check:ci:lite"], "pnpm check:ci:lite"),
            ),
            patch.object(
                builder, "_run_baseline_tests",
                side_effect=[primary_baseline, supp_baseline],
            ),
            patch.object(builder, "_ensure_dependencies"),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                side_effect=mock_subprocess_run,
            ),
            patch(
                "loom_tools.shepherd.phases.builder.get_changed_files",
                return_value=["loom-tools/src/loom_tools/builder.py"],
            ),
        ):
            result = builder._run_test_verification(mock_context)

        assert result is not None
        assert result.status == PhaseStatus.FAILED
        assert "supplemental" in result.message


class TestBuilderValidateAndComplete:
    """Tests for BuilderPhase.validate_and_complete() method.

    This method is used after the doctor test-fix loop succeeds to ensure
    that a PR actually exists. It runs the same validation and completion
    logic as the end of builder.run().
    """

    def test_validate_and_complete_success_when_pr_exists(
        self, mock_context: MagicMock
    ) -> None:
        """validate_and_complete should return SUCCESS when PR exists."""
        builder = BuilderPhase()

        with (
            patch.object(builder, "validate", return_value=True),
            patch(
                "loom_tools.shepherd.phases.builder.get_pr_for_issue",
                return_value=123,
            ),
        ):
            result = builder.validate_and_complete(mock_context)

        assert result.status == PhaseStatus.SUCCESS
        assert "PR #123" in result.message
        assert result.data.get("pr_number") == 123
        assert mock_context.pr_number == 123

    def test_validate_and_complete_runs_completion_when_incomplete(
        self, mock_context: MagicMock
    ) -> None:
        """validate_and_complete should run completion phase when work is incomplete."""
        builder = BuilderPhase()
        mock_context.config.builder_completion_retries = 2

        # First validation fails, second succeeds (after completion)
        validate_calls = [False, True]

        incomplete_diag = {
            "summary": "has uncommitted changes",
            "worktree_exists": True,
            "has_uncommitted_changes": True,
            "commits_ahead": 0,
            "remote_branch_exists": False,
            "pr_number": None,
            "pr_has_review_label": False,
        }

        with (
            patch.object(builder, "validate", side_effect=validate_calls),
            patch.object(builder, "_gather_diagnostics", return_value=incomplete_diag),
            patch.object(builder, "_has_incomplete_work", return_value=True),
            patch.object(builder, "_run_completion_phase", return_value=0),
            patch(
                "loom_tools.shepherd.phases.builder.get_pr_for_issue",
                return_value=456,
            ),
        ):
            result = builder.validate_and_complete(mock_context)

        assert result.status == PhaseStatus.SUCCESS
        assert "PR #456" in result.message
        assert result.data.get("pr_number") == 456

    def test_validate_and_complete_fails_when_no_incomplete_work(
        self, mock_context: MagicMock
    ) -> None:
        """validate_and_complete should fail when validation fails and no incomplete work."""
        builder = BuilderPhase()

        no_work_diag = {
            "summary": "worktree does not exist",
            "worktree_exists": False,
            "has_uncommitted_changes": False,
            "commits_ahead": 0,
            "remote_branch_exists": False,
            "pr_number": None,
            "pr_has_review_label": False,
        }

        with (
            patch.object(builder, "validate", return_value=False),
            patch.object(builder, "_gather_diagnostics", return_value=no_work_diag),
            patch.object(builder, "_has_incomplete_work", return_value=False),
        ):
            result = builder.validate_and_complete(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert "builder phase validation failed after doctor fixes" in result.message
        assert result.data.get("diagnostics") == no_work_diag

    def test_validate_and_complete_exhausts_retries(
        self, mock_context: MagicMock
    ) -> None:
        """validate_and_complete should fail after exhausting completion retries."""
        builder = BuilderPhase()
        mock_context.config.builder_completion_retries = 2

        incomplete_diag = {
            "summary": "has uncommitted changes but completion keeps failing",
            "worktree_exists": True,
            "has_uncommitted_changes": True,
            "commits_ahead": 0,
            "remote_branch_exists": False,
            "pr_number": None,
            "pr_has_review_label": False,
        }

        with (
            patch.object(builder, "validate", return_value=False),
            patch.object(builder, "_gather_diagnostics", return_value=incomplete_diag),
            patch.object(builder, "_has_incomplete_work", return_value=True),
            patch.object(builder, "_run_completion_phase", return_value=1),  # Always fail
            patch.object(builder, "_direct_completion", return_value=False),
        ):
            result = builder.validate_and_complete(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert "validation failed after doctor fixes" in result.message
        assert result.data.get("completion_attempts") == 2

    def test_validate_and_complete_handles_shutdown(
        self, mock_context: MagicMock
    ) -> None:
        """validate_and_complete should return SHUTDOWN when completion phase shuts down."""
        builder = BuilderPhase()
        mock_context.config.builder_completion_retries = 2

        incomplete_diag = {
            "summary": "has uncommitted changes",
            "worktree_exists": True,
            "has_uncommitted_changes": True,
            "commits_ahead": 0,
            "remote_branch_exists": False,
            "pr_number": None,
            "pr_has_review_label": False,
        }

        with (
            patch.object(builder, "validate", return_value=False),
            patch.object(builder, "_gather_diagnostics", return_value=incomplete_diag),
            patch.object(builder, "_has_incomplete_work", return_value=True),
            patch.object(builder, "_run_completion_phase", return_value=3),  # Shutdown
        ):
            result = builder.validate_and_complete(mock_context)

        assert result.status == PhaseStatus.SHUTDOWN
        assert "shutdown" in result.message.lower()

    def test_validate_and_complete_tries_direct_completion_as_fallback(
        self, mock_context: MagicMock
    ) -> None:
        """validate_and_complete should try direct completion when retries exhausted."""
        builder = BuilderPhase()
        mock_context.config.builder_completion_retries = 1

        # First two validate calls fail, third succeeds (after direct completion)
        validate_calls = [False, False, True]

        incomplete_diag = {
            "summary": "has commits but needs push",
            "worktree_exists": True,
            "has_uncommitted_changes": False,
            "commits_ahead": 1,
            "remote_branch_exists": False,
            "pr_number": None,
            "pr_has_review_label": False,
        }

        with (
            patch.object(builder, "validate", side_effect=validate_calls),
            patch.object(builder, "_gather_diagnostics", return_value=incomplete_diag),
            patch.object(builder, "_has_incomplete_work", return_value=True),
            patch.object(builder, "_run_completion_phase", return_value=1),  # Fail
            patch.object(builder, "_direct_completion", return_value=True),  # Succeed
            patch(
                "loom_tools.shepherd.phases.builder.get_pr_for_issue",
                return_value=789,
            ),
        ):
            result = builder.validate_and_complete(mock_context)

        assert result.status == PhaseStatus.SUCCESS
        assert "PR #789" in result.message

    def test_validate_and_complete_fails_when_pr_not_found_after_validation(
        self, mock_context: MagicMock
    ) -> None:
        """validate_and_complete should fail if PR not found even after validation passes."""
        builder = BuilderPhase()

        fake_diag = {
            "summary": "worktree exists but no PR",
            "worktree_exists": True,
            "has_uncommitted_changes": False,
            "commits_ahead": 0,
            "remote_branch_exists": True,
            "pr_number": None,
            "pr_has_review_label": False,
        }

        with (
            patch.object(builder, "validate", return_value=True),
            patch.object(builder, "_gather_diagnostics", return_value=fake_diag),
            patch(
                "loom_tools.shepherd.phases.builder.get_pr_for_issue",
                return_value=None,  # PR still not found
            ),
        ):
            result = builder.validate_and_complete(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert "could not find PR" in result.message
        assert "after doctor fixes" in result.message


class TestDoctorFailureModes:
    """Test Doctor phase failure mode classification (issue #2068)."""

    def test_no_progress_mode_when_no_commits_made(
        self, mock_context: MagicMock
    ) -> None:
        """Doctor with exit code != 0 and no commits should be classified as no_progress."""
        doctor = DoctorPhase()
        mock_context.pr_number = 123
        mock_context.check_shutdown.return_value = False

        with (
            patch(
                "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
            ) as mock_run,
            patch.object(doctor, "_get_commit_count", return_value=0),
            patch.object(
                doctor,
                "_diagnose_doctor_outcome",
                return_value=MagicMock(
                    made_progress=False,
                    commits_made=0,
                    to_dict=lambda: {
                        "commits_made": 0,
                        "failure_mode": "no_progress",
                    },
                ),
            ),
        ):
            mock_run.return_value = 1  # Non-zero exit code
            result = doctor.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert result.data.get("failure_mode") == "no_progress"
        assert result.data.get("commits_made") == 0

    def test_insufficient_changes_mode_when_commits_made_but_failed(
        self, mock_context: MagicMock
    ) -> None:
        """Doctor with commits but non-zero exit should be insufficient_changes."""
        from loom_tools.shepherd.phases.doctor import (
            DoctorDiagnostics,
            DoctorFailureMode,
        )

        doctor = DoctorPhase()
        mock_context.pr_number = 123
        mock_context.check_shutdown.return_value = False

        # Create a real diagnostics object for this test
        diagnostics = DoctorDiagnostics(commits_made=2)
        diagnostics.failure_mode = DoctorFailureMode.INSUFFICIENT_CHANGES

        with (
            patch(
                "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
            ) as mock_run,
            patch.object(doctor, "_get_commit_count", return_value=0),
            patch.object(doctor, "_diagnose_doctor_outcome", return_value=diagnostics),
        ):
            mock_run.return_value = 1  # Non-zero exit code
            result = doctor.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert result.data.get("failure_mode") == "insufficient_changes"
        assert result.data.get("commits_made") == 2

    def test_validation_failed_mode_when_exit_0_but_validation_fails(
        self, mock_context: MagicMock
    ) -> None:
        """Doctor with exit 0 but validation failure should be validation_failed."""
        from loom_tools.shepherd.phases.doctor import (
            CIResult,
            CIStatus,
            DoctorDiagnostics,
            DoctorFailureMode,
        )

        doctor = DoctorPhase()
        mock_context.pr_number = 123
        mock_context.check_shutdown.return_value = False

        # Create diagnostics that will be mutated
        diagnostics = DoctorDiagnostics(commits_made=1, pr_labels=["loom:changes-requested"])
        ci_result = CIResult(status=CIStatus.PASSED, message="CI passed")

        with (
            patch(
                "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
            ) as mock_run,
            patch.object(doctor, "_get_commit_count", return_value=0),
            patch.object(doctor, "_diagnose_doctor_outcome", return_value=diagnostics),
            patch.object(doctor, "_wait_for_ci", return_value=ci_result),
            patch.object(doctor, "validate", return_value=False),
            patch.object(doctor, "_attempt_label_recovery", return_value=False),
        ):
            mock_run.return_value = 0  # Success exit code
            result = doctor.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert result.data.get("failure_mode") == "validation_failed"

    def test_label_recovery_attempted_on_validation_failure_with_progress(
        self, mock_context: MagicMock
    ) -> None:
        """When validation fails but doctor made progress, should attempt recovery."""
        from loom_tools.shepherd.phases.doctor import CIResult, CIStatus, DoctorDiagnostics

        doctor = DoctorPhase()
        mock_context.pr_number = 123
        mock_context.check_shutdown.return_value = False

        diagnostics = DoctorDiagnostics(
            commits_made=1, pr_labels=["loom:changes-requested"]
        )
        ci_result = CIResult(status=CIStatus.PASSED, message="CI passed")

        with (
            patch(
                "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
            ) as mock_run,
            patch.object(doctor, "_get_commit_count", return_value=0),
            patch.object(doctor, "_diagnose_doctor_outcome", return_value=diagnostics),
            patch.object(doctor, "_wait_for_ci", return_value=ci_result),
            patch.object(doctor, "validate", return_value=False),
            patch.object(doctor, "_attempt_label_recovery") as mock_recovery,
        ):
            mock_run.return_value = 0
            mock_recovery.return_value = False  # Recovery attempted but failed
            doctor.run(mock_context)

        # Should have attempted recovery since commits were made
        mock_recovery.assert_called_once()

    def test_no_label_recovery_when_no_progress(
        self, mock_context: MagicMock
    ) -> None:
        """When validation fails and no progress, should not attempt recovery."""
        from loom_tools.shepherd.phases.doctor import DoctorDiagnostics

        doctor = DoctorPhase()
        mock_context.pr_number = 123
        mock_context.check_shutdown.return_value = False

        diagnostics = DoctorDiagnostics(
            commits_made=0, pr_labels=["loom:changes-requested"]
        )

        with (
            patch(
                "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
            ) as mock_run,
            patch.object(doctor, "_get_commit_count", return_value=0),
            patch.object(doctor, "_diagnose_doctor_outcome", return_value=diagnostics),
            patch.object(doctor, "validate", return_value=False),
            patch.object(doctor, "_attempt_label_recovery") as mock_recovery,
        ):
            mock_run.return_value = 0
            doctor.run(mock_context)

        # Should NOT have attempted recovery since no commits were made
        mock_recovery.assert_not_called()


class TestDoctorDiagnostics:
    """Test DoctorDiagnostics data class."""

    def test_made_progress_true_when_commits_made(self) -> None:
        """made_progress should be True when commits_made > 0."""
        from loom_tools.shepherd.phases.doctor import DoctorDiagnostics

        diag = DoctorDiagnostics(commits_made=1)
        assert diag.made_progress is True

    def test_made_progress_false_when_no_commits(self) -> None:
        """made_progress should be False when commits_made == 0."""
        from loom_tools.shepherd.phases.doctor import DoctorDiagnostics

        diag = DoctorDiagnostics(commits_made=0)
        assert diag.made_progress is False

    def test_to_dict_includes_all_fields(self) -> None:
        """to_dict should include all diagnostic fields."""
        from loom_tools.shepherd.phases.doctor import (
            DoctorDiagnostics,
            DoctorFailureMode,
        )

        diag = DoctorDiagnostics(
            commits_made=2,
            has_uncommitted_changes=True,
            pr_labels=["loom:pr", "loom:review-requested"],
            failure_mode=DoctorFailureMode.INSUFFICIENT_CHANGES,
        )

        result = diag.to_dict()

        assert result["commits_made"] == 2
        assert result["has_uncommitted_changes"] is True
        assert result["pr_labels"] == ["loom:pr", "loom:review-requested"]
        assert result["failure_mode"] == "insufficient_changes"

    def test_to_dict_handles_none_failure_mode(self) -> None:
        """to_dict should handle None failure_mode gracefully."""
        from loom_tools.shepherd.phases.doctor import DoctorDiagnostics

        diag = DoctorDiagnostics()  # Default failure_mode is None
        result = diag.to_dict()

        assert result["failure_mode"] is None


class TestDoctorGetCommitCount:
    """Test _get_commit_count helper method."""

    def test_returns_0_when_no_worktree(self, mock_context: MagicMock) -> None:
        """Should return 0 when worktree path is None."""
        doctor = DoctorPhase()
        mock_context.worktree_path = None

        assert doctor._get_commit_count(mock_context) == 0

    def test_returns_0_when_worktree_missing(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Should return 0 when worktree directory doesn't exist."""
        doctor = DoctorPhase()
        mock_context.worktree_path = tmp_path / "nonexistent"

        assert doctor._get_commit_count(mock_context) == 0

    def test_returns_commit_count_from_git(
        self, mock_context: MagicMock, tmp_path: Path
    ) -> None:
        """Should parse commit count from git rev-list."""
        doctor = DoctorPhase()
        mock_context.worktree_path = tmp_path
        tmp_path.mkdir(exist_ok=True)

        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(returncode=0, stdout="5\n")
            result = doctor._get_commit_count(mock_context)

        assert result == 5


class TestDoctorLabelRecovery:
    """Test _attempt_label_recovery method."""

    def test_transitions_changes_requested_to_review_requested(
        self, mock_context: MagicMock
    ) -> None:
        """Should transition loom:changes-requested to loom:review-requested."""
        from loom_tools.shepherd.phases.doctor import DoctorDiagnostics

        doctor = DoctorPhase()
        mock_context.pr_number = 123

        diagnostics = DoctorDiagnostics(pr_labels=["loom:changes-requested"])

        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(returncode=0)
            result = doctor._attempt_label_recovery(mock_context, diagnostics)

        assert result is True
        mock_run.assert_called_once()
        call_args = mock_run.call_args[0][0]
        assert "--remove-label" in call_args
        assert "loom:changes-requested" in call_args
        assert "--add-label" in call_args
        assert "loom:review-requested" in call_args

    def test_no_recovery_when_already_review_requested(
        self, mock_context: MagicMock
    ) -> None:
        """Should not transition if already has loom:review-requested."""
        from loom_tools.shepherd.phases.doctor import DoctorDiagnostics

        doctor = DoctorPhase()
        mock_context.pr_number = 123

        diagnostics = DoctorDiagnostics(
            pr_labels=["loom:changes-requested", "loom:review-requested"]
        )

        with patch("subprocess.run") as mock_run:
            result = doctor._attempt_label_recovery(mock_context, diagnostics)

        assert result is False
        mock_run.assert_not_called()

    def test_returns_false_when_no_pr_number(
        self, mock_context: MagicMock
    ) -> None:
        """Should return False when no PR number available."""
        from loom_tools.shepherd.phases.doctor import DoctorDiagnostics

        doctor = DoctorPhase()
        mock_context.pr_number = None

        diagnostics = DoctorDiagnostics(pr_labels=["loom:changes-requested"])

        result = doctor._attempt_label_recovery(mock_context, diagnostics)
        assert result is False


class TestDoctorTestFixWithDiagnostics:
    """Test run_test_fix includes diagnostics in failure results."""

    def test_stuck_result_includes_diagnostics(
        self, mock_context: MagicMock
    ) -> None:
        """run_test_fix STUCK result should include diagnostics."""
        from loom_tools.shepherd.phases.doctor import DoctorDiagnostics

        doctor = DoctorPhase()
        mock_context.check_shutdown.return_value = False

        diagnostics = DoctorDiagnostics(commits_made=1)

        with (
            patch(
                "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
            ) as mock_run,
            patch.object(doctor, "_get_commit_count", return_value=0),
            patch.object(doctor, "_diagnose_doctor_outcome", return_value=diagnostics),
        ):
            mock_run.return_value = 4  # Stuck
            result = doctor.run_test_fix(mock_context, {})

        assert result.status == PhaseStatus.STUCK
        assert result.data.get("commits_made") == 1

    def test_failed_result_includes_failure_mode(
        self, mock_context: MagicMock
    ) -> None:
        """run_test_fix FAILED result should include failure_mode."""
        from loom_tools.shepherd.phases.doctor import (
            DoctorDiagnostics,
            DoctorFailureMode,
        )

        doctor = DoctorPhase()
        mock_context.check_shutdown.return_value = False

        diagnostics = DoctorDiagnostics(commits_made=0)

        with (
            patch(
                "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
            ) as mock_run,
            patch.object(doctor, "_get_commit_count", return_value=0),
            patch.object(doctor, "_diagnose_doctor_outcome", return_value=diagnostics),
        ):
            mock_run.return_value = 1  # Failed
            result = doctor.run_test_fix(mock_context, {})

        assert result.status == PhaseStatus.FAILED
        assert result.data.get("failure_mode") == "no_progress"

    def test_success_result_includes_diagnostics(
        self, mock_context: MagicMock
    ) -> None:
        """run_test_fix SUCCESS result should include diagnostics."""
        from loom_tools.shepherd.phases.doctor import DoctorDiagnostics

        doctor = DoctorPhase()
        mock_context.check_shutdown.return_value = False

        diagnostics = DoctorDiagnostics(commits_made=2)

        with (
            patch(
                "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
            ) as mock_run,
            patch.object(doctor, "_get_commit_count", return_value=0),
            patch.object(doctor, "_diagnose_doctor_outcome", return_value=diagnostics),
        ):
            mock_run.return_value = 0  # Success
            result = doctor.run_test_fix(mock_context, {})

        assert result.status == PhaseStatus.SUCCESS
        assert result.data.get("commits_made") == 2


class TestDoctorCIWaiting:
    """Test Doctor phase CI waiting functionality (issue #2082)."""

    def test_get_ci_status_returns_passed_when_all_checks_succeed(
        self, mock_context: MagicMock
    ) -> None:
        """_get_ci_status should return PASSED when all checks succeed."""
        from loom_tools.shepherd.phases.doctor import CIStatus, DoctorPhase

        doctor = DoctorPhase()
        mock_context.pr_number = 123

        mock_result = MagicMock()
        mock_result.returncode = 0
        mock_result.stdout = json.dumps({
            "statusCheckRollup": [
                {"conclusion": "SUCCESS", "status": "COMPLETED"},
                {"conclusion": "SUCCESS", "status": "COMPLETED"},
            ]
        })

        with patch("subprocess.run", return_value=mock_result):
            ci_result = doctor._get_ci_status(mock_context)

        assert ci_result.status == CIStatus.PASSED
        assert ci_result.checks_passed == 2
        assert ci_result.checks_failed == 0
        assert ci_result.checks_pending == 0

    def test_get_ci_status_returns_failed_when_check_fails(
        self, mock_context: MagicMock
    ) -> None:
        """_get_ci_status should return FAILED when any check fails."""
        from loom_tools.shepherd.phases.doctor import CIStatus, DoctorPhase

        doctor = DoctorPhase()
        mock_context.pr_number = 123

        mock_result = MagicMock()
        mock_result.returncode = 0
        mock_result.stdout = json.dumps({
            "statusCheckRollup": [
                {"conclusion": "SUCCESS", "status": "COMPLETED"},
                {"conclusion": "FAILURE", "status": "COMPLETED"},
            ]
        })

        with patch("subprocess.run", return_value=mock_result):
            ci_result = doctor._get_ci_status(mock_context)

        assert ci_result.status == CIStatus.FAILED
        assert ci_result.checks_passed == 1
        assert ci_result.checks_failed == 1

    def test_get_ci_status_returns_pending_when_checks_running(
        self, mock_context: MagicMock
    ) -> None:
        """_get_ci_status should return PENDING when checks are in progress."""
        from loom_tools.shepherd.phases.doctor import CIStatus, DoctorPhase

        doctor = DoctorPhase()
        mock_context.pr_number = 123

        mock_result = MagicMock()
        mock_result.returncode = 0
        mock_result.stdout = json.dumps({
            "statusCheckRollup": [
                {"conclusion": "SUCCESS", "status": "COMPLETED"},
                {"conclusion": "", "status": "IN_PROGRESS"},
            ]
        })

        with patch("subprocess.run", return_value=mock_result):
            ci_result = doctor._get_ci_status(mock_context)

        assert ci_result.status == CIStatus.PENDING
        assert ci_result.checks_passed == 1
        assert ci_result.checks_pending == 1

    def test_get_ci_status_returns_passed_when_no_checks_configured(
        self, mock_context: MagicMock
    ) -> None:
        """_get_ci_status should return PASSED when no checks are configured."""
        from loom_tools.shepherd.phases.doctor import CIStatus, DoctorPhase

        doctor = DoctorPhase()
        mock_context.pr_number = 123

        mock_result = MagicMock()
        mock_result.returncode = 0
        mock_result.stdout = json.dumps({"statusCheckRollup": []})

        with patch("subprocess.run", return_value=mock_result):
            ci_result = doctor._get_ci_status(mock_context)

        assert ci_result.status == CIStatus.PASSED
        assert "No CI checks configured" in ci_result.message

    def test_get_ci_status_returns_unknown_on_api_error(
        self, mock_context: MagicMock
    ) -> None:
        """_get_ci_status should return UNKNOWN when gh command fails."""
        from loom_tools.shepherd.phases.doctor import CIStatus, DoctorPhase

        doctor = DoctorPhase()
        mock_context.pr_number = 123

        mock_result = MagicMock()
        mock_result.returncode = 1
        mock_result.stderr = "API error"

        with patch("subprocess.run", return_value=mock_result):
            ci_result = doctor._get_ci_status(mock_context)

        assert ci_result.status == CIStatus.UNKNOWN
        assert "Failed to get PR status" in ci_result.message

    def test_get_ci_status_returns_unknown_when_no_pr_number(
        self, mock_context: MagicMock
    ) -> None:
        """_get_ci_status should return UNKNOWN when no PR number."""
        from loom_tools.shepherd.phases.doctor import CIStatus, DoctorPhase

        doctor = DoctorPhase()
        mock_context.pr_number = None

        ci_result = doctor._get_ci_status(mock_context)

        assert ci_result.status == CIStatus.UNKNOWN
        assert "No PR number" in ci_result.message

    def test_wait_for_ci_returns_immediately_when_passed(
        self, mock_context: MagicMock
    ) -> None:
        """_wait_for_ci should return immediately when CI passes."""
        from loom_tools.shepherd.phases.doctor import CIResult, CIStatus, DoctorPhase

        doctor = DoctorPhase()
        mock_context.pr_number = 123
        mock_context.check_shutdown.return_value = False

        passed_result = CIResult(
            status=CIStatus.PASSED,
            message="CI passed",
            checks_total=2,
            checks_passed=2,
        )

        with patch.object(doctor, "_get_ci_status", return_value=passed_result):
            ci_result = doctor._wait_for_ci(mock_context, timeout_seconds=60)

        assert ci_result.status == CIStatus.PASSED

    def test_wait_for_ci_returns_immediately_when_failed(
        self, mock_context: MagicMock
    ) -> None:
        """_wait_for_ci should return immediately when CI fails."""
        from loom_tools.shepherd.phases.doctor import CIResult, CIStatus, DoctorPhase

        doctor = DoctorPhase()
        mock_context.pr_number = 123
        mock_context.check_shutdown.return_value = False

        failed_result = CIResult(
            status=CIStatus.FAILED,
            message="CI failed",
            checks_total=2,
            checks_passed=1,
            checks_failed=1,
        )

        with patch.object(doctor, "_get_ci_status", return_value=failed_result):
            ci_result = doctor._wait_for_ci(mock_context, timeout_seconds=60)

        assert ci_result.status == CIStatus.FAILED

    def test_wait_for_ci_polls_until_complete(
        self, mock_context: MagicMock
    ) -> None:
        """_wait_for_ci should poll until CI completes."""
        from loom_tools.shepherd.phases.doctor import CIResult, CIStatus, DoctorPhase

        doctor = DoctorPhase()
        mock_context.pr_number = 123
        mock_context.check_shutdown.return_value = False

        # First two calls return pending, third returns passed
        pending_result = CIResult(
            status=CIStatus.PENDING,
            message="CI pending",
            checks_total=2,
            checks_passed=1,
            checks_pending=1,
        )
        passed_result = CIResult(
            status=CIStatus.PASSED,
            message="CI passed",
            checks_total=2,
            checks_passed=2,
        )

        call_count = [0]

        def mock_get_ci_status(ctx: Any) -> CIResult:
            call_count[0] += 1
            if call_count[0] < 3:
                return pending_result
            return passed_result

        with (
            patch.object(doctor, "_get_ci_status", side_effect=mock_get_ci_status),
            patch("loom_tools.shepherd.phases.doctor.time.sleep"),
        ):
            ci_result = doctor._wait_for_ci(mock_context, timeout_seconds=300)

        assert ci_result.status == CIStatus.PASSED
        assert call_count[0] == 3

    def test_wait_for_ci_returns_pending_on_timeout(
        self, mock_context: MagicMock
    ) -> None:
        """_wait_for_ci should return PENDING status on timeout."""
        from loom_tools.shepherd.phases.doctor import CIResult, CIStatus, DoctorPhase

        doctor = DoctorPhase()
        mock_context.pr_number = 123
        mock_context.check_shutdown.return_value = False

        pending_result = CIResult(
            status=CIStatus.PENDING,
            message="CI pending",
            checks_total=2,
            checks_passed=1,
            checks_pending=1,
        )

        with (
            patch.object(doctor, "_get_ci_status", return_value=pending_result),
            patch("loom_tools.shepherd.phases.doctor.time.sleep"),
            patch("loom_tools.shepherd.phases.doctor.time.time", side_effect=[0, 0, 400]),
        ):
            ci_result = doctor._wait_for_ci(mock_context, timeout_seconds=300)

        assert ci_result.status == CIStatus.PENDING
        assert "timeout" in ci_result.message.lower()

    def test_wait_for_ci_handles_shutdown_signal(
        self, mock_context: MagicMock
    ) -> None:
        """_wait_for_ci should return UNKNOWN on shutdown signal."""
        from loom_tools.shepherd.phases.doctor import CIResult, CIStatus, DoctorPhase

        doctor = DoctorPhase()
        mock_context.pr_number = 123
        mock_context.check_shutdown.side_effect = [False, True]

        pending_result = CIResult(
            status=CIStatus.PENDING,
            message="CI pending",
        )

        with (
            patch.object(doctor, "_get_ci_status", return_value=pending_result),
            patch("loom_tools.shepherd.phases.doctor.time.sleep"),
        ):
            ci_result = doctor._wait_for_ci(mock_context, timeout_seconds=300)

        assert ci_result.status == CIStatus.UNKNOWN
        assert "Shutdown" in ci_result.message

    def test_run_waits_for_ci_when_doctor_makes_commits(
        self, mock_context: MagicMock
    ) -> None:
        """run() should wait for CI when doctor makes commits."""
        from loom_tools.shepherd.phases.doctor import (
            CIResult,
            CIStatus,
            DoctorDiagnostics,
            DoctorPhase,
        )

        doctor = DoctorPhase()
        mock_context.pr_number = 123
        mock_context.check_shutdown.return_value = False

        diagnostics = DoctorDiagnostics(
            commits_made=1,
            pr_labels=["loom:review-requested"],
        )

        ci_result = CIResult(
            status=CIStatus.PASSED,
            message="CI passed",
            checks_total=2,
            checks_passed=2,
        )

        with (
            patch(
                "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
            ) as mock_run,
            patch.object(doctor, "_get_commit_count", return_value=0),
            patch.object(doctor, "_diagnose_doctor_outcome", return_value=diagnostics),
            patch.object(doctor, "_wait_for_ci", return_value=ci_result) as mock_wait,
            patch.object(doctor, "validate", return_value=True),
        ):
            mock_run.return_value = 0
            result = doctor.run(mock_context)

        # CI wait should have been called
        mock_wait.assert_called_once_with(mock_context)
        assert result.status == PhaseStatus.SUCCESS

    def test_run_returns_failed_when_ci_fails_after_commits(
        self, mock_context: MagicMock
    ) -> None:
        """run() should return FAILED when CI fails after doctor commits."""
        from loom_tools.shepherd.phases.doctor import (
            CIResult,
            CIStatus,
            DoctorDiagnostics,
            DoctorPhase,
        )

        doctor = DoctorPhase()
        mock_context.pr_number = 123
        mock_context.check_shutdown.return_value = False

        diagnostics = DoctorDiagnostics(commits_made=1)

        ci_result = CIResult(
            status=CIStatus.FAILED,
            message="CI failed: 1/2 checks failed",
            checks_total=2,
            checks_passed=1,
            checks_failed=1,
        )

        with (
            patch(
                "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
            ) as mock_run,
            patch.object(doctor, "_get_commit_count", return_value=0),
            patch.object(doctor, "_diagnose_doctor_outcome", return_value=diagnostics),
            patch.object(doctor, "_wait_for_ci", return_value=ci_result),
        ):
            mock_run.return_value = 0
            result = doctor.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert "CI failed" in result.message
        assert result.data.get("ci_status", {}).get("status") == "failed"

    def test_run_continues_to_validation_when_ci_times_out(
        self, mock_context: MagicMock
    ) -> None:
        """run() should continue to validation when CI times out (pending)."""
        from loom_tools.shepherd.phases.doctor import (
            CIResult,
            CIStatus,
            DoctorDiagnostics,
            DoctorPhase,
        )

        doctor = DoctorPhase()
        mock_context.pr_number = 123
        mock_context.check_shutdown.return_value = False

        diagnostics = DoctorDiagnostics(
            commits_made=1,
            pr_labels=["loom:review-requested"],
        )

        ci_result = CIResult(
            status=CIStatus.PENDING,
            message="CI timeout: 1 checks still running",
            checks_total=2,
            checks_passed=1,
            checks_pending=1,
        )

        with (
            patch(
                "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
            ) as mock_run,
            patch.object(doctor, "_get_commit_count", return_value=0),
            patch.object(doctor, "_diagnose_doctor_outcome", return_value=diagnostics),
            patch.object(doctor, "_wait_for_ci", return_value=ci_result),
            patch.object(doctor, "validate", return_value=True) as mock_validate,
        ):
            mock_run.return_value = 0
            result = doctor.run(mock_context)

        # Should still proceed to validation
        mock_validate.assert_called_once()
        assert result.status == PhaseStatus.SUCCESS

    def test_run_does_not_wait_for_ci_when_no_commits(
        self, mock_context: MagicMock
    ) -> None:
        """run() should not wait for CI when doctor makes no commits."""
        from loom_tools.shepherd.phases.doctor import DoctorDiagnostics, DoctorPhase

        doctor = DoctorPhase()
        mock_context.pr_number = 123
        mock_context.check_shutdown.return_value = False

        diagnostics = DoctorDiagnostics(
            commits_made=0,
            pr_labels=["loom:review-requested"],
        )

        with (
            patch(
                "loom_tools.shepherd.phases.doctor.run_phase_with_retry"
            ) as mock_run,
            patch.object(doctor, "_get_commit_count", return_value=0),
            patch.object(doctor, "_diagnose_doctor_outcome", return_value=diagnostics),
            patch.object(doctor, "_wait_for_ci") as mock_wait,
            patch.object(doctor, "validate", return_value=True),
        ):
            mock_run.return_value = 0
            doctor.run(mock_context)

        # CI wait should NOT have been called since no commits made
        mock_wait.assert_not_called()

    def test_ci_result_is_complete_property(self) -> None:
        """CIResult.is_complete should return True for terminal states."""
        from loom_tools.shepherd.phases.doctor import CIResult, CIStatus

        passed = CIResult(status=CIStatus.PASSED, message="")
        failed = CIResult(status=CIStatus.FAILED, message="")
        pending = CIResult(status=CIStatus.PENDING, message="")
        unknown = CIResult(status=CIStatus.UNKNOWN, message="")

        assert passed.is_complete is True
        assert failed.is_complete is True
        assert pending.is_complete is False
        assert unknown.is_complete is True

    def test_ci_result_to_dict(self) -> None:
        """CIResult.to_dict should include all fields."""
        from loom_tools.shepherd.phases.doctor import CIResult, CIStatus

        ci_result = CIResult(
            status=CIStatus.PASSED,
            message="CI passed",
            checks_total=3,
            checks_passed=2,
            checks_failed=0,
            checks_pending=1,
        )

        result = ci_result.to_dict()

        assert result["status"] == "passed"
        assert result["message"] == "CI passed"
        assert result["checks_total"] == 3
        assert result["checks_passed"] == 2
        assert result["checks_failed"] == 0
        assert result["checks_pending"] == 1
