"""Tests for support role completion detection via tmux liveness checks.

Verifies that the daemon detects when support role tmux sessions exit
and transitions their state from "running" to "idle".
"""

from __future__ import annotations

import pathlib
from unittest import mock

import pytest

from loom_tools.daemon_v2.actions.completions import CompletionEntry
from loom_tools.daemon_v2.actions.support_roles import reclaim_completed_support_roles
from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.daemon_v2.iteration import _reclaim_completed_support_roles
from loom_tools.models.daemon_state import DaemonState, SupportRoleEntry


def _make_ctx(
    tmp_path: pathlib.Path,
    support_roles: dict[str, SupportRoleEntry] | None = None,
) -> DaemonContext:
    """Create a minimal DaemonContext with support roles."""
    config = DaemonConfig(max_shepherds=3)
    ctx = DaemonContext(config=config, repo_root=tmp_path)
    ctx.state = DaemonState(support_roles=support_roles or {})
    ctx.snapshot = {
        "computed": {
            "active_shepherds": 0,
            "available_shepherd_slots": 3,
            "recommended_actions": [],
        },
    }
    return ctx


class TestReclaimCompletedSupportRoles:
    """Tests for reclaim_completed_support_roles in support_roles.py."""

    def test_dead_session_produces_completion(self, tmp_path: pathlib.Path) -> None:
        """A running role with a dead tmux session should produce a CompletionEntry."""
        ctx = _make_ctx(
            tmp_path,
            support_roles={
                "judge": SupportRoleEntry(
                    status="running",
                    tmux_session="loom-judge",
                    started="2026-01-30T10:00:00Z",
                ),
            },
        )

        with mock.patch(
            "loom_tools.daemon_v2.actions.support_roles.session_exists",
            return_value=False,
        ):
            completed = reclaim_completed_support_roles(ctx)

        assert len(completed) == 1
        assert completed[0].type == "support_role"
        assert completed[0].name == "judge"

    def test_alive_session_not_reclaimed(self, tmp_path: pathlib.Path) -> None:
        """A running role with a live tmux session should NOT be reclaimed."""
        ctx = _make_ctx(
            tmp_path,
            support_roles={
                "champion": SupportRoleEntry(
                    status="running",
                    tmux_session="loom-champion",
                    started="2026-01-30T10:00:00Z",
                ),
            },
        )

        with mock.patch(
            "loom_tools.daemon_v2.actions.support_roles.session_exists",
            return_value=True,
        ):
            completed = reclaim_completed_support_roles(ctx)

        assert len(completed) == 0

    def test_idle_roles_skipped(self, tmp_path: pathlib.Path) -> None:
        """Idle roles should not be checked or reclaimed."""
        ctx = _make_ctx(
            tmp_path,
            support_roles={
                "guide": SupportRoleEntry(
                    status="idle",
                    last_completed="2026-01-30T09:00:00Z",
                ),
            },
        )

        with mock.patch(
            "loom_tools.daemon_v2.actions.support_roles.session_exists",
        ) as mock_exists:
            completed = reclaim_completed_support_roles(ctx)

        assert len(completed) == 0
        mock_exists.assert_not_called()

    def test_multiple_roles_mixed_states(self, tmp_path: pathlib.Path) -> None:
        """Multiple roles: only running ones with dead sessions are reclaimed."""
        ctx = _make_ctx(
            tmp_path,
            support_roles={
                "judge": SupportRoleEntry(
                    status="running",
                    tmux_session="loom-judge",
                    started="2026-01-30T10:00:00Z",
                ),
                "champion": SupportRoleEntry(
                    status="running",
                    tmux_session="loom-champion",
                    started="2026-01-30T10:00:00Z",
                ),
                "guide": SupportRoleEntry(status="idle"),
                "doctor": SupportRoleEntry(
                    status="running",
                    tmux_session="loom-doctor",
                    started="2026-01-30T10:00:00Z",
                ),
            },
        )

        def mock_session_exists(name: str) -> bool:
            # judge dead, champion alive, doctor dead
            return name == "champion"

        with mock.patch(
            "loom_tools.daemon_v2.actions.support_roles.session_exists",
            side_effect=mock_session_exists,
        ):
            completed = reclaim_completed_support_roles(ctx)

        assert len(completed) == 2
        names = {c.name for c in completed}
        assert names == {"judge", "doctor"}

    def test_no_state_returns_empty(self, tmp_path: pathlib.Path) -> None:
        """When ctx.state is None, returns empty list."""
        ctx = _make_ctx(tmp_path)
        ctx.state = None

        completed = reclaim_completed_support_roles(ctx)
        assert completed == []


class TestHandleCompletionIntegration:
    """Verify that CompletionEntry from reclaim flows through handle_completion."""

    def test_completion_updates_state_to_idle(self, tmp_path: pathlib.Path) -> None:
        """handle_completion should transition role from running to idle."""
        from loom_tools.daemon_v2.actions.completions import handle_completion

        ctx = _make_ctx(
            tmp_path,
            support_roles={
                "judge": SupportRoleEntry(
                    status="running",
                    tmux_session="loom-judge",
                    started="2026-01-30T10:00:00Z",
                ),
            },
        )

        completion = CompletionEntry(type="support_role", name="judge")
        handle_completion(ctx, completion)

        entry = ctx.state.support_roles["judge"]
        assert entry.status == "idle"
        assert entry.last_completed is not None
        assert entry.tmux_session is None

    def test_completion_for_missing_role_is_noop(self, tmp_path: pathlib.Path) -> None:
        """handle_completion for unknown role doesn't crash."""
        from loom_tools.daemon_v2.actions.completions import handle_completion

        ctx = _make_ctx(tmp_path)
        completion = CompletionEntry(type="support_role", name="nonexistent")
        handle_completion(ctx, completion)  # Should not raise


class TestIterationWrapperFunction:
    """Tests for _reclaim_completed_support_roles in iteration.py."""

    def test_no_running_roles_skips_check(self, tmp_path: pathlib.Path) -> None:
        """When no roles are running, session_exists is never called."""
        ctx = _make_ctx(
            tmp_path,
            support_roles={
                "guide": SupportRoleEntry(status="idle"),
                "champion": SupportRoleEntry(status="idle"),
            },
        )

        with mock.patch(
            "loom_tools.daemon_v2.actions.support_roles.session_exists",
        ) as mock_exists:
            result = _reclaim_completed_support_roles(ctx)

        assert result == []
        mock_exists.assert_not_called()

    def test_running_role_triggers_check(self, tmp_path: pathlib.Path) -> None:
        """When a role is running, the check runs and detects dead session."""
        ctx = _make_ctx(
            tmp_path,
            support_roles={
                "auditor": SupportRoleEntry(
                    status="running",
                    tmux_session="loom-auditor",
                    started="2026-01-30T10:00:00Z",
                ),
            },
        )

        with mock.patch(
            "loom_tools.daemon_v2.actions.support_roles.session_exists",
            return_value=False,
        ):
            result = _reclaim_completed_support_roles(ctx)

        assert len(result) == 1
        assert result[0].name == "auditor"

    def test_none_state_returns_empty(self, tmp_path: pathlib.Path) -> None:
        """When ctx.state is None, returns empty list."""
        ctx = _make_ctx(tmp_path)
        ctx.state = None

        result = _reclaim_completed_support_roles(ctx)
        assert result == []


class TestRunIterationWithSupportRoleReclaim:
    """Integration: verify run_iteration includes support role reclaim."""

    def test_dead_support_role_reclaimed_during_iteration(
        self, tmp_path: pathlib.Path
    ) -> None:
        """run_iteration should detect and handle a dead support role session."""
        import json

        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir(parents=True)
        (loom_dir / "progress").mkdir()

        state = DaemonState(
            support_roles={
                "judge": SupportRoleEntry(
                    status="running",
                    tmux_session="loom-judge",
                    started="2026-01-30T10:00:00Z",
                ),
            },
        )
        (loom_dir / "daemon-state.json").write_text(json.dumps(state.to_dict()))

        config = DaemonConfig(max_shepherds=3)
        ctx = DaemonContext(config=config, repo_root=tmp_path, iteration=1)

        mock_snapshot = {
            "timestamp": "2026-01-30T18:00:00Z",
            "pipeline": {"ready_issues": []},
            "proposals": {},
            "prs": {},
            "shepherds": {"progress": [], "stale_heartbeat_count": 0},
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
                "active_shepherds": 0,
                "available_shepherd_slots": 3,
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
            mock.patch(
                "loom_tools.daemon_v2.iteration.build_snapshot",
                return_value=mock_snapshot,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.read_daemon_state",
                return_value=state,
            ),
            mock.patch("loom_tools.daemon_v2.iteration.write_json_file"),
            mock.patch(
                "loom_tools.daemon_v2.actions.support_roles.session_exists",
                return_value=False,
            ),
        ):
            from loom_tools.daemon_v2.iteration import run_iteration

            result = run_iteration(ctx)

        # The judge role should now be idle
        assert state.support_roles["judge"].status == "idle"
        assert state.support_roles["judge"].last_completed is not None
        assert state.support_roles["judge"].tmux_session is None
        # Completion should be counted
        assert result.completions_handled == 1
