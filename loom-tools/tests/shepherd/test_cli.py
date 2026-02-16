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
    _is_loom_runtime,
    _create_config,
    _format_diagnostics_for_comment,
    _format_diagnostics_for_log,
    _gather_no_pr_diagnostics,
    _mark_builder_no_pr,
    _mark_judge_exhausted,
    _parse_args,
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
             patch("loom_tools.shepherd.cli._auto_navigate_out_of_worktree"):
            result = main(["42"])
            assert isinstance(result, int)

    def test_passes_exit_code_from_orchestrate(self) -> None:
        """main should return orchestrate's exit code."""
        with patch("loom_tools.shepherd.cli.orchestrate", return_value=1), \
             patch("loom_tools.shepherd.cli.ShepherdContext"), \
             patch("loom_tools.shepherd.cli.find_repo_root", return_value=Path("/fake/repo")), \
             patch("loom_tools.shepherd.cli._auto_navigate_out_of_worktree"):
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
             patch("loom_tools.shepherd.cli._auto_navigate_out_of_worktree", side_effect=track_navigate):
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
             patch("loom_tools.shepherd.cli.get_uncommitted_files", return_value=["M file.py"]):
            result = main(["42", "--allow-dirty-main"])
            assert result == 0

    def test_clean_repo_proceeds(self) -> None:
        """main should proceed when repo is clean."""
        with patch("loom_tools.shepherd.cli.orchestrate", return_value=0), \
             patch("loom_tools.shepherd.cli.ShepherdContext"), \
             patch("loom_tools.shepherd.cli.find_repo_root", return_value=Path("/fake/repo")), \
             patch("loom_tools.shepherd.cli._auto_navigate_out_of_worktree"), \
             patch("loom_tools.shepherd.cli.get_uncommitted_files", return_value=[]):
            result = main(["42"])
            assert result == 0


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
    return ctx


def _success_result(phase: str = "", **data: object) -> PhaseResult:
    """Create a successful PhaseResult."""
    return PhaseResult(status=PhaseStatus.SUCCESS, message=f"{phase} done", phase_name=phase, data=data)


class TestPhaseTiming:
    """Test per-phase timing in orchestrate()."""

    @patch("loom_tools.shepherd.cli.MergePhase")
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
        MockMerge: MagicMock,
    ) -> None:
        """Phase durations dict should be populated after orchestration."""
        # Simulate time progression: each phase takes a known duration
        # time.time() calls: start_time, curator_start, curator_end, approval_start, approval_end,
        #   builder_start, builder_end, judge_start, judge_end, merge_start, merge_end, duration_calc
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

        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        captured = capsys.readouterr()
        # log_info writes to stderr
        assert "Builder: 350s" in captured.err
        assert "Judge: 150s" in captured.err
        assert "Merge: 50s" in captured.err

    @patch("loom_tools.shepherd.cli.MergePhase")
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
        MockMerge: MagicMock,
    ) -> None:
        """Judge/Doctor retry loop should accumulate timing per attempt."""
        # Flow: curator(skip) -> approval -> builder(skip) -> judge1(changes) -> doctor1 -> judge2(approved) -> merge
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
            # Merge
            300,   # merge phase_start
            310,   # merge elapsed (10s)
            310,   # duration calc
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

        # Merge: success
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

        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        captured = capsys.readouterr()
        # log_success/log_info write to stderr
        assert "(30s)" in captured.err   # Curator
        assert "(170s)" in captured.err  # Builder
        assert "(100s)" in captured.err  # Judge
        assert "(5s)" in captured.err    # Merge


class TestDoctorSkippedHeader:
    """Test Doctor phase skipped header when Judge approves first try (issue #1767)."""

    @patch("loom_tools.shepherd.cli.MergePhase")
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

        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        captured = capsys.readouterr()
        assert "PHASE 5: DOCTOR (skipped - no changes requested)" in captured.err

    @patch("loom_tools.shepherd.cli.MergePhase")
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
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == 0

        captured = capsys.readouterr()
        # Should see Doctor phase header with attempt number, NOT skipped
        assert "PHASE 5: DOCTOR (attempt 1)" in captured.err
        assert "PHASE 5: DOCTOR (skipped" not in captured.err

    @patch("loom_tools.shepherd.cli.MergePhase")
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
        MockMerge: MagicMock,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        """Should NOT print Doctor skipped header when Judge itself was skipped."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
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

    @patch("loom_tools.shepherd.cli._run_reflection")
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
        mock_reflection: MagicMock,
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
            15,    # _run_reflection duration
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
        MockMerge: MagicMock,
    ) -> None:
        """Approved outcome on first try should work identically to before."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # judge phase_start
            15,    # judge elapsed
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


class TestMarkJudgeExhausted:
    """Test _mark_judge_exhausted helper function."""

    @patch("subprocess.run")
    def test_transitions_labels(self, mock_run: MagicMock) -> None:
        """Should run gh command to transition loom:building -> loom:failed:judge."""
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
        assert "loom:failed:judge" in cmd

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
        """Should run gh command to transition loom:building -> loom:failed:builder."""
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
        assert "loom:failed:builder" in cmd

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
            150,   # merge phase_start
            160,   # merge elapsed
            160,   # duration calc
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
            # Merge
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
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.SUCCESS

        # Should NOT mark as failure
        mock_mark_failure.assert_not_called()
        # Doctor was called
        doctor_inst.run_test_fix.assert_called_once()

    @patch("loom_tools.shepherd.cli._run_reflection")
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
        mock_reflection: MagicMock,
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
        time_values.append(360)  # _run_reflection duration

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

    @patch("loom_tools.shepherd.cli._run_reflection")
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
        mock_reflection: MagicMock,
    ) -> None:
        """Non-test builder failures should NOT route to Doctor."""
        mock_time.time = MagicMock(side_effect=[
            0,     # start_time
            0,     # approval phase_start
            5,     # approval elapsed
            5,     # builder phase_start
            100,   # builder elapsed
            100,   # _run_reflection duration
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


class TestDoctorRegressionGuard:
    """Test that the doctor test-fix loop aborts when doctor makes things worse."""

    @patch("loom_tools.shepherd.cli._run_reflection")
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
        mock_reflection: MagicMock,
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
            # Regression detected → duration calc for _run_reflection
            160,
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

    @patch("loom_tools.shepherd.cli._run_reflection")
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
        mock_reflection: MagicMock,
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
        time_values.append(360)  # _run_reflection duration

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
        MockMerge.return_value.run.return_value = _success_result("merge", merged=True)

        result = orchestrate(ctx)
        assert result == ShepherdExitCode.SUCCESS

        # Doctor was called once, tests passed
        assert doctor_inst.run_test_fix.call_count == 1
        mock_mark_failure.assert_not_called()
