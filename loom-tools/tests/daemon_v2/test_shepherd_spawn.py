"""Tests for daemon shepherd spawning logic."""

from __future__ import annotations

import pathlib
from unittest import mock

import pytest

from loom_tools.daemon_v2.actions.shepherds import _spawn_single_shepherd, spawn_shepherds
from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.models.daemon_state import DaemonState, ShepherdEntry


def _make_ctx(
    tmp_path: pathlib.Path,
    *,
    force_mode: bool = False,
) -> DaemonContext:
    """Create a DaemonContext with minimal state for testing."""
    config = DaemonConfig(force_mode=force_mode)
    ctx = DaemonContext(config=config, repo_root=tmp_path)
    ctx.state = DaemonState()
    ctx.state.shepherds["shepherd-1"] = ShepherdEntry(status="idle")
    return ctx


class TestSpawnSingleShepherd:
    """Tests for _spawn_single_shepherd args building."""

    @mock.patch("loom_tools.daemon_v2.actions.shepherds.spawn_agent")
    @mock.patch("loom_tools.daemon_v2.actions.shepherds._claim_issue", return_value=True)
    @mock.patch("loom_tools.daemon_v2.actions.shepherds.claim_issue", return_value=0)
    @mock.patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=False)
    def test_always_passes_merge_flag(
        self, _mock_session, _mock_claim, _mock_gh_claim, mock_spawn, tmp_path
    ):
        """Daemon-spawned shepherds always get --merge to complete PR lifecycle (#2387)."""
        mock_spawn.return_value = mock.MagicMock(
            status="ok", session="loom-shepherd-1", log="/tmp/test.log"
        )
        ctx = _make_ctx(tmp_path, force_mode=False)

        _spawn_single_shepherd(ctx, "shepherd-1", 42)

        mock_spawn.assert_called_once()
        args = mock_spawn.call_args.kwargs.get("args", "")
        assert "--merge" in args, f"Expected --merge in args: {args!r}"

    @mock.patch("loom_tools.daemon_v2.actions.shepherds.spawn_agent")
    @mock.patch("loom_tools.daemon_v2.actions.shepherds._claim_issue", return_value=True)
    @mock.patch("loom_tools.daemon_v2.actions.shepherds.claim_issue", return_value=0)
    @mock.patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=False)
    def test_merge_flag_in_force_mode_too(
        self, _mock_session, _mock_claim, _mock_gh_claim, mock_spawn, tmp_path
    ):
        """Force mode should also pass --merge (not --force anymore)."""
        mock_spawn.return_value = mock.MagicMock(
            status="ok", session="loom-shepherd-1", log="/tmp/test.log"
        )
        ctx = _make_ctx(tmp_path, force_mode=True)

        _spawn_single_shepherd(ctx, "shepherd-1", 42)

        mock_spawn.assert_called_once()
        args = mock_spawn.call_args.kwargs.get("args", "")
        assert "--merge" in args, f"Expected --merge in args: {args!r}"

    @mock.patch("loom_tools.daemon_v2.actions.shepherds.spawn_agent")
    @mock.patch("loom_tools.daemon_v2.actions.shepherds._claim_issue", return_value=True)
    @mock.patch("loom_tools.daemon_v2.actions.shepherds.claim_issue", return_value=0)
    @mock.patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=False)
    def test_allow_dirty_main_always_passed(
        self, _mock_session, _mock_claim, _mock_gh_claim, mock_spawn, tmp_path
    ):
        """--allow-dirty-main should always be included."""
        mock_spawn.return_value = mock.MagicMock(
            status="ok", session="loom-shepherd-1", log="/tmp/test.log"
        )
        ctx = _make_ctx(tmp_path)

        _spawn_single_shepherd(ctx, "shepherd-1", 42)

        args = mock_spawn.call_args.kwargs.get("args", "")
        assert "--allow-dirty-main" in args


class TestSpawnStagger:
    """Tests for stagger delay between shepherd spawns."""

    def _make_multi_ctx(
        self,
        tmp_path: pathlib.Path,
        *,
        spawn_stagger_delay: int = 3,
        max_shepherds: int = 5,
    ) -> DaemonContext:
        """Create a DaemonContext with multiple idle shepherd slots."""
        config = DaemonConfig(
            spawn_stagger_delay=spawn_stagger_delay,
            max_shepherds=max_shepherds,
        )
        ctx = DaemonContext(config=config, repo_root=tmp_path)
        ctx.state = DaemonState()
        ctx.snapshot = {
            "pipeline": {
                "ready_issues": [
                    {"number": 1, "title": "Issue 1"},
                    {"number": 2, "title": "Issue 2"},
                    {"number": 3, "title": "Issue 3"},
                ],
            },
            "computed": {
                "available_shepherd_slots": 3,
                "active_shepherds": 0,
            },
            "shepherds": {"progress": []},
        }
        # Pre-create idle shepherd slots
        for i in range(1, 4):
            ctx.state.shepherds[f"shepherd-{i}"] = ShepherdEntry(status="idle")
        return ctx

    @mock.patch("loom_tools.daemon_v2.actions.shepherds.time.sleep")
    @mock.patch("loom_tools.daemon_v2.actions.shepherds._spawn_single_shepherd")
    def test_stagger_delay_between_spawns(
        self, mock_spawn, mock_sleep, tmp_path: pathlib.Path
    ) -> None:
        """Sleep is called between successful spawns, not before the first."""
        mock_spawn.return_value = True
        ctx = self._make_multi_ctx(tmp_path, spawn_stagger_delay=5)

        spawned = spawn_shepherds(ctx)

        assert spawned == 3
        # Sleep should be called before 2nd and 3rd spawn, not before the 1st
        assert mock_sleep.call_count == 2
        mock_sleep.assert_called_with(5)

    @mock.patch("loom_tools.daemon_v2.actions.shepherds.time.sleep")
    @mock.patch("loom_tools.daemon_v2.actions.shepherds._spawn_single_shepherd")
    def test_no_stagger_when_delay_is_zero(
        self, mock_spawn, mock_sleep, tmp_path: pathlib.Path
    ) -> None:
        """No sleep when LOOM_SPAWN_STAGGER=0."""
        mock_spawn.return_value = True
        ctx = self._make_multi_ctx(tmp_path, spawn_stagger_delay=0)

        spawned = spawn_shepherds(ctx)

        assert spawned == 3
        mock_sleep.assert_not_called()

    @mock.patch("loom_tools.daemon_v2.actions.shepherds.time.sleep")
    @mock.patch("loom_tools.daemon_v2.actions.shepherds._spawn_single_shepherd")
    def test_no_stagger_for_single_spawn(
        self, mock_spawn, mock_sleep, tmp_path: pathlib.Path
    ) -> None:
        """No sleep when only one shepherd is spawned."""
        mock_spawn.return_value = True
        ctx = self._make_multi_ctx(tmp_path, spawn_stagger_delay=3)
        # Only one ready issue
        ctx.snapshot["pipeline"]["ready_issues"] = [{"number": 1, "title": "Issue 1"}]

        spawned = spawn_shepherds(ctx)

        assert spawned == 1
        mock_sleep.assert_not_called()

    @mock.patch("loom_tools.daemon_v2.actions.shepherds.time.sleep")
    @mock.patch("loom_tools.daemon_v2.actions.shepherds._spawn_single_shepherd")
    def test_stagger_default_is_3_seconds(
        self, mock_spawn, mock_sleep, tmp_path: pathlib.Path
    ) -> None:
        """Default stagger delay should be 3 seconds."""
        mock_spawn.return_value = True
        # Use default config
        config = DaemonConfig()
        ctx = DaemonContext(config=config, repo_root=tmp_path)
        ctx.state = DaemonState()
        ctx.snapshot = {
            "pipeline": {
                "ready_issues": [
                    {"number": 1, "title": "Issue 1"},
                    {"number": 2, "title": "Issue 2"},
                ],
            },
            "computed": {
                "available_shepherd_slots": 2,
                "active_shepherds": 0,
            },
            "shepherds": {"progress": []},
        }
        ctx.state.shepherds["shepherd-1"] = ShepherdEntry(status="idle")
        ctx.state.shepherds["shepherd-2"] = ShepherdEntry(status="idle")

        spawned = spawn_shepherds(ctx)

        assert spawned == 2
        assert mock_sleep.call_count == 1
        mock_sleep.assert_called_with(3)

    def test_config_from_env_reads_spawn_stagger(self) -> None:
        """LOOM_SPAWN_STAGGER env var is read by DaemonConfig.from_env."""
        with mock.patch.dict("os.environ", {"LOOM_SPAWN_STAGGER": "7"}):
            config = DaemonConfig.from_env()
        assert config.spawn_stagger_delay == 7

    def test_config_default_spawn_stagger(self) -> None:
        """Default spawn stagger delay is 3 seconds."""
        config = DaemonConfig()
        assert config.spawn_stagger_delay == 3
