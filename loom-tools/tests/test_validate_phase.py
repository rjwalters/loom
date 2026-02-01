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
    def test_issue_closed(self, mock_gh: MagicMock, mock_find: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="CLOSED\n")
        result = validate_builder(42, repo)
        assert result.status == ValidationStatus.SATISFIED
        assert "closed" in result.message.lower()

    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._mark_blocked")
    @patch("loom_tools.validate_phase._run_gh")
    def test_no_pr_no_worktree(self, mock_gh: MagicMock, mock_blocked: MagicMock, mock_find: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="OPEN\n")
        mock_find.return_value = None
        result = validate_builder(42, repo, worktree=None)
        assert result.status == ValidationStatus.FAILED
        mock_blocked.assert_called_once()

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
# validate_builder: recovery PR body tests
# ---------------------------------------------------------------------------


class TestBuilderRecoveryPRBody:
    """Tests for enhanced PR body in builder worktree recovery."""

    def _make_worktree(self, tmp_path: Path) -> Path:
        """Create a fake worktree directory."""
        wt = tmp_path / ".loom" / "worktrees" / "issue-42"
        wt.mkdir(parents=True)
        (wt / ".git").mkdir()  # Minimal git marker
        return wt

    @patch("loom_tools.validate_phase.subprocess.run")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_recovery_pr_body_includes_commits(
        self, mock_gh: MagicMock, mock_find: MagicMock, mock_run: MagicMock, tmp_path: Path
    ):
        """Verify commit messages are included in recovery PR body."""
        repo = _make_repo(tmp_path)
        wt = self._make_worktree(tmp_path)

        # Setup mocks
        mock_gh.return_value = _completed(stdout="OPEN\n")  # issue state
        mock_find.return_value = None  # no existing PR

        # Track subprocess calls to verify PR body
        pr_body_captured = []

        def side_effect_fn(*args, **kwargs):
            cmd = args[0] if args else kwargs.get("args", [])
            # git -C worktree status --porcelain
            if "status" in cmd and "--porcelain" in cmd:
                return _completed(stdout="M src/file.py\n")
            # git -C worktree add -A
            if "add" in cmd and "-A" in cmd:
                return _completed()
            # git -C worktree commit
            if "commit" in cmd:
                return _completed()
            # git -C worktree rev-parse --abbrev-ref HEAD
            if "rev-parse" in cmd and "--abbrev-ref" in cmd and "HEAD" in cmd:
                return _completed(stdout="feature/issue-42\n")
            # git -C worktree rev-parse --abbrev-ref @{upstream}
            if "rev-parse" in cmd and "@{upstream}" in cmd:
                return _completed(returncode=1)  # no upstream
            # git -C worktree push
            if "push" in cmd:
                return _completed()
            # git -C worktree log --oneline main..HEAD
            if "log" in cmd and "--oneline" in cmd and "main..HEAD" in cmd:
                return _completed(stdout="abc1234 First commit\ndef5678 Second commit\n")
            # git -C worktree diff --stat main..HEAD
            if "diff" in cmd and "--stat" in cmd:
                return _completed(stdout=" src/file.py | 10 ++++++++++\n 1 file changed, 10 insertions(+)\n")
            # gh pr create
            if "gh" in cmd and "pr" in cmd and "create" in cmd:
                # Capture the body argument
                for i, arg in enumerate(cmd):
                    if arg == "--body" and i + 1 < len(cmd):
                        pr_body_captured.append(cmd[i + 1])
                return _completed(stdout="https://github.com/test/repo/pull/123\n")
            return _completed()

        mock_run.side_effect = side_effect_fn

        result = validate_builder(42, repo, worktree=str(wt))

        assert result.status == ValidationStatus.RECOVERED
        assert len(pr_body_captured) == 1
        body = pr_body_captured[0]
        assert "## Commits" in body
        assert "abc1234 First commit" in body
        assert "def5678 Second commit" in body

    @patch("loom_tools.validate_phase.subprocess.run")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_recovery_pr_body_includes_file_stats(
        self, mock_gh: MagicMock, mock_find: MagicMock, mock_run: MagicMock, tmp_path: Path
    ):
        """Verify file statistics are included in recovery PR body."""
        repo = _make_repo(tmp_path)
        wt = self._make_worktree(tmp_path)

        mock_gh.return_value = _completed(stdout="OPEN\n")
        mock_find.return_value = None

        pr_body_captured = []

        def side_effect_fn(*args, **kwargs):
            cmd = args[0] if args else kwargs.get("args", [])
            if "status" in cmd and "--porcelain" in cmd:
                return _completed(stdout="M src/file.py\n")
            if "add" in cmd and "-A" in cmd:
                return _completed()
            if "commit" in cmd:
                return _completed()
            if "rev-parse" in cmd and "--abbrev-ref" in cmd and "HEAD" in cmd:
                return _completed(stdout="feature/issue-42\n")
            if "rev-parse" in cmd and "@{upstream}" in cmd:
                return _completed(returncode=1)
            if "push" in cmd:
                return _completed()
            if "log" in cmd and "--oneline" in cmd and "main..HEAD" in cmd:
                return _completed(stdout="abc1234 Some commit\n")
            if "diff" in cmd and "--stat" in cmd:
                return _completed(stdout=" src/file.py | 10 ++++++++++\n 1 file changed, 10 insertions(+)\n")
            if "gh" in cmd and "pr" in cmd and "create" in cmd:
                for i, arg in enumerate(cmd):
                    if arg == "--body" and i + 1 < len(cmd):
                        pr_body_captured.append(cmd[i + 1])
                return _completed(stdout="https://github.com/test/repo/pull/123\n")
            return _completed()

        mock_run.side_effect = side_effect_fn

        result = validate_builder(42, repo, worktree=str(wt))

        assert result.status == ValidationStatus.RECOVERED
        assert len(pr_body_captured) == 1
        body = pr_body_captured[0]
        assert "## Changes" in body
        assert "src/file.py" in body
        assert "10 insertions" in body

    @patch("loom_tools.validate_phase.subprocess.run")
    @patch("loom_tools.validate_phase._find_pr_for_issue")
    @patch("loom_tools.validate_phase._run_gh")
    def test_recovery_pr_body_fallback_on_git_failures(
        self, mock_gh: MagicMock, mock_find: MagicMock, mock_run: MagicMock, tmp_path: Path
    ):
        """Verify graceful fallback if git log/diff commands fail."""
        repo = _make_repo(tmp_path)
        wt = self._make_worktree(tmp_path)

        mock_gh.return_value = _completed(stdout="OPEN\n")
        mock_find.return_value = None

        pr_body_captured = []

        def side_effect_fn(*args, **kwargs):
            cmd = args[0] if args else kwargs.get("args", [])
            if "status" in cmd and "--porcelain" in cmd:
                return _completed(stdout="M src/file.py\n")
            if "add" in cmd and "-A" in cmd:
                return _completed()
            if "commit" in cmd:
                return _completed()
            if "rev-parse" in cmd and "--abbrev-ref" in cmd and "HEAD" in cmd:
                return _completed(stdout="feature/issue-42\n")
            if "rev-parse" in cmd and "@{upstream}" in cmd:
                return _completed(returncode=1)
            if "push" in cmd:
                return _completed()
            # Both log and diff fail - should still create PR
            if "log" in cmd and "--oneline" in cmd and "main..HEAD" in cmd:
                return _completed(returncode=1, stderr="fatal: bad revision")
            if "diff" in cmd and "--stat" in cmd:
                return _completed(returncode=1, stderr="fatal: bad revision")
            if "gh" in cmd and "pr" in cmd and "create" in cmd:
                for i, arg in enumerate(cmd):
                    if arg == "--body" and i + 1 < len(cmd):
                        pr_body_captured.append(cmd[i + 1])
                return _completed(stdout="https://github.com/test/repo/pull/123\n")
            return _completed()

        mock_run.side_effect = side_effect_fn

        result = validate_builder(42, repo, worktree=str(wt))

        # Recovery should still succeed
        assert result.status == ValidationStatus.RECOVERED
        assert len(pr_body_captured) == 1
        body = pr_body_captured[0]
        # Should have basic info but not the sections that failed
        assert "Closes #42" in body
        assert "_PR created by shepherd recovery" in body
        # Should NOT have sections since git commands failed
        assert "## Commits" not in body
        assert "## Changes" not in body


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

    @patch("loom_tools.validate_phase._mark_blocked")
    @patch("loom_tools.validate_phase._run_gh")
    def test_neither_label(self, mock_gh: MagicMock, mock_blocked: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:review-requested\n")
        result = validate_judge(42, repo, pr_number=100)
        assert result.status == ValidationStatus.FAILED
        mock_blocked.assert_called_once()

    @patch("loom_tools.validate_phase._run_gh")
    def test_neither_label_check_only(self, mock_gh: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:review-requested\n")
        result = validate_judge(42, repo, pr_number=100, check_only=True)
        assert result.status == ValidationStatus.FAILED

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

    @patch("loom_tools.validate_phase._mark_blocked")
    @patch("loom_tools.validate_phase._run_gh")
    def test_missing_label(self, mock_gh: MagicMock, mock_blocked: MagicMock, tmp_path: Path):
        repo = _make_repo(tmp_path)
        mock_gh.return_value = _completed(stdout="loom:pr\n")
        result = validate_doctor(42, repo, pr_number=100)
        assert result.status == ValidationStatus.FAILED
        mock_blocked.assert_called_once()

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
