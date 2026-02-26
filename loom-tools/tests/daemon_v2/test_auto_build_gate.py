"""Tests for the auto_build gate in daemon iteration.

When auto_build=False (the new default), spawn_shepherds must NOT be called
even when the snapshot recommends it and shepherd slots are available.
"""

from __future__ import annotations

import pathlib
from unittest import mock

import pytest

from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.daemon_v2.iteration import run_iteration
from loom_tools.models.daemon_state import DaemonState, ShepherdEntry


def _make_ctx(
    tmp_path: pathlib.Path,
    *,
    auto_build: bool,
    force_mode: bool = False,
) -> DaemonContext:
    """Return a DaemonContext with minimal state for unit tests."""
    config = DaemonConfig(auto_build=auto_build, force_mode=force_mode)
    ctx = DaemonContext(config=config, repo_root=tmp_path)
    ctx.state = DaemonState()
    return ctx


def _snapshot_with_spawn_shepherds_action() -> dict:
    """Build a minimal snapshot that recommends spawn_shepherds."""
    return {
        "pipeline": {
            "ready_issues": [{"number": 42, "title": "Test issue"}],
        },
        "prs": {"open": [], "spinning": []},
        "shepherds": {"progress": []},
        "pipeline_health": {"retryable_issues": [], "escalation_needed": []},
        "computed": {
            "total_ready": 1,
            "total_building": 0,
            "total_blocked": 0,
            "active_shepherds": 0,
            "available_shepherd_slots": 3,
            "health_status": "healthy",
            "health_warnings": [],
            "needs_human_input": [],
            "recommended_actions": ["spawn_shepherds"],
        },
        "support_roles": {},
    }


class TestAutoBuildGateInIteration:
    """run_iteration must gate spawn_shepherds on config.auto_build."""

    @mock.patch("loom_tools.daemon_v2.iteration.spawn_shepherds")
    @mock.patch("loom_tools.daemon_v2.iteration.spawn_roles_from_actions", return_value=0)
    @mock.patch("loom_tools.daemon_v2.iteration.check_completions", return_value=[])
    @mock.patch("loom_tools.daemon_v2.iteration.reclaim_completed_support_roles", return_value=[])
    @mock.patch("loom_tools.daemon_v2.iteration.force_reclaim_stale_shepherds", return_value=0)
    @mock.patch("loom_tools.daemon_v2.iteration.read_daemon_state")
    @mock.patch("loom_tools.daemon_v2.iteration.build_snapshot")
    @mock.patch("loom_tools.daemon_v2.iteration._save_daemon_state")
    def test_spawn_shepherds_not_called_when_auto_build_false(
        self,
        _mock_save,
        mock_snapshot,
        mock_read_state,
        _mock_force_reclaim,
        _mock_reclaim_support,
        _mock_completions,
        _mock_roles,
        mock_spawn,
        tmp_path: pathlib.Path,
    ) -> None:
        """spawn_shepherds must NOT be called when auto_build=False."""
        mock_snapshot.return_value = _snapshot_with_spawn_shepherds_action()
        state = DaemonState()
        mock_read_state.return_value = state

        ctx = _make_ctx(tmp_path, auto_build=False)
        ctx.state = state

        run_iteration(ctx)

        mock_spawn.assert_not_called()

    @mock.patch("loom_tools.daemon_v2.iteration.spawn_shepherds", return_value=1)
    @mock.patch("loom_tools.daemon_v2.iteration.spawn_roles_from_actions", return_value=0)
    @mock.patch("loom_tools.daemon_v2.iteration.check_completions", return_value=[])
    @mock.patch("loom_tools.daemon_v2.iteration.reclaim_completed_support_roles", return_value=[])
    @mock.patch("loom_tools.daemon_v2.iteration.force_reclaim_stale_shepherds", return_value=0)
    @mock.patch("loom_tools.daemon_v2.iteration.read_daemon_state")
    @mock.patch("loom_tools.daemon_v2.iteration.build_snapshot")
    @mock.patch("loom_tools.daemon_v2.iteration._save_daemon_state")
    def test_spawn_shepherds_called_when_auto_build_true(
        self,
        _mock_save,
        mock_snapshot,
        mock_read_state,
        _mock_force_reclaim,
        _mock_reclaim_support,
        _mock_completions,
        _mock_roles,
        mock_spawn,
        tmp_path: pathlib.Path,
    ) -> None:
        """spawn_shepherds must be called when auto_build=True and action recommended."""
        mock_snapshot.return_value = _snapshot_with_spawn_shepherds_action()
        state = DaemonState()
        mock_read_state.return_value = state

        ctx = _make_ctx(tmp_path, auto_build=True)
        ctx.state = state

        run_iteration(ctx)

        mock_spawn.assert_called_once_with(ctx)
