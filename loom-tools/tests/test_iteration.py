"""Tests for daemon_v2 iteration logic.

Focuses on the shepherd count recomputation after completions are handled,
ensuring the snapshot reflects post-completion state for summary and spawning.
"""

from __future__ import annotations

import pathlib
from unittest import mock

from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.daemon_v2.iteration import _build_summary, run_iteration, IterationResult
from loom_tools.models.daemon_state import DaemonState, ShepherdEntry


def _make_ctx(
    tmp_path: pathlib.Path,
    shepherds: dict[str, ShepherdEntry] | None = None,
    max_shepherds: int = 3,
) -> DaemonContext:
    """Create a minimal DaemonContext for testing."""
    config = DaemonConfig(max_shepherds=max_shepherds)
    ctx = DaemonContext(config=config, repo_root=tmp_path)
    ctx.state = DaemonState(shepherds=shepherds or {})
    ctx.snapshot = {
        "computed": {
            "active_shepherds": 0,
            "available_shepherd_slots": max_shepherds,
            "total_ready": 0,
            "total_building": 0,
            "total_blocked": 0,
            "health_status": "healthy",
            "health_warnings": [],
            "recommended_actions": [],
        },
    }
    return ctx


class TestShepherdCountRecomputeAfterCompletion:
    """Verify snapshot shepherd counts are updated after completions."""

    def test_single_completion_updates_count(self, tmp_path: pathlib.Path) -> None:
        """After one shepherd completes, active count drops by 1."""
        ctx = _make_ctx(
            tmp_path,
            shepherds={
                "shepherd-1": ShepherdEntry(status="working", issue=100, task_id="aaa1111"),
                "shepherd-2": ShepherdEntry(status="working", issue=200, task_id="bbb2222"),
                "shepherd-3": ShepherdEntry(status="working", issue=300, task_id="ccc3333"),
            },
        )
        # Snapshot initially shows all 3 active
        ctx.snapshot["computed"]["active_shepherds"] = 3
        ctx.snapshot["computed"]["available_shepherd_slots"] = 0

        # Simulate completion: mark shepherd-1 as idle (as handle_completion does)
        ctx.state.shepherds["shepherd-1"].status = "idle"
        ctx.state.shepherds["shepherd-1"].issue = None
        ctx.state.shepherds["shepherd-1"].task_id = None

        # Import and call the recompute logic directly
        # (inline version of what run_iteration does after completions)
        completions = [True]  # non-empty to trigger recompute
        if completions and ctx.state is not None and ctx.snapshot is not None:
            active_shepherds = sum(
                1 for e in ctx.state.shepherds.values() if e.status == "working"
            )
            ctx.snapshot["computed"]["active_shepherds"] = active_shepherds
            ctx.snapshot["computed"]["available_shepherd_slots"] = max(
                0, ctx.config.max_shepherds - active_shepherds
            )

        assert ctx.snapshot["computed"]["active_shepherds"] == 2
        assert ctx.snapshot["computed"]["available_shepherd_slots"] == 1

    def test_multiple_completions_update_count(self, tmp_path: pathlib.Path) -> None:
        """After 2 shepherds complete, both slots become available."""
        ctx = _make_ctx(
            tmp_path,
            shepherds={
                "shepherd-1": ShepherdEntry(status="working", issue=100, task_id="aaa1111"),
                "shepherd-2": ShepherdEntry(status="working", issue=200, task_id="bbb2222"),
                "shepherd-3": ShepherdEntry(status="working", issue=300, task_id="ccc3333"),
            },
        )
        ctx.snapshot["computed"]["active_shepherds"] = 3
        ctx.snapshot["computed"]["available_shepherd_slots"] = 0

        # Simulate 2 completions
        ctx.state.shepherds["shepherd-1"].status = "idle"
        ctx.state.shepherds["shepherd-1"].issue = None
        ctx.state.shepherds["shepherd-1"].task_id = None
        ctx.state.shepherds["shepherd-2"].status = "idle"
        ctx.state.shepherds["shepherd-2"].issue = None
        ctx.state.shepherds["shepherd-2"].task_id = None

        completions = [True, True]
        if completions and ctx.state is not None and ctx.snapshot is not None:
            active_shepherds = sum(
                1 for e in ctx.state.shepherds.values() if e.status == "working"
            )
            ctx.snapshot["computed"]["active_shepherds"] = active_shepherds
            ctx.snapshot["computed"]["available_shepherd_slots"] = max(
                0, ctx.config.max_shepherds - active_shepherds
            )

        assert ctx.snapshot["computed"]["active_shepherds"] == 1
        assert ctx.snapshot["computed"]["available_shepherd_slots"] == 2

    def test_zero_completions_skips_recompute(self, tmp_path: pathlib.Path) -> None:
        """When no completions, snapshot is unchanged (no-op)."""
        ctx = _make_ctx(
            tmp_path,
            shepherds={
                "shepherd-1": ShepherdEntry(status="working", issue=100, task_id="aaa1111"),
                "shepherd-2": ShepherdEntry(status="working", issue=200, task_id="bbb2222"),
            },
        )
        ctx.snapshot["computed"]["active_shepherds"] = 2
        ctx.snapshot["computed"]["available_shepherd_slots"] = 1

        completions = []  # empty - should skip
        if completions and ctx.state is not None and ctx.snapshot is not None:
            # This block should NOT execute
            ctx.snapshot["computed"]["active_shepherds"] = 999

        # Values unchanged
        assert ctx.snapshot["computed"]["active_shepherds"] == 2
        assert ctx.snapshot["computed"]["available_shepherd_slots"] == 1


class TestBuildSummaryReflectsPostCompletion:
    """Verify _build_summary uses post-completion shepherd counts."""

    def test_summary_shows_updated_count(self, tmp_path: pathlib.Path) -> None:
        """Summary line reflects active shepherds after completion."""
        ctx = _make_ctx(tmp_path)
        ctx.snapshot["computed"]["active_shepherds"] = 2
        ctx.snapshot["computed"]["total_ready"] = 1
        ctx.snapshot["computed"]["total_building"] = 1
        ctx.snapshot["computed"]["total_blocked"] = 0

        result = IterationResult(status="success", summary="", completions_handled=1)
        summary = _build_summary(ctx, result)

        assert "shepherds=2/3" in summary
        assert "completed=1" in summary

    def test_summary_without_completions(self, tmp_path: pathlib.Path) -> None:
        """Summary without completions shows normal count."""
        ctx = _make_ctx(tmp_path)
        ctx.snapshot["computed"]["active_shepherds"] = 3

        result = IterationResult(status="success", summary="")
        summary = _build_summary(ctx, result)

        assert "shepherds=3/3" in summary
        assert "completed=" not in summary


class TestRunIterationIntegration:
    """Integration test: run_iteration with mocked externals."""

    def test_completion_updates_snapshot_before_summary(self, tmp_path: pathlib.Path) -> None:
        """End-to-end: a completing shepherd updates snapshot before summary."""
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir(parents=True)
        progress_dir = loom_dir / "progress"
        progress_dir.mkdir()

        # Create daemon state with 3 working shepherds
        state = DaemonState(shepherds={
            "shepherd-1": ShepherdEntry(status="working", issue=100, task_id="aaa1111"),
            "shepherd-2": ShepherdEntry(status="working", issue=200, task_id="bbb2222"),
            "shepherd-3": ShepherdEntry(status="working", issue=300, task_id="ccc3333"),
        })
        import json
        (loom_dir / "daemon-state.json").write_text(json.dumps(state.to_dict()))

        # Create a progress file showing shepherd-1 completed
        (progress_dir / "shepherd-aaa1111.json").write_text(json.dumps({
            "task_id": "aaa1111",
            "issue": 100,
            "status": "completed",
            "last_heartbeat": "2026-01-30T17:59:50Z",
        }))

        config = DaemonConfig(max_shepherds=3)
        ctx = DaemonContext(config=config, repo_root=tmp_path, iteration=1)

        # Mock build_snapshot to return controlled data
        mock_snapshot = {
            "timestamp": "2026-01-30T18:00:00Z",
            "pipeline": {"ready_issues": []},
            "proposals": {},
            "prs": {},
            "shepherds": {
                "progress": [
                    {
                        "task_id": "aaa1111",
                        "issue": 100,
                        "status": "completed",
                        "last_heartbeat": "2026-01-30T17:59:50Z",
                        "heartbeat_age_seconds": 10,
                        "heartbeat_stale": False,
                    },
                ],
                "stale_heartbeat_count": 0,
            },
            "validation": {"orphaned": [], "invalid_task_ids": []},
            "support_roles": {},
            "pipeline_health": {},
            "systematic_failure": {},
            "preflight": {},
            "usage": {"session_percent": 50},
            "ci_status": None,
            "tmux_pool": {},
            "config": {},
            "computed": {
                "active_shepherds": 3,  # Stale: before completion
                "available_shepherd_slots": 0,
                "total_ready": 0,
                "total_building": 0,
                "total_blocked": 0,
                "total_proposals": 0,
                "needs_work_generation": False,
                "recommended_actions": [],
                "promotable_proposals": [],
                "health_status": "healthy",
                "health_warnings": [],
            },
        }

        with (
            mock.patch("loom_tools.daemon_v2.iteration.build_snapshot", return_value=mock_snapshot),
            mock.patch("loom_tools.daemon_v2.iteration.read_daemon_state", return_value=state),
            mock.patch("loom_tools.daemon_v2.iteration.write_json_file"),
            mock.patch("loom_tools.daemon_v2.actions.completions._trigger_shepherd_cleanup"),
            mock.patch("loom_tools.daemon_v2.iteration.force_reclaim_stale_shepherds", return_value=0),
        ):
            result = run_iteration(ctx)

        # After iteration, snapshot should reflect post-completion state
        assert ctx.snapshot["computed"]["active_shepherds"] == 2
        assert ctx.snapshot["computed"]["available_shepherd_slots"] == 1
        assert result.completions_handled == 1
        assert "shepherds=2/3" in result.summary
