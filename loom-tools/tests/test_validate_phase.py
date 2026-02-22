"""Tests for loom_tools.validate_phase."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.validate_phase import (
    BuilderDiagnostics,
    ValidationResult,
    ValidationStatus,
    validate_curator,
    validate_builder,
    validate_doctor,
    validate_judge,
    validate_phase,
    main,
    _parse_args,
    _gather_builder_diagnostics,
    _build_recovery_pr_body,
    _is_rate_limited_builder_exit,
    VALID_PHASES,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _completed(returncode: int = 0, stdout: str = "", stderr: str = "") -> subprocess.CompletedProcess[str]:
    """Shorthand for a subprocess.CompletedProcess."""
    return subprocess.CompletedProcess(args=[], returncode=returncode, stdout=stdout, stderr=stderr)


def _make_repo(tmp_path: Path) -> Path:
    """Create a minimal fake repo root."""
    (tmp_path / ".git").mkdir()
    (tmp_path / ".loom" / "scripts").mkdir(parents=True)
    return tmp_path


# ---------------------------------------------------------------------------
# ValidationResult dataclass
# ---------------------------------------------------------------------------


class TestValidationResult:
    def test_satisfied_property_satisfied(self):
        r = ValidationResult("curator", 1, ValidationStatus.SATISFIED, "ok")
        assert r.satisfied is True

    def test_satisfied_property_recovered(self):
        r = ValidationResult("curator", 1, ValidationStatus.RECOVERED, "fixed")
        assert r.satisfied is True

    def test_satisfied_property_failed(self):
        r = ValidationResult("curator", 1, ValidationStatus.FAILED, "bad")
        assert r.satisfied is False

    def test_to_dict(self):
        r = ValidationResult("builder", 42, ValidationStatus.SATISFIED, "PR exists", "none")
        d = r.to_dict()
        assert d == {
            "phase": "builder",
            "issue": 42,
            "status": "satisfied",
            "message": "PR exists",
            "recovery_action": "none",
        }

    def test_to_json(self):
        r = ValidationResult("judge", 7, ValidationStatus.FAILED, "no label")
        data = json.loads(r.to_json())
        assert data["phase"] == "judge"
        assert data["status"] == "failed"

    def test_to_json_matches_bash_shape(self):
        """JSON shape must match bash script output for ContractCheckResult.from_json()."""
        r = ValidationResult("curator", 10, ValidationStatus.RECOVERED, "Applied label", "apply_label")
        data = json.loads(r.to_json())
        # These are the fields ContractCheckResult.from_json() reads
        assert "phase" in data
        assert "issue" in data
        assert "status" in data
        assert "message" in data
        assert "recovery_action" in data

    def test_contract_check_result_compatibility(self):
        """Verify JSON output is compatible with ContractCheckResult.from_json()."""
        from loom_tools.models.agent_wait import ContractCheckResult

        r = ValidationResult("builder", 5, ValidationStatus.SATISFIED, "PR #10 ok")
        data = r.to_dict()
        ccr = ContractCheckResult.from_json(data)
        assert ccr.satisfied is True
        assert ccr.status == "satisfied"
        assert ccr.message == "PR #10 ok"

        r2 = ValidationResult("judge", 5, ValidationStatus.FAILED, "no decision")
        ccr2 = ContractCheckResult.from_json(r2.to_dict())
        assert ccr2.satisfied is False


# ---------------------------------------------------------------------------
# BuilderDiagnostics
# ---------------------------------------------------------------------------


class TestBuilderDiagnostics:
    def test_to_markdown_worktree_missing(self):
        d = BuilderDiagnostics(worktree_path="/tmp/wt", issue=42)
        md = d.to_markdown()
        assert "does not exist" in md
        assert "Option A" in md
        assert "42" in md

    def test_to_markdown_worktree_exists(self):
        d = BuilderDiagnostics(
            worktree_path="/tmp/wt",
            worktree_exists=True,
            branch="feature/issue-42",
            commits_ahead="3",
            commits_behind="0",
            has_remote_tracking=True,
            issue=42,
        )
        md = d.to_markdown()
        assert "feature/issue-42" in md
        assert "configured" in md

    def test_to_markdown_with_progress_info(self):
        """Verify previous attempt section is rendered with progress info."""
        d = BuilderDiagnostics(
            worktree_path="/tmp/wt",
            worktree_exists=True,
            branch="feature/issue-42",
            commits_ahead="0",
            issue=42,
            worktree_mtime="2026-01-15T10:30:00Z",
            progress_status="builder",
            progress_started_at="2026-01-15T10:00:00Z",
            progress_last_heartbeat="2026-01-15T10:25:00Z",
            progress_milestones=[
                "started at 2026-01-15T10:00:00Z ({'issue': 42})",
                "phase_entered at 2026-01-15T10:01:00Z ({'phase': 'curator'})",
                "phase_entered at 2026-01-15T10:05:00Z ({'phase': 'builder'})",
            ],
        )
        md = d.to_markdown()
        assert "### Previous Attempt" in md
        assert "**Started**: 2026-01-15T10:00:00Z" in md
        assert "**Worktree last modified**: 2026-01-15T10:30:00Z" in md
        assert "**Last phase**: `builder`" in md
        assert "**Last heartbeat**: 2026-01-15T10:25:00Z" in md
        assert "**Recent milestones**:" in md
        assert "phase_entered at 2026-01-15T10:05:00Z" in md

    def test_to_markdown_with_partial_progress_info(self):
        """Verify partial progress info is rendered correctly."""
        d = BuilderDiagnostics(
            worktree_path="/tmp/wt",
            worktree_exists=True,
            issue=42,
            worktree_mtime="2026-01-15T10:30:00Z",
            # No progress file found
            progress_status="",
            progress_started_at="",
        )
        md = d.to_markdown()
        # Should still show Previous Attempt section with worktree mtime
        assert "### Previous Attempt" in md
        assert "**Worktree last modified**: 2026-01-15T10:30:00Z" in md
        # But not the progress-specific fields
        assert "**Last phase**" not in md

    def test_to_markdown_milestones_limited_to_last_five(self):
        """Verify only last 5 milestones are shown."""
        d = BuilderDiagnostics(
            worktree_path="/tmp/wt",
            issue=42,
            progress_started_at="2026-01-15T10:00:00Z",
            progress_milestones=[
                f"event_{i} at time_{i}" for i in range(10)
            ],
        )
        md = d.to_markdown()
        # Should show last 5 only
        assert "event_5" in md
        assert "event_9" in md
        assert "event_0" not in md
        assert "event_4" not in md


# ---------------------------------------------------------------------------
# _gather_builder_diagnostics
# ---------------------------------------------------------------------------


class TestGatherBuilderDiagnostics:
    def test_populates_worktree_mtime(self, tmp_path: Path):
        """Verify worktree modification time is captured."""
        repo = _make_repo(tmp_path)
        wt = tmp_path / ".loom" / "worktrees" / "issue-42"
        wt.mkdir(parents=True)
        (wt / ".git").mkdir()

        with patch("loom_tools.validate_phase._run_gh") as mock_gh:
            mock_gh.return_value = _completed(stdout="loom:building\n")
            diag = _gather_builder_diagnostics(42, str(wt), repo)

        assert diag.worktree_exists is True
        assert diag.worktree_mtime != ""
        # Should be ISO format
        assert "T" in diag.worktree_mtime
        assert "Z" in diag.worktree_mtime

    def test_populates_progress_info(self, tmp_path: Path):
        """Verify progress file info is captured."""
        repo = _make_repo(tmp_path)
        wt = tmp_path / ".loom" / "worktrees" / "issue-42"
        wt.mkdir(parents=True)
        (wt / ".git").mkdir()

        # Create progress file
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        (progress_dir / "shepherd-abc123.json").write_text(
            json.dumps({
                "task_id": "abc123",
                "issue": 42,
                "started_at": "2026-01-15T10:00:00Z",
                "current_phase": "builder",
                "last_heartbeat": "2026-01-15T10:25:00Z",
                "milestones": [
                    {"event": "started", "timestamp": "2026-01-15T10:00:00Z", "data": {"issue": 42}},
                    {"event": "phase_entered", "timestamp": "2026-01-15T10:05:00Z", "data": {"phase": "builder"}},
                ],
            })
        )

        with patch("loom_tools.validate_phase._run_gh") as mock_gh:
            mock_gh.return_value = _completed(stdout="loom:building\n")
            diag = _gather_builder_diagnostics(42, str(wt), repo)

        assert diag.progress_status == "builder"
        assert diag.progress_started_at == "2026-01-15T10:00:00Z"
        assert diag.progress_last_heartbeat == "2026-01-15T10:25:00Z"
        assert diag.progress_milestones is not None
        assert len(diag.progress_milestones) == 2
        assert "started at 2026-01-15T10:00:00Z" in diag.progress_milestones[0]

    def test_strips_ansi_from_log_tail(self, tmp_path: Path):
        """Verify ANSI sequences are stripped from log output."""
        repo = _make_repo(tmp_path)
        wt = tmp_path / ".loom" / "worktrees" / "issue-42"
        wt.mkdir(parents=True)
        (wt / ".git").mkdir()

        # Create a log file with ANSI sequences
        log_path = Path("/tmp/loom-loom-builder-issue-42.out")
        log_content = "\x1b[31mError:\x1b[0m Something failed\n\x1b[32mInfo:\x1b[0m Trying again"
        log_path.write_text(log_content)

        try:
            with patch("loom_tools.validate_phase._run_gh") as mock_gh:
                mock_gh.return_value = _completed(stdout="loom:building\n")
                diag = _gather_builder_diagnostics(42, str(wt), repo)

            # ANSI codes should be stripped
            assert "\x1b[" not in diag.log_tail
            assert "Error: Something failed" in diag.log_tail
            assert "Info: Trying again" in diag.log_tail
        finally:
            log_path.unlink(missing_ok=True)

    def test_handles_missing_progress_file(self, tmp_path: Path):
        """Verify graceful handling when no progress file exists."""
        repo = _make_repo(tmp_path)
        wt = tmp_path / ".loom" / "worktrees" / "issue-42"
        wt.mkdir(parents=True)
        (wt / ".git").mkdir()

        with patch("loom_tools.validate_phase._run_gh") as mock_gh:
            mock_gh.return_value = _completed(stdout="loom:building\n")
            diag = _gather_builder_diagnostics(42, str(wt), repo)

        # Should have empty progress fields
        assert diag.progress_status == ""
        assert diag.progress_started_at == ""
        assert diag.progress_milestones is None

    def test_handles_missing_worktree(self, tmp_path: Path):
        """Verify graceful handling when worktree doesn't exist."""
        repo = _make_repo(tmp_path)
        wt = tmp_path / ".loom" / "worktrees" / "issue-42"
        # Don't create worktree

        with patch("loom_tools.validate_phase._run_gh") as mock_gh:
            mock_gh.return_value = _completed(stdout="loom:building\n")
            diag = _gather_builder_diagnostics(42, str(wt), repo)

        assert diag.worktree_exists is False
        assert diag.worktree_mtime == ""


# ---------------------------------------------------------------------------
# validate_curator
# ---------------------------------------------------------------------------


class TestValidateCurator:
    @patch("loom_tools.validate_phase._run_gh")
    def test_label_present(self, mock_gh: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:curated\nloom:issue\n")
        result = validate_curator(42, repo)
        assert result.status == ValidationStatus.SATISFIED
        assert result.satisfied

    @patch("loom_tools.validate_phase._run_gh")
    def test_label_missing_check_only(self, mock_gh: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:issue\n")
        result = validate_curator(42, repo, check_only=True)
        assert result.status == ValidationStatus.FAILED
        assert "check-only" in result.message

    @patch("loom_tools.validate_phase.subprocess.run")
    @patch("loom_tools.validate_phase._run_gh")
    def test_recovery_success(self, mock_gh: MagicMock, mock_run: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:issue\n")
        mock_run.return_value = _completed(returncode=0)
        result = validate_curator(42, repo)
        assert result.status == ValidationStatus.RECOVERED
        assert result.recovery_action == "apply_label"

    @patch("loom_tools.validate_phase.subprocess.run")
    @patch("loom_tools.validate_phase._run_gh")
    def test_recovery_failure(self, mock_gh: MagicMock, mock_run: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:issue\n")
        mock_run.return_value = _completed(returncode=1)
        result = validate_curator(42, repo)
        assert result.status == ValidationStatus.FAILED

    @patch("loom_tools.validate_phase._run_gh")
    def test_fetch_fails(self, mock_gh: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(returncode=1)
        result = validate_curator(42, repo)
        assert result.status == ValidationStatus.FAILED
        assert "Could not fetch" in result.message


# ---------------------------------------------------------------------------
# validate_builder
# ---------------------------------------------------------------------------


class TestValidateBuilder:
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_pr_exists_with_label(self, mock_gh: MagicMock, mock_find: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        # _run_gh is called for issue state check, then for PR labels
        mock_gh.side_effect = [
            _completed(stdout="OPEN\n"),  # issue state
            _completed(stdout="loom:review-requested\n"),  # PR labels
        ]
        mock_find.return_value = (100, "branch_name")
        result = validate_builder(42, repo, check_only=True)
        assert result.status == ValidationStatus.SATISFIED

    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_pr_missing_label_check_only(self, mock_gh: MagicMock, mock_find: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.side_effect = [
            _completed(stdout="OPEN\n"),
            _completed(stdout="loom:building\n"),
        ]
        mock_find.return_value = (100, "branch_name")
        result = validate_builder(42, repo, check_only=True)
        assert result.status == ValidationStatus.FAILED
        assert "check-only" in result.message

    @patch("loom_tools.validate_phase.subprocess.run")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_pr_missing_label_recovery(self, mock_gh: MagicMock, mock_find: MagicMock, mock_run: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.side_effect = [
            _completed(stdout="OPEN\n"),           # issue state
            _completed(stdout="## Summary\nGood body.\n\nCloses #42\n"),  # PR body (ensure ref)
            _completed(stdout="fix: a good title\n"),  # PR title (generic check)
            _completed(stdout="## Summary\nGood body.\n\nCloses #42\n"),  # PR body (minimal body check)
            _completed(stdout="loom:building\n"),  # PR labels
        ]
        mock_find.return_value = (100, "closes_keyword")
        mock_run.return_value = _completed(returncode=0)
        result = validate_builder(42, repo)
        assert result.status == ValidationStatus.RECOVERED
        assert result.recovery_action == "add_label"

    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_no_pr_check_only(self, mock_gh: MagicMock, mock_find: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="OPEN\n")
        mock_find.return_value = None
        result = validate_builder(42, repo, check_only=True)
        assert result.status == ValidationStatus.FAILED

    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_issue_closed_with_pr(self, mock_gh: MagicMock, mock_find: MagicMock, tmp_path: Path):
        """Issue closed + PR exists = legitimate completion."""
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="CLOSED\n")
        mock_find.return_value = (100, "branch_name")
        result = validate_builder(42, repo)
        assert result.status == ValidationStatus.SATISFIED
        assert "closed" in result.message.lower()
        assert "PR #100" in result.message

    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_issue_closed_with_merged_pr(self, mock_gh: MagicMock, mock_find: MagicMock, tmp_path: Path):
        """Issue closed + merged PR exists = legitimate completion."""
        repo = _make_repo(tmp_path)
        mock_gh.side_effect = [
            _completed(stdout="CLOSED\n"),  # issue state
            _completed(stdout="55\n"),       # merged PR search
        ]
        mock_find.return_value = None  # no open PR
        result = validate_builder(42, repo)
        assert result.status == ValidationStatus.SATISFIED
        assert "merged PR #55" in result.message

    @patch("loom_tools.validate_phase._mark_phase_failed")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_issue_closed_without_pr_fails(self, mock_gh: MagicMock, mock_find: MagicMock, mock_phase_failed: MagicMock, tmp_path: Path):
        """Issue closed + no PR = builder abandoned issue, issue reopened."""
        repo = _make_repo(tmp_path)
        mock_gh.side_effect = [
            _completed(stdout="CLOSED\n"),  # issue state
            _completed(stdout="\n"),         # merged PR search returns nothing
            _completed(stdout=""),           # issue reopen
        ]
        mock_find.return_value = None  # no open PR
        result = validate_builder(42, repo)
        assert result.status == ValidationStatus.FAILED
        assert "abandoned" in result.message.lower()
        assert "reopened" in result.message.lower()
        # Verify issue was reopened
        reopen_call = mock_gh.call_args_list[2]
        assert "reopen" in reopen_call[0][0]
        mock_phase_failed.assert_called_once()
        call_kwargs = mock_phase_failed.call_args[1]
        assert call_kwargs.get("failure_label") == "loom:blocked"

    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_issue_closed_without_pr_check_only(self, mock_gh: MagicMock, mock_find: MagicMock, tmp_path: Path):
        """In check-only mode, closed issue without PR still fails but no side effects."""
        repo = _make_repo(tmp_path)
        mock_gh.side_effect = [
            _completed(stdout="CLOSED\n"),  # issue state
            _completed(stdout="\n"),         # merged PR search returns nothing
        ]
        mock_find.return_value = None
        result = validate_builder(42, repo, check_only=True)
        assert result.status == ValidationStatus.FAILED
        assert "abandoned" in result.message.lower()

    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._mark_phase_failed")
    @patch("loom_tools.validate_phase._run_gh")
    def test_no_pr_no_worktree(self, mock_gh: MagicMock, mock_phase_failed: MagicMock, mock_find: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="OPEN\n")
        mock_find.return_value = None
        result = validate_builder(42, repo, worktree=None)
        assert result.status == ValidationStatus.FAILED
        mock_phase_failed.assert_called_once()
        # Verify failure_label is passed
        call_kwargs = mock_phase_failed.call_args[1]
        assert call_kwargs.get("failure_label") == "loom:blocked"

    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_cached_pr_passed(self, mock_gh: MagicMock, mock_find: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.side_effect = [
            _completed(stdout="OPEN\n"),
            _completed(stdout="loom:review-requested\n"),
        ]
        mock_find.return_value = (55, "caller_cached")
        result = validate_builder(42, repo, pr_number=55, check_only=True)
        assert result.status == ValidationStatus.SATISFIED

    @patch("loom_tools.validate_phase.time.sleep")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_checkpoint_pr_created_retries_on_missing_pr(
        self, mock_gh: MagicMock, mock_find: MagicMock, mock_sleep: MagicMock, tmp_path: Path,
    ):
        """When checkpoint says pr_created but PR isn't visible yet, retry after delay (#2710)."""
        repo = _make_repo(tmp_path)
        wt = tmp_path / "worktree"
        wt.mkdir()
        # Write a pr_created checkpoint
        checkpoint_data = {"stage": "pr_created", "timestamp": "2026-01-01T00:00:00Z", "issue": 42}
        (wt / ".loom-checkpoint").write_text(json.dumps(checkpoint_data))

        # First call: no PR found; second call (after retry): PR found
        mock_find.side_effect = [None, (200, "branch_name")]
        mock_gh.side_effect = [
            _completed(stdout="OPEN\n"),           # issue state
            _completed(stdout="loom:review-requested\n"),  # PR labels
        ]
        result = validate_builder(42, repo, worktree=str(wt), check_only=True)
        assert result.status == ValidationStatus.SATISFIED
        assert mock_find.call_count == 2
        mock_sleep.assert_called_once_with(2)

    @patch("loom_tools.validate_phase.time.sleep")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_no_checkpoint_no_retry(
        self, mock_gh: MagicMock, mock_find: MagicMock, mock_sleep: MagicMock, tmp_path: Path,
    ):
        """Without a pr_created checkpoint, no retry is attempted."""
        repo = _make_repo(tmp_path)
        wt = tmp_path / "worktree"
        wt.mkdir()
        # No checkpoint file

        mock_find.return_value = None
        mock_gh.return_value = _completed(stdout="OPEN\n")
        result = validate_builder(42, repo, worktree=str(wt), check_only=True)
        assert result.status == ValidationStatus.FAILED
        assert mock_find.call_count == 1
        mock_sleep.assert_not_called()

    @patch("loom_tools.validate_phase.time.sleep")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_checkpoint_implementing_no_retry(
        self, mock_gh: MagicMock, mock_find: MagicMock, mock_sleep: MagicMock, tmp_path: Path,
    ):
        """Checkpoint at 'implementing' stage should NOT trigger retry."""
        repo = _make_repo(tmp_path)
        wt = tmp_path / "worktree"
        wt.mkdir()
        checkpoint_data = {"stage": "implementing", "timestamp": "2026-01-01T00:00:00Z", "issue": 42}
        (wt / ".loom-checkpoint").write_text(json.dumps(checkpoint_data))

        mock_find.return_value = None
        mock_gh.return_value = _completed(stdout="OPEN\n")
        result = validate_builder(42, repo, worktree=str(wt), check_only=True)
        assert result.status == ValidationStatus.FAILED
        assert mock_find.call_count == 1
        mock_sleep.assert_not_called()


# ---------------------------------------------------------------------------
# _warn_generic_pr_title
# ---------------------------------------------------------------------------


class TestWarnGenericPrTitle:
    """Tests for the generic PR title anti-pattern detection."""

    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase._run_gh")
    def test_generic_title_triggers_warning(self, mock_gh, mock_milestone, tmp_path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="feat: implement changes for issue #42\n")

        from loom_tools.validate_phase import _warn_generic_pr_title
        _warn_generic_pr_title(10, 42, repo, "task123")

        mock_milestone.assert_called_once()
        call_kwargs = mock_milestone.call_args
        assert "warning" in call_kwargs.kwargs.get("action", call_kwargs[1].get("action", ""))

    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase._run_gh")
    def test_good_title_no_warning(self, mock_gh, mock_milestone, tmp_path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="fix: validate PR title format in phase validator\n")

        from loom_tools.validate_phase import _warn_generic_pr_title
        _warn_generic_pr_title(10, 42, repo, "task123")

        mock_milestone.assert_not_called()

    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase._run_gh")
    def test_address_issue_pattern(self, mock_gh, mock_milestone, tmp_path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="fix: address issue #2557\n")

        from loom_tools.validate_phase import _warn_generic_pr_title
        _warn_generic_pr_title(10, 2557, repo, "task123")

        mock_milestone.assert_called_once()

    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase._run_gh")
    def test_bare_issue_number_pattern(self, mock_gh, mock_milestone, tmp_path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="Issue #123\n")

        from loom_tools.validate_phase import _warn_generic_pr_title
        _warn_generic_pr_title(10, 123, repo, "task123")

        mock_milestone.assert_called_once()

    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase._run_gh")
    def test_gh_failure_is_noop(self, mock_gh, mock_milestone, tmp_path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(returncode=1)

        from loom_tools.validate_phase import _warn_generic_pr_title
        _warn_generic_pr_title(10, 42, repo, "task123")

        mock_milestone.assert_not_called()


# ---------------------------------------------------------------------------
# _ensure_pr_body_references_issue tests
# ---------------------------------------------------------------------------


class TestEnsurePrBodyReferencesIssue:
    """Tests for the wrong-issue closing keyword guard."""

    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase.subprocess")
    @patch("loom_tools.validate_phase._run_gh")
    def test_correct_ref_no_change(
        self, mock_gh, mock_subprocess, mock_milestone, tmp_path,
    ):
        """PR already has correct Closes reference -- no edit needed."""
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(
            stdout="## Summary\nFix stuff.\n\nCloses #42\n",
        )

        from loom_tools.validate_phase import _ensure_pr_body_references_issue
        _ensure_pr_body_references_issue(10, 42, repo, "task123")

        mock_subprocess.run.assert_not_called()

    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase.subprocess")
    @patch("loom_tools.validate_phase._run_gh")
    def test_missing_ref_gets_added(
        self, mock_gh, mock_subprocess, mock_milestone, tmp_path,
    ):
        """PR has no closing keywords -- Closes #N is appended."""
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="## Summary\nFix stuff.\n")
        mock_subprocess.run.return_value = _completed()

        from loom_tools.validate_phase import _ensure_pr_body_references_issue
        _ensure_pr_body_references_issue(10, 42, repo, "task123")

        mock_subprocess.run.assert_called_once()
        new_body = mock_subprocess.run.call_args[0][0][5]
        assert "Closes #42" in new_body

    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase.subprocess")
    @patch("loom_tools.validate_phase._run_gh")
    def test_wrong_issue_ref_gets_removed(
        self, mock_gh, mock_subprocess, mock_milestone, tmp_path,
    ):
        """PR references wrong issue -- keyword is struck through."""
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(
            stdout="## Summary\nFix stuff.\n\nCloses #999\n",
        )
        mock_subprocess.run.return_value = _completed()

        from loom_tools.validate_phase import _ensure_pr_body_references_issue
        _ensure_pr_body_references_issue(10, 42, repo, "task123")

        mock_subprocess.run.assert_called_once()
        new_body = mock_subprocess.run.call_args[0][0][5]
        assert "~~Closes #999~~" in new_body
        assert "Closes #42" in new_body
        # Milestone reports the wrong-issue warning
        calls = [
            c for c in mock_milestone.call_args_list
            if "wrong issue" in str(c)
        ]
        assert len(calls) >= 1

    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase.subprocess")
    @patch("loom_tools.validate_phase._run_gh")
    def test_wrong_and_correct_refs_removes_only_wrong(
        self, mock_gh, mock_subprocess, mock_milestone, tmp_path,
    ):
        """PR has both correct and wrong refs -- only wrong is removed."""
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(
            stdout="## Summary\n\nCloses #999\nCloses #42\n",
        )
        mock_subprocess.run.return_value = _completed()

        from loom_tools.validate_phase import _ensure_pr_body_references_issue
        _ensure_pr_body_references_issue(10, 42, repo, "task123")

        mock_subprocess.run.assert_called_once()
        new_body = mock_subprocess.run.call_args[0][0][5]
        assert "~~Closes #999~~" in new_body
        assert "Closes #42" in new_body
        assert "~~Closes #42~~" not in new_body

    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase.subprocess")
    @patch("loom_tools.validate_phase._run_gh")
    def test_multiple_wrong_refs_all_removed(
        self, mock_gh, mock_subprocess, mock_milestone, tmp_path,
    ):
        """PR references multiple wrong issues -- all are removed."""
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(
            stdout="Fixes #100\nResolves #200\n",
        )
        mock_subprocess.run.return_value = _completed()

        from loom_tools.validate_phase import _ensure_pr_body_references_issue
        _ensure_pr_body_references_issue(10, 42, repo, "task123")

        mock_subprocess.run.assert_called_once()
        new_body = mock_subprocess.run.call_args[0][0][5]
        assert "~~Fixes #100~~" in new_body
        assert "~~Resolves #200~~" in new_body
        assert "Closes #42" in new_body

    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase._run_gh")
    def test_gh_failure_is_noop(self, mock_gh, mock_milestone, tmp_path):
        """gh pr view failure -- nothing happens."""
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(returncode=1)

        from loom_tools.validate_phase import _ensure_pr_body_references_issue
        _ensure_pr_body_references_issue(10, 42, repo, "task123")

        mock_milestone.assert_not_called()

    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase.subprocess")
    @patch("loom_tools.validate_phase._run_gh")
    def test_empty_body_gets_correct_ref(
        self, mock_gh, mock_subprocess, mock_milestone, tmp_path,
    ):
        """Empty or null body gets Closes #N."""
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="null\n")
        mock_subprocess.run.return_value = _completed()

        from loom_tools.validate_phase import _ensure_pr_body_references_issue
        _ensure_pr_body_references_issue(10, 42, repo, "task123")

        mock_subprocess.run.assert_called_once()
        new_body = mock_subprocess.run.call_args[0][0][5]
        assert new_body == "Closes #42"


# ---------------------------------------------------------------------------
# _recover_minimal_pr_body tests
# ---------------------------------------------------------------------------


class TestRecoverMinimalPrBody:
    """Tests for auto-enrichment of minimal PR bodies."""

    @patch("loom_tools.validate_phase._log_recovery_event")
    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase.subprocess")
    @patch("loom_tools.validate_phase._run_gh")
    def test_minimal_body_gets_enriched(
        self, mock_gh, mock_subprocess, mock_milestone, mock_recovery, tmp_path,
    ):
        repo = _make_repo(tmp_path)
        mock_gh.side_effect = [
            _completed(stdout="Closes #42\n"),
            _completed(stdout="src/app.py (+10/-2)\nREADME.md (+1/-0)\n"),
        ]
        mock_subprocess.run.return_value = _completed(stdout="")

        from loom_tools.validate_phase import _recover_minimal_pr_body
        _recover_minimal_pr_body(10, 42, repo, "task123")

        mock_subprocess.run.assert_called_once()
        call_args = mock_subprocess.run.call_args
        assert call_args[0][0][:4] == ["gh", "pr", "edit", "10"]
        new_body = call_args[0][0][5]
        assert "## Summary" in new_body
        assert "## Changes" in new_body
        assert "src/app.py" in new_body
        assert "Closes #42" in new_body
        mock_milestone.assert_called_once()
        mock_recovery.assert_called_once()

    @patch("loom_tools.validate_phase._log_recovery_event")
    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase.subprocess")
    @patch("loom_tools.validate_phase._run_gh")
    def test_adequate_body_no_change(
        self, mock_gh, mock_subprocess, mock_milestone, mock_recovery, tmp_path,
    ):
        repo = _make_repo(tmp_path)
        body = "## Summary\nThis PR adds feature X.\n\nCloses #42\n"
        mock_gh.return_value = _completed(stdout=body)

        from loom_tools.validate_phase import _recover_minimal_pr_body
        _recover_minimal_pr_body(10, 42, repo, "task123")

        mock_gh.assert_called_once()
        mock_subprocess.run.assert_not_called()
        mock_milestone.assert_not_called()

    @patch("loom_tools.validate_phase._log_recovery_event")
    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase.subprocess")
    @patch("loom_tools.validate_phase._run_gh")
    def test_long_body_without_summary_no_change(
        self, mock_gh, mock_subprocess, mock_milestone, mock_recovery, tmp_path,
    ):
        repo = _make_repo(tmp_path)
        body = "This is a long description that explains the change in detail. " * 5 + "\nCloses #42\n"
        mock_gh.return_value = _completed(stdout=body)

        from loom_tools.validate_phase import _recover_minimal_pr_body
        _recover_minimal_pr_body(10, 42, repo, "task123")

        mock_gh.assert_called_once()
        mock_subprocess.run.assert_not_called()

    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase._run_gh")
    def test_gh_failure_is_noop(self, mock_gh, mock_milestone, tmp_path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(returncode=1)

        from loom_tools.validate_phase import _recover_minimal_pr_body
        _recover_minimal_pr_body(10, 42, repo, "task123")

        mock_milestone.assert_not_called()

    @patch("loom_tools.validate_phase._log_recovery_event")
    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase.subprocess")
    @patch("loom_tools.validate_phase._run_gh")
    def test_empty_body_gets_enriched(
        self, mock_gh, mock_subprocess, mock_milestone, mock_recovery, tmp_path,
    ):
        repo = _make_repo(tmp_path)
        mock_gh.side_effect = [
            _completed(stdout="null\n"),
            _completed(stdout="src/main.py (+5/-3)\n"),
        ]
        mock_subprocess.run.return_value = _completed(stdout="")

        from loom_tools.validate_phase import _recover_minimal_pr_body
        _recover_minimal_pr_body(10, 42, repo, "task123")

        mock_subprocess.run.assert_called_once()
        call_args = mock_subprocess.run.call_args
        new_body = call_args[0][0][5]
        assert "## Summary" in new_body
        assert "src/main.py" in new_body


# ---------------------------------------------------------------------------
# validate_builder: no auto-recovery tests
# ---------------------------------------------------------------------------


class TestBuilderRecoveryFromUncommittedChanges:
    """Tests verifying auto-recovery from substantive uncommitted changes.

    When the builder exits with substantive uncommitted changes in the worktree,
    validate_builder should attempt mechanical recovery (stage, commit, push,
    create PR) instead of immediately failing.
    """

    def _make_worktree(self, tmp_path: Path) -> Path:
        """Create a fake worktree directory."""
        wt = tmp_path / ".loom" / "worktrees" / "issue-42"
        wt.mkdir(parents=True)
        (wt / ".git").mkdir()  # Minimal git marker
        return wt

    @patch("loom_tools.validate_phase._log_recovery_event")
    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase.subprocess.run")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_worktree_with_changes_recovers_via_commit_push_pr(
        self, mock_gh: MagicMock, mock_find: MagicMock, mock_run: MagicMock,
        mock_milestone: MagicMock, mock_log_recovery: MagicMock, tmp_path: Path,
    ):
        """Verify that substantive uncommitted changes trigger recovery."""
        repo = _make_repo(tmp_path)
        wt = self._make_worktree(tmp_path)

        # First _run_gh call: issue state check (OPEN)
        # Second _run_gh call: issue title for PR
        mock_gh.side_effect = [
            _completed(stdout="OPEN\n"),
            _completed(stdout="Fix the widget\n"),
        ]
        mock_find.return_value = None  # no existing PR

        def side_effect_fn(*args, **kwargs):
            cmd = args[0] if args else kwargs.get("args", [])
            cmd_str = " ".join(str(c) for c in cmd)
            # git -C worktree status --porcelain
            if "status" in cmd and "--porcelain" in cmd:
                return _completed(stdout=" M src/file.py\n M src/other.py\n")
            # git add
            if "git" in cmd_str and "add" in cmd:
                return _completed()
            # git commit
            if "git" in cmd_str and "commit" in cmd:
                return _completed()
            # git push
            if "git" in cmd_str and "push" in cmd:
                return _completed()
            # gh pr create
            if "gh" in cmd_str and "pr" in cmd and "create" in cmd:
                return _completed(stdout="https://github.com/org/repo/pull/99\n")
            return _completed()

        mock_run.side_effect = side_effect_fn

        result = validate_builder(42, repo, worktree=str(wt))

        assert result.status == ValidationStatus.RECOVERED
        assert "commit_and_pr" == result.recovery_action
        mock_log_recovery.assert_called_once()

    @patch("loom_tools.validate_phase.subprocess.run")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_recovery_fails_on_git_add_marks_failed(
        self, mock_gh: MagicMock, mock_find: MagicMock, mock_run: MagicMock, tmp_path: Path,
    ):
        """Verify that failed git add results in FAILED status with diagnostics."""
        repo = _make_repo(tmp_path)
        wt = self._make_worktree(tmp_path)

        mock_gh.return_value = _completed(stdout="OPEN\n")
        mock_find.return_value = None

        def side_effect_fn(*args, **kwargs):
            cmd = args[0] if args else kwargs.get("args", [])
            cmd_str = " ".join(str(c) for c in cmd)
            if "status" in cmd and "--porcelain" in cmd:
                return _completed(stdout=" M src/file.py\n")
            if "git" in cmd_str and "add" in cmd:
                return _completed(returncode=1, stderr="fatal: error")
            # For _mark_phase_failed and _gather_builder_diagnostics
            if "rev-parse" in cmd_str:
                return _completed(stdout="feature/issue-42\n")
            if "rev-list" in cmd_str:
                return _completed(stdout="0\n")
            if "ls-remote" in cmd_str:
                return _completed(stdout="")
            if "pr" in cmd_str and "list" in cmd_str:
                return _completed(stdout="")
            if "issue" in cmd_str and "view" in cmd_str:
                return _completed(stdout="loom:building\n")
            return _completed()

        mock_run.side_effect = side_effect_fn

        result = validate_builder(42, repo, worktree=str(wt))

        assert result.status == ValidationStatus.FAILED
        assert "could not stage changes" in result.message.lower()

    @patch("loom_tools.validate_phase.subprocess.run")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_only_marker_files_still_fails(
        self, mock_gh: MagicMock, mock_find: MagicMock, mock_run: MagicMock, tmp_path: Path,
    ):
        """Verify that only marker/infrastructure files do not trigger recovery."""
        repo = _make_repo(tmp_path)
        wt = self._make_worktree(tmp_path)

        mock_gh.return_value = _completed(stdout="OPEN\n")
        mock_find.return_value = None

        def side_effect_fn(*args, **kwargs):
            cmd = args[0] if args else kwargs.get("args", [])
            cmd_str = " ".join(str(c) for c in cmd)
            if "status" in cmd and "--porcelain" in cmd:
                return _completed(stdout="?? .loom-in-use\n")
            # For _mark_phase_failed and _gather_builder_diagnostics
            if "rev-parse" in cmd_str:
                return _completed(stdout="feature/issue-42\n")
            if "rev-list" in cmd_str:
                return _completed(stdout="0\n")
            if "ls-remote" in cmd_str:
                return _completed(stdout="")
            if "pr" in cmd_str and "list" in cmd_str:
                return _completed(stdout="")
            if "issue" in cmd_str and "view" in cmd_str:
                return _completed(stdout="loom:building\n")
            return _completed()

        mock_run.side_effect = side_effect_fn

        result = validate_builder(42, repo, worktree=str(wt))

        assert result.status == ValidationStatus.FAILED
        assert "marker files" in result.message.lower()

    @patch("loom_tools.validate_phase.subprocess.run")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_committed_no_changes_needed_marker_skips_recovery_pr(
        self, mock_gh: MagicMock, mock_find: MagicMock, mock_run: MagicMock, tmp_path: Path,
    ):
        """Verify that a committed .no-changes-needed marker (no uncommitted changes)
        does not trigger a recovery PR.

        This tests the parallel case to test_only_marker_files_still_fails:
        the builder committed .no-changes-needed and exited cleanly, leaving
        the worktree with no uncommitted changes but one unpushed commit.
        validate_builder should detect this and return FAILED without creating a PR.
        """
        repo = _make_repo(tmp_path)
        wt = self._make_worktree(tmp_path)

        mock_gh.return_value = _completed(stdout="OPEN\n")
        mock_find.return_value = None

        def side_effect_fn(*args, **kwargs):
            cmd = args[0] if args else kwargs.get("args", [])
            cmd_str = " ".join(str(c) for c in cmd)
            # git status --porcelain: empty (nothing uncommitted)
            if "status" in cmd and "--porcelain" in cmd:
                return _completed(stdout="")
            # git log @{upstream}..HEAD: non-empty (one unpushed commit)
            if "log" in cmd and "--oneline" in cmd:
                return _completed(stdout="abc1234 Builder: committed no-changes-needed\n")
            # git diff --name-only @{upstream}..HEAD: only .no-changes-needed
            if "diff" in cmd and "--name-only" in cmd:
                return _completed(stdout=".no-changes-needed\n")
            # For _mark_phase_failed and _gather_builder_diagnostics
            if "rev-parse" in cmd_str:
                return _completed(stdout="feature/issue-42\n")
            if "rev-list" in cmd_str:
                return _completed(stdout="0\n")
            if "ls-remote" in cmd_str:
                return _completed(stdout="")
            if "pr" in cmd_str and "list" in cmd_str:
                return _completed(stdout="")
            if "issue" in cmd_str and "view" in cmd_str:
                return _completed(stdout="loom:building\n")
            return _completed()

        mock_run.side_effect = side_effect_fn

        result = validate_builder(42, repo, worktree=str(wt))

        assert result.status == ValidationStatus.FAILED
        assert "no-changes-needed" in result.message.lower() or "no substantive changes" in result.message.lower()
        # Verify no gh pr create call was made
        for call in mock_run.call_args_list:
            cmd = call.args[0] if call.args else call.kwargs.get("args", [])
            # Only check actual subprocess.run calls (list args), not _run_gh calls
            if not isinstance(cmd, list):
                continue
            assert not (len(cmd) >= 4 and cmd[0] == "gh" and cmd[1] == "pr" and cmd[2] == "create"), (
                f"gh pr create should not have been called, but got: {cmd}"
            )


# ---------------------------------------------------------------------------
# Rate-limited builder exit detection (issue #2774)
# ---------------------------------------------------------------------------


class TestRateLimitedBuilderExit:
    """Tests for rate-limit detection and differentiated recovery PR messaging."""

    def test_detects_rate_limit_pattern_in_log(self, tmp_path: Path):
        """_is_rate_limited_builder_exit returns True when log contains /rate-limit-options."""
        repo = _make_repo(tmp_path)
        logs_dir = tmp_path / ".loom" / "logs"
        logs_dir.mkdir(parents=True)
        log_file = logs_dir / "loom-builder-issue-42.log"
        log_file.write_text(
            "Some output...\n"
            "❯ /rate-limit-options\n"
            "What do you want to do?\n"
            "❯ 1. Stop and wait for limit to reset\n"
        )
        assert _is_rate_limited_builder_exit(42, repo) is True

    def test_no_rate_limit_pattern_returns_false(self, tmp_path: Path):
        """_is_rate_limited_builder_exit returns False when log has normal output."""
        repo = _make_repo(tmp_path)
        logs_dir = tmp_path / ".loom" / "logs"
        logs_dir.mkdir(parents=True)
        log_file = logs_dir / "loom-builder-issue-42.log"
        log_file.write_text("Normal builder output.\nAll tests passed.\n")
        assert _is_rate_limited_builder_exit(42, repo) is False

    def test_no_log_file_returns_false(self, tmp_path: Path):
        """_is_rate_limited_builder_exit returns False when no log exists."""
        repo = _make_repo(tmp_path)
        assert _is_rate_limited_builder_exit(42, repo) is False

    def test_no_logs_dir_returns_false(self, tmp_path: Path):
        """_is_rate_limited_builder_exit returns False when logs dir missing."""
        repo = _make_repo(tmp_path)
        # Don't create .loom/logs
        assert _is_rate_limited_builder_exit(42, repo) is False

    def test_uses_most_recent_log_with_retry_suffix(self, tmp_path: Path):
        """When multiple logs exist (retries), checks the most recent one."""
        repo = _make_repo(tmp_path)
        logs_dir = tmp_path / ".loom" / "logs"
        logs_dir.mkdir(parents=True)

        # Older log without rate limit
        old_log = logs_dir / "loom-builder-issue-42.log"
        old_log.write_text("Normal output.\n")

        # Newer retry log with rate limit
        import time
        time.sleep(0.05)  # ensure different mtime
        new_log = logs_dir / "loom-builder-issue-42-a1.log"
        new_log.write_text("Output...\n/rate-limit-options\n")

        assert _is_rate_limited_builder_exit(42, repo) is True

    @patch("loom_tools.validate_phase.subprocess.run")
    def test_recovery_pr_body_rate_limited(self, mock_run: MagicMock):
        """Rate-limited recovery PR body uses less cautionary messaging."""
        mock_run.return_value = _completed(stdout="")

        body = _build_recovery_pr_body(42, "/tmp/wt", rate_limited=True)

        assert "rate-limited after completing work" in body
        assert "examine the diff carefully" not in body
        assert "Confirm tests pass" in body

    @patch("loom_tools.validate_phase.subprocess.run")
    def test_recovery_pr_body_not_rate_limited(self, mock_run: MagicMock):
        """Non-rate-limited recovery PR body uses cautionary messaging."""
        mock_run.return_value = _completed(stdout="")

        body = _build_recovery_pr_body(42, "/tmp/wt", rate_limited=False)

        assert "examine the diff carefully" in body
        assert "rate-limited" not in body
        assert "Review diff carefully" in body

    def test_recovery_pr_body_uses_pr_body_file_with_closes(self, tmp_path: Path):
        """When .loom/pr-body.md exists and contains Closes #N, return it as-is."""
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        pr_body_content = "## Summary\nFix the widget.\n\nCloses #42"
        (loom_dir / "pr-body.md").write_text(pr_body_content)

        body = _build_recovery_pr_body(42, str(tmp_path))

        assert body == pr_body_content
        assert "Closes #42" in body
        # Should not include the fallback recovery note
        assert "examine the diff carefully" not in body

    def test_recovery_pr_body_uses_pr_body_file_appends_closes_when_missing(
        self, tmp_path: Path
    ):
        """When .loom/pr-body.md lacks Closes #N, it is appended."""
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "pr-body.md").write_text("## Summary\nFix the widget.")

        body = _build_recovery_pr_body(42, str(tmp_path))

        assert "Closes #42" in body
        assert body.endswith("\n\nCloses #42")
        assert "examine the diff carefully" not in body

    @patch("loom_tools.validate_phase.subprocess.run")
    def test_recovery_pr_body_no_file_uses_fallback_diff_stats(
        self, mock_run: MagicMock, tmp_path: Path
    ):
        """When .loom/pr-body.md does not exist, fall back to diff-stats body."""
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        # No pr-body.md created
        mock_run.return_value = _completed(stdout="src/foo.py | 5 ++---\n")

        body = _build_recovery_pr_body(42, str(tmp_path))

        assert "Closes #42" in body
        assert "examine the diff carefully" in body
        assert "src/foo.py | 5 ++---" in body

    @patch("loom_tools.validate_phase.subprocess.run")
    def test_recovery_pr_body_rate_limited_no_file_uses_rate_limited_fallback(
        self, mock_run: MagicMock, tmp_path: Path
    ):
        """When rate_limited=True and no pr-body.md, use rate-limited fallback messaging."""
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        # No pr-body.md created
        mock_run.return_value = _completed(stdout="")

        body = _build_recovery_pr_body(42, str(tmp_path), rate_limited=True)

        assert "Closes #42" in body
        assert "rate-limited after completing work" in body
        assert "examine the diff carefully" not in body

    @patch("loom_tools.validate_phase._log_recovery_event")
    @patch("loom_tools.validate_phase._report_milestone")
    @patch("loom_tools.validate_phase.subprocess.run")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_recovery_detects_rate_limit_and_adjusts_messaging(
        self, mock_gh: MagicMock, mock_find: MagicMock, mock_run: MagicMock,
        mock_milestone: MagicMock, mock_log_recovery: MagicMock, tmp_path: Path,
    ):
        """Recovery flow detects rate-limited exit and passes flag through."""
        repo = _make_repo(tmp_path)
        wt = tmp_path / ".loom" / "worktrees" / "issue-42"
        wt.mkdir(parents=True)
        (wt / ".git").mkdir()

        # Create builder log with rate-limit pattern
        logs_dir = tmp_path / ".loom" / "logs"
        logs_dir.mkdir(parents=True)
        (logs_dir / "loom-builder-issue-42.log").write_text(
            "Working...\n/rate-limit-options\n"
        )

        mock_gh.side_effect = [
            _completed(stdout="OPEN\n"),
            _completed(stdout="Fix widget\n"),
        ]
        mock_find.return_value = None

        def side_effect_fn(*args, **kwargs):
            cmd = args[0] if args else kwargs.get("args", [])
            cmd_str = " ".join(str(c) for c in cmd)
            if "status" in cmd and "--porcelain" in cmd:
                return _completed(stdout=" M src/file.py\n")
            if "git" in cmd_str and "add" in cmd:
                return _completed()
            if "git" in cmd_str and "commit" in cmd:
                return _completed()
            if "git" in cmd_str and "push" in cmd:
                return _completed()
            if "gh" in cmd_str and "pr" in cmd and "create" in cmd:
                return _completed(stdout="https://github.com/org/repo/pull/99\n")
            return _completed()

        mock_run.side_effect = side_effect_fn

        result = validate_builder(42, repo, worktree=str(wt))

        assert result.status == ValidationStatus.RECOVERED
        assert "rate-limited" in result.message

        # Verify the PR body passed to gh pr create contains rate-limited messaging
        pr_create_calls = [
            call for call in mock_run.call_args_list
            if any("pr" in str(a) for a in call[0][0]) and any("create" in str(a) for a in call[0][0])
        ]
        assert len(pr_create_calls) == 1
        pr_body_arg = pr_create_calls[0][0][0]  # get the args list
        body_idx = pr_body_arg.index("--body") + 1
        pr_body = pr_body_arg[body_idx]
        assert "rate-limited after completing work" in pr_body

        # Verify recovery event logged with rate_limited reason
        mock_log_recovery.assert_called_once()
        call_kwargs = mock_log_recovery.call_args[1]
        assert call_kwargs.get("builder_exit_reason") == "rate_limited"
        assert call_kwargs.get("reason") == "rate_limited" if "reason" in call_kwargs else mock_log_recovery.call_args[0][2] == "rate_limited"


# ---------------------------------------------------------------------------
# quiet mode (issue #2609)
# ---------------------------------------------------------------------------


class TestQuietModeSuppressesSideEffects:
    """Tests that quiet=True suppresses diagnostic comments and label changes.

    When the shepherd's retry loop calls validate_builder() on intermediate
    attempts, quiet=True prevents noisy comments from accumulating on the
    issue even if the shepherd later recovers (issue #2609).
    """

    @patch("loom_tools.validate_phase.subprocess.run")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_builder_quiet_skips_comment_and_label(
        self, mock_gh: MagicMock, mock_find: MagicMock, mock_run: MagicMock, tmp_path: Path,
    ):
        """quiet=True should not post comments or change labels on failure."""
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="OPEN\n")
        mock_find.return_value = None  # no PR
        mock_run.return_value = _completed()
        result = validate_builder(42, repo, worktree=None, quiet=True)
        assert result.status == ValidationStatus.FAILED
        # No subprocess calls should have been made (quiet skips _mark_phase_failed entirely)
        mock_run.assert_not_called()

    @patch("loom_tools.validate_phase._mark_phase_failed")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_builder_quiet_still_attempts_recovery(
        self, mock_gh: MagicMock, mock_find: MagicMock, mock_mark: MagicMock, tmp_path: Path,
    ):
        """quiet=True should still attempt mechanical recovery (stage/commit/push/PR)."""
        repo = _make_repo(tmp_path)
        wt = tmp_path / ".loom" / "worktrees" / "issue-42"
        wt.mkdir(parents=True)
        (wt / ".git").mkdir()

        mock_gh.side_effect = [
            _completed(stdout="OPEN\n"),  # issue state
            _completed(stdout="Fix the widget\n"),  # issue title for PR
        ]
        mock_find.return_value = None

        recovery_calls = []

        def side_effect_fn(*args, **kwargs):
            cmd = args[0] if args else kwargs.get("args", [])
            cmd_str = " ".join(str(c) for c in cmd)
            if "status" in cmd and "--porcelain" in cmd:
                return _completed(stdout=" M src/file.py\n")
            if "git" in cmd_str and "add" in cmd:
                recovery_calls.append("git_add")
                return _completed()
            if "git" in cmd_str and "commit" in cmd:
                recovery_calls.append("git_commit")
                return _completed()
            if "git" in cmd_str and "push" in cmd:
                recovery_calls.append("git_push")
                return _completed()
            if "gh" in cmd_str and "pr" in cmd and "create" in cmd:
                recovery_calls.append("pr_create")
                return _completed(stdout="https://github.com/org/repo/pull/99\n")
            return _completed()

        with patch("loom_tools.validate_phase.subprocess.run", side_effect=side_effect_fn), \
             patch("loom_tools.validate_phase._log_recovery_event"), \
             patch("loom_tools.validate_phase._report_milestone"):
            result = validate_builder(42, repo, worktree=str(wt), quiet=True)

        # Recovery should have run the full mechanical pipeline
        assert "git_add" in recovery_calls
        assert "git_commit" in recovery_calls
        assert "git_push" in recovery_calls
        assert "pr_create" in recovery_calls
        assert result.status == ValidationStatus.RECOVERED
        # And _mark_phase_failed should NOT have been called
        mock_mark.assert_not_called()

    @patch("loom_tools.validate_phase.subprocess.run")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_builder_quiet_recovery_failure_no_comment(
        self, mock_gh: MagicMock, mock_find: MagicMock, mock_run: MagicMock, tmp_path: Path,
    ):
        """When recovery fails with quiet=True, no comment or label change should happen."""
        repo = _make_repo(tmp_path)
        wt = tmp_path / ".loom" / "worktrees" / "issue-42"
        wt.mkdir(parents=True)
        (wt / ".git").mkdir()

        mock_gh.return_value = _completed(stdout="OPEN\n")
        mock_find.return_value = None

        gh_issue_comment_calls = []

        def side_effect_fn(*args, **kwargs):
            cmd = args[0] if args else kwargs.get("args", [])
            cmd_str = " ".join(str(c) for c in cmd)
            if "status" in cmd and "--porcelain" in cmd:
                return _completed(stdout=" M src/file.py\n")
            if "git" in cmd_str and "add" in cmd:
                return _completed(returncode=1, stderr="fatal: error")
            # Track any gh issue comment or edit calls
            if "issue" in cmd_str and "comment" in cmd_str:
                gh_issue_comment_calls.append(cmd)
            if "issue" in cmd_str and "edit" in cmd_str:
                gh_issue_comment_calls.append(cmd)
            # For diagnostics gathering
            if "rev-parse" in cmd_str:
                return _completed(stdout="feature/issue-42\n")
            if "rev-list" in cmd_str:
                return _completed(stdout="0\n")
            return _completed()

        mock_run.side_effect = side_effect_fn

        result = validate_builder(42, repo, worktree=str(wt), quiet=True)
        assert result.status == ValidationStatus.FAILED
        # No gh issue comment or edit calls should have been made
        assert len(gh_issue_comment_calls) == 0

    @patch("loom_tools.validate_phase.subprocess.run")
    @patch("loom_tools.validate_phase._run_gh")
    def test_judge_quiet_skips_comment_and_label(
        self, mock_gh: MagicMock, mock_run: MagicMock, tmp_path: Path,
    ):
        """Judge quiet=True should not post comments or change labels."""
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:review-requested\n")
        mock_run.return_value = _completed()
        result = validate_judge(42, repo, pr_number=100, quiet=True)
        assert result.status == ValidationStatus.FAILED
        # No subprocess calls for gh issue comment/edit
        mock_run.assert_not_called()

    @patch("loom_tools.validate_phase.subprocess.run")
    @patch("loom_tools.validate_phase._run_gh")
    def test_doctor_quiet_skips_comment_and_label(
        self, mock_gh: MagicMock, mock_run: MagicMock, tmp_path: Path,
    ):
        """Doctor quiet=True should not post comments or change labels."""
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:pr\n")
        mock_run.return_value = _completed()
        result = validate_doctor(42, repo, pr_number=100, quiet=True)
        assert result.status == ValidationStatus.FAILED
        # No subprocess calls for gh issue comment/edit
        mock_run.assert_not_called()


# ---------------------------------------------------------------------------
# validate_judge
# ---------------------------------------------------------------------------


class TestValidateJudge:
    @patch("loom_tools.validate_phase._run_gh")
    def test_loom_pr_label(self, mock_gh: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:pr\n")
        result = validate_judge(42, repo, pr_number=100)
        assert result.status == ValidationStatus.SATISFIED
        assert "approved" in result.message.lower()

    @patch("loom_tools.validate_phase._run_gh")
    def test_changes_requested_label(self, mock_gh: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:changes-requested\n")
        result = validate_judge(42, repo, pr_number=100)
        assert result.status == ValidationStatus.SATISFIED
        assert "changes requested" in result.message.lower()

    @patch("loom_tools.validate_phase._mark_phase_failed")
    @patch("loom_tools.validate_phase._run_gh")
    def test_neither_label(self, mock_gh: MagicMock, mock_phase_failed: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:review-requested\n")
        result = validate_judge(42, repo, pr_number=100)
        assert result.status == ValidationStatus.FAILED
        mock_phase_failed.assert_called_once()
        # Verify failure_label is passed
        call_kwargs = mock_phase_failed.call_args[1]
        assert call_kwargs.get("failure_label") == "loom:blocked"

    @patch("loom_tools.validate_phase._run_gh")
    def test_neither_label_check_only(self, mock_gh: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:review-requested\n")
        result = validate_judge(42, repo, pr_number=100, check_only=True)
        assert result.status == ValidationStatus.FAILED

    @patch("loom_tools.validate_phase._run_gh")
    def test_doctor_fixed_intermediate_state_message(self, mock_gh: MagicMock, tmp_path: Path):
        """Issue #1998: Verify informative message when Doctor fixed but Judge hasn't applied outcome."""
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:review-requested\n")
        result = validate_judge(42, repo, pr_number=100, check_only=True)
        assert result.status == ValidationStatus.FAILED
        # Should include context about Doctor having applied fixes
        assert "loom:review-requested" in result.message
        assert "Doctor applied fixes" in result.message

    @patch("loom_tools.validate_phase._mark_blocked")
    @patch("loom_tools.validate_phase._run_gh")
    def test_no_labels_at_all_message(self, mock_gh: MagicMock, mock_blocked: MagicMock, tmp_path: Path):
        """Verify message when no loom labels present (not Doctor-fixed intermediate state)."""
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="")
        result = validate_judge(42, repo, pr_number=100)
        assert result.status == ValidationStatus.FAILED
        # Should NOT mention Doctor-fixed state
        assert "Doctor applied fixes" not in result.message
        assert "did not produce" in result.message

    def test_no_pr_number(self, tmp_path: Path):
        repo = _make_repo(tmp_path)
        result = validate_judge(42, repo)
        assert result.status == ValidationStatus.FAILED
        assert "required" in result.message.lower()

    @patch("loom_tools.validate_phase._run_gh")
    def test_fetch_fails(self, mock_gh: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(returncode=1)
        result = validate_judge(42, repo, pr_number=100)
        assert result.status == ValidationStatus.FAILED


# ---------------------------------------------------------------------------
# validate_doctor
# ---------------------------------------------------------------------------


class TestValidateDoctor:
    @patch("loom_tools.validate_phase._run_gh")
    def test_review_requested(self, mock_gh: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:review-requested\n")
        result = validate_doctor(42, repo, pr_number=100)
        assert result.status == ValidationStatus.SATISFIED

    @patch("loom_tools.validate_phase._mark_phase_failed")
    @patch("loom_tools.validate_phase._run_gh")
    def test_missing_label(self, mock_gh: MagicMock, mock_phase_failed: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:pr\n")
        result = validate_doctor(42, repo, pr_number=100)
        assert result.status == ValidationStatus.FAILED
        mock_phase_failed.assert_called_once()
        # Verify failure_label is passed
        call_kwargs = mock_phase_failed.call_args[1]
        assert call_kwargs.get("failure_label") == "loom:blocked"

    @patch("loom_tools.validate_phase._run_gh")
    def test_missing_label_check_only(self, mock_gh: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:pr\n")
        result = validate_doctor(42, repo, pr_number=100, check_only=True)
        assert result.status == ValidationStatus.FAILED

    def test_no_pr_number(self, tmp_path: Path):
        repo = _make_repo(tmp_path)
        result = validate_doctor(42, repo)
        assert result.status == ValidationStatus.FAILED


# ---------------------------------------------------------------------------
# validate_phase (dispatch)
# ---------------------------------------------------------------------------


class TestValidatePhase:
    def test_invalid_phase(self, tmp_path: Path):
        repo = _make_repo(tmp_path)
        result = validate_phase("invalid", 1, repo)
        assert result.status == ValidationStatus.FAILED
        assert "Invalid phase" in result.message

    @patch("loom_tools.validate_phase._run_gh")
    def test_dispatch_curator(self, mock_gh: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:curated\n")
        result = validate_phase("curator", 42, repo)
        assert result.status == ValidationStatus.SATISFIED

    @patch("loom_tools.validate_phase._run_gh")
    def test_dispatch_judge(self, mock_gh: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:pr\n")
        result = validate_phase("judge", 42, repo, pr_number=10)
        assert result.status == ValidationStatus.SATISFIED

    @patch("loom_tools.validate_phase._run_gh")
    def test_dispatch_doctor(self, mock_gh: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:review-requested\n")
        result = validate_phase("doctor", 42, repo, pr_number=10)
        assert result.status == ValidationStatus.SATISFIED


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


class TestCLI:
    def test_parse_args_minimal(self):
        args = _parse_args(["curator", "42"])
        assert args.phase == "curator"
        assert args.issue == 42
        assert args.json_output is False
        assert args.check_only is False

    def test_parse_args_all_flags(self):
        args = _parse_args([
            "builder", "100",
            "--worktree", "/tmp/wt",
            "--pr", "55",
            "--task-id", "abc",
            "--json",
            "--check-only",
        ])
        assert args.phase == "builder"
        assert args.issue == 100
        assert args.worktree == "/tmp/wt"
        assert args.pr_number == 55
        assert args.task_id == "abc"
        assert args.json_output is True
        assert args.check_only is True

    def test_parse_args_invalid_phase(self):
        with pytest.raises(SystemExit):
            _parse_args(["invalid", "42"])

    @patch("loom_tools.validate_phase.validate_phase")
    def test_main_json_output(self, mock_validate: MagicMock, capsys):
        mock_validate.return_value = ValidationResult(
            "curator", 42, ValidationStatus.SATISFIED, "ok",
        )
        with pytest.raises(SystemExit) as exc:
            main(["curator", "42", "--json"])
        assert exc.value.code == 0
        captured = capsys.readouterr()
        data = json.loads(captured.out)
        assert data["status"] == "satisfied"

    @patch("loom_tools.validate_phase.validate_phase")
    def test_main_text_output_satisfied(self, mock_validate: MagicMock, capsys):
        mock_validate.return_value = ValidationResult(
            "curator", 42, ValidationStatus.SATISFIED, "has label",
        )
        with pytest.raises(SystemExit) as exc:
            main(["curator", "42"])
        assert exc.value.code == 0
        captured = capsys.readouterr()
        assert "\u2713" in captured.out  # ✓

    @patch("loom_tools.validate_phase.validate_phase")
    def test_main_text_output_failed(self, mock_validate: MagicMock, capsys):
        mock_validate.return_value = ValidationResult(
            "judge", 42, ValidationStatus.FAILED, "no decision",
        )
        with pytest.raises(SystemExit) as exc:
            main(["judge", "42", "--pr", "10"])
        assert exc.value.code == 1
        captured = capsys.readouterr()
        assert "\u2717" in captured.out  # ✗

    @patch("loom_tools.validate_phase.validate_phase")
    def test_main_exit_code_recovered(self, mock_validate: MagicMock):
        mock_validate.return_value = ValidationResult(
            "curator", 42, ValidationStatus.RECOVERED, "fixed", "apply_label",
        )
        with pytest.raises(SystemExit) as exc:
            main(["curator", "42"])
        assert exc.value.code == 0  # recovered counts as success
