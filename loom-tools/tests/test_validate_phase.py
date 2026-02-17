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
            _completed(stdout="OPEN\n"),
            _completed(stdout="loom:building\n"),
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
        """Issue closed + no PR = builder abandoned issue."""
        repo = _make_repo(tmp_path)
        mock_gh.side_effect = [
            _completed(stdout="CLOSED\n"),  # issue state
            _completed(stdout="\n"),         # merged PR search returns nothing
        ]
        mock_find.return_value = None  # no open PR
        result = validate_builder(42, repo)
        assert result.status == ValidationStatus.FAILED
        assert "abandoned" in result.message.lower()
        mock_phase_failed.assert_called_once()
        call_kwargs = mock_phase_failed.call_args[1]
        assert call_kwargs.get("failure_label") == "loom:failed:builder"

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
        assert call_kwargs.get("failure_label") == "loom:failed:builder"

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


# ---------------------------------------------------------------------------
# validate_builder: no auto-recovery tests
# ---------------------------------------------------------------------------


class TestBuilderNoAutoRecovery:
    """Tests verifying auto-recovery was removed from builder validation.

    Note: Auto-recovery (commit, push, create PR) was removed in favor of
    explicit failure labels. These tests verify the new behavior.
    """

    def _make_worktree(self, tmp_path: Path) -> Path:
        """Create a fake worktree directory."""
        wt = tmp_path / ".loom" / "worktrees" / "issue-42"
        wt.mkdir(parents=True)
        (wt / ".git").mkdir()  # Minimal git marker
        return wt

    @patch("loom_tools.validate_phase.subprocess.run")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_worktree_with_changes_fails_instead_of_recovering(
        self, mock_gh: MagicMock, mock_find: MagicMock, mock_run: MagicMock, tmp_path: Path
    ):
        """Verify that uncommitted changes result in failure, not recovery."""
        repo = _make_repo(tmp_path)
        wt = self._make_worktree(tmp_path)

        mock_gh.return_value = _completed(stdout="OPEN\n")  # issue state
        mock_find.return_value = None  # no existing PR

        def side_effect_fn(*args, **kwargs):
            cmd = args[0] if args else kwargs.get("args", [])
            # git -C worktree status --porcelain
            if "status" in cmd and "--porcelain" in cmd:
                return _completed(stdout="M src/file.py\n")  # Has changes
            # git -C worktree log --oneline @{upstream}..HEAD
            if "log" in cmd and "@{upstream}" in cmd:
                return _completed(stdout="abc123 Some commit\n")  # Has unpushed commits
            # gh issue edit (for _mark_phase_failed)
            if "gh" in cmd and "issue" in cmd and "edit" in cmd:
                return _completed()
            # gh issue comment (for _mark_phase_failed)
            if "gh" in cmd and "issue" in cmd and "comment" in cmd:
                return _completed()
            return _completed()

        mock_run.side_effect = side_effect_fn

        result = validate_builder(42, repo, worktree=str(wt))

        # Should FAIL, not RECOVER - no auto-recovery anymore
        assert result.status == ValidationStatus.FAILED
        assert "no auto-recovery" in result.message.lower()

    @patch("loom_tools.validate_phase.subprocess.run")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_failure_applies_correct_label(
        self, mock_gh: MagicMock, mock_find: MagicMock, mock_run: MagicMock, tmp_path: Path
    ):
        """Verify failure applies loom:failed:builder label."""
        repo = _make_repo(tmp_path)
        wt = self._make_worktree(tmp_path)

        mock_gh.return_value = _completed(stdout="OPEN\n")
        mock_find.return_value = None

        label_calls = []

        def side_effect_fn(*args, **kwargs):
            cmd = args[0] if args else kwargs.get("args", [])
            if "status" in cmd and "--porcelain" in cmd:
                return _completed(stdout="M src/file.py\n")
            if "log" in cmd and "@{upstream}" in cmd:
                return _completed(stdout="abc123 Commit\n")
            if "gh" in cmd and "issue" in cmd and "edit" in cmd:
                label_calls.append(cmd)
                return _completed()
            if "gh" in cmd and "issue" in cmd and "comment" in cmd:
                return _completed()
            return _completed()

        mock_run.side_effect = side_effect_fn

        result = validate_builder(42, repo, worktree=str(wt))

        assert result.status == ValidationStatus.FAILED
        assert len(label_calls) == 1
        assert "loom:failed:builder" in label_calls[0]


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
        assert call_kwargs.get("failure_label") == "loom:failed:judge"

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
        assert call_kwargs.get("failure_label") == "loom:failed:doctor"

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
