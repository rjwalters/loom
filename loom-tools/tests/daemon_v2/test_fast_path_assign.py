"""Tests for fast-path shepherd assignment during sleep ticks.

The fast-path assigns ready issues to idle shepherd slots during responsive-sleep
ticks without waiting for the next full iteration. This reduces shepherd idle time
from up to poll_interval seconds to at most tick seconds (default 2s).
"""

from __future__ import annotations

import pathlib
from unittest import mock

import pytest

from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.daemon_v2.loop import _fast_path_assign
from loom_tools.models.daemon_state import DaemonState, ShepherdEntry


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_ctx(
    tmp_path: pathlib.Path,
    *,
    max_shepherds: int = 3,
    auto_build: bool = True,
) -> DaemonContext:
    """Return a DaemonContext with minimal state for unit tests."""
    config = DaemonConfig(max_shepherds=max_shepherds, auto_build=auto_build)
    ctx = DaemonContext(config=config, repo_root=tmp_path)
    ctx.state = DaemonState()
    return ctx


def _snapshot_with_ready_issues(issue_numbers: list[int]) -> dict:
    """Build a minimal snapshot dict with the given ready issues."""
    ready_issues = [{"number": n, "title": f"Issue #{n}"} for n in issue_numbers]
    return {
        "pipeline": {"ready_issues": ready_issues},
        "computed": {
            "available_shepherd_slots": len(ready_issues),
            "total_ready": len(ready_issues),
            "recommended_actions": ["spawn_shepherds"] if ready_issues else [],
        },
    }


def _snapshot_empty() -> dict:
    """Build a minimal snapshot with no ready issues."""
    return {
        "pipeline": {"ready_issues": []},
        "computed": {
            "available_shepherd_slots": 0,
            "total_ready": 0,
            "recommended_actions": [],
        },
    }


# ---------------------------------------------------------------------------
# Tests: fast-path no-ops
# ---------------------------------------------------------------------------


class TestFastPathAssignNoOps:
    """_fast_path_assign should be a no-op when conditions are not met."""

    def test_noop_when_no_state(self, tmp_path: pathlib.Path) -> None:
        """No state — must not raise, must not spawn."""
        ctx = _make_ctx(tmp_path)
        ctx.state = None
        ctx.snapshot = _snapshot_with_ready_issues([42])

        with mock.patch("loom_tools.daemon_v2.loop.spawn_shepherds") as mock_spawn:
            _fast_path_assign(ctx)
            mock_spawn.assert_not_called()

    def test_noop_when_no_snapshot(self, tmp_path: pathlib.Path) -> None:
        """No snapshot — must not raise, must not spawn."""
        ctx = _make_ctx(tmp_path)
        ctx.snapshot = None

        with mock.patch("loom_tools.daemon_v2.loop.spawn_shepherds") as mock_spawn:
            _fast_path_assign(ctx)
            mock_spawn.assert_not_called()

    def test_noop_when_no_ready_issues_in_snapshot(self, tmp_path: pathlib.Path) -> None:
        """Empty ready issues list — no spawn should occur."""
        ctx = _make_ctx(tmp_path)
        ctx.snapshot = _snapshot_empty()

        with mock.patch("loom_tools.daemon_v2.loop.spawn_shepherds") as mock_spawn:
            _fast_path_assign(ctx)
            mock_spawn.assert_not_called()

    def test_noop_when_all_slots_working(self, tmp_path: pathlib.Path) -> None:
        """All shepherd slots occupied — no spawn should occur."""
        ctx = _make_ctx(tmp_path, max_shepherds=2)
        ctx.state.shepherds["shepherd-1"] = ShepherdEntry(status="working", issue=10)
        ctx.state.shepherds["shepherd-2"] = ShepherdEntry(status="working", issue=20)
        ctx.snapshot = _snapshot_with_ready_issues([42])

        with mock.patch("loom_tools.daemon_v2.loop.spawn_shepherds") as mock_spawn:
            _fast_path_assign(ctx)
            mock_spawn.assert_not_called()


# ---------------------------------------------------------------------------
# Tests: fast-path triggers spawn
# ---------------------------------------------------------------------------


class TestFastPathAssignTriggers:
    """_fast_path_assign should call spawn_shepherds when conditions are met."""

    def test_triggers_when_idle_slot_exists(self, tmp_path: pathlib.Path) -> None:
        """Idle shepherd slot + ready issues should trigger spawn_shepherds."""
        ctx = _make_ctx(tmp_path, max_shepherds=2)
        ctx.state.shepherds["shepherd-1"] = ShepherdEntry(status="idle")
        ctx.snapshot = _snapshot_with_ready_issues([42])

        with mock.patch("loom_tools.daemon_v2.loop.spawn_shepherds", return_value=1) as mock_spawn:
            with mock.patch("loom_tools.daemon_v2.loop._write_state"):
                _fast_path_assign(ctx)
                mock_spawn.assert_called_once_with(ctx)

    def test_triggers_when_new_slot_can_be_created(self, tmp_path: pathlib.Path) -> None:
        """No existing shepherds + ready issues should trigger spawn (new slot)."""
        ctx = _make_ctx(tmp_path, max_shepherds=3)
        # No shepherd entries at all — slots can be created up to max_shepherds
        ctx.snapshot = _snapshot_with_ready_issues([42])

        with mock.patch("loom_tools.daemon_v2.loop.spawn_shepherds", return_value=1) as mock_spawn:
            with mock.patch("loom_tools.daemon_v2.loop._write_state"):
                _fast_path_assign(ctx)
                mock_spawn.assert_called_once_with(ctx)

    def test_triggers_with_mixed_shepherd_states(self, tmp_path: pathlib.Path) -> None:
        """One working, one idle shepherd — fast-path should trigger for idle."""
        ctx = _make_ctx(tmp_path, max_shepherds=2)
        ctx.state.shepherds["shepherd-1"] = ShepherdEntry(status="working", issue=10)
        ctx.state.shepherds["shepherd-2"] = ShepherdEntry(status="idle")
        ctx.snapshot = _snapshot_with_ready_issues([99])

        with mock.patch("loom_tools.daemon_v2.loop.spawn_shepherds", return_value=1) as mock_spawn:
            with mock.patch("loom_tools.daemon_v2.loop._write_state"):
                _fast_path_assign(ctx)
                mock_spawn.assert_called_once_with(ctx)

    def test_writes_state_after_successful_spawn(self, tmp_path: pathlib.Path) -> None:
        """State should be persisted after a successful fast-path spawn."""
        ctx = _make_ctx(tmp_path)
        ctx.snapshot = _snapshot_with_ready_issues([42])

        with mock.patch("loom_tools.daemon_v2.loop.spawn_shepherds", return_value=1):
            with mock.patch("loom_tools.daemon_v2.loop._write_state") as mock_write:
                _fast_path_assign(ctx)
                mock_write.assert_called_once_with(ctx)

    def test_no_state_write_when_spawn_returns_zero(self, tmp_path: pathlib.Path) -> None:
        """State should NOT be written if spawn_shepherds returns 0."""
        ctx = _make_ctx(tmp_path)
        ctx.snapshot = _snapshot_with_ready_issues([42])

        with mock.patch("loom_tools.daemon_v2.loop.spawn_shepherds", return_value=0):
            with mock.patch("loom_tools.daemon_v2.loop._write_state") as mock_write:
                _fast_path_assign(ctx)
                mock_write.assert_not_called()


# ---------------------------------------------------------------------------
# Tests: auto_build gate
# ---------------------------------------------------------------------------


class TestFastPathAutoBuiltGate:
    """_fast_path_assign must not spawn when auto_build=False."""

    def test_noop_when_auto_build_false(self, tmp_path: pathlib.Path) -> None:
        """auto_build=False prevents fast-path shepherd spawning."""
        ctx = _make_ctx(tmp_path, auto_build=False)
        ctx.state.shepherds["shepherd-1"] = ShepherdEntry(status="idle")
        ctx.snapshot = _snapshot_with_ready_issues([42])

        with mock.patch("loom_tools.daemon_v2.loop.spawn_shepherds") as mock_spawn:
            _fast_path_assign(ctx)
            mock_spawn.assert_not_called()

    def test_spawns_when_auto_build_true(self, tmp_path: pathlib.Path) -> None:
        """auto_build=True allows fast-path shepherd spawning."""
        ctx = _make_ctx(tmp_path, auto_build=True)
        ctx.state.shepherds["shepherd-1"] = ShepherdEntry(status="idle")
        ctx.snapshot = _snapshot_with_ready_issues([42])

        with mock.patch("loom_tools.daemon_v2.loop.spawn_shepherds", return_value=1) as mock_spawn:
            with mock.patch("loom_tools.daemon_v2.loop._write_state"):
                _fast_path_assign(ctx)
                mock_spawn.assert_called_once_with(ctx)
