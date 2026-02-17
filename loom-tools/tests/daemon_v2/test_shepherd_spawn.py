"""Tests for daemon shepherd spawning logic."""

from __future__ import annotations

import pathlib
from unittest import mock

import pytest

from loom_tools.daemon_v2.actions.shepherds import _spawn_single_shepherd
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
