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
from loom_tools.shepherd.phases.judge import APPROVAL_PATTERNS, NEGATIVE_PREFIXES


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
            patch("loom_tools.shepherd.phases.builder.remove_issue_label"),
            patch("loom_tools.shepherd.phases.builder.add_issue_label"),
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
            patch("loom_tools.shepherd.phases.builder.remove_issue_label"),
            patch("loom_tools.shepherd.phases.builder.add_issue_label"),
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
            patch("loom_tools.shepherd.phases.builder.remove_issue_label"),
            patch("loom_tools.shepherd.phases.builder.add_issue_label"),
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
            patch("loom_tools.shepherd.phases.builder.remove_issue_label"),
            patch("loom_tools.shepherd.phases.builder.add_issue_label"),
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
            patch("loom_tools.shepherd.phases.builder.remove_issue_label"),
            patch("loom_tools.shepherd.phases.builder.add_issue_label"),
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
            patch("loom_tools.shepherd.phases.builder.remove_issue_label"),
            patch("loom_tools.shepherd.phases.builder.add_issue_label"),
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
        # Should have 4 semicolon-separated sections
        parts = summary.split("; ")
        assert len(parts) == 4
        assert "worktree" in parts[0]
        assert "remote branch" in parts[1]
        assert "labels=" in parts[2]
        assert "log=" in parts[3]


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
            builder._run_quality_validation(mock_context)

        # Should not have reported any milestone
        mock_context.report_milestone.assert_not_called()


class TestBuilderTestVerification:
    """Test builder phase test verification."""

    def test_detect_test_command_pnpm_check_ci(self, tmp_path: Path) -> None:
        """Should detect pnpm check:ci when available in package.json."""
        builder = BuilderPhase()
        pkg = {"scripts": {"check:ci": "pnpm lint && pnpm test", "test": "vitest"}}
        (tmp_path / "package.json").write_text(json.dumps(pkg))

        result = builder._detect_test_command(tmp_path)
        assert result is not None
        assert result == (["pnpm", "check:ci"], "pnpm check:ci")

    def test_detect_test_command_pnpm_test(self, tmp_path: Path) -> None:
        """Should detect pnpm test when no check:ci available."""
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
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                return_value=completed,
            ),
        ):
            result = builder._run_test_verification(mock_context)

        assert result is None
        # Should have reported milestones
        assert mock_context.report_milestone.call_count >= 1

    def test_run_test_verification_fails(self, mock_context: MagicMock) -> None:
        """Should return FAILED result when tests fail."""
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
        should fail and return FAILED status.
        """
        mock_context.pr_number = 100
        mock_context.check_shutdown.return_value = False

        judge = JudgePhase()
        with (
            patch.object(judge, "validate", return_value=False) as mock_validate,
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep") as mock_sleep,
        ):
            result = judge.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert "validation failed" in result.message
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

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
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

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            # No approval comment
            patch.object(judge, "_has_approval_comment", return_value=False),
            patch.object(judge, "_pr_checks_passing", return_value=True),
        ):
            result = judge.run(ctx)

        assert result.status == PhaseStatus.FAILED

    def test_fallback_denied_without_passing_checks(
        self, mock_context: MagicMock
    ) -> None:
        """Fallback should fail when PR checks are not passing."""
        ctx = self._make_force_context(mock_context)

        judge = JudgePhase()

        with (
            patch.object(judge, "validate", return_value=False),
            patch(
                "loom_tools.shepherd.phases.judge.run_phase_with_retry", return_value=0
            ),
            patch("loom_tools.shepherd.phases.judge.time.sleep"),
            patch.object(judge, "_has_approval_comment", return_value=True),
            # Checks not passing
            patch.object(judge, "_pr_checks_passing", return_value=False),
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
        ):
            result = judge.run(ctx)

        assert result.status == PhaseStatus.FAILED


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
