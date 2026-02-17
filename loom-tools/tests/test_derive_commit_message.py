"""Tests for derive_commit_message in loom_tools.common.git."""

from __future__ import annotations

from unittest.mock import MagicMock, patch

from loom_tools.common.git import derive_commit_message


class TestDeriveCommitMessage:
    """Unit tests for deriving meaningful commit messages from issue context."""

    @patch("loom_tools.common.git.subprocess.run")
    def test_uses_issue_title_when_available(self, mock_run: MagicMock) -> None:
        """Should use the issue title via NamingConventions.pr_title()."""
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout="Builder recovery creates generic commit messages\n",
        )
        result = derive_commit_message(42, "/tmp/worktree", "/tmp/repo")
        assert result == "feat: builder recovery creates generic commit messages"

    @patch("loom_tools.common.git.subprocess.run")
    def test_preserves_conventional_prefix_from_title(
        self, mock_run: MagicMock
    ) -> None:
        """Should preserve existing conventional commit prefixes in issue title."""
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout="fix: validate PR title format\n",
        )
        result = derive_commit_message(42, "/tmp/worktree")
        assert result == "fix: validate PR title format"

    @patch("loom_tools.common.git.subprocess.run")
    def test_falls_back_to_files_when_gh_fails(self, mock_run: MagicMock) -> None:
        """Should use file names when gh issue view fails."""
        mock_run.return_value = MagicMock(returncode=1, stdout="", stderr="error")
        result = derive_commit_message(
            42,
            "/tmp/worktree",
            staged_files=["src/main.py", "src/utils.py"],
        )
        assert result == "feat: update main.py, utils.py for issue #42"

    @patch("loom_tools.common.git.subprocess.run")
    def test_truncates_long_file_list(self, mock_run: MagicMock) -> None:
        """Should truncate file lists longer than 3 files."""
        mock_run.return_value = MagicMock(returncode=1, stdout="")
        files = ["a.py", "b.py", "c.py", "d.py", "e.py"]
        result = derive_commit_message(42, "/tmp/worktree", staged_files=files)
        assert "a.py, b.py, c.py and 2 more" in result
        assert "issue #42" in result

    @patch("loom_tools.common.git.subprocess.run")
    def test_falls_back_to_generic_when_no_context(
        self, mock_run: MagicMock
    ) -> None:
        """Should produce generic message when nothing else is available."""
        mock_run.return_value = MagicMock(returncode=1, stdout="")
        result = derive_commit_message(42, "/tmp/worktree")
        assert result == "feat: implement changes for issue #42"

    @patch("loom_tools.common.git.subprocess.run")
    def test_handles_empty_issue_title(self, mock_run: MagicMock) -> None:
        """Should fall back when issue title is empty string."""
        mock_run.return_value = MagicMock(returncode=0, stdout="\n")
        result = derive_commit_message(
            42, "/tmp/worktree", staged_files=["config.py"]
        )
        assert result == "feat: update config.py for issue #42"

    @patch("loom_tools.common.git.subprocess.run")
    def test_handles_subprocess_exception(self, mock_run: MagicMock) -> None:
        """Should not crash when subprocess raises an exception."""
        mock_run.side_effect = OSError("gh not found")
        result = derive_commit_message(42, "/tmp/worktree")
        assert result == "feat: implement changes for issue #42"

    @patch("loom_tools.common.git.subprocess.run")
    def test_uses_repo_root_for_gh_cwd(self, mock_run: MagicMock) -> None:
        """Should pass repo_root as cwd to gh when provided."""
        mock_run.return_value = MagicMock(returncode=0, stdout="Fix bug\n")
        derive_commit_message(42, "/tmp/worktree", "/tmp/repo")
        call_kwargs = mock_run.call_args
        assert call_kwargs.kwargs.get("cwd") == "/tmp/repo"

    @patch("loom_tools.common.git.subprocess.run")
    def test_uses_worktree_as_cwd_when_no_repo_root(
        self, mock_run: MagicMock
    ) -> None:
        """Should fall back to worktree path as cwd when repo_root is None."""
        mock_run.return_value = MagicMock(returncode=0, stdout="Fix bug\n")
        derive_commit_message(42, "/tmp/worktree")
        call_kwargs = mock_run.call_args
        assert call_kwargs.kwargs.get("cwd") == "/tmp/worktree"
