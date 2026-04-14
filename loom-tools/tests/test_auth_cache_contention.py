"""Tests for auth cache contention mitigation (issue #3109).

Tests cover:
1. Auth cache pre-warming in the daemon loop
2. Staggered support role spawning
3. Staggered shepherd spawning
"""

from __future__ import annotations

import json
import pathlib
from unittest import mock

import pytest

from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.models.daemon_state import DaemonState, ShepherdEntry, SupportRoleEntry


def _make_ctx(
    tmp_path: pathlib.Path,
    max_shepherds: int = 3,
    auto_build: bool = True,
) -> DaemonContext:
    """Create a minimal DaemonContext for testing."""
    config = DaemonConfig(max_shepherds=max_shepherds, auto_build=auto_build)
    ctx = DaemonContext(config=config, repo_root=tmp_path)
    ctx.state = DaemonState()
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


class TestPrewarmAuthCache:
    """Tests for _prewarm_auth_cache."""

    def test_prewarm_writes_cache_file_on_success(self, tmp_path: pathlib.Path) -> None:
        """Pre-warm writes a valid cache file when auth succeeds."""
        from loom_tools.daemon_v2.loop import _prewarm_auth_cache

        ctx = _make_ctx(tmp_path)
        auth_output = json.dumps({"loggedIn": True, "user": "test"})

        with mock.patch("subprocess.run") as mock_run, \
             mock.patch("os.getuid", return_value=12345):
            mock_run.return_value = mock.Mock(
                returncode=0,
                stdout=auth_output,
            )
            _prewarm_auth_cache(ctx)

        # Verify the cache file was written
        cache_file = pathlib.Path("/tmp/claude-auth-cache-12345.json")
        assert cache_file.exists()
        data = json.loads(cache_file.read_text())
        assert data["exit_code"] == 0
        assert "loggedIn" in data["output"]
        assert isinstance(data["time"], int)

        # Clean up
        cache_file.unlink(missing_ok=True)

    def test_prewarm_handles_timeout_gracefully(self, tmp_path: pathlib.Path) -> None:
        """Pre-warm does not raise on timeout (non-fatal)."""
        import subprocess
        from loom_tools.daemon_v2.loop import _prewarm_auth_cache

        ctx = _make_ctx(tmp_path)

        with mock.patch("subprocess.run", side_effect=subprocess.TimeoutExpired("claude", 15)):
            # Should not raise
            _prewarm_auth_cache(ctx)

    def test_prewarm_handles_nonzero_exit(self, tmp_path: pathlib.Path) -> None:
        """Pre-warm logs warning on non-zero exit code."""
        from loom_tools.daemon_v2.loop import _prewarm_auth_cache

        ctx = _make_ctx(tmp_path)

        with mock.patch("subprocess.run") as mock_run:
            mock_run.return_value = mock.Mock(returncode=1, stdout="")
            # Should not raise
            _prewarm_auth_cache(ctx)

    def test_prewarm_handles_exception_gracefully(self, tmp_path: pathlib.Path) -> None:
        """Pre-warm does not raise on unexpected exceptions."""
        from loom_tools.daemon_v2.loop import _prewarm_auth_cache

        ctx = _make_ctx(tmp_path)

        with mock.patch("subprocess.run", side_effect=OSError("no such command")):
            # Should not raise
            _prewarm_auth_cache(ctx)


class TestSupportRoleStagger:
    """Tests for staggered support role spawning."""

    def test_first_spawn_has_no_delay(self, tmp_path: pathlib.Path) -> None:
        """The first role spawn should not sleep."""
        from loom_tools.daemon_v2.actions.support_roles import spawn_roles_from_actions

        ctx = _make_ctx(tmp_path)
        ctx.snapshot["computed"]["recommended_actions"] = ["trigger_guide"]

        with mock.patch(
            "loom_tools.daemon_v2.actions.support_roles.spawn_support_role",
            return_value=True,
        ) as mock_spawn, mock.patch(
            "loom_tools.daemon_v2.actions.support_roles.time.sleep"
        ) as mock_sleep:
            result = spawn_roles_from_actions(ctx)

        assert result == 1
        mock_spawn.assert_called_once()
        mock_sleep.assert_not_called()

    def test_second_spawn_has_stagger_delay(self, tmp_path: pathlib.Path) -> None:
        """Second and subsequent role spawns should sleep between them."""
        from loom_tools.daemon_v2.actions.support_roles import (
            SPAWN_STAGGER_DELAY,
            spawn_roles_from_actions,
        )

        ctx = _make_ctx(tmp_path)
        ctx.snapshot["computed"]["recommended_actions"] = [
            "trigger_guide",
            "trigger_auditor",
        ]

        with mock.patch(
            "loom_tools.daemon_v2.actions.support_roles.spawn_support_role",
            return_value=True,
        ), mock.patch(
            "loom_tools.daemon_v2.actions.support_roles.time.sleep"
        ) as mock_sleep:
            result = spawn_roles_from_actions(ctx)

        assert result == 2
        # Exactly one sleep call (before the second spawn)
        mock_sleep.assert_called_once_with(SPAWN_STAGGER_DELAY)

    def test_failed_spawn_does_not_trigger_stagger(self, tmp_path: pathlib.Path) -> None:
        """A failed spawn (returns False) does not increment counter or stagger."""
        from loom_tools.daemon_v2.actions.support_roles import spawn_roles_from_actions

        ctx = _make_ctx(tmp_path)
        ctx.snapshot["computed"]["recommended_actions"] = [
            "trigger_guide",
            "trigger_auditor",
        ]

        # First call fails, second succeeds
        with mock.patch(
            "loom_tools.daemon_v2.actions.support_roles.spawn_support_role",
            side_effect=[False, True],
        ), mock.patch(
            "loom_tools.daemon_v2.actions.support_roles.time.sleep"
        ) as mock_sleep:
            result = spawn_roles_from_actions(ctx)

        assert result == 1
        # No stagger since the first spawn failed (spawned count was 0 before second)
        mock_sleep.assert_not_called()

    def test_three_spawns_stagger_twice(self, tmp_path: pathlib.Path) -> None:
        """Three successful spawns should stagger between each consecutive pair."""
        from loom_tools.daemon_v2.actions.support_roles import (
            SPAWN_STAGGER_DELAY,
            spawn_roles_from_actions,
        )

        ctx = _make_ctx(tmp_path)
        ctx.snapshot["computed"]["recommended_actions"] = [
            "trigger_guide",
            "trigger_auditor",
            "trigger_architect",
        ]

        with mock.patch(
            "loom_tools.daemon_v2.actions.support_roles.spawn_support_role",
            return_value=True,
        ), mock.patch(
            "loom_tools.daemon_v2.actions.support_roles.time.sleep"
        ) as mock_sleep:
            result = spawn_roles_from_actions(ctx)

        assert result == 3
        assert mock_sleep.call_count == 2
        for call in mock_sleep.call_args_list:
            assert call == mock.call(SPAWN_STAGGER_DELAY)


class TestShepherdSpawnStagger:
    """Tests for staggered shepherd spawning."""

    def test_single_shepherd_no_delay(self, tmp_path: pathlib.Path) -> None:
        """Spawning a single shepherd should not sleep."""
        from loom_tools.daemon_v2.actions.shepherds import spawn_shepherds

        ctx = _make_ctx(tmp_path)
        ctx.snapshot["computed"]["available_shepherd_slots"] = 3
        ctx.snapshot["pipeline"] = {"ready_issues": [{"number": 42}]}
        ctx.snapshot["computed"]["total_ready"] = 1
        ctx.snapshot["computed"]["recommended_actions"] = ["spawn_shepherds"]

        with mock.patch(
            "loom_tools.daemon_v2.actions.shepherds._spawn_single_shepherd",
            return_value=True,
        ), mock.patch(
            "loom_tools.daemon_v2.actions.shepherds.time.sleep"
        ) as mock_sleep:
            result = spawn_shepherds(ctx)

        assert result == 1
        mock_sleep.assert_not_called()

    def test_multiple_shepherds_stagger(self, tmp_path: pathlib.Path) -> None:
        """Spawning multiple shepherds should sleep between them."""
        from loom_tools.daemon_v2.actions.shepherds import (
            SHEPHERD_SPAWN_STAGGER_DELAY,
            spawn_shepherds,
        )

        ctx = _make_ctx(tmp_path)
        ctx.snapshot["computed"]["available_shepherd_slots"] = 3
        ctx.snapshot["pipeline"] = {
            "ready_issues": [{"number": 42}, {"number": 43}]
        }
        ctx.snapshot["computed"]["total_ready"] = 2
        ctx.snapshot["computed"]["recommended_actions"] = ["spawn_shepherds"]

        with mock.patch(
            "loom_tools.daemon_v2.actions.shepherds._spawn_single_shepherd",
            return_value=True,
        ), mock.patch(
            "loom_tools.daemon_v2.actions.shepherds._find_idle_shepherd",
            side_effect=["shepherd-1", "shepherd-2"],
        ), mock.patch(
            "loom_tools.daemon_v2.actions.shepherds.time.sleep"
        ) as mock_sleep:
            result = spawn_shepherds(ctx)

        assert result == 2
        # Only one sleep (before the second spawn)
        mock_sleep.assert_called_once_with(SHEPHERD_SPAWN_STAGGER_DELAY)

    def test_failed_spawn_no_stagger(self, tmp_path: pathlib.Path) -> None:
        """A failed spawn does not count toward stagger."""
        from loom_tools.daemon_v2.actions.shepherds import spawn_shepherds

        ctx = _make_ctx(tmp_path)
        ctx.snapshot["computed"]["available_shepherd_slots"] = 3
        ctx.snapshot["pipeline"] = {
            "ready_issues": [{"number": 42}, {"number": 43}]
        }
        ctx.snapshot["computed"]["total_ready"] = 2

        with mock.patch(
            "loom_tools.daemon_v2.actions.shepherds._spawn_single_shepherd",
            side_effect=[False, True],
        ), mock.patch(
            "loom_tools.daemon_v2.actions.shepherds._find_idle_shepherd",
            side_effect=["shepherd-1", "shepherd-2"],
        ), mock.patch(
            "loom_tools.daemon_v2.actions.shepherds.time.sleep"
        ) as mock_sleep:
            result = spawn_shepherds(ctx)

        assert result == 1
        # No stagger: first spawn failed so spawned=0 when second runs
        mock_sleep.assert_not_called()
