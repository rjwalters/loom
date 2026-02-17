"""Tests for builder worktree escape and wrong-issue detection (issue #2630).

Tests the early detection of:
1. Builder escaping worktree and modifying main instead
2. Builder commits referencing a different issue number
3. Pre-flight worktree anchor verification
"""

from __future__ import annotations

import subprocess
from pathlib import Path
from unittest.mock import MagicMock, PropertyMock, patch

import pytest

from loom_tools.shepherd.config import ShepherdConfig
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.phases import BuilderPhase, PhaseStatus


def _make_mock_path(is_dir: bool = True, name: str = "issue-42") -> MagicMock:
    """Create a MagicMock that behaves like a Path with is_dir()."""
    mock_path = MagicMock()
    mock_path.is_dir.return_value = is_dir
    mock_path.name = name
    mock_path.__str__ = lambda self: f"/fake/repo/.loom/worktrees/{name}"
    mock_path.__truediv__ = lambda self, other: MagicMock()
    return mock_path


@pytest.fixture
def mock_context() -> MagicMock:
    """Create a mock ShepherdContext."""
    ctx = MagicMock(spec=ShepherdContext)
    ctx.config = ShepherdConfig(issue=42)
    ctx.repo_root = Path("/fake/repo")
    ctx.scripts_dir = Path("/fake/repo/.loom/scripts")
    ctx.worktree_path = _make_mock_path(is_dir=True)
    ctx.pr_number = None
    ctx.label_cache = MagicMock()
    return ctx


def _make_run_result(
    returncode: int = 0,
    stdout: str = "",
    stderr: str = "",
) -> subprocess.CompletedProcess[str]:
    return subprocess.CompletedProcess(
        args=[], returncode=returncode, stdout=stdout, stderr=stderr
    )


class TestGetNewMainDirtyFiles:
    """Test _get_new_main_dirty_files filtering."""

    def test_no_dirty_files(self, mock_context: MagicMock) -> None:
        builder = BuilderPhase()
        builder._main_dirty_baseline = set()
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=_make_run_result(stdout=""),
        ):
            result = builder._get_new_main_dirty_files(mock_context)
        assert result == []

    def test_all_new_dirty_files(self, mock_context: MagicMock) -> None:
        builder = BuilderPhase()
        builder._main_dirty_baseline = set()
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=_make_run_result(stdout="?? new_file.py\nM changed.py\n"),
        ):
            result = builder._get_new_main_dirty_files(mock_context)
        assert len(result) == 2

    def test_filters_preexisting_dirty_files(self, mock_context: MagicMock) -> None:
        builder = BuilderPhase()
        builder._main_dirty_baseline = {"M preexisting.py"}
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=_make_run_result(
                stdout="M preexisting.py\n?? new_file.py\n"
            ),
        ):
            result = builder._get_new_main_dirty_files(mock_context)
        assert result == ["?? new_file.py"]

    def test_no_baseline_treats_all_as_new(self, mock_context: MagicMock) -> None:
        builder = BuilderPhase()
        builder._main_dirty_baseline = None
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=_make_run_result(stdout="M file.py\n"),
        ):
            result = builder._get_new_main_dirty_files(mock_context)
        assert result == ["M file.py"]


class TestDetectWrongIssue:
    """Test _detect_wrong_issue commit message analysis."""

    def test_no_commits(self, mock_context: MagicMock) -> None:
        builder = BuilderPhase()
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=_make_run_result(stdout=""),
        ):
            result = builder._detect_wrong_issue(mock_context)
        assert result is None

    def test_commits_reference_correct_issue(self, mock_context: MagicMock) -> None:
        builder = BuilderPhase()
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=_make_run_result(stdout="fix: handle edge case (#42)\n"),
        ):
            result = builder._detect_wrong_issue(mock_context)
        assert result is None

    def test_commits_reference_wrong_issue_only(self, mock_context: MagicMock) -> None:
        builder = BuilderPhase()
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=_make_run_result(
                stdout="fix: handle post-curator blocked label (#603)\n"
            ),
        ):
            result = builder._detect_wrong_issue(mock_context)
        assert result is not None
        wrong_issues, messages = result
        assert 603 in wrong_issues
        assert 42 not in wrong_issues

    def test_commits_reference_both_issues_not_flagged(
        self, mock_context: MagicMock
    ) -> None:
        """When commits reference both the assigned AND other issues, don't flag."""
        builder = BuilderPhase()
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=_make_run_result(
                stdout="fix: resolve #42 (related to #603)\n"
            ),
        ):
            result = builder._detect_wrong_issue(mock_context)
        assert result is None

    def test_multiple_wrong_issues(self, mock_context: MagicMock) -> None:
        builder = BuilderPhase()
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=_make_run_result(
                stdout="fix: handle #100 and #200\nchore: cleanup for #300\n"
            ),
        ):
            result = builder._detect_wrong_issue(mock_context)
        assert result is not None
        wrong_issues, messages = result
        assert wrong_issues == {100, 200, 300}

    def test_no_worktree(self, mock_context: MagicMock) -> None:
        builder = BuilderPhase()
        mock_context.worktree_path = None
        result = builder._detect_wrong_issue(mock_context)
        assert result is None

    def test_no_issue_refs_in_commits(self, mock_context: MagicMock) -> None:
        """Commits without any issue references should not flag."""
        builder = BuilderPhase()
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=_make_run_result(
                stdout="fix: handle edge case\nchore: update tests\n"
            ),
        ):
            result = builder._detect_wrong_issue(mock_context)
        assert result is None


class TestDetectWorktreeEscape:
    """Test _detect_worktree_escape early detection."""

    def test_no_escape_clean_main(self, mock_context: MagicMock) -> None:
        """No escape when main is clean."""
        builder = BuilderPhase()
        builder._main_dirty_baseline = set()

        with patch.object(
            builder, "_get_new_main_dirty_files", return_value=[]
        ), patch.object(builder, "_detect_wrong_issue", return_value=None):
            result = builder._detect_worktree_escape(mock_context)
        assert result is None

    def test_escape_dirty_main_clean_worktree(self, mock_context: MagicMock) -> None:
        """Escape detected: main is dirty, worktree is clean."""
        builder = BuilderPhase()
        builder._main_dirty_baseline = set()

        with (
            patch.object(
                builder,
                "_get_new_main_dirty_files",
                return_value=["?? new_file.py"],
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                side_effect=[
                    # git status --porcelain (worktree) - clean
                    _make_run_result(stdout=""),
                    # git log --oneline origin/main..HEAD (worktree) - no commits
                    _make_run_result(stdout=""),
                ],
            ),
            patch.object(builder, "_cleanup_stale_worktree"),
        ):
            result = builder._detect_worktree_escape(mock_context)

        assert result is not None
        assert result.status == PhaseStatus.FAILED
        assert result.data["worktree_escape"] is True

    def test_no_escape_when_worktree_has_commits(
        self, mock_context: MagicMock
    ) -> None:
        """Not an escape when worktree has commits (builder worked in both)."""
        builder = BuilderPhase()
        builder._main_dirty_baseline = set()

        with (
            patch.object(
                builder,
                "_get_new_main_dirty_files",
                return_value=["?? leaked_file.py"],
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                side_effect=[
                    # git status --porcelain (worktree) - clean
                    _make_run_result(stdout=""),
                    # git log --oneline origin/main..HEAD (worktree) - has commits
                    _make_run_result(stdout="abc1234 feat: implement feature\n"),
                ],
            ),
            patch.object(builder, "_detect_wrong_issue", return_value=None),
        ):
            result = builder._detect_worktree_escape(mock_context)

        # Not flagged as escape because worktree has commits
        assert result is None

    def test_wrong_issue_detected(self, mock_context: MagicMock) -> None:
        """Wrong-issue confusion detected via commit messages."""
        builder = BuilderPhase()
        builder._main_dirty_baseline = set()

        with (
            patch.object(builder, "_get_new_main_dirty_files", return_value=[]),
            patch.object(
                builder,
                "_detect_wrong_issue",
                return_value=({603}, ["fix: handle #603"]),
            ),
            patch.object(builder, "_cleanup_stale_worktree"),
        ):
            result = builder._detect_worktree_escape(mock_context)

        assert result is not None
        assert result.status == PhaseStatus.FAILED
        assert result.data["wrong_issue"] is True
        assert 603 in result.data["referenced_issues"]

    def test_no_worktree_returns_none(self, mock_context: MagicMock) -> None:
        """No check needed when worktree doesn't exist."""
        builder = BuilderPhase()
        mock_context.worktree_path = _make_mock_path(is_dir=False)
        result = builder._detect_worktree_escape(mock_context)
        assert result is None

    def test_escape_message_includes_file_count(
        self, mock_context: MagicMock
    ) -> None:
        """Error message should include the number of dirty files."""
        builder = BuilderPhase()
        builder._main_dirty_baseline = set()

        dirty_files = ["?? file1.py", "?? file2.py", "M file3.py"]
        with (
            patch.object(
                builder, "_get_new_main_dirty_files", return_value=dirty_files
            ),
            patch(
                "loom_tools.shepherd.phases.builder.subprocess.run",
                side_effect=[
                    _make_run_result(stdout=""),
                    _make_run_result(stdout=""),
                ],
            ),
            patch.object(builder, "_cleanup_stale_worktree"),
        ):
            result = builder._detect_worktree_escape(mock_context)

        assert result is not None
        assert "3 new dirty file(s)" in result.message
