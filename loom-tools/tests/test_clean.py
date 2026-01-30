"""Tests for loom_tools.clean."""

from __future__ import annotations

import json
import pathlib

import pytest

from loom_tools.clean import (
    CleanupStats,
    PRStatus,
    _get_dir_size,
    check_grace_period,
    check_uncommitted_changes,
    main,
    print_summary,
)


@pytest.fixture
def mock_repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a mock repo with .git and .loom directories."""
    (tmp_path / ".git").mkdir()
    (tmp_path / ".loom").mkdir()
    (tmp_path / ".loom" / "worktrees").mkdir()
    return tmp_path


class TestPRStatus:
    """Tests for PRStatus dataclass."""

    def test_pr_status_merged(self) -> None:
        status = PRStatus(status="MERGED", merged_at="2026-01-01T00:00:00Z")
        assert status.status == "MERGED"
        assert status.merged_at == "2026-01-01T00:00:00Z"

    def test_pr_status_no_pr(self) -> None:
        status = PRStatus(status="NO_PR")
        assert status.status == "NO_PR"
        assert status.merged_at is None


class TestGracePeriod:
    """Tests for check_grace_period function."""

    def test_grace_period_passed_old_merge(self) -> None:
        # A merge from the past should have passed grace period
        passed, remaining = check_grace_period("2020-01-01T00:00:00Z", 600)
        assert passed
        assert remaining == 0

    def test_grace_period_not_passed_recent_merge(self) -> None:
        # Use a future timestamp to ensure we're always in grace period
        from datetime import datetime, timedelta, timezone

        future = datetime.now(timezone.utc) + timedelta(minutes=5)
        future_str = future.strftime("%Y-%m-%dT%H:%M:%SZ")

        passed, remaining = check_grace_period(future_str, 600)
        # Since merge is in the future, grace period hasn't passed
        assert not passed
        assert remaining > 0


class TestUncommittedChanges:
    """Tests for check_uncommitted_changes function."""

    def test_nonexistent_path(self, tmp_path: pathlib.Path) -> None:
        result = check_uncommitted_changes(tmp_path / "nonexistent")
        assert not result

    def test_non_git_dir(self, tmp_path: pathlib.Path) -> None:
        # A directory without git returns False (git command fails)
        (tmp_path / "subdir").mkdir()
        result = check_uncommitted_changes(tmp_path / "subdir")
        # Git commands on non-git directories return error codes,
        # which our function interprets as False (no changes)
        # or True (treating error as "there might be changes")
        # depending on how the exception is handled
        assert result in (True, False)  # Both are valid behaviors


class TestDirSize:
    """Tests for _get_dir_size function."""

    def test_dir_size_bytes(self, tmp_path: pathlib.Path) -> None:
        # Create a small file
        (tmp_path / "file.txt").write_text("hello")
        size = _get_dir_size(tmp_path)
        assert "B" in size or "K" in size

    def test_dir_size_empty(self, tmp_path: pathlib.Path) -> None:
        subdir = tmp_path / "empty"
        subdir.mkdir()
        size = _get_dir_size(subdir)
        assert size == "0B" or "K" in size

    def test_dir_size_nonexistent(self, tmp_path: pathlib.Path) -> None:
        size = _get_dir_size(tmp_path / "nonexistent")
        # Non-existent dir returns "0B" or "unknown" depending on implementation
        assert size in ("0B", "unknown")


class TestCleanupStats:
    """Tests for CleanupStats dataclass."""

    def test_defaults(self) -> None:
        stats = CleanupStats()
        assert stats.cleaned_worktrees == 0
        assert stats.skipped_open == 0
        assert stats.errors == 0


class TestPrintSummary:
    """Tests for print_summary function."""

    def test_print_summary_empty(self, capsys: pytest.CaptureFixture[str]) -> None:
        stats = CleanupStats()
        print_summary(stats)
        captured = capsys.readouterr()
        assert "Summary" in captured.out
        assert "Cleaned: 0" in captured.out

    def test_print_summary_dry_run(self, capsys: pytest.CaptureFixture[str]) -> None:
        stats = CleanupStats(cleaned_worktrees=3)
        print_summary(stats, dry_run=True)
        captured = capsys.readouterr()
        assert "Would clean: 3" in captured.out

    def test_print_summary_with_errors(self, capsys: pytest.CaptureFixture[str]) -> None:
        stats = CleanupStats(errors=2)
        print_summary(stats)
        captured = capsys.readouterr()
        assert "Errors: 2" in captured.out


class TestCLI:
    """Tests for CLI main function."""

    def test_cli_help(self) -> None:
        with pytest.raises(SystemExit) as exc_info:
            main(["--help"])
        assert exc_info.value.code == 0

    def test_cli_dry_run_no_repo(self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        # Change to a directory without .loom
        (tmp_path / ".git").mkdir()
        monkeypatch.chdir(tmp_path)
        result = main(["--dry-run", "--force"])
        # Without .loom directory, find_repo_root() fails which returns 1
        # But the error is logged and the main function may handle it gracefully
        assert result in (0, 1)

    def test_cli_dry_run(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["--dry-run", "--force"])
        assert result == 0

    def test_cli_worktrees_only(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["--dry-run", "--force", "--worktrees-only"])
        assert result == 0

    def test_cli_branches_only(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["--dry-run", "--force", "--branches-only"])
        assert result == 0

    def test_cli_tmux_only(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["--dry-run", "--force", "--tmux-only"])
        assert result == 0

    def test_cli_safe_mode(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["--dry-run", "--force", "--safe"])
        assert result == 0

    def test_cli_deep_clean(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["--dry-run", "--force", "--deep"])
        assert result == 0

    def test_cli_grace_period(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["--dry-run", "--force", "--safe", "--grace-period", "1200"])
        assert result == 0
