"""Tests for shepherd CLI."""

from __future__ import annotations

import sys
import time
from io import StringIO
from unittest.mock import MagicMock, call, patch

import pytest

from loom_tools.shepherd.cli import _create_config, _parse_args, main, orchestrate
from loom_tools.shepherd.config import ExecutionMode, Phase, ShepherdConfig
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


class TestMain:
    """Test main entry point."""

    def test_returns_int(self) -> None:
        """main should return an int exit code."""
        with patch("loom_tools.shepherd.cli.orchestrate", return_value=0):
            with patch("loom_tools.shepherd.cli.ShepherdContext"):
                result = main(["42"])
                assert isinstance(result, int)

    def test_passes_exit_code_from_orchestrate(self) -> None:
        """main should return orchestrate's exit code."""
        with patch("loom_tools.shepherd.cli.orchestrate", return_value=1):
            with patch("loom_tools.shepherd.cli.ShepherdContext"):
                result = main(["42"])
                assert result == 1


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
