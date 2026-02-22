"""Tests for shepherd CLI."""

from __future__ import annotations

import os
import sys
import time
from io import StringIO
from pathlib import Path
from unittest.mock import MagicMock, call, patch

import pytest

from loom_tools.shepherd.cli import (
    _auto_navigate_out_of_worktree,
    _check_main_repo_clean,
    _cleanup_labels_on_failure,
    _cleanup_pr_labels_on_failure,
    _find_source_issues_for_dirty_files,
    _get_prior_failure_info,
    _is_loom_runtime,
    _create_config,
    _format_diagnostics_for_comment,
    _format_diagnostics_for_log,
    _gather_no_pr_diagnostics,
    _mark_builder_no_pr,
    _mark_judge_exhausted,
    _parse_args,
    _post_fallback_failure_comment,
    _record_fallback_failure,
    main,
    orchestrate,
)
from loom_tools.shepherd.config import ExecutionMode, Phase, ShepherdConfig
from loom_tools.shepherd.exit_codes import ShepherdExitCode
from loom_tools.shepherd.phases import PhaseResult, PhaseStatus


class TestParseArgs:
    """Test CLI argument parsing."""

    def test_requires_issue_number(self) -> None:
        """Should require issue number."""
        with pytest.raises(SystemExit):
            _parse_args([])

    def test_parses_issue_number(self) -> None:
        """Should parse issue number."""
        args = _parse_args(["42"])
        assert args.issue == 42

    def test_parses_force_flag(self) -> None:
        """Should parse --force flag."""
        args = _parse_args(["42", "--force"])
        assert args.force is True

    def test_parses_force_short_flag(self) -> None:
        """Should parse -f flag."""
        args = _parse_args(["42", "-f"])
        assert args.force is True

    def test_parses_from_phase(self) -> None:
        """Should parse --from phase."""
        args = _parse_args(["42", "--from", "builder"])
        assert args.start_from == "builder"

    def test_from_validates_phase(self) -> None:
        """Should reject invalid --from phase."""
        with pytest.raises(SystemExit):
            _parse_args(["42", "--from", "invalid"])

    def test_parses_to_phase(self) -> None:
        """Should parse --to phase."""
        args = _parse_args(["42", "--to", "curated"])
        assert args.stop_after == "curated"

    def test_to_validates_phase(self) -> None:
        """Should reject invalid --to phase."""
        with pytest.raises(SystemExit):
            _parse_args(["42", "--to", "invalid"])

    def test_parses_task_id(self) -> None:
        """Should parse --task-id."""
        args = _parse_args(["42", "--task-id", "abc1234"])
        assert args.task_id == "abc1234"

    def test_parses_allow_dirty_main(self) -> None:
        """Should parse --allow-dirty-main flag."""
        args = _parse_args(["42", "--allow-dirty-main"])
        assert args.allow_dirty_main is True

    def test_allow_dirty_main_default_false(self) -> None:
        """--allow-dirty-main should default to False."""
        args = _parse_args(["42"])
        assert args.allow_dirty_main is False

    def test_parses_skip_builder(self) -> None:
        """Should parse --skip-builder flag."""
        args = _parse_args(["42", "--skip-builder"])
        assert args.skip_builder is True

    def test_skip_builder_default_false(self) -> None:
        """--skip-builder should default to False."""
        args = _parse_args(["42"])
        assert args.skip_builder is False

    def test_parses_pr_number(self) -> None:
        """Should parse --pr with integer."""
        args = _parse_args(["42", "--pr", "312"])
        assert args.pr_number == 312

    def test_pr_default_none(self) -> None:
        """--pr should default to None."""
        args = _parse_args(["42"])
        assert args.pr_number is None

    def test_pr_rejects_non_integer(self) -> None:
        """--pr should reject non-integer values."""
        with pytest.raises(SystemExit):
            _parse_args(["42", "--pr", "abc"])

    def test_parses_resume(self) -> None:
        """Should parse --resume flag."""
        args = _parse_args(["42", "--resume"])
        assert args.resume is True

    def test_resume_default_false(self) -> None:
        """--resume should default to False."""
        args = _parse_args(["42"])
        assert args.resume is False


class TestCreateConfig:
    """Test config creation from args."""

    def test_default_mode(self) -> None:
        """Default mode should be DEFAULT."""
        args = _parse_args(["42"])
        config = _create_config(args)
        assert config.mode == ExecutionMode.DEFAULT

    def test_force_mode(self) -> None:
        """--force should set FORCE_MERGE mode."""
        args = _parse_args(["42", "--force"])
        config = _create_config(args)
        assert config.mode == ExecutionMode.FORCE_MERGE

    def test_deprecated_force_merge(self) -> None:
        """--force-merge should set FORCE_MERGE mode with warning."""
        args = _parse_args(["42", "--force-merge"])
        with patch("loom_tools.shepherd.cli.log_warning") as mock_warn:
            config = _create_config(args)
            assert config.mode == ExecutionMode.FORCE_MERGE
            mock_warn.assert_called()

    def test_deprecated_force_pr(self) -> None:
        """--force-pr should set DEFAULT mode with warning."""
        args = _parse_args(["42", "--force-pr"])
        with patch("loom_tools.shepherd.cli.log_warning") as mock_warn:
            config = _create_config(args)
            assert config.mode == ExecutionMode.DEFAULT
            mock_warn.assert_called()

    def test_deprecated_wait(self) -> None:
        """--wait should set NORMAL mode with warning."""
        args = _parse_args(["42", "--wait"])
        with patch("loom_tools.shepherd.cli.log_warning") as mock_warn:
            config = _create_config(args)
            assert config.mode == ExecutionMode.NORMAL
            mock_warn.assert_called()

    def test_from_phase_mapping(self) -> None:
        """--from should map to Phase enum."""
        for phase_str, phase_enum in [
            ("curator", Phase.CURATOR),
            ("builder", Phase.BUILDER),
            ("judge", Phase.JUDGE),
            ("merge", Phase.MERGE),
        ]:
            args = _parse_args(["42", "--from", phase_str])
            config = _create_config(args)
            assert config.start_from == phase_enum

    def test_stop_after(self) -> None:
        """--to should set stop_after."""
        args = _parse_args(["42", "--to", "curated"])
        config = _create_config(args)
        assert config.stop_after == "curated"

    def test_task_id(self) -> None:
        """--task-id should set task_id."""
        args = _parse_args(["42", "--task-id", "abc1234"])
        config = _create_config(args)
        assert config.task_id == "abc1234"

    def test_skip_builder(self) -> None:
        """--skip-builder should set skip_builder."""
        args = _parse_args(["42", "--skip-builder"])
        config = _create_config(args)
        assert config.skip_builder is True
        assert config.pr_number_override is None

    def test_pr_number_sets_skip_builder(self) -> None:
        """--pr should set pr_number_override and imply skip_builder."""
        args = _parse_args(["42", "--pr", "312"])
        config = _create_config(args)
        assert config.pr_number_override == 312
        assert config.skip_builder is True

    def test_skip_builder_default(self) -> None:
        """Default config should have skip_builder=False."""
        args = _parse_args(["42"])
        config = _create_config(args)
        assert config.skip_builder is False
        assert config.pr_number_override is None

    def test_resume_sets_config(self) -> None:
        """--resume should set resume=True in config."""
        args = _parse_args(["42", "--resume"])
        config = _create_config(args)
        assert config.resume is True

    def test_resume_default(self) -> None:
        """Default config should have resume=False."""
        args = _parse_args(["42"])
        config = _create_config(args)
        assert config.resume is False


class TestAutoNavigateOutOfWorktree:
    """Test _auto_navigate_out_of_worktree function (issue #1957)."""

    def test_navigates_when_cwd_is_inside_worktree(self, tmp_path: Path) -> None:
        """Should change CWD to repo root when inside worktree."""
        # Setup: create mock repo structure
        repo_root = tmp_path / "repo"
        repo_root.mkdir()
        (repo_root / ".loom").mkdir()
        worktree_dir = repo_root / ".loom" / "worktrees" / "issue-42"
        worktree_dir.mkdir(parents=True)

        # Save original CWD
        original_cwd = os.getcwd()

        try:
            # Set CWD to inside worktree
            os.chdir(worktree_dir)

            # Call the function
            with patch("loom_tools.shepherd.cli.log_warning") as mock_warn:
                _auto_navigate_out_of_worktree(repo_root)

            # Verify CWD changed to repo root
            assert Path.cwd().resolve() == repo_root.resolve()

            # Verify warning was logged
            mock_warn.assert_called_once()
            call_arg = mock_warn.call_args[0][0]
            assert "inside worktree" in call_arg.lower()
            assert str(worktree_dir) in call_arg

        finally:
            # Restore original CWD
            os.chdir(original_cwd)

    def test_no_change_when_cwd_is_repo_root(self, tmp_path: Path) -> None:
        """Should not change CWD when already at repo root."""
        # Setup: create mock repo structure
        repo_root = tmp_path / "repo"
        repo_root.mkdir()
        (repo_root / ".loom").mkdir()
        (repo_root / ".loom" / "worktrees").mkdir(parents=True)

        # Save original CWD
        original_cwd = os.getcwd()

        try:
            # Set CWD to repo root
            os.chdir(repo_root)

            # Call the function
            with patch("loom_tools.shepherd.cli.log_warning") as mock_warn:
                _auto_navigate_out_of_worktree(repo_root)

            # Verify CWD unchanged
            assert Path.cwd().resolve() == repo_root.resolve()

            # Verify no warning was logged
            mock_warn.assert_not_called()

        finally:
            # Restore original CWD
            os.chdir(original_cwd)

    def test_no_change_when_cwd_is_outside_repo(self, tmp_path: Path) -> None:
        """Should not change CWD when outside repo entirely."""
        # Setup: create mock repo structure
        repo_root = tmp_path / "repo"
        repo_root.mkdir()
        (repo_root / ".loom").mkdir()
        (repo_root / ".loom" / "worktrees").mkdir(parents=True)

        other_dir = tmp_path / "other"
        other_dir.mkdir()

        # Save original CWD
        original_cwd = os.getcwd()

        try:
            # Set CWD to outside repo
            os.chdir(other_dir)

            # Call the function
            with patch("loom_tools.shepherd.cli.log_warning") as mock_warn:
                _auto_navigate_out_of_worktree(repo_root)

            # Verify CWD unchanged
            assert Path.cwd().resolve() == other_dir.resolve()

            # Verify no warning was logged
            mock_warn.assert_not_called()

        finally:
            # Restore original CWD
            os.chdir(original_cwd)

    def test_handles_nested_worktree_subdirectory(self, tmp_path: Path) -> None:
        """Should navigate out even when CWD is nested inside worktree."""
        # Setup: create mock repo structure with nested dirs
        repo_root = tmp_path / "repo"
        repo_root.mkdir()
        (repo_root / ".loom").mkdir()
        worktree_dir = repo_root / ".loom" / "worktrees" / "issue-42"
        nested_dir = worktree_dir / "src" / "components"
        nested_dir.mkdir(parents=True)

        # Save original CWD
        original_cwd = os.getcwd()

        try:
            # Set CWD to nested directory inside worktree
            os.chdir(nested_dir)

            # Call the function
            with patch("loom_tools.shepherd.cli.log_warning") as mock_warn:
                _auto_navigate_out_of_worktree(repo_root)

            # Verify CWD changed to repo root
            assert Path.cwd().resolve() == repo_root.resolve()

            # Verify warning was logged
            mock_warn.assert_called_once()

        finally:
            # Restore original CWD
            os.chdir(original_cwd)


class TestMain:
    """Test main entry point."""

    def test_returns_int(self) -> None:
        """main should return an int exit code."""
        with patch("loom_tools.shepherd.cli.orchestrate", return_value=0), \
             patch("loom_tools.shepherd.cli.ShepherdContext"), \
             patch("loom_tools.shepherd.cli.find_repo_root", return_value=Path("/fake/repo")), \
             patch("loom_tools.shepherd.cli._auto_navigate_out_of_worktree"), \
             patch("loom_tools.common.github.gh_issue_view", return_value={"state": "OPEN"}), \
             patch("loom_tools.claim.claim_issue", return_value=0), \
             patch("loom_tools.claim.release_claim"):
            result = main(["42"])
            assert isinstance(result, int)

    def test_passes_exit_code_from_orchestrate(self) -> None:
        """main should return orchestrate's exit code."""
        with patch("loom_tools.shepherd.cli.orchestrate", return_value=1), \
             patch("loom_tools.shepherd.cli.ShepherdContext"), \
             patch("loom_tools.shepherd.cli.find_repo_root", return_value=Path("/fake/repo")), \
             patch("loom_tools.shepherd.cli._auto_navigate_out_of_worktree"), \
             patch("loom_tools.common.github.gh_issue_view", return_value={"state": "OPEN"}), \
             patch("loom_tools.claim.claim_issue", return_value=0), \
             patch("loom_tools.claim.release_claim"):
            result = main(["42"])
            assert result == 1

    def test_calls_auto_navigate_before_context(self) -> None:
        """main should call _auto_navigate_out_of_worktree before creating context."""
        call_order = []

        def track_navigate(repo_root: Path) -> None:
            call_order.append("navigate")

        def track_context(*args: object, **kwargs: object) -> MagicMock:
            call_order.append("context")
            return MagicMock()

        with patch("loom_tools.shepherd.cli.orchestrate", return_value=0), \
             patch("loom_tools.shepherd.cli.ShepherdContext", side_effect=track_context), \
             patch("loom_tools.shepherd.cli.find_repo_root", return_value=Path("/fake/repo")), \
             patch("loom_tools.shepherd.cli._auto_navigate_out_of_worktree", side_effect=track_navigate), \
             patch("loom_tools.common.github.gh_issue_view", return_value={"state": "OPEN"}), \
             patch("loom_tools.claim.claim_issue", return_value=0), \
             patch("loom_tools.claim.release_claim"):
            main(["42"])

        # Navigate must be called before context is created
        assert call_order == ["navigate", "context"]

    def test_dirty_main_blocks_without_flag(self) -> None:
        """main should exit with NEEDS_INTERVENTION when repo is dirty and no --allow-dirty-main."""
        with patch("loom_tools.shepherd.cli.find_repo_root", return_value=Path("/fake/repo")), \
             patch("loom_tools.shepherd.cli._auto_navigate_out_of_worktree"), \
             patch("loom_tools.shepherd.cli.get_uncommitted_files", return_value=["M file.py"]):
            result = main(["42"])
            assert result == ShepherdExitCode.NEEDS_INTERVENTION

    def test_dirty_main_proceeds_with_flag(self) -> None:
        """main should proceed when repo is dirty but --allow-dirty-main is set."""
        with patch("loom_tools.shepherd.cli.orchestrate", return_value=0), \
             patch("loom_tools.shepherd.cli.ShepherdContext"), \
             patch("loom_tools.shepherd.cli.find_repo_root", return_value=Path("/fake/repo")), \
             patch("loom_tools.shepherd.cli._auto_navigate_out_of_worktree"), \
             patch("loom_tools.shepherd.cli.get_uncommitted_files", return_value=["M file.py"]), \
             patch("loom_tools.common.github.gh_issue_view", return_value={"state": "OPEN"}), \
             patch("loom_tools.claim.claim_issue", return_value=0), \
             patch("loom_tools.claim.release_claim"):
            result = main(["42", "--allow-dirty-main"])
            assert result == 0

    def test_clean_repo_proceeds(self) -> None:
        """main should proceed when repo is clean."""
        with patch("loom_tools.shepherd.cli.orchestrate", return_value=0), \
             patch("loom_tools.shepherd.cli.ShepherdContext"), \
             patch("loom_tools.shepherd.cli.find_repo_root", return_value=Path("/fake/repo")), \
             patch("loom_tools.shepherd.cli._auto_navigate_out_of_worktree"), \
             patch("loom_tools.shepherd.cli.get_uncommitted_files", return_value=[]), \
             patch("loom_tools.common.github.gh_issue_view", return_value={"state": "OPEN"}), \
             patch("loom_tools.claim.claim_issue", return_value=0), \
             patch("loom_tools.claim.release_claim"):
            result = main(["42"])
            assert result == 0

    def test_closed_issue_skips_without_claiming(self) -> None:
        """main should return SKIPPED and not call claim_issue when issue is already closed."""
        mock_claim = MagicMock(return_value=0)
        with patch("loom_tools.shepherd.cli.find_repo_root", return_value=Path("/fake/repo")), \
             patch("loom_tools.shepherd.cli._auto_navigate_out_of_worktree"), \
             patch("loom_tools.common.github.gh_issue_view", return_value={"state": "CLOSED"}), \
             patch("loom_tools.claim.claim_issue", mock_claim), \
             patch("loom_tools.claim.release_claim"):
            result = main(["42"])
            assert result == ShepherdExitCode.SKIPPED
            mock_claim.assert_not_called()


class TestIsLoomRuntime:
    """Tests for _is_loom_runtime filter (issue #2158)."""

    def test_loom_directory_files(self) -> None:
        """Should match files under .loom/ directory."""
        assert _is_loom_runtime("?? .loom/daemon-state.json") is True
        assert _is_loom_runtime("M  .loom/state.json") is True
        assert _is_loom_runtime("?? .loom/logs/agent.log") is True

    def test_loom_dash_root_files(self) -> None:
        """Should match .loom-* root-level files like .loom-checkpoint."""
        assert _is_loom_runtime("?? .loom-checkpoint") is True
        assert _is_loom_runtime("M  .loom-in-use") is True

    def test_non_loom_files(self) -> None:
        """Should NOT match regular source files."""
        assert _is_loom_runtime("M  src/main.py") is False
        assert _is_loom_runtime("?? README.md") is False
        assert _is_loom_runtime("M  .github/workflows/ci.yml") is False

    def test_rename_uses_destination(self) -> None:
        """Should use destination path for renames."""
        assert _is_loom_runtime("R  old.txt -> .loom/new.txt") is True
        assert _is_loom_runtime("R  .loom/old.txt -> src/new.txt") is False

    def test_empty_and_short_lines(self) -> None:
        """Should handle edge cases gracefully."""
        assert _is_loom_runtime("") is False
        assert _is_loom_runtime("M ") is False


class TestCheckMainRepoClean:
    """Tests for _check_main_repo_clean pre-flight check (issue #1996)."""

    def test_returns_true_when_clean(self) -> None:
        """Should return True when no uncommitted files."""
        with patch("loom_tools.shepherd.cli.get_uncommitted_files", return_value=[]):
            result = _check_main_repo_clean(Path("/fake/repo"), allow_dirty=False)
            assert result is True

    def test_returns_false_when_dirty_and_not_allowed(self) -> None:
        """Should return False when files are dirty and not allowed."""
        with patch("loom_tools.shepherd.cli.get_uncommitted_files", return_value=["M file.py"]):
            result = _check_main_repo_clean(Path("/fake/repo"), allow_dirty=False)
            assert result is False

    def test_returns_true_when_dirty_but_allowed(self) -> None:
        """Should return True when files are dirty but allowed."""
        with patch("loom_tools.shepherd.cli.get_uncommitted_files", return_value=["M file.py"]):
            result = _check_main_repo_clean(Path("/fake/repo"), allow_dirty=True)
            assert result is True

    def test_warn_message_uses_default_reason(self) -> None:
        """Default reason should say '--allow-dirty-main specified'."""
        with patch("loom_tools.shepherd.cli.get_uncommitted_files", return_value=["M file.py"]), \
             patch("loom_tools.shepherd.cli.log_warning") as mock_warn:
            _check_main_repo_clean(Path("/fake/repo"), allow_dirty=True)
            messages = [call[0][0] for call in mock_warn.call_args_list]
            assert any("--allow-dirty-main specified" in msg for msg in messages)

    def test_warn_message_uses_custom_reason(self) -> None:
        """Custom reason should appear in the warning message (issue #2827)."""
        with patch("loom_tools.shepherd.cli.get_uncommitted_files", return_value=["M file.py"]), \
             patch("loom_tools.shepherd.cli.log_warning") as mock_warn:
            _check_main_repo_clean(
                Path("/fake/repo"), allow_dirty=True, allow_dirty_reason="implied by --merge"
            )
            messages = [call[0][0] for call in mock_warn.call_args_list]
            assert any("implied by --merge" in msg for msg in messages)
            assert not any("--allow-dirty-main specified" in msg for msg in messages)

    def test_logs_warning_with_file_list(self) -> None:
        """Should log warning with list of changed files."""
        with patch("loom_tools.shepherd.cli.get_uncommitted_files", return_value=["M file.py", "?? new.txt"]), \
             patch("loom_tools.shepherd.cli.log_warning") as mock_warn:
            _check_main_repo_clean(Path("/fake/repo"), allow_dirty=False)
            mock_warn.assert_called()
            # First call should mention the count
            assert "2 uncommitted" in mock_warn.call_args_list[0][0][0]

    def test_truncates_long_file_list(self) -> None:
        """Should truncate file list at 10 files."""
        files = [f"M file{i}.py" for i in range(15)]
        with patch("loom_tools.shepherd.cli.get_uncommitted_files", return_value=files), \
             patch("loom_tools.shepherd.cli.log_warning"):
            # Just check it doesn't crash with many files
            result = _check_main_repo_clean(Path("/fake/repo"), allow_dirty=True)
            assert result is True

    def test_filters_loom_checkpoint_file(self) -> None:
        """Should treat .loom-checkpoint as clean (issue #2158)."""
        with patch("loom_tools.shepherd.cli.get_uncommitted_files", return_value=["?? .loom-checkpoint"]):
            result = _check_main_repo_clean(Path("/fake/repo"), allow_dirty=False)
            assert result is True

    def test_filters_loom_runtime_but_keeps_source_changes(self) -> None:
        """Should filter .loom/ files but still detect real source changes."""
        files = ["?? .loom/daemon-state.json", "?? .loom-checkpoint", "M  src/main.py"]
        with patch("loom_tools.shepherd.cli.get_uncommitted_files", return_value=files), \
             patch("loom_tools.shepherd.cli.log_warning"):
            result = _check_main_repo_clean(Path("/fake/repo"), allow_dirty=False)
            assert result is False

    def test_suggests_recovery_when_source_issues_found(self) -> None:
        """Should print recovery commands when dirty files map to worktrees (issue #2837)."""
        with patch("loom_tools.shepherd.cli.get_uncommitted_files", return_value=["M src/foo.py"]), \
             patch("loom_tools.shepherd.cli._find_source_issues_for_dirty_files", return_value={42: ["src/foo.py"]}), \
             patch("loom_tools.shepherd.cli.log_warning") as mock_warn:
            _check_main_repo_clean(Path("/fake/repo"), allow_dirty=True)
            messages = [call[0][0] for call in mock_warn.call_args_list]
            # Should mention the issue number
            assert any("#42" in msg for msg in messages)
            # Should suggest at least one recovery command
            assert any("git stash" in msg or "git checkout" in msg for msg in messages)

    def test_no_recovery_suggestion_when_no_worktree_match(self) -> None:
        """Should not print recovery commands when no worktrees match (issue #2837)."""
        with patch("loom_tools.shepherd.cli.get_uncommitted_files", return_value=["M src/foo.py"]), \
             patch("loom_tools.shepherd.cli._find_source_issues_for_dirty_files", return_value={}), \
             patch("loom_tools.shepherd.cli.log_warning") as mock_warn:
            _check_main_repo_clean(Path("/fake/repo"), allow_dirty=True)
            messages = [call[0][0] for call in mock_warn.call_args_list]
            assert not any("git stash" in msg for msg in messages)


class TestFindSourceIssuesForDirtyFiles:
    """Tests for _find_source_issues_for_dirty_files (issue #2837)."""

    def test_returns_empty_when_worktrees_dir_missing(self, tmp_path: Path) -> None:
        """Should return empty dict when .loom/worktrees does not exist."""
        result = _find_source_issues_for_dirty_files(tmp_path, ["src/foo.py"])
        assert result == {}

    def test_returns_empty_when_no_files_match(self, tmp_path: Path) -> None:
        """Should return empty dict when dirty files don't exist in any worktree."""
        worktrees = tmp_path / ".loom" / "worktrees" / "issue-42"
        worktrees.mkdir(parents=True)
        result = _find_source_issues_for_dirty_files(tmp_path, ["src/foo.py"])
        assert result == {}

    def test_returns_match_when_file_exists_in_worktree(self, tmp_path: Path) -> None:
        """Should return issue->files mapping when a dirty file exists in worktree."""
        worktree = tmp_path / ".loom" / "worktrees" / "issue-42"
        (worktree / "src").mkdir(parents=True)
        (worktree / "src" / "foo.py").write_text("# content")
        result = _find_source_issues_for_dirty_files(tmp_path, ["src/foo.py"])
        assert result == {42: ["src/foo.py"]}

    def test_matches_multiple_files_and_issues(self, tmp_path: Path) -> None:
        """Should return all matching files across multiple worktrees."""
        for issue in (10, 20):
            wt = tmp_path / ".loom" / "worktrees" / f"issue-{issue}"
            (wt / "pkg").mkdir(parents=True)
            (wt / "pkg" / "mod.py").write_text("")
        result = _find_source_issues_for_dirty_files(tmp_path, ["pkg/mod.py", "other.py"])
        assert set(result.keys()) == {10, 20}
        assert result[10] == ["pkg/mod.py"]
        assert result[20] == ["pkg/mod.py"]

    def test_skips_non_issue_directories(self, tmp_path: Path) -> None:
        """Should ignore worktree directories that don't match issue-N naming."""
        terminal = tmp_path / ".loom" / "worktrees" / "terminal-1"
        (terminal / "src").mkdir(parents=True)
        (terminal / "src" / "foo.py").write_text("")
        result = _find_source_issues_for_dirty_files(tmp_path, ["src/foo.py"])
        assert result == {}

    def test_handles_oserror_gracefully(self, tmp_path: Path) -> None:
        """Should return empty dict (not raise) on OSError."""
        worktrees = tmp_path / ".loom" / "worktrees"
        worktrees.mkdir(parents=True)
        with patch("pathlib.Path.iterdir", side_effect=OSError("permission denied")):
            result = _find_source_issues_for_dirty_files(tmp_path, ["src/foo.py"])
        assert result == {}


def _make_ctx(
    *,
    mode: ExecutionMode = ExecutionMode.FORCE_MERGE,
    start_from: Phase | None = None,
    stop_after: str | None = None,
) -> MagicMock:
    """Create a mock ShepherdContext for orchestration tests."""
    config = ShepherdConfig(
        issue=42,
        mode=mode,
        start_from=start_from,
        stop_after=stop_after,
        task_id="test123",
    )
    ctx = MagicMock()
    ctx.config = config
    ctx.repo_root = "/fake/repo"
    ctx.issue_title = "Test issue"
    ctx.pr_number = 100
    ctx.report_milestone = MagicMock(return_value=True)
    # Default label checks to False so label-detection code paths
    # don't interfere with tests that don't explicitly test them.
    ctx.has_issue_label.return_value = False
    ctx.has_pr_label.return_value = False
    return ctx


def _success_result(phase: str = "", **data: object) -> PhaseResult:
    """Create a successful PhaseResult."""
    return PhaseResult(status=PhaseStatus.SUCCESS, message=f"{phase} done", phase_name=phase, data=data)


class TestPhaseTiming:
    """Test per-phase timing in orchestrate()."""

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_phase_durations_populated(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
    ) -> None:
        """Phase durations dict should be populated after orchestration."""
        # Simulate time progression: each phase takes a known duration
        # time.time() calls: start_time, curator_start, curator_end, approval_start, approval_end,
        #   builder_start, builder_end, judge_start, judge_end,
        #   rebase_start, rebase_end, merge_start, merge_end, duration_calc
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # curator phase_start
            10,    # curator elapsed (10s)
            10,    # approval phase_start
            15,    # approval elapsed (5s)
            15,    # builder phase_start
            115,   # builder elapsed (100s)
            115,   # judge phase_start
            165,   # judge elapsed (50s)
            165,   # rebase phase_start
            165,   # rebase elapsed (0s, skipped)
            165,   # merge phase_start
            170,   # merge elapsed (5s)
            170,   # duration calc
        ])

        ctx = _make_ctx()

        # Curator: runs normally
        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (False, "")
        curator_inst.run.return_value = _success_result("curator")

        # Approval: success
        MockApproval.return_value.run.return_value = _success_result("approval")

        # Builder: success
        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        builder_inst.run.return_value = _success_result("builder")

        # Judge: approved
        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        # Rebase: skipped (up to date)
        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )

        # Merge: merged
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        # Verify phase_completed milestones were reported with timing
        milestone_calls = [
            c for c in ctx.report_milestone.call_args_list
            if c[0][0] == "phase_completed"
        ]
        assert len(milestone_calls) == 5  # curator, approval, builder, judge, merge

        # Check specific phase timing in milestone calls
        phases_reported = {c.kwargs["phase"]: c.kwargs["duration_seconds"] for c in milestone_calls}
        assert phases_reported["curator"] == 10
        assert phases_reported["approval"] == 5
        assert phases_reported["builder"] == 100
        assert phases_reported["judge"] == 50
        assert phases_reported["merge"] == 5

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_summary_includes_duration_and_percentage(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        """Final summary should include per-phase duration and percentage."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # curator phase_start
            50,    # curator elapsed (50s)
            50,    # approval phase_start
            50,    # approval elapsed (0s)
            50,    # builder phase_start
            400,   # builder elapsed (350s)
            400,   # judge phase_start
            550,   # judge elapsed (150s)
            550,   # rebase phase_start
            550,   # rebase elapsed (0s, skipped)
            550,   # merge phase_start
            600,   # merge elapsed (50s)
            600,   # duration calc
        ])

        ctx = _make_ctx()

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (False, "")
        curator_inst.run.return_value = _success_result("curator")

        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        builder_inst.run.return_value = _success_result("builder")

        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        captured = capsys.readouterr()
        # log_info writes to stderr
        assert "Builder: 350s" in captured.err
        assert "Judge: 150s" in captured.err
        assert "Merge: 50s" in captured.err

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_judge_doctor_retry_accumulates_timing(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockDoctor: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
    ) -> None:
        """Judge/Doctor retry loop should accumulate timing per attempt."""
        # Flow: curator(skip) -> approval -> builder(skip) -> judge1(changes) -> doctor1 -> judge2(approved) -> rebase -> merge
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed (5s)
            # Judge attempt 1
            5,     # judge phase_start
            125,   # judge elapsed (120s)
            # Doctor attempt 1
            125,   # doctor phase_start
            220,   # doctor elapsed (95s)
            # Judge attempt 2
            220,   # judge phase_start
            300,   # judge elapsed (80s)
            # Rebase
            300,   # rebase phase_start
            303,   # rebase elapsed (3s)
            # Merge
            303,   # merge phase_start
            313,   # merge elapsed (10s)
            313,   # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)

        # Curator: skipped (start_from=builder means curator & builder skip)
        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")

        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (True, "skipped via --from")

        # Judge: first attempt requests changes, second approves
        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.side_effect = [
            _success_result("judge", changes_requested=True),
            _success_result("judge", approved=True),
        ]

        # Doctor: applies fixes
        MockDoctor.return_value.run.return_value = _success_result("doctor")

        # Rebase: skipped

        # Merge: success
        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        # Verify milestones: should have two judge phase_completed and one doctor phase_completed
        milestone_calls = [
            c for c in ctx.report_milestone.call_args_list
            if c[0][0] == "phase_completed"
        ]
        judge_milestones = [c for c in milestone_calls if c.kwargs["phase"] == "judge"]
        doctor_milestones = [c for c in milestone_calls if c.kwargs["phase"] == "doctor"]

        assert len(judge_milestones) == 2
        assert judge_milestones[0].kwargs["duration_seconds"] == 120
        assert judge_milestones[0].kwargs["status"] == "changes_requested"
        assert judge_milestones[1].kwargs["duration_seconds"] == 80
        assert judge_milestones[1].kwargs["status"] == "approved"

        assert len(doctor_milestones) == 1
        assert doctor_milestones[0].kwargs["duration_seconds"] == 95

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_skipped_phases_not_in_timing(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        """Skipped phases should not appear in timing summary."""
        # Curator and Builder skipped via --from judge
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            2,     # approval elapsed (2s)
            2,     # judge phase_start
            52,    # judge elapsed (50s)
            52,   # rebase phase_start
            52,   # rebase elapsed (0s, skipped)
            52,    # merge phase_start
            60,    # merge elapsed (8s)
            60,    # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.JUDGE)

        # Curator: skipped
        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")

        MockApproval.return_value.run.return_value = _success_result("approval")

        # Builder: skipped
        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (True, "skipped via --from")

        # Judge: skipped (--from merge skips judge too)
        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        captured = capsys.readouterr()
        # Curator and Builder should NOT appear in timing output
        assert "Curator:" not in captured.err
        assert "Builder:" not in captured.err
        # Judge and Merge should appear
        assert "Judge: 50s" in captured.err
        assert "Merge: 8s" in captured.err

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_completion_messages_include_elapsed(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        """Each phase completion log should include elapsed time."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # curator phase_start
            30,    # curator elapsed (30s)
            30,    # approval phase_start
            30,    # approval elapsed (0s)
            30,    # builder phase_start
            200,   # builder elapsed (170s)
            200,   # judge phase_start
            300,   # judge elapsed (100s)
            300,   # rebase phase_start
            300,   # rebase elapsed (0s, skipped)
            300,   # merge phase_start
            305,   # merge elapsed (5s)
            305,   # duration calc
        ])

        ctx = _make_ctx()

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (False, "")
        curator_inst.run.return_value = _success_result("curator")

        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        builder_inst.run.return_value = _success_result("builder")

        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        captured = capsys.readouterr()
        # log_success/log_info write to stderr
        assert "(30s)" in captured.err   # Curator
        assert "(170s)" in captured.err  # Builder
        assert "(100s)" in captured.err  # Judge
        assert "(5s)" in captured.err    # Merge


class TestPostCuratorBlockedCheck:
    """Test that shepherd aborts when curator flags issue as blocked (issue #2603)."""

    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_aborts_when_curator_adds_blocked_label(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
    ) -> None:
        """Should abort pipeline when curator adds loom:blocked during this run."""
        mock_time.time = MagicMock(side_effect=[0, 0, 10, 10])

        ctx = _make_ctx()
        # Simulate curator running successfully
        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (False, "")
        curator_inst.run.return_value = _success_result("curator")

        # After curator, label cache refresh shows loom:blocked
        ctx.has_issue_label.side_effect = lambda label: label == "loom:blocked"

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.NO_CHANGES_NEEDED

        # Approval phase should never have been instantiated/run
        MockApproval.return_value.run.assert_not_called()

        # Should report blocked milestone
        blocked_calls = [
            c for c in ctx.report_milestone.call_args_list
            if c[0][0] == "blocked" and c.kwargs.get("reason") == "curator_blocked"
        ]
        assert len(blocked_calls) == 1

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_proceeds_when_no_blocked_label(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
    ) -> None:
        """Should proceed normally when curator does not add loom:blocked."""
        mock_time.time = MagicMock(side_effect=[
            0, 0, 10, 10, 15, 15, 115, 115, 165, 165, 165, 165, 170, 170,
        ])

        ctx = _make_ctx()
        ctx.has_issue_label.return_value = False

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (False, "")
        curator_inst.run.return_value = _success_result("curator")

        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        builder_inst.run.return_value = _success_result("builder")

        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.SUCCESS

        # Approval phase should have been called
        MockApproval.return_value.run.assert_called_once()

    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_aborts_even_in_merge_mode(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
    ) -> None:
        """Merge mode should NOT override a fresh loom:blocked from the current curator."""
        mock_time.time = MagicMock(side_effect=[0, 0, 10, 10])

        ctx = _make_ctx(mode=ExecutionMode.FORCE_MERGE)
        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (False, "")
        curator_inst.run.return_value = _success_result("curator")

        # Curator added loom:blocked during this run
        ctx.has_issue_label.side_effect = lambda label: label == "loom:blocked"

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.NO_CHANGES_NEEDED
        MockApproval.return_value.run.assert_not_called()


class TestDoctorSkippedHeader:
    """Test Doctor phase skipped header when Judge approves first try (issue #1767)."""

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_doctor_skipped_header_when_judge_approves_first_try(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        """Should print 'PHASE 5: DOCTOR (skipped)' when Judge approves without changes."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # curator phase_start
            10,    # curator elapsed
            10,    # approval phase_start
            15,    # approval elapsed
            15,    # builder phase_start
            100,   # builder elapsed
            100,   # judge phase_start
            150,   # judge elapsed
            150,   # rebase phase_start
            150,   # rebase elapsed (0s, skipped)
            150,   # merge phase_start
            160,   # merge elapsed
            160,   # duration calc
        ])

        ctx = _make_ctx()

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (False, "")
        curator_inst.run.return_value = _success_result("curator")

        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        builder_inst.run.return_value = _success_result("builder")

        # Judge approves on first attempt - no changes requested
        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        captured = capsys.readouterr()
        assert "PHASE 5: DOCTOR (skipped - no changes requested)" in captured.err

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_no_skipped_header_when_doctor_runs(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockDoctor: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        """Should NOT print skipped header when Doctor actually runs."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # judge attempt 1 phase_start
            50,    # judge attempt 1 elapsed
            50,    # doctor phase_start
            100,   # doctor elapsed
            100,   # judge attempt 2 phase_start
            150,   # judge attempt 2 elapsed
            150,   # rebase phase_start
            150,   # rebase elapsed (0s, skipped)
            150,   # merge phase_start
            160,   # merge elapsed
            160,   # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")

        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (True, "skipped via --from")

        # Judge: first requests changes, second approves
        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.side_effect = [
            _success_result("judge", changes_requested=True),
            _success_result("judge", approved=True),
        ]

        MockDoctor.return_value.run.return_value = _success_result("doctor")
        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        captured = capsys.readouterr()
        # Should see Doctor phase header with attempt number, NOT skipped
        assert "PHASE 5: DOCTOR (attempt 1)" in captured.err
        assert "PHASE 5: DOCTOR (skipped" not in captured.err

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_no_skipped_header_when_judge_skipped(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        """Should NOT print Doctor skipped header when Judge itself was skipped."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,   # rebase phase_start
            5,   # rebase elapsed (0s, skipped)
            5,     # merge phase_start
            10,    # merge elapsed
            10,    # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.MERGE)

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")

        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (True, "skipped via --from")

        # Judge: skipped entirely
        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (True, "skipped via --from")

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        captured = capsys.readouterr()
        # Neither Doctor header should appear when Judge was skipped
        assert "PHASE 5: DOCTOR" not in captured.err


class TestApprovalPhaseSummary:
    """Test approval phase summary formatting (issue #1713)."""

    def test_approval_uses_summary_from_data(self) -> None:
        """Approval completed_phases entry should use data['summary'] for clean output."""
        result = PhaseResult(
            status=PhaseStatus.SUCCESS,
            message="issue already approved (has loom:issue label)",
            phase_name="approval",
            data={"summary": "already approved"},
        )
        entry = f"Approval ({result.data.get('summary', result.message)})"
        assert entry == "Approval (already approved)"

    def test_approval_falls_back_to_message_without_summary(self) -> None:
        """Approval should fall back to full message when data has no summary."""
        result = PhaseResult(
            status=PhaseStatus.SUCCESS,
            message="approval done",
            phase_name="approval",
        )
        entry = f"Approval ({result.data.get('summary', result.message)})"
        assert entry == "Approval (approval done)"

    def test_old_parsing_was_broken_for_parenthesized_messages(self) -> None:
        """Verify the old parsing produced malformed output (regression guard)."""
        # The old code: f"Approval ({result.message.split('(')[-1].rstrip(')')}"
        # For "issue already approved (has loom:issue label)", this produced:
        #   "Approval (has loom:issue label" (missing closing paren)
        message = "issue already approved (has loom:issue label)"
        old_output = f"Approval ({message.split('(')[-1].rstrip(')')}"
        assert old_output == "Approval (has loom:issue label"  # broken - no closing paren

        # New code produces correct output
        result = PhaseResult(
            status=PhaseStatus.SUCCESS,
            message=message,
            phase_name="approval",
            data={"summary": "already approved"},
        )
        new_output = f"Approval ({result.data.get('summary', result.message)})"
        assert new_output == "Approval (already approved)"  # correct

    def test_old_parsing_was_broken_for_no_paren_messages(self) -> None:
        """Verify the old parsing was broken for messages without parens."""
        # For "issue approved by human" (no parens), split('(')[-1] returns full string
        message = "issue approved by human"
        old_output = f"Approval ({message.split('(')[-1].rstrip(')')}"
        assert old_output == "Approval (issue approved by human"  # broken

        # New code with summary produces correct output
        result = PhaseResult(
            status=PhaseStatus.SUCCESS,
            message=message,
            phase_name="approval",
            data={"summary": "human approved"},
        )
        new_output = f"Approval ({result.data.get('summary', result.message)})"
        assert new_output == "Approval (human approved)"  # correct


def _failed_result(phase: str = "", message: str = "phase failed") -> PhaseResult:
    """Create a failed PhaseResult."""
    return PhaseResult(status=PhaseStatus.FAILED, message=message, phase_name=phase)


class TestJudgePhaseHeader:
    """Test judge phase header shows correct attempt count (issue #2008)."""

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_judge_header_shows_correct_retry_count(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        """Judge phase header should show judge_retries, not doctor_attempts.

        Before fix (bug): PHASE 4: JUDGE (attempt 1) on first run, then
        PHASE 4: JUDGE (attempt 1) again on retry (used doctor_attempts).

        After fix: PHASE 4: JUDGE (attempt 1) on first run, then
        PHASE 4: JUDGE (attempt 2) on retry (uses judge_retries).
        """
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            # Judge attempt 1 (fails)
            5,     # judge phase_start
            10,    # judge elapsed
            # Judge attempt 2 (succeeds)
            10,    # judge phase_start
            20,    # judge elapsed
            # Merge
            20,   # rebase phase_start
            20,   # rebase elapsed (0s, skipped)
            20,    # merge phase_start
            25,    # merge elapsed
            25,    # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")
        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (True, "skipped via --from")

        # Judge: first call fails, second approves
        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.side_effect = [
            _failed_result("judge", "validation failed"),
            _success_result("judge", approved=True),
        ]

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        captured = capsys.readouterr()
        # Should see "attempt 1" then "attempt 2" (not "attempt 1" twice)
        assert "PHASE 4: JUDGE (attempt 1)" in captured.err
        assert "PHASE 4: JUDGE (attempt 2)" in captured.err


class TestJudgeRetry:
    """Test judge retry logic when judge phase fails (issue #1909)."""

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_judge_failure_triggers_retry_then_succeeds(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
    ) -> None:
        """Judge FAILED on first call, SUCCESS with approved on second — should complete."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            # Judge attempt 1 (fails)
            5,     # judge phase_start
            10,    # judge elapsed
            # Judge attempt 2 (succeeds with approved)
            10,    # judge phase_start
            20,    # judge elapsed
            # Merge
            20,   # rebase phase_start
            20,   # rebase elapsed (0s, skipped)
            20,    # merge phase_start
            25,    # merge elapsed
            25,    # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)
        # Default judge_max_retries=3, so 3 retries allowed
        assert ctx.config.judge_max_retries == 3

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")
        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (True, "skipped via --from")

        # Judge: first call fails, second approves
        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.side_effect = [
            _failed_result("judge", "judge phase validation failed"),
            _success_result("judge", approved=True),
        ]

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        # Verify judge_retry milestone was reported
        retry_calls = [
            c for c in ctx.report_milestone.call_args_list
            if c[0][0] == "judge_retry"
        ]
        assert len(retry_calls) == 1
        assert retry_calls[0].kwargs["attempt"] == 1

    @patch("loom_tools.shepherd.cli._mark_judge_exhausted")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_judge_retry_exhaustion_marks_blocked(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        mock_mark_exhausted: MagicMock,
    ) -> None:
        """Judge always fails — should call _mark_judge_exhausted and return 1."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            # Judge attempt 1 (fails — triggers retry)
            5,     # judge phase_start
            10,    # judge elapsed
            # Judge attempt 2 (fails again — retries exhausted)
            10,    # judge phase_start
            15,    # judge elapsed
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)
        # Use judge_max_retries=1 for this test to focus on exhaustion logic
        ctx.config.judge_max_retries = 1

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")
        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (True, "skipped via --from")

        # Judge: always fails
        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _failed_result("judge", "validation failed")

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.NEEDS_INTERVENTION

        # Verify _mark_judge_exhausted was called with retry count (1):
        # first fail triggers retry (judge_retries=1), second fail exhausts retries
        mock_mark_exhausted.assert_called_once_with(ctx, 1)

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_approved_path_unchanged_with_retry_logic(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
    ) -> None:
        """Approved outcome on first try should work identically to before."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # judge phase_start
            15,    # judge elapsed
            15,   # rebase phase_start
            15,   # rebase elapsed (0s, skipped)
            15,    # merge phase_start
            20,    # merge elapsed
            20,    # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")
        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (True, "skipped via --from")

        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        # No retry milestones should be reported
        retry_calls = [
            c for c in ctx.report_milestone.call_args_list
            if c[0][0] == "judge_retry"
        ]
        assert len(retry_calls) == 0

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_changes_requested_path_unchanged_with_retry_logic(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockDoctor: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
    ) -> None:
        """Changes-requested -> doctor -> approved flow should work as before."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            # Judge 1: changes requested
            5,     # judge phase_start
            15,    # judge elapsed
            # Doctor
            15,    # doctor phase_start
            25,    # doctor elapsed
            # Judge 2: approved
            25,    # judge phase_start
            35,    # judge elapsed
            # Merge
            35,   # rebase phase_start
            35,   # rebase elapsed (0s, skipped)
            35,    # merge phase_start
            40,    # merge elapsed
            40,    # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")
        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (True, "skipped via --from")

        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.side_effect = [
            _success_result("judge", changes_requested=True),
            _success_result("judge", approved=True),
        ]

        MockDoctor.return_value.run.return_value = _success_result("doctor")
        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        # No judge retry milestones
        retry_calls = [
            c for c in ctx.report_milestone.call_args_list
            if c[0][0] == "judge_retry"
        ]
        assert len(retry_calls) == 0

        # Doctor milestone should be present
        doctor_calls = [
            c for c in ctx.report_milestone.call_args_list
            if c[0][0] == "phase_completed" and c.kwargs.get("phase") == "doctor"
        ]
        assert len(doctor_calls) == 1

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_judge_failure_retry_reports_milestones(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
    ) -> None:
        """Judge retry milestone should include attempt count and reason."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            # Judge attempt 1 (fails)
            5,     # judge phase_start
            10,    # judge elapsed
            # Judge attempt 2 (approves)
            10,    # judge phase_start
            20,    # judge elapsed
            # Merge
            20,   # rebase phase_start
            20,   # rebase elapsed (0s, skipped)
            20,    # merge phase_start
            25,    # merge elapsed
            25,    # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")
        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (True, "skipped via --from")

        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.side_effect = [
            _failed_result("judge", "validation failed"),
            _success_result("judge", approved=True),
        ]

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        # Verify milestone content
        retry_calls = [
            c for c in ctx.report_milestone.call_args_list
            if c[0][0] == "judge_retry"
        ]
        assert len(retry_calls) == 1
        assert retry_calls[0].kwargs["attempt"] == 1
        assert retry_calls[0].kwargs["max_retries"] == 3
        assert "validation failed" in retry_calls[0].kwargs["reason"]

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_infrastructure_failure_adds_backoff_before_retry(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
    ) -> None:
        """Infrastructure failures (low_output, mcp_failure, ghost_session) should
        trigger backoff sleep before judge retry.  See issue #2666."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            # Judge attempt 1 (infrastructure failure)
            5,     # judge phase_start
            10,    # judge elapsed
            # Judge attempt 2 (succeeds)
            10,    # judge phase_start
            20,    # judge elapsed
            # Merge
            20,   # rebase phase_start
            20,   # rebase elapsed (0s, skipped)
            20,    # merge phase_start
            25,    # merge elapsed
            25,    # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")
        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (True, "skipped via --from")

        # Judge: first call fails with low_output infrastructure flag,
        # second call succeeds
        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.side_effect = [
            PhaseResult(
                status=PhaseStatus.FAILED,
                message="judge low output after retry",
                phase_name="judge",
                data={"low_output": True},
            ),
            _success_result("judge", approved=True),
        ]

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        # Verify backoff sleep was called (30s for first retry)
        sleep_calls = mock_time.sleep.call_args_list
        assert any(c[0][0] == 30 for c in sleep_calls), (
            f"Expected 30s backoff sleep for infrastructure failure, "
            f"got: {sleep_calls}"
        )

        # Verify heartbeat milestone reported the backoff
        heartbeat_calls = [
            c for c in ctx.report_milestone.call_args_list
            if c[0][0] == "heartbeat"
            and "infrastructure backoff" in c.kwargs.get("action", "")
        ]
        assert len(heartbeat_calls) == 1

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_non_infrastructure_failure_no_backoff(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
    ) -> None:
        """Non-infrastructure judge failures should NOT add backoff sleep.
        See issue #2666."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            # Judge attempt 1 (non-infra failure)
            5,     # judge phase_start
            10,    # judge elapsed
            # Judge attempt 2 (succeeds)
            10,    # judge phase_start
            20,    # judge elapsed
            # Merge
            20,   # rebase phase_start
            20,   # rebase elapsed (0s, skipped)
            20,    # merge phase_start
            25,    # merge elapsed
            25,    # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")
        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (True, "skipped via --from")

        # Judge: first call fails with NO infrastructure flags, second succeeds
        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.side_effect = [
            _failed_result("judge", "validation failed"),
            _success_result("judge", approved=True),
        ]

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        # Verify NO backoff sleep was called (non-infrastructure failure)
        sleep_calls = mock_time.sleep.call_args_list
        assert not any(c[0][0] >= 30 for c in sleep_calls), (
            f"Expected no infrastructure backoff for non-infra failure, "
            f"got: {sleep_calls}"
        )

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_unexpected_result_with_changes_requested_label_enters_doctor(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockDoctor: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
    ) -> None:
        """Unexpected judge result with loom:changes-requested label should enter doctor loop.

        When the judge returns an unexpected result (no approved/changes_requested
        data) but the loom:changes-requested label is present, the shepherd should
        detect the label and route to the doctor loop instead of retrying the judge.
        See issue #2345.
        """
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            # Judge attempt 1 (unexpected result, but label present)
            5,     # judge phase_start
            10,    # judge elapsed
            # Doctor (fixes changes)
            10,    # doctor phase_start
            20,    # doctor elapsed
            # Judge attempt 2 (approves after doctor fix)
            20,    # judge phase_start
            30,    # judge elapsed
            # Merge
            30,   # rebase phase_start
            30,   # rebase elapsed (0s, skipped)
            30,    # merge phase_start
            35,    # merge elapsed
            35,    # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")
        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (True, "skipped via --from")

        # Judge: first call returns unexpected result (no approved/changes_requested),
        # second call approves after doctor fixes.
        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.side_effect = [
            _success_result("judge"),  # unexpected: no approved or changes_requested
            _success_result("judge", approved=True),
        ]

        # Configure has_pr_label to return True only for loom:changes-requested
        def label_side_effect(label: str) -> bool:
            return label == "loom:changes-requested"
        ctx.has_pr_label.side_effect = label_side_effect

        MockDoctor.return_value.run.return_value = _success_result("doctor")
        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        # Label cache should have been invalidated
        ctx.label_cache.invalidate_pr.assert_called()

        # Doctor should have been invoked (changes_requested detected via label)
        MockDoctor.return_value.run.assert_called_once()

        # No judge_retry milestones — label detection should skip retry
        retry_calls = [
            c for c in ctx.report_milestone.call_args_list
            if c[0][0] == "judge_retry"
        ]
        assert len(retry_calls) == 0

        # Doctor phase_completed milestone should be present
        doctor_calls = [
            c for c in ctx.report_milestone.call_args_list
            if c[0][0] == "phase_completed" and c.kwargs.get("phase") == "doctor"
        ]
        assert len(doctor_calls) == 1


class TestMarkJudgeExhausted:
    """Test _mark_judge_exhausted helper function."""

    @patch("subprocess.run")
    def test_transitions_labels(self, mock_run: MagicMock) -> None:
        """Should run gh command to transition loom:building -> loom:blocked."""
        mock_run.return_value = MagicMock(returncode=0)

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = "/fake/repo"

        with patch("loom_tools.common.systematic_failure.record_blocked_reason"), \
             patch("loom_tools.common.systematic_failure.detect_systematic_failure"):
            _mark_judge_exhausted(ctx, 1)

        # First subprocess call should be the label transition
        label_call = mock_run.call_args_list[0]
        cmd = label_call[0][0]
        assert "gh" in cmd
        assert "--remove-label" in cmd
        assert "loom:building" in cmd
        assert "--add-label" in cmd
        assert "loom:blocked" in cmd

    @patch("subprocess.run")
    def test_records_blocked_reason(self, mock_run: MagicMock) -> None:
        """Should record judge_exhausted as the blocked reason."""
        mock_run.return_value = MagicMock(returncode=0)

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = "/fake/repo"

        with patch("loom_tools.common.systematic_failure.record_blocked_reason") as mock_record, \
             patch("loom_tools.common.systematic_failure.detect_systematic_failure"):
            _mark_judge_exhausted(ctx, 2)

        mock_record.assert_called_once_with(
            "/fake/repo",
            42,
            error_class="judge_exhausted",
            phase="judge",
            details="judge failed after 2 retry attempt(s)",
        )

    @patch("subprocess.run")
    def test_adds_diagnostic_comment(self, mock_run: MagicMock) -> None:
        """Should add a comment with diagnostic info."""
        mock_run.return_value = MagicMock(returncode=0)

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = "/fake/repo"

        with patch("loom_tools.common.systematic_failure.record_blocked_reason"), \
             patch("loom_tools.common.systematic_failure.detect_systematic_failure"):
            _mark_judge_exhausted(ctx, 1)

        # Second subprocess call should be the comment
        comment_call = mock_run.call_args_list[1]
        cmd = comment_call[0][0]
        assert "comment" in cmd
        body_idx = cmd.index("--body") + 1
        assert "Judge phase failed" in cmd[body_idx]
        assert "1 retry" in cmd[body_idx]


class TestMarkBuilderNoPr:
    """Test _mark_builder_no_pr helper function (issue #1982)."""

    @patch("loom_tools.shepherd.cli._gather_no_pr_diagnostics")
    @patch("subprocess.run")
    def test_transitions_labels(
        self, mock_run: MagicMock, mock_gather: MagicMock
    ) -> None:
        """Should run gh command to transition loom:building -> loom:blocked."""
        mock_run.return_value = MagicMock(returncode=0)
        mock_gather.return_value = {
            "worktree_exists": False,
            "uncommitted_files": [],
            "uncommitted_count": 0,
            "commits_ahead_of_main": 0,
            "remote_branch_exists": False,
            "current_branch": "main",
            "suggested_recovery": "re-run shepherd",
        }

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = "/fake/repo"

        with patch("loom_tools.common.systematic_failure.record_blocked_reason"), \
             patch("loom_tools.common.systematic_failure.detect_systematic_failure"):
            _mark_builder_no_pr(ctx)

        # First subprocess call should be the label transition
        label_call = mock_run.call_args_list[0]
        cmd = label_call[0][0]
        assert "gh" in cmd
        assert "--remove-label" in cmd
        assert "loom:building" in cmd
        assert "--add-label" in cmd
        assert "loom:blocked" in cmd

    @patch("loom_tools.shepherd.cli._gather_no_pr_diagnostics")
    @patch("subprocess.run")
    def test_records_blocked_reason(
        self, mock_run: MagicMock, mock_gather: MagicMock
    ) -> None:
        """Should record builder_no_pr as the blocked reason."""
        mock_run.return_value = MagicMock(returncode=0)
        mock_gather.return_value = {
            "worktree_exists": False,
            "uncommitted_files": [],
            "uncommitted_count": 0,
            "commits_ahead_of_main": 0,
            "remote_branch_exists": False,
            "current_branch": "main",
            "suggested_recovery": "re-run shepherd",
        }

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = "/fake/repo"

        with patch("loom_tools.common.systematic_failure.record_blocked_reason") as mock_record, \
             patch("loom_tools.common.systematic_failure.detect_systematic_failure"):
            _mark_builder_no_pr(ctx)

        mock_record.assert_called_once_with(
            "/fake/repo",
            42,
            error_class="builder_no_pr",
            phase="builder",
            details="Builder phase completed but no PR was created",
        )

    @patch("loom_tools.shepherd.cli._gather_no_pr_diagnostics")
    @patch("subprocess.run")
    def test_adds_diagnostic_comment(
        self, mock_run: MagicMock, mock_gather: MagicMock
    ) -> None:
        """Should add a comment with diagnostic info."""
        mock_run.return_value = MagicMock(returncode=0)
        mock_gather.return_value = {
            "worktree_exists": True,
            "worktree_path": "/fake/repo/.loom/worktrees/issue-42",
            "uncommitted_files": ["M src/file.py"],
            "uncommitted_count": 1,
            "commits_ahead_of_main": 0,
            "remote_branch_exists": False,
            "current_branch": "feature/issue-42",
            "suggested_recovery": "commit changes, push, create PR manually",
        }

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = "/fake/repo"
        ctx.issue_title = "fix: broken widget"

        with patch("loom_tools.common.systematic_failure.record_blocked_reason"), \
             patch("loom_tools.common.systematic_failure.detect_systematic_failure"):
            _mark_builder_no_pr(ctx)

        # Second subprocess call should be the comment
        comment_call = mock_run.call_args_list[1]
        cmd = comment_call[0][0]
        assert "comment" in cmd
        body_idx = cmd.index("--body") + 1
        assert "Builder phase failed" in cmd[body_idx]
        assert "No PR was created" in cmd[body_idx]
        # Verify diagnostic info is in the comment
        assert "Diagnostics" in cmd[body_idx]
        # Verify issue title is used in recovery commands
        assert "fix: broken widget" in cmd[body_idx]
        assert "Worktree exists | yes" in cmd[body_idx]
        assert "Suggested Recovery" in cmd[body_idx]


class TestGatherNoPrDiagnostics:
    """Test _gather_no_pr_diagnostics helper function (issue #2065)."""

    @patch("loom_tools.common.git.run_git")
    @patch("loom_tools.common.git.get_current_branch")
    @patch("loom_tools.common.git.get_commit_count")
    @patch("loom_tools.common.git.get_uncommitted_files")
    def test_gathers_worktree_state_when_exists(
        self,
        mock_uncommitted: MagicMock,
        mock_commits: MagicMock,
        mock_branch: MagicMock,
        mock_run_git: MagicMock,
        tmp_path: Path,
    ) -> None:
        """Should gather git state from worktree when it exists."""
        # Create a mock worktree directory
        worktree_path = tmp_path / ".loom" / "worktrees" / "issue-42"
        worktree_path.mkdir(parents=True)

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = tmp_path
        ctx.worktree_path = worktree_path

        # Mock git operations
        mock_uncommitted.return_value = ["M src/file1.py", "?? src/file2.py"]
        mock_commits.return_value = 3
        mock_branch.return_value = "feature/issue-42"
        mock_run_git.return_value = MagicMock(returncode=0, stdout="abc123\trefs/heads/feature/issue-42\n")

        diagnostics = _gather_no_pr_diagnostics(ctx)

        assert diagnostics["worktree_exists"] is True
        assert diagnostics["worktree_path"] == str(worktree_path)
        assert diagnostics["uncommitted_count"] == 2
        assert diagnostics["commits_ahead_of_main"] == 3
        assert diagnostics["current_branch"] == "feature/issue-42"
        assert diagnostics["remote_branch_exists"] is True

    @patch("loom_tools.common.git.run_git")
    @patch("loom_tools.common.git.get_current_branch")
    @patch("loom_tools.common.git.get_commit_count")
    @patch("loom_tools.common.git.get_uncommitted_files")
    def test_handles_no_worktree(
        self,
        mock_uncommitted: MagicMock,
        mock_commits: MagicMock,
        mock_branch: MagicMock,
        mock_run_git: MagicMock,
        tmp_path: Path,
    ) -> None:
        """Should handle case when worktree doesn't exist."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = tmp_path
        ctx.worktree_path = tmp_path / ".loom" / "worktrees" / "issue-42"  # Doesn't exist

        mock_uncommitted.return_value = []
        mock_commits.return_value = 0
        mock_branch.return_value = "main"
        mock_run_git.return_value = MagicMock(returncode=1, stdout="")

        diagnostics = _gather_no_pr_diagnostics(ctx)

        assert diagnostics["worktree_exists"] is False
        assert diagnostics["suggested_recovery"] == "re-run shepherd or create worktree manually"

    @patch("loom_tools.common.git.run_git")
    @patch("loom_tools.common.git.get_current_branch")
    @patch("loom_tools.common.git.get_commit_count")
    @patch("loom_tools.common.git.get_uncommitted_files")
    def test_suggests_commit_when_uncommitted_no_commits(
        self,
        mock_uncommitted: MagicMock,
        mock_commits: MagicMock,
        mock_branch: MagicMock,
        mock_run_git: MagicMock,
        tmp_path: Path,
    ) -> None:
        """Should suggest commit when there are uncommitted changes but no commits."""
        worktree_path = tmp_path / ".loom" / "worktrees" / "issue-42"
        worktree_path.mkdir(parents=True)

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = tmp_path
        ctx.worktree_path = worktree_path

        mock_uncommitted.return_value = ["M src/file1.py"]
        mock_commits.return_value = 0  # No commits ahead
        mock_branch.return_value = "feature/issue-42"
        mock_run_git.return_value = MagicMock(returncode=1, stdout="")

        diagnostics = _gather_no_pr_diagnostics(ctx)

        assert diagnostics["suggested_recovery"] == "commit changes, push, create PR manually"

    @patch("loom_tools.common.git.run_git")
    @patch("loom_tools.common.git.get_current_branch")
    @patch("loom_tools.common.git.get_commit_count")
    @patch("loom_tools.common.git.get_uncommitted_files")
    def test_suggests_push_when_commits_no_remote(
        self,
        mock_uncommitted: MagicMock,
        mock_commits: MagicMock,
        mock_branch: MagicMock,
        mock_run_git: MagicMock,
        tmp_path: Path,
    ) -> None:
        """Should suggest push when commits exist but remote branch doesn't."""
        worktree_path = tmp_path / ".loom" / "worktrees" / "issue-42"
        worktree_path.mkdir(parents=True)

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = tmp_path
        ctx.worktree_path = worktree_path

        mock_uncommitted.return_value = []  # No uncommitted
        mock_commits.return_value = 2  # Commits ahead
        mock_branch.return_value = "feature/issue-42"
        mock_run_git.return_value = MagicMock(returncode=1, stdout="")  # No remote

        diagnostics = _gather_no_pr_diagnostics(ctx)

        assert diagnostics["suggested_recovery"] == "push branch, create PR manually"

    @patch("loom_tools.common.git.run_git")
    @patch("loom_tools.common.git.get_current_branch")
    @patch("loom_tools.common.git.get_commit_count")
    @patch("loom_tools.common.git.get_uncommitted_files")
    def test_suggests_create_pr_when_branch_pushed(
        self,
        mock_uncommitted: MagicMock,
        mock_commits: MagicMock,
        mock_branch: MagicMock,
        mock_run_git: MagicMock,
        tmp_path: Path,
    ) -> None:
        """Should suggest create PR when branch already pushed."""
        worktree_path = tmp_path / ".loom" / "worktrees" / "issue-42"
        worktree_path.mkdir(parents=True)

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = tmp_path
        ctx.worktree_path = worktree_path

        mock_uncommitted.return_value = []
        mock_commits.return_value = 2
        mock_branch.return_value = "feature/issue-42"
        mock_run_git.return_value = MagicMock(returncode=0, stdout="abc123\trefs/heads/feature/issue-42\n")

        diagnostics = _gather_no_pr_diagnostics(ctx)

        assert diagnostics["suggested_recovery"] == "create PR manually (branch already pushed)"


class TestFormatDiagnosticsForLog:
    """Test _format_diagnostics_for_log helper function (issue #2065)."""

    def test_formats_full_diagnostics(self) -> None:
        """Should format all diagnostic fields for log output."""
        diagnostics = {
            "worktree_exists": True,
            "worktree_path": "/path/to/.loom/worktrees/issue-42",
            "uncommitted_files": ["M src/file1.py", "?? src/file2.py"],
            "uncommitted_count": 2,
            "commits_ahead_of_main": 3,
            "remote_branch_exists": True,
            "current_branch": "feature/issue-42",
            "suggested_recovery": "create PR manually",
        }

        output = _format_diagnostics_for_log(diagnostics)

        assert "Diagnostics:" in output
        assert "Worktree exists: yes" in output
        assert "/path/to/.loom/worktrees/issue-42" in output
        assert "Uncommitted changes: 2 file(s)" in output
        assert "M src/file1.py" in output
        assert "Commits ahead of main: 3" in output
        assert "Remote branch exists: yes" in output
        assert "Current branch: feature/issue-42" in output
        assert "Suggested recovery: create PR manually" in output

    def test_formats_no_uncommitted(self) -> None:
        """Should show 'none' when no uncommitted files."""
        diagnostics = {
            "worktree_exists": True,
            "worktree_path": "/path",
            "uncommitted_files": [],
            "uncommitted_count": 0,
            "commits_ahead_of_main": 0,
            "remote_branch_exists": False,
            "current_branch": "main",
            "suggested_recovery": "investigate",
        }

        output = _format_diagnostics_for_log(diagnostics)

        assert "Uncommitted changes: none" in output

    def test_truncates_long_file_list(self) -> None:
        """Should show first 5 files and count of remaining."""
        diagnostics = {
            "worktree_exists": True,
            "worktree_path": "/path",
            "uncommitted_files": [f"M file{i}.py" for i in range(10)],
            "uncommitted_count": 10,
            "commits_ahead_of_main": 0,
            "remote_branch_exists": False,
            "current_branch": "main",
            "suggested_recovery": "commit changes",
        }

        output = _format_diagnostics_for_log(diagnostics)

        assert "M file0.py" in output
        assert "M file4.py" in output
        assert "... and 5 more" in output


class TestFormatDiagnosticsForComment:
    """Test _format_diagnostics_for_comment helper function (issue #2065)."""

    def test_formats_markdown_table(self) -> None:
        """Should format diagnostics as markdown table."""
        diagnostics = {
            "worktree_exists": True,
            "worktree_path": "/path/to/.loom/worktrees/issue-42",
            "uncommitted_files": ["M src/file1.py"],
            "uncommitted_count": 1,
            "commits_ahead_of_main": 2,
            "remote_branch_exists": False,
            "current_branch": "feature/issue-42",
            "suggested_recovery": "push branch, create PR manually",
        }

        output = _format_diagnostics_for_comment(diagnostics, 42)

        assert "**Builder phase failed**" in output
        assert "### Diagnostics" in output
        assert "| Property | Value |" in output
        assert "| Worktree exists | yes |" in output
        assert "| Uncommitted changes | 1 file(s) |" in output
        assert "| Commits ahead of main | 2 |" in output
        assert "### Suggested Recovery" in output

    def test_includes_recovery_commands(self) -> None:
        """Should include concrete recovery bash commands."""
        diagnostics = {
            "worktree_exists": True,
            "worktree_path": "/path/to/.loom/worktrees/issue-42",
            "uncommitted_files": ["M src/file1.py"],
            "uncommitted_count": 1,
            "commits_ahead_of_main": 0,
            "remote_branch_exists": False,
            "current_branch": "feature/issue-42",
            "suggested_recovery": "commit changes, push, create PR manually",
        }

        output = _format_diagnostics_for_comment(diagnostics, 42)

        assert "```bash" in output
        assert "git add ." in output
        assert "git commit" in output
        assert "git push -u origin feature/issue-42" in output
        assert "gh pr create" in output
        assert "Closes #42" in output

    def test_lists_uncommitted_files(self) -> None:
        """Should list uncommitted files in comment."""
        diagnostics = {
            "worktree_exists": True,
            "worktree_path": "/path",
            "uncommitted_files": ["M src/a.py", "?? src/b.py", "D src/c.py"],
            "uncommitted_count": 3,
            "commits_ahead_of_main": 0,
            "remote_branch_exists": False,
            "current_branch": "feature/issue-42",
            "suggested_recovery": "commit changes",
        }

        output = _format_diagnostics_for_comment(diagnostics, 42)

        assert "**Uncommitted files:**" in output
        assert "- `M src/a.py`" in output
        assert "- `?? src/b.py`" in output
        assert "- `D src/c.py`" in output

    def test_uses_issue_title_in_recovery_commands(self) -> None:
        """Should use issue title instead of 'Issue #N' in gh pr create."""
        diagnostics = {
            "worktree_exists": True,
            "worktree_path": "/path/to/.loom/worktrees/issue-42",
            "uncommitted_files": [],
            "uncommitted_count": 0,
            "commits_ahead_of_main": 1,
            "remote_branch_exists": True,
            "current_branch": "feature/issue-42",
            "suggested_recovery": "create PR manually",
        }

        output = _format_diagnostics_for_comment(diagnostics, 42, "fix: broken widget")

        assert "fix: broken widget" in output
        assert "Issue #42" not in output

    def test_falls_back_to_conventional_title_when_no_title(self) -> None:
        """Should fall back to conventional commit title when issue_title is empty."""
        diagnostics = {
            "worktree_exists": True,
            "worktree_path": "/path/to/.loom/worktrees/issue-42",
            "uncommitted_files": [],
            "uncommitted_count": 0,
            "commits_ahead_of_main": 1,
            "remote_branch_exists": True,
            "current_branch": "feature/issue-42",
            "suggested_recovery": "create PR manually",
        }

        output = _format_diagnostics_for_comment(diagnostics, 42)

        assert "feat: implement changes for issue #42" in output
        assert "Issue #42" not in output

    def test_escapes_special_characters_in_title(self) -> None:
        """Should shell-escape special characters in issue titles."""
        diagnostics = {
            "worktree_exists": True,
            "worktree_path": "/path/to/.loom/worktrees/issue-42",
            "uncommitted_files": [],
            "uncommitted_count": 0,
            "commits_ahead_of_main": 1,
            "remote_branch_exists": True,
            "current_branch": "feature/issue-42",
            "suggested_recovery": "create PR manually",
        }

        output = _format_diagnostics_for_comment(
            diagnostics, 42, "fix: handle `backtick` and 'quote'"
        )

        # shlex.quote wraps the title in single quotes and escapes internal quotes
        assert "gh pr create --title" in output
        assert "backtick" in output


class TestBuilderNoPrPrecondition:
    """Test that Judge phase is not entered when PR doesn't exist (issue #1982)."""

    @patch("loom_tools.shepherd.cli._mark_builder_no_pr")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_no_pr_skips_judge_and_marks_blocked(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        mock_mark_no_pr: MagicMock,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        """When builder completes without PR, Judge should NOT be entered."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)
        # Simulate builder completing but NOT creating a PR
        ctx.pr_number = None

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")
        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (True, "skipped via --from")

        result = orchestrate(ctx)
        assert result == 1

        # Verify _mark_builder_no_pr was called
        mock_mark_no_pr.assert_called_once_with(ctx)

        # Verify the error message
        captured = capsys.readouterr()
        assert "no PR was created" in captured.err

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_pr_exists_continues_to_judge(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
    ) -> None:
        """When PR exists, Judge phase should proceed normally."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
            100,   # judge phase_start
            150,   # judge elapsed
            150,   # rebase phase_start
            153,   # rebase elapsed
            153,   # merge phase_start
            163,   # merge elapsed
            163,   # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)
        # PR exists
        ctx.pr_number = 100

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")
        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (True, "skipped via --from")

        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        # Judge should have been called
        judge_inst.run.assert_called_once()

    @patch("loom_tools.shepherd.cli._mark_builder_no_pr")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_no_judge_retry_when_no_pr(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        mock_mark_no_pr: MagicMock,
    ) -> None:
        """Judge should NOT be retried when there's no PR (it's a precondition failure)."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)
        ctx.pr_number = None

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")
        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (True, "skipped via --from")

        result = orchestrate(ctx)
        assert result == 1

        # Judge should NEVER have been called
        judge_inst = MockJudge.return_value
        judge_inst.run.assert_not_called()

        # No judge retry milestones
        retry_calls = [
            c for c in ctx.report_milestone.call_args_list
            if c[0][0] == "judge_retry"
        ]
        assert len(retry_calls) == 0


class TestDoctorTestFixLoop:
    """Test builder test failure → Doctor test-fix loop (issue #2046)."""

    @patch("loom_tools.shepherd.cli._mark_builder_test_failure")
    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_test_failure_routes_to_doctor(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockDoctor: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
        mock_mark_failure: MagicMock,
    ) -> None:
        """Builder test failure should invoke Doctor test-fix, then re-verify tests."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
            # Doctor test-fix
            100,   # doctor phase_start
            200,   # doctor elapsed
            # Test re-verification
            200,   # test_start
            210,   # test elapsed
            # Completion validation (after Doctor fixes)
            210,   # completion_start
            220,   # completion elapsed
            # Judge
            220,   # judge phase_start
            270,   # judge elapsed
            # Rebase
            270,   # rebase phase_start
            273,   # rebase elapsed
            # Merge
            273,   # merge phase_start
            283,   # merge elapsed
            283,   # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)
        ctx.pr_number = 100
        ctx.worktree_path = Path("/fake/repo/.loom/worktrees/issue-42")

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        # Builder reports test failure
        builder_inst.run.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={"test_failure": True, "test_output_tail": "FAILED tests", "test_command": "pnpm test"},
        )
        # After Doctor fix, test verification passes
        builder_inst.run_test_verification_only.return_value = None  # None = tests pass
        # Validation and completion passes (PR created)
        builder_inst.validate_and_complete.return_value = _success_result("builder", committed=True, pr_created=True)

        # Doctor succeeds
        doctor_inst = MockDoctor.return_value
        doctor_inst.run_test_fix.return_value = _success_result("doctor")

        # Judge approves
        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        # Merge
        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.SUCCESS

        # Doctor test-fix was invoked
        doctor_inst.run_test_fix.assert_called_once()
        # Test re-verification was run
        builder_inst.run_test_verification_only.assert_called_once()
        # Should NOT mark as test failure since Doctor fixed it
        mock_mark_failure.assert_not_called()

    @patch("loom_tools.shepherd.cli._mark_builder_test_failure")
    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_doctor_preexisting_skips_to_success(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockDoctor: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
        mock_mark_failure: MagicMock,
    ) -> None:
        """When Doctor signals pre-existing failures (SKIPPED), builder continues."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
            # Doctor test-fix (pre-existing)
            100,   # doctor phase_start
            110,   # doctor elapsed
            # Completion validation (pre-existing failures)
            110,   # completion_start
            160,   # completion elapsed
            # Judge
            160,   # judge phase_start
            210,   # judge elapsed
            # Merge
            210,   # rebase phase_start
            210,   # rebase elapsed (0s, skipped)
            210,   # merge phase_start
            220,   # merge elapsed
            220,   # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)
        ctx.pr_number = 100

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        builder_inst.run.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={"test_failure": True},
        )
        # Validation and completion passes (PR created)
        builder_inst.validate_and_complete.return_value = _success_result("builder", committed=True, pr_created=True)

        # Doctor says failures are pre-existing
        doctor_inst = MockDoctor.return_value
        doctor_inst.run_test_fix.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED,
            message="doctor determined test failures are pre-existing",
            phase_name="doctor",
            data={"preexisting": True},
        )

        # Judge approves
        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        # Merge
        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.SUCCESS

        # Should NOT mark as failure
        mock_mark_failure.assert_not_called()
        # Doctor was called
        doctor_inst.run_test_fix.assert_called_once()

    @patch("loom_tools.shepherd.cli._mark_builder_test_failure")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_doctor_exhaustion_marks_test_failure(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockDoctor: MagicMock,
        mock_mark_failure: MagicMock,
    ) -> None:
        """When Doctor retries are exhausted, should mark builder test failure."""
        # test_fix_max_retries defaults to 2
        time_values = [
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
        ]
        # Two Doctor attempts, each followed by test re-verification
        for i in range(2):
            time_values.extend([
                100 + i * 100,   # doctor phase_start
                150 + i * 100,   # doctor elapsed
                150 + i * 100,   # test_start
                160 + i * 100,   # test elapsed
            ])

        mock_time.time = MagicMock(side_effect=time_values)

        ctx = _make_ctx(start_from=Phase.BUILDER)
        ctx.pr_number = 100

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        builder_inst.run.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={"test_failure": True},
        )
        # Tests keep failing after each Doctor attempt
        builder_inst.run_test_verification_only.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={"test_failure": True},
        )

        # Doctor succeeds each time but tests still fail
        doctor_inst = MockDoctor.return_value
        doctor_inst.run_test_fix.return_value = _success_result("doctor")

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.PR_TESTS_FAILED

        # Doctor was called max times (2)
        assert doctor_inst.run_test_fix.call_count == 2
        # Mark as test failure after exhaustion
        mock_mark_failure.assert_called_once()

    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_non_test_failure_bypasses_doctor(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockDoctor: MagicMock,
    ) -> None:
        """Non-test builder failures should NOT route to Doctor."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        # Regular failure (not test_failure)
        builder_inst.run.return_value = _failed_result("builder", "validation failed")

        result = orchestrate(ctx)
        assert result == 1

        # Doctor should NOT have been called
        doctor_inst = MockDoctor.return_value
        doctor_inst.run_test_fix.assert_not_called()

    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_doctor_shutdown_propagates(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockDoctor: MagicMock,
    ) -> None:
        """Shutdown during Doctor test-fix should propagate gracefully."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
            100,   # doctor phase_start
            110,   # doctor elapsed
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        builder_inst.run.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={"test_failure": True},
        )

        # Doctor returns shutdown
        doctor_inst = MockDoctor.return_value
        doctor_inst.run_test_fix.return_value = PhaseResult(
            status=PhaseStatus.SHUTDOWN,
            message="shutdown signal detected",
            phase_name="doctor",
        )

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.SHUTDOWN


class TestTestTimeoutSkipsDoctor:
    """Test that test timeouts skip the Doctor loop (issue #2391)."""

    @patch("loom_tools.shepherd.cli._mark_builder_test_failure")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_timeout_skips_doctor(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockDoctor: MagicMock,
        mock_mark_failure: MagicMock,
    ) -> None:
        """Test timeout returns PR_TESTS_FAILED without invoking Doctor."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            305,   # builder elapsed (timeout)
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)
        ctx.worktree_path = Path("/fake/repo/.loom/worktrees/issue-42")

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        # Builder reports test timeout (not just failure)
        builder_inst.run.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification timed out after 300s (pytest)",
            phase_name="builder",
            data={"test_failure": True, "test_timeout": True},
        )

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.PR_TESTS_FAILED

        # Doctor should NOT have been called
        doctor_inst = MockDoctor.return_value
        doctor_inst.run_test_fix.assert_not_called()
        # Should be marked as test failure
        mock_mark_failure.assert_called_once()
        # Should report test_timeout status
        ctx.report_milestone.assert_any_call(
            "phase_completed",
            phase="builder",
            duration_seconds=300,
            status="test_timeout",
        )


class TestDoctorRegressionGuard:
    """Test that the doctor test-fix loop aborts when doctor makes things worse."""

    @patch("loom_tools.shepherd.cli._mark_builder_test_failure")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_doctor_regression_aborts_loop(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockDoctor: MagicMock,
        mock_mark_failure: MagicMock,
    ) -> None:
        """When doctor increases error count, loop should abort immediately."""
        time_values = [
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
            # Doctor attempt 1
            100,   # doctor phase_start
            150,   # doctor elapsed
            # Test re-verification
            150,   # test_start
            160,   # test elapsed
        ]
        mock_time.time = MagicMock(side_effect=time_values)

        ctx = _make_ctx(start_from=Phase.BUILDER)
        ctx.pr_number = 100

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        # Builder reports 1 new error
        builder_inst.run.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={"test_failure": True, "new_error_count": 1},
        )
        # After Doctor, tests have 5 new errors (regression)
        builder_inst.run_test_verification_only.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={"test_failure": True, "new_error_count": 5},
        )

        # Doctor succeeds (but makes things worse)
        doctor_inst = MockDoctor.return_value
        doctor_inst.run_test_fix.return_value = _success_result("doctor")

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.PR_TESTS_FAILED

        # Doctor was called only once (aborted after regression detected)
        assert doctor_inst.run_test_fix.call_count == 1
        # Marked as test failure
        mock_mark_failure.assert_called_once()

    @patch("loom_tools.shepherd.cli._mark_builder_test_failure")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_doctor_same_error_count_continues(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockDoctor: MagicMock,
        mock_mark_failure: MagicMock,
    ) -> None:
        """When doctor keeps same error count, loop should continue retrying."""
        # test_fix_max_retries defaults to 2
        time_values = [
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
        ]
        # Two Doctor attempts, each followed by test re-verification
        for i in range(2):
            time_values.extend([
                100 + i * 100,   # doctor phase_start
                150 + i * 100,   # doctor elapsed
                150 + i * 100,   # test_start
                160 + i * 100,   # test elapsed
            ])

        mock_time.time = MagicMock(side_effect=time_values)

        ctx = _make_ctx(start_from=Phase.BUILDER)
        ctx.pr_number = 100

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        # Same error count throughout (1 new error)
        builder_inst.run.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={"test_failure": True, "new_error_count": 1},
        )
        builder_inst.run_test_verification_only.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={"test_failure": True, "new_error_count": 1},
        )

        # Doctor succeeds each time but tests still fail with same count
        doctor_inst = MockDoctor.return_value
        doctor_inst.run_test_fix.return_value = _success_result("doctor")

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.PR_TESTS_FAILED

        # Doctor was called max times (2) — not short-circuited
        assert doctor_inst.run_test_fix.call_count == 2

    @patch("loom_tools.shepherd.cli._mark_builder_test_failure")
    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_doctor_reduces_errors_continues(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockDoctor: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
        mock_mark_failure: MagicMock,
    ) -> None:
        """When doctor reduces error count, loop should continue (and succeed if tests pass)."""
        time_values = [
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
            # Doctor attempt 1
            100,   # doctor phase_start
            150,   # doctor elapsed
            # Test re-verification (passes)
            150,   # test_start
            160,   # test elapsed
            # Completion validation
            160,   # completion_start
            170,   # completion elapsed
            # Judge
            170,   # judge phase_start
            220,   # judge elapsed
            # Rebase
            220,   # rebase phase_start
            220,   # rebase elapsed (0s, skipped)
            # Merge
            220,   # merge phase_start
            230,   # merge elapsed
            230,   # duration calc
        ]
        mock_time.time = MagicMock(side_effect=time_values)

        ctx = _make_ctx(start_from=Phase.BUILDER)
        ctx.pr_number = 100

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        # Builder reports 3 new errors
        builder_inst.run.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={"test_failure": True, "new_error_count": 3},
        )
        # After Doctor, tests pass
        builder_inst.run_test_verification_only.return_value = None
        builder_inst.validate_and_complete.return_value = _success_result(
            "builder", committed=True, pr_created=True
        )

        # Doctor succeeds
        doctor_inst = MockDoctor.return_value
        doctor_inst.run_test_fix.return_value = _success_result("doctor")

        # Judge approves
        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        # Merge
        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.SUCCESS

        # Doctor was called once, tests passed
        assert doctor_inst.run_test_fix.call_count == 1
        mock_mark_failure.assert_not_called()


class TestPostDoctorPush:
    """Test that doctor fixes are pushed to remote before test re-verification (#2342)."""

    @patch("loom_tools.shepherd.cli._mark_builder_test_failure")
    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_push_called_after_doctor_success(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockDoctor: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
        mock_mark_failure: MagicMock,
    ) -> None:
        """push_branch should be called after Doctor successfully applies fixes."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
            100,   # doctor phase_start
            200,   # doctor elapsed
            200,   # test_start
            210,   # test elapsed
            210,   # completion_start
            220,   # completion elapsed
            220,   # judge phase_start
            270,   # judge elapsed
            270,   # rebase phase_start
            270,   # rebase elapsed (0s, skipped)
            270,   # merge phase_start
            280,   # merge elapsed
            280,   # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)
        ctx.pr_number = 100
        ctx.worktree_path = Path("/fake/repo/.loom/worktrees/issue-42")

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        builder_inst.run.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={"test_failure": True, "test_output_tail": "FAILED", "test_command": "pnpm test"},
        )
        builder_inst.run_test_verification_only.return_value = None  # tests pass
        builder_inst.validate_and_complete.return_value = _success_result(
            "builder", committed=True, pr_created=True
        )
        builder_inst.push_branch.return_value = True

        doctor_inst = MockDoctor.return_value
        doctor_inst.run_test_fix.return_value = _success_result("doctor")

        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.SUCCESS

        # push_branch was called after Doctor succeeded
        builder_inst.push_branch.assert_called_once_with(ctx)

    @patch("loom_tools.shepherd.cli._mark_builder_test_failure")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_push_not_called_after_doctor_failure(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockDoctor: MagicMock,
        mock_mark_failure: MagicMock,
    ) -> None:
        """push_branch should NOT be called when Doctor fails."""
        time_values = [
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
        ]
        # Two Doctor attempts (max retries=2), each fails then test re-verify
        for i in range(2):
            time_values.extend([
                100 + i * 100,   # doctor phase_start
                150 + i * 100,   # doctor elapsed
                150 + i * 100,   # test_start
                160 + i * 100,   # test elapsed
            ])

        mock_time.time = MagicMock(side_effect=time_values)

        ctx = _make_ctx(start_from=Phase.BUILDER)
        ctx.pr_number = 100

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        builder_inst.run.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={"test_failure": True},
        )
        builder_inst.run_test_verification_only.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={"test_failure": True},
        )

        # Doctor fails each time
        doctor_inst = MockDoctor.return_value
        doctor_inst.run_test_fix.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="could not fix tests",
            phase_name="doctor",
        )

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.PR_TESTS_FAILED

        # push_branch should NOT have been called since Doctor failed
        builder_inst.push_branch.assert_not_called()

    @patch("loom_tools.shepherd.cli._mark_builder_test_failure")
    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_push_failure_is_nonfatal(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockDoctor: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
        mock_mark_failure: MagicMock,
    ) -> None:
        """Push failure after Doctor success should log warning but not block the loop."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
            100,   # doctor phase_start
            200,   # doctor elapsed
            200,   # test_start
            210,   # test elapsed
            210,   # completion_start
            220,   # completion elapsed
            220,   # judge phase_start
            270,   # judge elapsed
            270,   # rebase phase_start
            270,   # rebase elapsed (0s, skipped)
            270,   # merge phase_start
            280,   # merge elapsed
            280,   # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)
        ctx.pr_number = 100
        ctx.worktree_path = Path("/fake/repo/.loom/worktrees/issue-42")

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (True, "skipped via --from")
        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        builder_inst.run.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={"test_failure": True, "test_output_tail": "FAILED", "test_command": "pnpm test"},
        )
        builder_inst.run_test_verification_only.return_value = None  # tests pass
        builder_inst.validate_and_complete.return_value = _success_result(
            "builder", committed=True, pr_created=True
        )
        # Push fails
        builder_inst.push_branch.return_value = False

        doctor_inst = MockDoctor.return_value
        doctor_inst.run_test_fix.return_value = _success_result("doctor")

        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        # Should still succeed despite push failure
        assert result == ShepherdExitCode.SUCCESS

        # push_branch was called but returned False
        builder_inst.push_branch.assert_called_once_with(ctx)
        # Workflow continued successfully
        mock_mark_failure.assert_not_called()


class TestCleanupPrLabelsOnFailure:
    """Test _cleanup_pr_labels_on_failure helper."""

    def _make_cleanup_ctx(
        self,
        pr_number: int | None = None,
        pr_labels: set[str] | None = None,
    ) -> MagicMock:
        """Create a mock ShepherdContext for PR cleanup tests."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.pr_number = pr_number
        ctx.repo_root = Path("/fake/repo")
        ctx.label_cache = MagicMock()
        if pr_labels is not None:
            ctx.label_cache.get_pr_labels.return_value = pr_labels
        return ctx

    @patch("loom_tools.shepherd.cli.transition_labels")
    @patch("loom_tools.shepherd.cli.get_pr_for_issue", return_value=None)
    def test_no_cleanup_when_no_pr(
        self, mock_get_pr: MagicMock, mock_transition: MagicMock
    ) -> None:
        """Should do nothing when no PR exists for the issue."""
        ctx = self._make_cleanup_ctx(pr_number=None)
        _cleanup_pr_labels_on_failure(ctx)
        mock_transition.assert_not_called()

    @patch("loom_tools.shepherd.cli.transition_labels")
    def test_removes_workflow_labels_from_pr(
        self, mock_transition: MagicMock
    ) -> None:
        """Should remove stale workflow labels but preserve review-requested
        when judge produced no outcome."""
        ctx = self._make_cleanup_ctx(
            pr_number=100,
            pr_labels={
                "loom:review-requested",
                "loom:treating",
                "loom:merge-conflict",
            },
        )
        _cleanup_pr_labels_on_failure(ctx)

        # loom:review-requested preserved (no judge outcome), only loom:treating removed
        mock_transition.assert_called_once_with(
            "pr",
            100,
            remove=["loom:treating"],
            repo_root=Path("/fake/repo"),
        )

    @patch("loom_tools.shepherd.cli.transition_labels")
    def test_removes_all_workflow_labels(
        self, mock_transition: MagicMock
    ) -> None:
        """Should remove all four workflow labels when all are present."""
        ctx = self._make_cleanup_ctx(
            pr_number=100,
            pr_labels={
                "loom:review-requested",
                "loom:changes-requested",
                "loom:treating",
                "loom:reviewing",
            },
        )
        _cleanup_pr_labels_on_failure(ctx)

        mock_transition.assert_called_once_with(
            "pr",
            100,
            remove=[
                "loom:changes-requested",
                "loom:review-requested",
                "loom:reviewing",
                "loom:treating",
            ],
            repo_root=Path("/fake/repo"),
        )

    @patch("loom_tools.shepherd.cli.transition_labels")
    def test_keeps_factual_labels(
        self, mock_transition: MagicMock
    ) -> None:
        """Should keep factual status labels like loom:merge-conflict."""
        ctx = self._make_cleanup_ctx(
            pr_number=100,
            pr_labels={"loom:merge-conflict", "loom:ci-failure", "loom:pr"},
        )
        _cleanup_pr_labels_on_failure(ctx)

        # No workflow labels to remove
        mock_transition.assert_not_called()

    @patch("loom_tools.shepherd.cli.transition_labels")
    @patch("loom_tools.shepherd.cli.get_pr_for_issue", return_value=200)
    def test_looks_up_pr_when_not_in_context(
        self, mock_get_pr: MagicMock, mock_transition: MagicMock
    ) -> None:
        """Should look up PR from issue when ctx.pr_number is None."""
        ctx = self._make_cleanup_ctx(
            pr_number=None,
            pr_labels={"loom:reviewing"},
        )
        # get_pr_for_issue returns 200, so we need to set up labels for PR 200
        ctx.label_cache.get_pr_labels.return_value = {"loom:reviewing"}

        _cleanup_pr_labels_on_failure(ctx)

        mock_get_pr.assert_called_once_with(42, repo_root=Path("/fake/repo"))
        mock_transition.assert_called_once_with(
            "pr",
            200,
            remove=["loom:reviewing"],
            repo_root=Path("/fake/repo"),
        )

    @patch("loom_tools.shepherd.cli.transition_labels")
    def test_survives_api_failure(
        self, mock_transition: MagicMock
    ) -> None:
        """Should not raise when GitHub API fails."""
        ctx = self._make_cleanup_ctx(pr_number=100)
        ctx.label_cache.get_pr_labels.side_effect = RuntimeError("API down")

        # Should not raise
        _cleanup_pr_labels_on_failure(ctx)
        mock_transition.assert_not_called()

    @patch("loom_tools.shepherd.cli.transition_labels")
    def test_preserves_review_requested_when_no_judge_outcome(
        self, mock_transition: MagicMock
    ) -> None:
        """Should preserve loom:review-requested when judge produced no outcome."""
        ctx = self._make_cleanup_ctx(
            pr_number=100,
            pr_labels={"loom:review-requested"},
        )
        _cleanup_pr_labels_on_failure(ctx)

        # No judge outcome (no loom:pr or loom:changes-requested),
        # so loom:review-requested is preserved — nothing to remove
        mock_transition.assert_not_called()

    @patch("loom_tools.shepherd.cli.transition_labels")
    def test_removes_review_requested_when_judge_approved(
        self, mock_transition: MagicMock
    ) -> None:
        """Should remove loom:review-requested when judge approved (loom:pr present)."""
        ctx = self._make_cleanup_ctx(
            pr_number=100,
            pr_labels={"loom:review-requested", "loom:pr"},
        )
        _cleanup_pr_labels_on_failure(ctx)

        mock_transition.assert_called_once_with(
            "pr",
            100,
            remove=["loom:review-requested"],
            repo_root=Path("/fake/repo"),
        )

    @patch("loom_tools.shepherd.cli.transition_labels")
    def test_removes_review_requested_when_judge_requested_changes(
        self, mock_transition: MagicMock
    ) -> None:
        """Should remove loom:review-requested when judge requested changes."""
        ctx = self._make_cleanup_ctx(
            pr_number=100,
            pr_labels={
                "loom:review-requested",
                "loom:changes-requested",
                "loom:treating",
            },
        )
        _cleanup_pr_labels_on_failure(ctx)

        mock_transition.assert_called_once_with(
            "pr",
            100,
            remove=[
                "loom:changes-requested",
                "loom:review-requested",
                "loom:treating",
            ],
            repo_root=Path("/fake/repo"),
        )


class TestCleanupLabelsOnFailure:
    """Test _cleanup_labels_on_failure defense-in-depth handler."""

    def _make_cleanup_ctx(
        self,
        issue_labels: set[str] | None = None,
    ) -> MagicMock:
        """Create a mock ShepherdContext for cleanup tests."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.pr_number = None
        ctx.repo_root = Path("/fake/repo")
        ctx.label_cache = MagicMock()
        if issue_labels is not None:
            ctx.label_cache.get_issue_labels.return_value = issue_labels
        return ctx

    def test_no_cleanup_on_success(self) -> None:
        """Should not attempt cleanup when exit code is SUCCESS."""
        ctx = self._make_cleanup_ctx()
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.SUCCESS)
        ctx.label_cache.get_issue_labels.assert_not_called()

    def test_no_cleanup_on_skipped(self) -> None:
        """Should not attempt cleanup when exit code is SKIPPED."""
        ctx = self._make_cleanup_ctx()
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.SKIPPED)
        ctx.label_cache.get_issue_labels.assert_not_called()

    @patch("loom_tools.shepherd.cli._cleanup_pr_labels_on_failure")
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    def test_reverts_building_to_issue_on_failure(
        self, mock_transition: MagicMock, mock_pr_cleanup: MagicMock
    ) -> None:
        """Should revert loom:building → loom:issue when no failure label exists."""
        ctx = self._make_cleanup_ctx(
            issue_labels={"loom:building", "loom:curated"}
        )
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.BUILDER_FAILED)

        mock_transition.assert_called_once_with(
            42,
            add=["loom:issue"],
            remove=["loom:building"],
            repo_root=Path("/fake/repo"),
        )
        mock_pr_cleanup.assert_called_once_with(ctx)

    @patch("loom_tools.shepherd.cli._cleanup_pr_labels_on_failure")
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    def test_reverts_building_on_shutdown(
        self, mock_transition: MagicMock, mock_pr_cleanup: MagicMock
    ) -> None:
        """Should revert loom:building → loom:issue on shutdown signal."""
        ctx = self._make_cleanup_ctx(
            issue_labels={"loom:building", "loom:curated"}
        )
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.SHUTDOWN)

        mock_transition.assert_called_once_with(
            42,
            add=["loom:issue"],
            remove=["loom:building"],
            repo_root=Path("/fake/repo"),
        )

    @patch("loom_tools.shepherd.cli._cleanup_pr_labels_on_failure")
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    def test_no_revert_when_blocked_label_present(
        self, mock_transition: MagicMock, mock_pr_cleanup: MagicMock
    ) -> None:
        """Should not revert to loom:issue when loom:blocked label exists."""
        ctx = self._make_cleanup_ctx(
            issue_labels={"loom:blocked", "loom:curated"}
        )
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.PR_TESTS_FAILED)

        # No transition needed - _mark_* already handled it
        mock_transition.assert_not_called()

    @patch("loom_tools.shepherd.cli._cleanup_pr_labels_on_failure")
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    def test_removes_contradictory_building_label_alongside_blocked(
        self, mock_transition: MagicMock, mock_pr_cleanup: MagicMock
    ) -> None:
        """Should remove loom:building when it coexists with loom:blocked."""
        ctx = self._make_cleanup_ctx(
            issue_labels={"loom:blocked", "loom:building", "loom:curated"}
        )
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.BUILDER_FAILED)

        mock_transition.assert_called_once_with(
            42,
            remove=["loom:building"],
            repo_root=Path("/fake/repo"),
        )

    @patch("loom_tools.shepherd.cli._cleanup_pr_labels_on_failure")
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    def test_no_action_when_no_building_label(
        self, mock_transition: MagicMock, mock_pr_cleanup: MagicMock
    ) -> None:
        """Should do nothing when issue doesn't have loom:building."""
        ctx = self._make_cleanup_ctx(
            issue_labels={"loom:curated", "loom:triage"}
        )
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.BUILDER_FAILED)

        mock_transition.assert_not_called()

    @patch("loom_tools.shepherd.cli._cleanup_pr_labels_on_failure")
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    def test_survives_api_failure(
        self, mock_transition: MagicMock, mock_pr_cleanup: MagicMock
    ) -> None:
        """Should not raise when GitHub API is unreachable."""
        ctx = self._make_cleanup_ctx()
        ctx.label_cache.get_issue_labels.side_effect = RuntimeError("API down")

        # Should not raise
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.NEEDS_INTERVENTION)

        mock_transition.assert_not_called()

    @patch("loom_tools.shepherd.cli._cleanup_pr_labels_on_failure")
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    def test_survives_transition_failure(
        self, mock_transition: MagicMock, mock_pr_cleanup: MagicMock
    ) -> None:
        """Should not raise when label transition fails."""
        ctx = self._make_cleanup_ctx(
            issue_labels={"loom:building", "loom:curated"}
        )
        mock_transition.side_effect = RuntimeError("API error")

        # Should not raise
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.BUILDER_FAILED)

    @patch("loom_tools.shepherd.cli._cleanup_pr_labels_on_failure")
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    def test_pr_cleanup_called_on_failure(
        self, mock_transition: MagicMock, mock_pr_cleanup: MagicMock
    ) -> None:
        """Should call PR label cleanup on any failure."""
        ctx = self._make_cleanup_ctx(
            issue_labels={"loom:building", "loom:curated"}
        )
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.BUILDER_FAILED)

        mock_pr_cleanup.assert_called_once_with(ctx)

    @patch("loom_tools.shepherd.cli._cleanup_pr_labels_on_failure")
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    def test_pr_cleanup_not_called_on_success(
        self, mock_transition: MagicMock, mock_pr_cleanup: MagicMock
    ) -> None:
        """Should not call PR label cleanup on success."""
        ctx = self._make_cleanup_ctx()
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.SUCCESS)

        mock_pr_cleanup.assert_not_called()

    @patch("loom_tools.shepherd.cli._cleanup_pr_labels_on_failure")
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    def test_survives_pr_cleanup_failure(
        self, mock_transition: MagicMock, mock_pr_cleanup: MagicMock
    ) -> None:
        """Should continue with issue cleanup even if PR cleanup raises."""
        ctx = self._make_cleanup_ctx(
            issue_labels={"loom:building", "loom:curated"}
        )
        mock_pr_cleanup.side_effect = RuntimeError("PR cleanup failed")

        # Should not raise, and should still clean up issue labels
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.BUILDER_FAILED)

        mock_transition.assert_called_once_with(
            42,
            add=["loom:issue"],
            remove=["loom:building"],
            repo_root=Path("/fake/repo"),
        )

    @patch("loom_tools.shepherd.cli._cleanup_pr_labels_on_failure")
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    @patch("loom_tools.shepherd.phases.rebase._is_pr_merged", return_value=True)
    def test_no_revert_when_pr_already_merged(
        self,
        mock_merged: MagicMock,
        mock_transition: MagicMock,
        mock_pr_cleanup: MagicMock,
    ) -> None:
        """Should NOT revert loom:building when the PR is already merged (#2515)."""
        ctx = self._make_cleanup_ctx(
            issue_labels={"loom:building", "loom:curated"}
        )
        ctx.pr_number = 200
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.NEEDS_INTERVENTION)

        mock_merged.assert_called_once_with(200, Path("/fake/repo"))
        mock_transition.assert_not_called()

    @patch("loom_tools.shepherd.cli._cleanup_pr_labels_on_failure")
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    @patch("loom_tools.shepherd.cli.get_pr_for_issue", return_value=300)
    @patch("loom_tools.shepherd.phases.rebase._is_pr_merged", return_value=True)
    def test_no_revert_when_merged_pr_found_by_issue(
        self,
        mock_merged: MagicMock,
        mock_get_pr: MagicMock,
        mock_transition: MagicMock,
        mock_pr_cleanup: MagicMock,
    ) -> None:
        """Should find merged PR by issue number when ctx.pr_number is None (#2515)."""
        ctx = self._make_cleanup_ctx(
            issue_labels={"loom:building", "loom:curated"}
        )
        ctx.pr_number = None
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.NEEDS_INTERVENTION)

        mock_get_pr.assert_called_once_with(42, state="merged", repo_root=Path("/fake/repo"))
        mock_merged.assert_called_once_with(300, Path("/fake/repo"))
        mock_transition.assert_not_called()


class TestMainCleanupIntegration:
    """Test that main() calls _cleanup_labels_on_failure on failure."""

    def test_cleanup_called_on_orchestrate_failure(self) -> None:
        """main should call cleanup when orchestrate returns a failure exit code."""
        with patch("loom_tools.shepherd.cli.orchestrate", return_value=ShepherdExitCode.BUILDER_FAILED), \
             patch("loom_tools.shepherd.cli.ShepherdContext") as MockCtx, \
             patch("loom_tools.shepherd.cli.find_repo_root", return_value=Path("/fake/repo")), \
             patch("loom_tools.shepherd.cli._auto_navigate_out_of_worktree"), \
             patch("loom_tools.shepherd.cli._cleanup_labels_on_failure") as mock_cleanup, \
             patch("loom_tools.common.github.gh_issue_view", return_value={"state": "OPEN"}), \
             patch("loom_tools.claim.claim_issue", return_value=0), \
             patch("loom_tools.claim.release_claim"):
            ctx = MockCtx.return_value
            result = main(["42"])
            assert result == ShepherdExitCode.BUILDER_FAILED
            mock_cleanup.assert_called_once_with(ctx, ShepherdExitCode.BUILDER_FAILED)

    def test_cleanup_not_called_on_success(self) -> None:
        """main should not call cleanup when orchestrate succeeds."""
        with patch("loom_tools.shepherd.cli.orchestrate", return_value=ShepherdExitCode.SUCCESS), \
             patch("loom_tools.shepherd.cli.ShepherdContext"), \
             patch("loom_tools.shepherd.cli.find_repo_root", return_value=Path("/fake/repo")), \
             patch("loom_tools.shepherd.cli._auto_navigate_out_of_worktree"), \
             patch("loom_tools.shepherd.cli._cleanup_labels_on_failure") as mock_cleanup, \
             patch("loom_tools.common.github.gh_issue_view", return_value={"state": "OPEN"}), \
             patch("loom_tools.claim.claim_issue", return_value=0), \
             patch("loom_tools.claim.release_claim"):
            result = main(["42"])
            assert result == ShepherdExitCode.SUCCESS
            mock_cleanup.assert_not_called()

    def test_cleanup_called_on_unhandled_exception(self) -> None:
        """main should call cleanup when orchestrate raises an unhandled exception."""
        with patch("loom_tools.shepherd.cli.orchestrate", side_effect=RuntimeError("MCP crash")), \
             patch("loom_tools.shepherd.cli.ShepherdContext") as MockCtx, \
             patch("loom_tools.shepherd.cli.find_repo_root", return_value=Path("/fake/repo")), \
             patch("loom_tools.shepherd.cli._auto_navigate_out_of_worktree"), \
             patch("loom_tools.shepherd.cli._cleanup_labels_on_failure") as mock_cleanup, \
             patch("loom_tools.common.github.gh_issue_view", return_value={"state": "OPEN"}), \
             patch("loom_tools.claim.claim_issue", return_value=0), \
             patch("loom_tools.claim.release_claim"):
            ctx = MockCtx.return_value
            with pytest.raises(RuntimeError, match="MCP crash"):
                main(["42"])
            mock_cleanup.assert_called_once_with(ctx, ShepherdExitCode.NEEDS_INTERVENTION)


class TestRebasePhaseIntegration:
    """Test that the rebase phase is called at the right position in orchestration."""

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    def test_rebase_runs_between_judge_and_merge(
        self,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
    ) -> None:
        """Rebase phase should run after Judge approval and before Merge."""
        call_order: list[str] = []

        ctx = _make_ctx()

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (False, "")
        curator_inst.run.side_effect = lambda c: (call_order.append("curator") or _success_result("curator"))

        MockApproval.return_value.run.side_effect = lambda c: (call_order.append("approval") or _success_result("approval"))

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        builder_inst.run.side_effect = lambda c: (call_order.append("builder") or _success_result("builder"))

        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.side_effect = lambda c: (call_order.append("judge") or _success_result("judge", approved=True))

        MockRebase.return_value.run.side_effect = lambda c: (
            call_order.append("rebase")
            or PhaseResult(status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase")
        )

        MockMerge.return_value.run.side_effect = lambda c: (call_order.append("merge") or _success_result("merge", merged=True))

        result = orchestrate(ctx)
        assert result == 0

        # Verify rebase runs after judge and before merge
        assert "rebase" in call_order
        rebase_idx = call_order.index("rebase")
        judge_idx = call_order.index("judge")
        merge_idx = call_order.index("merge")
        assert judge_idx < rebase_idx < merge_idx

    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    def test_rebase_failure_returns_needs_intervention(
        self,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
    ) -> None:
        """When rebase fails, orchestration should return NEEDS_INTERVENTION."""
        ctx = _make_ctx()

        curator_inst = MockCurator.return_value
        curator_inst.should_skip.return_value = (False, "")
        curator_inst.run.return_value = _success_result("curator")

        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        builder_inst.run.return_value = _success_result("builder")

        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="rebase onto origin/main failed: conflicts",
            phase_name="rebase",
            data={"reason": "merge_conflict"},
        )

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.NEEDS_INTERVENTION
        # Merge should NOT be called
        MockMerge.return_value.run.assert_not_called()


class TestPostFallbackFailureComment:
    """Test _post_fallback_failure_comment helper (issue #2525, #2839)."""

    @patch("subprocess.run")
    def test_posts_comment_with_exit_code(self, mock_run: MagicMock) -> None:
        """Should post a comment including exit code and description."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = Path("/fake/repo")
        ctx.abandonment_info = None  # Exercise generic (exit-code-based) path

        _post_fallback_failure_comment(ctx, ShepherdExitCode.BUILDER_FAILED)

        mock_run.assert_called_once()
        cmd = mock_run.call_args[0][0]
        assert cmd[:3] == ["gh", "issue", "comment"]
        body_idx = cmd.index("--body") + 1
        body = cmd[body_idx]
        assert "Shepherd abandoned issue" in body
        assert "1" in body  # exit code
        assert "Builder failed" in body

    @patch("subprocess.run")
    def test_systemic_failure_identified_as_infrastructure(
        self, mock_run: MagicMock
    ) -> None:
        """Should identify auth/API failures as infrastructure failures."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = Path("/fake/repo")
        ctx.abandonment_info = None  # Exercise generic (exit-code-based) path

        _post_fallback_failure_comment(ctx, ShepherdExitCode.SYSTEMIC_FAILURE)

        mock_run.assert_called_once()
        cmd = mock_run.call_args[0][0]
        body_idx = cmd.index("--body") + 1
        body = cmd[body_idx]
        assert "infrastructure failure" in body
        assert "authentication tokens" in body

    @patch("subprocess.run")
    def test_non_auth_failure_gives_generic_advice(
        self, mock_run: MagicMock
    ) -> None:
        """Non-systemic failures should give generic investigation advice."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = Path("/fake/repo")
        ctx.abandonment_info = None  # Exercise generic (exit-code-based) path

        _post_fallback_failure_comment(ctx, ShepherdExitCode.NEEDS_INTERVENTION)

        mock_run.assert_called_once()
        cmd = mock_run.call_args[0][0]
        body_idx = cmd.index("--body") + 1
        body = cmd[body_idx]
        assert "manual investigation" in body
        assert "infrastructure failure" not in body

    @patch("subprocess.run")
    def test_survives_subprocess_failure(self, mock_run: MagicMock) -> None:
        """Should not raise when gh command fails."""
        mock_run.side_effect = OSError("command not found")

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = Path("/fake/repo")
        ctx.abandonment_info = None

        # Should not raise
        _post_fallback_failure_comment(ctx, ShepherdExitCode.BUILDER_FAILED)

    @patch("subprocess.run")
    def test_abandonment_info_thinking_stall(self, mock_run: MagicMock) -> None:
        """When abandonment_info has thinking_stall, posts detailed comment."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.config.task_id = "abc1234"
        ctx.repo_root = Path("/fake/repo")
        ctx.abandonment_info = {
            "phase": "builder",
            "exit_code": 14,
            "failure_data": {
                "thinking_stall": True,
                "log_file": "/fake/.loom/logs/loom-builder-issue-42.log",
            },
            "message": "builder thinking stall: extended thinking output with zero tool calls",
            "task_id": "abc1234",
        }

        _post_fallback_failure_comment(ctx, ShepherdExitCode.BUILDER_FAILED)

        mock_run.assert_called_once()
        cmd = mock_run.call_args[0][0]
        body_idx = cmd.index("--body") + 1
        body = cmd[body_idx]
        assert "Shepherd abandoned issue" in body
        assert "abc1234" in body
        assert "builder" in body
        assert "thinking stall" in body
        assert "retry budget exhausted" in body
        assert "loom-builder-issue-42.log" in body
        assert "safe to retry" in body

    @patch("subprocess.run")
    def test_abandonment_info_planning_stall(self, mock_run: MagicMock) -> None:
        """When abandonment_info has planning_stall, posts detailed comment."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.config.task_id = "def5678"
        ctx.repo_root = Path("/fake/repo")
        ctx.abandonment_info = {
            "phase": "builder",
            "exit_code": 8,
            "failure_data": {
                "planning_stall": True,
                "planning_timeout": 300,
                "log_file": "/fake/.loom/logs/loom-builder-issue-42.log",
            },
            "message": "builder stalled in planning checkpoint",
            "task_id": "def5678",
        }

        _post_fallback_failure_comment(ctx, ShepherdExitCode.BUILDER_FAILED)

        mock_run.assert_called_once()
        cmd = mock_run.call_args[0][0]
        body_idx = cmd.index("--body") + 1
        body = cmd[body_idx]
        assert "Shepherd abandoned issue" in body
        assert "planning stall" in body
        assert "300" in body
        assert "safe to retry" in body

    @patch("subprocess.run")
    def test_abandonment_info_auth_failure(self, mock_run: MagicMock) -> None:
        """When abandonment_info has auth_failure, posts non-retryable comment."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.config.task_id = "ghi9012"
        ctx.repo_root = Path("/fake/repo")
        ctx.abandonment_info = {
            "phase": "builder",
            "exit_code": 9,
            "failure_data": {
                "auth_failure": True,
                "log_file": "/fake/.loom/logs/loom-builder-issue-42.log",
            },
            "message": "builder auth pre-flight failed",
            "task_id": "ghi9012",
        }

        _post_fallback_failure_comment(ctx, ShepherdExitCode.SYSTEMIC_FAILURE)

        mock_run.assert_called_once()
        cmd = mock_run.call_args[0][0]
        body_idx = cmd.index("--body") + 1
        body = cmd[body_idx]
        assert "Shepherd abandoned issue" in body
        assert "auth pre-flight failure" in body
        assert "infrastructure failure" in body

    @patch("subprocess.run")
    def test_abandonment_info_rate_limit_abort(self, mock_run: MagicMock) -> None:
        """When abandonment_info has rate_limit_abort, posts rate-limit comment."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.config.task_id = "jkl3456"
        ctx.repo_root = Path("/fake/repo")
        ctx.abandonment_info = {
            "phase": "builder",
            "exit_code": 13,
            "failure_data": {
                "rate_limit_abort": True,
                "log_file": "/fake/.loom/logs/loom-builder-issue-42.log",
            },
            "message": "CLI hit usage/plan limit",
            "task_id": "jkl3456",
        }

        _post_fallback_failure_comment(ctx, ShepherdExitCode.RATE_LIMIT_ABORT)

        mock_run.assert_called_once()
        cmd = mock_run.call_args[0][0]
        body_idx = cmd.index("--body") + 1
        body = cmd[body_idx]
        assert "Shepherd abandoned issue" in body
        assert "rate limit abort" in body
        assert "usage/plan limit" in body

    @patch("subprocess.run")
    def test_abandonment_info_generic_failure(self, mock_run: MagicMock) -> None:
        """When abandonment_info has no specific flag, uses message as failure mode."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.config.task_id = "mno7890"
        ctx.repo_root = Path("/fake/repo")
        ctx.abandonment_info = {
            "phase": "builder",
            "exit_code": 1,
            "failure_data": {},
            "message": "unexpected builder exit",
            "task_id": "mno7890",
        }

        _post_fallback_failure_comment(ctx, ShepherdExitCode.BUILDER_FAILED)

        mock_run.assert_called_once()
        cmd = mock_run.call_args[0][0]
        body_idx = cmd.index("--body") + 1
        body = cmd[body_idx]
        assert "Shepherd abandoned issue" in body
        assert "unexpected builder exit" in body
        assert "safe to retry" in body


class TestRecordFallbackFailure:
    """Test _record_fallback_failure helper (issue #2525)."""

    @patch("loom_tools.shepherd.cli.get_pr_for_issue", return_value=None)
    @patch("loom_tools.common.systematic_failure.detect_systematic_failure", return_value=None)
    @patch("loom_tools.common.systematic_failure.record_blocked_reason")
    def test_records_auth_failure_class(
        self,
        mock_record: MagicMock,
        mock_detect: MagicMock,
        mock_get_pr: MagicMock,
    ) -> None:
        """Should record auth_infrastructure_failure for systemic exit code."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = Path("/fake/repo")

        _record_fallback_failure(ctx, ShepherdExitCode.SYSTEMIC_FAILURE)

        mock_record.assert_called_once_with(
            Path("/fake/repo"),
            42,
            error_class="auth_infrastructure_failure",
            phase="builder",
            details="Builder failed due to auth/API infrastructure issue (fallback cleanup)",
        )
        mock_detect.assert_called_once_with(Path("/fake/repo"))

    @patch("loom_tools.shepherd.cli.get_pr_for_issue", return_value=None)
    @patch("loom_tools.common.systematic_failure.detect_systematic_failure", return_value=None)
    @patch("loom_tools.common.systematic_failure.record_blocked_reason")
    def test_records_unknown_failure_class(
        self,
        mock_record: MagicMock,
        mock_detect: MagicMock,
        mock_get_pr: MagicMock,
    ) -> None:
        """Should record builder_unknown_failure for non-systemic exit codes."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = Path("/fake/repo")
        ctx.last_postmortem = None

        _record_fallback_failure(ctx, ShepherdExitCode.BUILDER_FAILED)

        mock_record.assert_called_once_with(
            Path("/fake/repo"),
            42,
            error_class="builder_unknown_failure",
            phase="builder",
            details="Builder failed without specific handler (exit code 1, fallback cleanup)",
        )
        mock_detect.assert_called_once_with(Path("/fake/repo"))

    @patch("loom_tools.shepherd.cli.get_pr_for_issue", return_value=None)
    @patch("loom_tools.common.systematic_failure.detect_systematic_failure", return_value=None)
    @patch("loom_tools.common.systematic_failure.record_blocked_reason")
    def test_unknown_failure_includes_postmortem(
        self,
        mock_record: MagicMock,
        mock_detect: MagicMock,
        mock_get_pr: MagicMock,
    ) -> None:
        """Should include post-mortem summary in unknown failure details (issue #2766)."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = Path("/fake/repo")
        ctx.last_postmortem = {
            "summary": "CLI started but produced zero output; wall: 5s; exit(wait=1)",
        }

        _record_fallback_failure(ctx, ShepherdExitCode.BUILDER_FAILED)

        call_args = mock_record.call_args
        details = call_args.kwargs.get("details", call_args[1].get("details", ""))
        assert "post-mortem:" in details
        assert "CLI started but produced zero output" in details

    @patch("loom_tools.shepherd.cli.get_pr_for_issue", return_value=None)
    @patch("loom_tools.common.systematic_failure.detect_systematic_failure", return_value=None)
    @patch("loom_tools.common.systematic_failure.record_blocked_reason")
    def test_records_worktree_escape_class(
        self,
        mock_record: MagicMock,
        mock_detect: MagicMock,
        mock_get_pr: MagicMock,
    ) -> None:
        """Should record builder_worktree_escape for worktree escape exit code."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = Path("/fake/repo")

        _record_fallback_failure(ctx, ShepherdExitCode.WORKTREE_ESCAPE)

        mock_record.assert_called_once_with(
            Path("/fake/repo"),
            42,
            error_class="builder_worktree_escape",
            phase="builder",
            details="Builder escaped worktree and modified main instead (fallback cleanup)",
        )
        mock_detect.assert_called_once_with(Path("/fake/repo"))

    @patch("loom_tools.common.systematic_failure.detect_systematic_failure", return_value=None)
    @patch("loom_tools.common.systematic_failure.record_blocked_reason")
    def test_records_mcp_failure_class(
        self,
        mock_record: MagicMock,
        mock_detect: MagicMock,
        tmp_path: Path,
    ) -> None:
        """Should record mcp_infrastructure_failure when builder log has MCP markers (issue #2768)."""
        # Create a fake builder log with MCP failure markers
        logs_dir = tmp_path / ".loom" / "logs"
        logs_dir.mkdir(parents=True)
        log_file = logs_dir / "loom-builder-issue-42.log"
        log_file.write_text("Starting session...\nMCP server failed to initialize\nExiting.")

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = tmp_path

        _record_fallback_failure(ctx, ShepherdExitCode.BUILDER_FAILED)

        mock_record.assert_called_once_with(
            tmp_path,
            42,
            error_class="mcp_infrastructure_failure",
            phase="builder",
            details="Builder failed due to MCP server failure (exit code 1, fallback cleanup)",
        )
        mock_detect.assert_called_once_with(tmp_path)

    @patch("loom_tools.common.systematic_failure.detect_systematic_failure", return_value=None)
    @patch("loom_tools.common.systematic_failure.record_blocked_reason")
    def test_records_unknown_when_no_mcp_markers(
        self,
        mock_record: MagicMock,
        mock_detect: MagicMock,
        tmp_path: Path,
    ) -> None:
        """Should record builder_unknown_failure when log exists but has no MCP markers."""
        # Create a builder log without MCP markers
        logs_dir = tmp_path / ".loom" / "logs"
        logs_dir.mkdir(parents=True)
        log_file = logs_dir / "loom-builder-issue-42.log"
        log_file.write_text("Starting session...\nSome random error\nExiting.")

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = tmp_path
        ctx.last_postmortem = None

        _record_fallback_failure(ctx, ShepherdExitCode.BUILDER_FAILED)

        mock_record.assert_called_once_with(
            tmp_path,
            42,
            error_class="builder_unknown_failure",
            phase="builder",
            details="Builder failed without specific handler (exit code 1, fallback cleanup)",
        )

    @patch("loom_tools.shepherd.cli.get_pr_for_issue", return_value=None)
    @patch("loom_tools.common.systematic_failure.detect_systematic_failure")
    @patch("loom_tools.common.systematic_failure.record_blocked_reason")
    def test_survives_record_failure(
        self,
        mock_record: MagicMock,
        mock_detect: MagicMock,
        mock_get_pr: MagicMock,
    ) -> None:
        """Should not raise when record_blocked_reason fails."""
        mock_record.side_effect = RuntimeError("disk full")

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = Path("/fake/repo")

        # Should not raise
        _record_fallback_failure(ctx, ShepherdExitCode.BUILDER_FAILED)

    @patch("loom_tools.shepherd.cli.get_pr_for_issue", return_value=None)
    @patch("subprocess.run")
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    @patch("loom_tools.common.systematic_failure.detect_systematic_failure")
    @patch("loom_tools.common.systematic_failure.record_blocked_reason")
    def test_escalates_on_systematic_failure(
        self,
        mock_record: MagicMock,
        mock_detect: MagicMock,
        mock_transition: MagicMock,
        mock_subprocess: MagicMock,
        mock_get_pr: MagicMock,
    ) -> None:
        """Should escalate to loom:blocked when systematic failure detected (issue #2707)."""
        from loom_tools.models.daemon_state import SystematicFailure

        mock_detect.return_value = SystematicFailure(
            active=True, pattern="builder_unknown_failure", count=3,
        )

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = Path("/fake/repo")

        _record_fallback_failure(ctx, ShepherdExitCode.BUILDER_FAILED)

        # Should transition labels
        mock_transition.assert_called_once_with(
            42,
            add=["loom:blocked"],
            remove=["loom:issue"],
            repo_root=Path("/fake/repo"),
        )
        # Should post escalation comment via subprocess.run
        mock_subprocess.assert_called_once()
        call_args = mock_subprocess.call_args
        cmd_list = call_args[0][0]  # First positional arg is the command list
        # Find the --body value (element after "--body")
        body_idx = cmd_list.index("--body") + 1
        comment_body = cmd_list[body_idx]
        assert "Systematic failure detected" in comment_body
        assert "builder_unknown_failure" in comment_body
        assert "3" in comment_body

    @patch("loom_tools.shepherd.cli.get_pr_for_issue", return_value=None)
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    @patch("loom_tools.common.systematic_failure.detect_systematic_failure", return_value=None)
    @patch("loom_tools.common.systematic_failure.record_blocked_reason")
    def test_no_escalation_when_no_systematic_failure(
        self,
        mock_record: MagicMock,
        mock_detect: MagicMock,
        mock_transition: MagicMock,
        mock_get_pr: MagicMock,
    ) -> None:
        """Should NOT escalate when detect_systematic_failure returns None."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = Path("/fake/repo")

        _record_fallback_failure(ctx, ShepherdExitCode.BUILDER_FAILED)

        # Should NOT transition labels
        mock_transition.assert_not_called()

    @patch("loom_tools.shepherd.cli.get_pr_for_issue", return_value=456)
    @patch("loom_tools.common.systematic_failure.detect_systematic_failure", return_value=None)
    @patch("loom_tools.common.systematic_failure.record_blocked_reason")
    def test_skips_failure_counter_when_pr_exists(
        self,
        mock_record: MagicMock,
        mock_detect: MagicMock,
        mock_get_pr: MagicMock,
    ) -> None:
        """Should skip failure counter when builder already created a PR (issue #2854)."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = Path("/fake/repo")

        _record_fallback_failure(ctx, ShepherdExitCode.BUILDER_FAILED)

        # PR exists — should NOT record failure or check systematic failures
        mock_record.assert_not_called()
        mock_detect.assert_not_called()

    @patch("loom_tools.shepherd.cli.get_pr_for_issue", return_value=None)
    @patch("loom_tools.common.systematic_failure.detect_systematic_failure", return_value=None)
    @patch("loom_tools.common.systematic_failure.record_blocked_reason")
    def test_records_worktree_conflict_class(
        self,
        mock_record: MagicMock,
        mock_detect: MagicMock,
        mock_get_pr: MagicMock,
    ) -> None:
        """Should record worktree_conflict (not auth_infrastructure_failure) when
        abandonment_info identifies the failure as a branch-in-use conflict.
        See issue #2918."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = Path("/fake/repo")
        ctx.abandonment_info = {
            "phase": "builder",
            "exit_code": 9,
            "failure_data": {
                "worktree_conflict": True,
                "error_detail": (
                    "fatal: 'feature/issue-42' is already used by worktree at "
                    "'/Users/user/GitHub/loom'"
                ),
            },
            "message": "failed to create worktree",
            "task_id": "abc123",
        }

        _record_fallback_failure(ctx, ShepherdExitCode.SYSTEMIC_FAILURE)

        mock_record.assert_called_once()
        call_args = mock_record.call_args
        assert call_args.args[0] == Path("/fake/repo")
        assert call_args.args[1] == 42
        assert call_args.kwargs["error_class"] == "worktree_conflict"
        assert call_args.kwargs["phase"] == "builder"
        assert "already checked out in another worktree" in call_args.kwargs["details"]
        # Error detail should be included in the details
        assert "feature/issue-42" in call_args.kwargs["details"]
        mock_detect.assert_called_once_with(Path("/fake/repo"))

    @patch("loom_tools.shepherd.cli.get_pr_for_issue", return_value=None)
    @patch("loom_tools.common.systematic_failure.detect_systematic_failure", return_value=None)
    @patch("loom_tools.common.systematic_failure.record_blocked_reason")
    def test_worktree_conflict_without_error_detail(
        self,
        mock_record: MagicMock,
        mock_detect: MagicMock,
        mock_get_pr: MagicMock,
    ) -> None:
        """worktree_conflict error class is recorded even without error_detail."""
        ctx = MagicMock()
        ctx.config.issue = 55
        ctx.repo_root = Path("/fake/repo")
        ctx.abandonment_info = {
            "phase": "builder",
            "exit_code": 9,
            "failure_data": {
                "worktree_conflict": True,
                # No error_detail key
            },
            "message": "failed to create worktree",
            "task_id": "xyz789",
        }

        _record_fallback_failure(ctx, ShepherdExitCode.SYSTEMIC_FAILURE)

        call_args = mock_record.call_args
        assert call_args.kwargs["error_class"] == "worktree_conflict"
        # Details should not contain the error_detail suffix
        assert "—" not in call_args.kwargs["details"]

    @patch("loom_tools.shepherd.cli.get_pr_for_issue", return_value=None)
    @patch("loom_tools.common.systematic_failure.detect_systematic_failure", return_value=None)
    @patch("loom_tools.common.systematic_failure.record_blocked_reason")
    def test_systemic_failure_without_worktree_conflict_stays_auth(
        self,
        mock_record: MagicMock,
        mock_detect: MagicMock,
        mock_get_pr: MagicMock,
    ) -> None:
        """SYSTEMIC_FAILURE without worktree_conflict flag stays auth_infrastructure_failure."""
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = Path("/fake/repo")
        # Set abandonment_info without worktree_conflict
        ctx.abandonment_info = {
            "phase": "builder",
            "exit_code": 9,
            "failure_data": {"auth_failure": True},
            "message": "auth timed out",
            "task_id": "abc123",
        }

        _record_fallback_failure(ctx, ShepherdExitCode.SYSTEMIC_FAILURE)

        call_args = mock_record.call_args
        assert call_args.kwargs["error_class"] == "auth_infrastructure_failure"


class TestCleanupFallbackIntegration:
    """Test that _cleanup_labels_on_failure calls the new fallback helpers (issue #2525)."""

    def _make_cleanup_ctx(
        self,
        issue_labels: set[str] | None = None,
    ) -> MagicMock:
        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.pr_number = None
        ctx.repo_root = Path("/fake/repo")
        ctx.label_cache = MagicMock()
        if issue_labels is not None:
            ctx.label_cache.get_issue_labels.return_value = issue_labels
        return ctx

    @patch("loom_tools.shepherd.cli._record_fallback_failure")
    @patch("loom_tools.shepherd.cli._post_fallback_failure_comment")
    @patch("loom_tools.shepherd.cli._cleanup_pr_labels_on_failure")
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    def test_fallback_comment_posted_on_building_revert(
        self,
        mock_transition: MagicMock,
        mock_pr_cleanup: MagicMock,
        mock_comment: MagicMock,
        mock_record: MagicMock,
    ) -> None:
        """Fallback path should post comment when reverting loom:building → loom:issue."""
        ctx = self._make_cleanup_ctx(
            issue_labels={"loom:building", "loom:curated"}
        )
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.BUILDER_FAILED)

        mock_comment.assert_called_once_with(ctx, ShepherdExitCode.BUILDER_FAILED)
        mock_record.assert_called_once_with(ctx, ShepherdExitCode.BUILDER_FAILED)

    @patch("loom_tools.shepherd.cli._record_fallback_failure")
    @patch("loom_tools.shepherd.cli._post_fallback_failure_comment")
    @patch("loom_tools.shepherd.cli._cleanup_pr_labels_on_failure")
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    def test_no_fallback_comment_when_mark_handler_ran(
        self,
        mock_transition: MagicMock,
        mock_pr_cleanup: MagicMock,
        mock_comment: MagicMock,
        mock_record: MagicMock,
    ) -> None:
        """Should NOT post fallback comment when _mark_* handler already set loom:blocked."""
        ctx = self._make_cleanup_ctx(
            issue_labels={"loom:blocked", "loom:curated"}
        )
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.PR_TESTS_FAILED)

        mock_comment.assert_not_called()
        mock_record.assert_not_called()

    @patch("loom_tools.shepherd.cli._record_fallback_failure")
    @patch("loom_tools.shepherd.cli._post_fallback_failure_comment")
    @patch("loom_tools.shepherd.cli._cleanup_pr_labels_on_failure")
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    def test_no_fallback_comment_when_no_building_label(
        self,
        mock_transition: MagicMock,
        mock_pr_cleanup: MagicMock,
        mock_comment: MagicMock,
        mock_record: MagicMock,
    ) -> None:
        """Should NOT post fallback comment when issue doesn't have loom:building."""
        ctx = self._make_cleanup_ctx(
            issue_labels={"loom:curated", "loom:triage"}
        )
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.BUILDER_FAILED)

        mock_comment.assert_not_called()
        mock_record.assert_not_called()

    @patch("loom_tools.shepherd.cli._record_fallback_failure")
    @patch("loom_tools.shepherd.cli._post_fallback_failure_comment")
    @patch("loom_tools.shepherd.cli._cleanup_pr_labels_on_failure")
    @patch("loom_tools.shepherd.cli.transition_issue_labels")
    def test_fallback_with_systemic_failure_exit_code(
        self,
        mock_transition: MagicMock,
        mock_pr_cleanup: MagicMock,
        mock_comment: MagicMock,
        mock_record: MagicMock,
    ) -> None:
        """Fallback should pass systemic failure exit code to helpers."""
        ctx = self._make_cleanup_ctx(
            issue_labels={"loom:building", "loom:curated"}
        )
        _cleanup_labels_on_failure(ctx, ShepherdExitCode.SYSTEMIC_FAILURE)

        mock_comment.assert_called_once_with(ctx, ShepherdExitCode.SYSTEMIC_FAILURE)
        mock_record.assert_called_once_with(ctx, ShepherdExitCode.SYSTEMIC_FAILURE)


class TestRebaseBeforeDoctor:
    """Test rebase-before-doctor optimization for unmodified-file test failures."""

    @patch("loom_tools.shepherd.cli.is_branch_behind")
    @patch("loom_tools.shepherd.cli.attempt_rebase")
    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_rebase_fixes_tests_skips_doctor(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockDoctor: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
        mock_attempt_rebase: MagicMock,
        mock_is_behind: MagicMock,
    ) -> None:
        """When failing tests are in unmodified files and rebase fixes them, Doctor is skipped."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
            # Rebase + re-test
            100,   # test_start (after rebase)
            110,   # test elapsed
            # Completion validation
            110,   # completion_start
            120,   # completion elapsed
            # Judge
            120,   # judge phase_start
            170,   # judge elapsed
            # Rebase phase
            170,   # rebase phase_start
            173,   # rebase elapsed
            # Merge
            173,   # merge phase_start
            183,   # merge elapsed
            183,   # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)
        ctx.pr_number = 100
        ctx.worktree_path = Path("/fake/repo/.loom/worktrees/issue-42")

        MockCurator.return_value.should_skip.return_value = (True, "skipped")
        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")
        # Builder reports test failure in files NOT modified by builder
        builder_inst.run.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={
                "test_failure": True,
                "test_output_tail": "FAILED tests/test_other.py::test_bar",
                "test_command": "pytest",
                "changed_files": ["src/main.py"],
                "failing_test_files": ["tests/test_other.py"],
            },
        )

        # Rebase succeeds
        mock_is_behind.return_value = True
        mock_attempt_rebase.return_value = (True, "")

        # After rebase, tests pass
        builder_inst.run_test_verification_only.return_value = None
        builder_inst.push_branch.return_value = True
        builder_inst.validate_and_complete.return_value = _success_result(
            "builder", committed=True, pr_created=True
        )

        # Judge approves
        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.SUCCESS

        # Doctor was never called
        MockDoctor.return_value.run_test_fix.assert_not_called()
        # Rebase was attempted
        mock_attempt_rebase.assert_called_once()

    @patch("loom_tools.shepherd.cli.is_branch_behind")
    @patch("loom_tools.shepherd.cli.attempt_rebase")
    @patch("loom_tools.shepherd.cli._mark_builder_test_failure")
    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_rebase_succeeds_but_tests_still_fail_treats_as_preexisting(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockDoctor: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
        mock_mark_failure: MagicMock,
        mock_attempt_rebase: MagicMock,
        mock_is_behind: MagicMock,
    ) -> None:
        """When rebase succeeds but tests still fail in unmodified files,
        treat as pre-existing and skip Doctor (issue #2809)."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
            # Rebase + re-test (still fails)
            100,   # test_start
            110,   # test elapsed
            # Completion validation (pre-existing path)
            110,   # completion_start
            200,   # completion elapsed
            # Judge
            200,   # judge phase_start
            210,   # judge elapsed
            # Rebase
            210,   # rebase phase_start
            213,   # rebase elapsed
            # Merge
            213,   # merge phase_start
            223,   # merge elapsed
            223,   # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)
        ctx.pr_number = 100
        ctx.worktree_path = Path("/fake/repo/.loom/worktrees/issue-42")

        MockCurator.return_value.should_skip.return_value = (True, "skipped")
        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")

        # Initial test failure in unmodified files
        test_fail_result = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={
                "test_failure": True,
                "test_output_tail": "FAILED tests/test_other.py::test_bar",
                "test_command": "pytest",
                "changed_files": ["src/main.py"],
                "failing_test_files": ["tests/test_other.py"],
            },
        )
        builder_inst.run.return_value = test_fail_result

        # Rebase succeeds but tests still fail after
        mock_is_behind.return_value = True
        mock_attempt_rebase.return_value = (True, "")
        builder_inst.push_branch.return_value = True

        # Tests still fail after rebase (return a PhaseResult)
        post_rebase_fail = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={"test_failure": True, "test_output_tail": "still failing"},
        )
        builder_inst.run_test_verification_only.return_value = post_rebase_fail

        builder_inst.validate_and_complete.return_value = _success_result(
            "builder", committed=True, pr_created=True
        )

        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.SUCCESS

        # Doctor was NOT called — pre-existing failures skip Doctor
        doctor_inst = MockDoctor.return_value
        doctor_inst.run_test_fix.assert_not_called()
        # Rebase was attempted
        mock_attempt_rebase.assert_called_once()
        # Completion validation was called
        builder_inst.validate_and_complete.assert_called_once()

    @patch("loom_tools.shepherd.cli.is_branch_behind")
    @patch("loom_tools.shepherd.cli.attempt_rebase")
    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_overlapping_files_skips_rebase(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockDoctor: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
        mock_attempt_rebase: MagicMock,
        mock_is_behind: MagicMock,
    ) -> None:
        """When failing test files overlap with changed files, rebase is skipped."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
            # Doctor test-fix (no rebase)
            100,   # doctor phase_start
            200,   # doctor elapsed
            # Test re-verification after doctor
            200,   # test_start
            210,   # test elapsed
            # Completion validation
            210,   # completion_start
            220,   # completion elapsed
            # Judge
            220,   # judge phase_start
            270,   # judge elapsed
            # Rebase
            270,   # rebase phase_start
            273,   # rebase elapsed
            # Merge
            273,   # merge phase_start
            283,   # merge elapsed
            283,   # duration calc
        ])

        ctx = _make_ctx(start_from=Phase.BUILDER)
        ctx.pr_number = 100
        ctx.worktree_path = Path("/fake/repo/.loom/worktrees/issue-42")

        MockCurator.return_value.should_skip.return_value = (True, "skipped")
        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")

        # Failing test file IS in the changed files — builder's fault
        builder_inst.run.return_value = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={
                "test_failure": True,
                "test_output_tail": "FAILED tests/test_foo.py::test_bar",
                "test_command": "pytest",
                "changed_files": ["tests/test_foo.py", "src/main.py"],
                "failing_test_files": ["tests/test_foo.py"],
            },
        )

        # After doctor, tests pass
        builder_inst.run_test_verification_only.return_value = None
        builder_inst.push_branch.return_value = True

        # Doctor succeeds
        doctor_inst = MockDoctor.return_value
        doctor_inst.run_test_fix.return_value = _success_result("doctor")

        builder_inst.validate_and_complete.return_value = _success_result(
            "builder", committed=True, pr_created=True
        )

        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.SUCCESS

        # Rebase was NOT attempted (files overlap)
        mock_is_behind.assert_not_called()
        mock_attempt_rebase.assert_not_called()
        # Doctor WAS called directly
        doctor_inst.run_test_fix.assert_called_once()

    @patch("loom_tools.shepherd.cli.is_branch_behind")
    @patch("loom_tools.shepherd.cli.attempt_rebase")
    @patch("loom_tools.shepherd.cli._mark_builder_test_failure")
    @patch("loom_tools.shepherd.cli.MergePhase")
    @patch("loom_tools.shepherd.cli.RebasePhase")
    @patch("loom_tools.shepherd.cli.JudgePhase")
    @patch("loom_tools.shepherd.cli.DoctorPhase")
    @patch("loom_tools.shepherd.cli.BuilderPhase")
    @patch("loom_tools.shepherd.cli.ApprovalPhase")
    @patch("loom_tools.shepherd.cli.CuratorPhase")
    @patch("loom_tools.shepherd.cli.time")
    def test_branch_up_to_date_unmodified_files_treats_as_preexisting(
        self,
        mock_time: MagicMock,
        MockCurator: MagicMock,
        MockApproval: MagicMock,
        MockBuilder: MagicMock,
        MockDoctor: MagicMock,
        MockJudge: MagicMock,
        MockRebase: MagicMock,
        MockMerge: MagicMock,
        mock_mark_failure: MagicMock,
        mock_attempt_rebase: MagicMock,
        mock_is_behind: MagicMock,
    ) -> None:
        """When branch is up-to-date with main and failing tests are in
        unmodified files, treat as pre-existing and skip Doctor (issue #2809)."""
        mock_time.time = MagicMock(side_effect=list(range(100)))

        ctx = _make_ctx(start_from=Phase.BUILDER)
        ctx.pr_number = 100
        ctx.worktree_path = Path("/fake/repo/.loom/worktrees/issue-42")
        ctx.config.test_fix_max_retries = 2

        MockCurator.return_value.should_skip.return_value = (True, "skipped")
        MockApproval.return_value.run.return_value = _success_result("approval")

        builder_inst = MockBuilder.return_value
        builder_inst.should_skip.return_value = (False, "")

        # Failing tests in unmodified files — rebase eligible
        test_fail = PhaseResult(
            status=PhaseStatus.FAILED,
            message="test verification failed",
            phase_name="builder",
            data={
                "test_failure": True,
                "test_output_tail": "FAILED tests/test_other.py::test_bar",
                "test_command": "pytest",
                "changed_files": ["src/main.py"],
                "failing_test_files": ["tests/test_other.py"],
            },
        )
        builder_inst.run.return_value = test_fail

        # Branch is up-to-date, so rebase is skipped on first attempt
        mock_is_behind.return_value = False

        builder_inst.validate_and_complete.return_value = _success_result(
            "builder", committed=True, pr_created=True
        )

        judge_inst = MockJudge.return_value
        judge_inst.should_skip.return_value = (False, "")
        judge_inst.run.return_value = _success_result("judge", approved=True)

        MockRebase.return_value.run.return_value = PhaseResult(
            status=PhaseStatus.SKIPPED, message="up to date", phase_name="rebase"
        )
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.SUCCESS

        # is_branch_behind was only checked once (first attempt)
        mock_is_behind.assert_called_once()
        # attempt_rebase was never called (branch was up-to-date)
        mock_attempt_rebase.assert_not_called()
        # Doctor was NOT called — pre-existing failures skip Doctor
        doctor_inst = MockDoctor.return_value
        doctor_inst.run_test_fix.assert_not_called()
        # Completion validation was called
        builder_inst.validate_and_complete.assert_called_once()



class TestGetPriorFailureInfo:
    """Tests for _get_prior_failure_info (issue #2824)."""

    def test_no_state_file(self, tmp_path: Path) -> None:
        """Should return zero count when daemon-state.json doesn't exist."""
        count, threshold, last_err = _get_prior_failure_info(tmp_path, 42)
        assert count == 0
        assert threshold == 3
        assert last_err is None

    def test_no_recent_failures(self, tmp_path: Path) -> None:
        """Should return zero count when recent_failures is empty."""
        import json

        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "daemon-state.json").write_text(json.dumps({"recent_failures": []}))

        count, threshold, last_err = _get_prior_failure_info(tmp_path, 42)
        assert count == 0
        assert threshold == 3
        assert last_err is None

    def test_counts_issue_specific_failures(self, tmp_path: Path) -> None:
        """Should count only failures for the specified issue."""
        import json

        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "daemon-state.json").write_text(json.dumps({
            "recent_failures": [
                {"issue": 42, "error_class": "builder_unknown_failure", "phase": "builder", "timestamp": "2026-01-01T00:00:00Z"},
                {"issue": 99, "error_class": "builder_test_failure", "phase": "builder", "timestamp": "2026-01-01T00:01:00Z"},
                {"issue": 42, "error_class": "builder_test_failure", "phase": "builder", "timestamp": "2026-01-01T00:02:00Z"},
            ],
        }))

        count, threshold, last_err = _get_prior_failure_info(tmp_path, 42)
        assert count == 2
        assert threshold == 3
        assert last_err == "builder_test_failure"

    def test_returns_last_error_class(self, tmp_path: Path) -> None:
        """Should return the most recent error class for the issue."""
        import json

        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "daemon-state.json").write_text(json.dumps({
            "recent_failures": [
                {"issue": 42, "error_class": "builder_unknown_failure", "phase": "builder", "timestamp": "2026-01-01T00:00:00Z"},
            ],
        }))

        count, threshold, last_err = _get_prior_failure_info(tmp_path, 42)
        assert count == 1
        assert last_err == "builder_unknown_failure"

    def test_no_failures_for_issue(self, tmp_path: Path) -> None:
        """Should return zero when no failures match the issue."""
        import json

        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "daemon-state.json").write_text(json.dumps({
            "recent_failures": [
                {"issue": 99, "error_class": "builder_test_failure", "phase": "builder", "timestamp": "2026-01-01T00:00:00Z"},
            ],
        }))

        count, threshold, last_err = _get_prior_failure_info(tmp_path, 42)
        assert count == 0
        assert last_err is None

    def test_handles_corrupt_state_file(self, tmp_path: Path) -> None:
        """Should return zero count when state file is corrupt."""
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "daemon-state.json").write_text("not valid json")

        count, threshold, last_err = _get_prior_failure_info(tmp_path, 42)
        assert count == 0
        assert last_err is None
