"""Tests for daemon error resilience (issue #3102).

Verifies that the daemon catches non-critical errors (FileNotFoundError,
PermissionError, etc.) in the main loop and iteration logic instead of
crashing the entire daemon process.
"""

from __future__ import annotations

import pathlib
from unittest import mock

import pytest

from loom_tools.daemon_v2.actions.completions import CompletionEntry
from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.daemon_v2.iteration import IterationResult, run_iteration
from loom_tools.models.daemon_state import DaemonState, ShepherdEntry


def _make_ctx(
    tmp_path: pathlib.Path,
    shepherds: dict[str, ShepherdEntry] | None = None,
    max_shepherds: int = 3,
) -> DaemonContext:
    """Create a minimal DaemonContext for testing."""
    config = DaemonConfig(max_shepherds=max_shepherds)
    ctx = DaemonContext(config=config, repo_root=tmp_path)
    ctx.iteration = 1
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
            "needs_human_input": [],
        },
        "shepherds": {"progress": []},
        "prs": {"spinning": []},
        "pipeline_health": {},
    }
    return ctx


class TestIterationCompletionErrorResilience:
    """Verify that errors in completion handling don't crash the iteration."""

    def test_check_completions_error_returns_success(
        self, tmp_path: pathlib.Path
    ) -> None:
        """If check_completions raises, run_iteration still succeeds."""
        ctx = _make_ctx(tmp_path)

        # Create state file so _save_daemon_state works
        state_file = tmp_path / ".loom" / "daemon-state.json"
        state_file.parent.mkdir(parents=True, exist_ok=True)
        state_file.write_text("{}")

        with (
            mock.patch(
                "loom_tools.daemon_v2.iteration.build_snapshot",
                return_value=ctx.snapshot,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.read_daemon_state",
                return_value=ctx.state,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.check_completions",
                side_effect=FileNotFoundError(
                    "[Errno 2] No such file or directory: '.loom/claude-config/shepherd-4/.claude.json.lock'"
                ),
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.spawn_shepherds",
                return_value=0,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.spawn_roles_from_actions",
                return_value=0,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.force_reclaim_stale_shepherds",
                return_value=0,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.reclaim_completed_support_roles",
                return_value=[],
            ),
        ):
            result = run_iteration(ctx)

        # Should be success, not a crash
        assert result.status == "success"
        assert result.completions_handled == 0

    def test_handle_completion_error_continues_to_next(
        self, tmp_path: pathlib.Path
    ) -> None:
        """If handle_completion raises for one entry, the rest still run."""
        ctx = _make_ctx(
            tmp_path,
            shepherds={
                "shepherd-1": ShepherdEntry(
                    status="working", issue=100, task_id="aaa1111"
                ),
                "shepherd-2": ShepherdEntry(
                    status="working", issue=200, task_id="bbb2222"
                ),
            },
        )

        state_file = tmp_path / ".loom" / "daemon-state.json"
        state_file.parent.mkdir(parents=True, exist_ok=True)
        state_file.write_text("{}")

        completion1 = CompletionEntry(
            type="shepherd", name="shepherd-1", issue=100, task_id="aaa1111"
        )
        completion2 = CompletionEntry(
            type="shepherd", name="shepherd-2", issue=200, task_id="bbb2222"
        )

        handle_call_count = 0

        def mock_handle(ctx, completion):
            nonlocal handle_call_count
            handle_call_count += 1
            if completion.name == "shepherd-1":
                raise PermissionError("cannot remove lock file")

        with (
            mock.patch(
                "loom_tools.daemon_v2.iteration.build_snapshot",
                return_value=ctx.snapshot,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.read_daemon_state",
                return_value=ctx.state,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.check_completions",
                return_value=[completion1, completion2],
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.handle_completion",
                side_effect=mock_handle,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.spawn_shepherds",
                return_value=0,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.spawn_roles_from_actions",
                return_value=0,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.force_reclaim_stale_shepherds",
                return_value=0,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.reclaim_completed_support_roles",
                return_value=[],
            ),
        ):
            result = run_iteration(ctx)

        # Both completions were attempted (2 calls), even though first raised
        assert handle_call_count == 2
        assert result.status == "success"

    def test_stale_shepherd_reclaim_error_continues(
        self, tmp_path: pathlib.Path
    ) -> None:
        """If stale shepherd reclaim raises, iteration still succeeds."""
        ctx = _make_ctx(tmp_path)

        state_file = tmp_path / ".loom" / "daemon-state.json"
        state_file.parent.mkdir(parents=True, exist_ok=True)
        state_file.write_text("{}")

        with (
            mock.patch(
                "loom_tools.daemon_v2.iteration.build_snapshot",
                return_value=ctx.snapshot,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.read_daemon_state",
                return_value=ctx.state,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.check_completions",
                return_value=[],
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.force_reclaim_stale_shepherds",
                side_effect=OSError("disk I/O error"),
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.spawn_shepherds",
                return_value=0,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.spawn_roles_from_actions",
                return_value=0,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.reclaim_completed_support_roles",
                return_value=[],
            ),
        ):
            result = run_iteration(ctx)

        assert result.status == "success"

    def test_support_role_reclaim_error_continues(
        self, tmp_path: pathlib.Path
    ) -> None:
        """If support role reclaim raises, iteration still succeeds."""
        ctx = _make_ctx(tmp_path)

        state_file = tmp_path / ".loom" / "daemon-state.json"
        state_file.parent.mkdir(parents=True, exist_ok=True)
        state_file.write_text("{}")

        with (
            mock.patch(
                "loom_tools.daemon_v2.iteration.build_snapshot",
                return_value=ctx.snapshot,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.read_daemon_state",
                return_value=ctx.state,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.check_completions",
                return_value=[],
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.force_reclaim_stale_shepherds",
                return_value=0,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.spawn_shepherds",
                return_value=0,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.spawn_roles_from_actions",
                return_value=0,
            ),
            mock.patch(
                "loom_tools.daemon_v2.iteration.reclaim_completed_support_roles",
                side_effect=FileNotFoundError("config dir missing"),
            ),
        ):
            result = run_iteration(ctx)

        assert result.status == "success"


class TestMainLoopErrorResilience:
    """Verify that the main loop catches non-critical errors."""

    def test_single_iteration_error_does_not_crash(
        self, tmp_path: pathlib.Path
    ) -> None:
        """A single non-critical error should log a warning and continue."""
        from loom_tools.daemon_v2.loop import run

        config = DaemonConfig(poll_interval=1, timeout_min=0)
        ctx = DaemonContext(config=config, repo_root=tmp_path)

        # Create required dirs/files
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir(parents=True)
        signals_dir = loom_dir / "signals"
        signals_dir.mkdir()
        pid_file = loom_dir / "daemon.pid"

        iteration_count = 0

        def mock_run_iteration(ctx):
            nonlocal iteration_count
            iteration_count += 1
            if iteration_count == 1:
                raise FileNotFoundError(
                    ".loom/claude-config/shepherd-4/.claude.json.lock"
                )
            # Stop after second iteration
            ctx.running = False
            return IterationResult(status="success", summary="ok")

        with (
            mock.patch(
                "loom_tools.daemon_v2.loop.check_existing_pid",
                return_value=(False, None),
            ),
            mock.patch("loom_tools.daemon_v2.loop._run_preflight_checks", return_value=[]),
            mock.patch("loom_tools.daemon_v2.loop.write_pid_file"),
            mock.patch("loom_tools.daemon_v2.loop._rotate_state_file"),
            mock.patch("loom_tools.daemon_v2.loop._init_state_file"),
            mock.patch("loom_tools.daemon_v2.loop._init_metrics_file"),
            mock.patch("loom_tools.daemon_v2.loop.clear_stop_signal"),
            mock.patch("loom_tools.daemon_v2.loop._print_header"),
            mock.patch("loom_tools.daemon_v2.loop.load_config"),
            mock.patch("loom_tools.daemon_v2.loop.handle_daemon_startup"),
            mock.patch("loom_tools.daemon_v2.loop.handle_daemon_shutdown"),
            mock.patch("loom_tools.daemon_v2.loop.cleanup_on_exit"),
            mock.patch("loom_tools.daemon_v2.loop.check_stop_signal", return_value=False),
            mock.patch("loom_tools.daemon_v2.loop.check_session_conflict", return_value=False),
            mock.patch("loom_tools.daemon_v2.loop.run_iteration", side_effect=mock_run_iteration),
            mock.patch("loom_tools.daemon_v2.loop._update_metrics"),
            mock.patch("loom_tools.daemon_v2.loop._responsive_sleep"),
            mock.patch("loom_tools.daemon_v2.loop.CommandPoller") as mock_poller_cls,
        ):
            mock_poller = mock.MagicMock()
            mock_poller.poll.return_value = []
            mock_poller.queue_depth.return_value = 0
            mock_poller_cls.return_value = mock_poller

            ctx.orchestration_active = True

            exit_code = run(ctx)

        # Daemon should complete gracefully (exit code 0), not crash (exit code 1)
        assert exit_code == 0
        # Both iterations should have run
        assert iteration_count == 2

    def test_consecutive_errors_eventually_crash(
        self, tmp_path: pathlib.Path
    ) -> None:
        """10 consecutive non-critical errors should be treated as fatal."""
        from loom_tools.daemon_v2.exit_codes import DaemonExitCode
        from loom_tools.daemon_v2.loop import run

        config = DaemonConfig(poll_interval=1, timeout_min=0)
        ctx = DaemonContext(config=config, repo_root=tmp_path)

        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir(parents=True)
        signals_dir = loom_dir / "signals"
        signals_dir.mkdir()

        def mock_run_iteration(ctx):
            raise FileNotFoundError("persistent failure")

        with (
            mock.patch(
                "loom_tools.daemon_v2.loop.check_existing_pid",
                return_value=(False, None),
            ),
            mock.patch("loom_tools.daemon_v2.loop._run_preflight_checks", return_value=[]),
            mock.patch("loom_tools.daemon_v2.loop.write_pid_file"),
            mock.patch("loom_tools.daemon_v2.loop._rotate_state_file"),
            mock.patch("loom_tools.daemon_v2.loop._init_state_file"),
            mock.patch("loom_tools.daemon_v2.loop._init_metrics_file"),
            mock.patch("loom_tools.daemon_v2.loop.clear_stop_signal"),
            mock.patch("loom_tools.daemon_v2.loop._print_header"),
            mock.patch("loom_tools.daemon_v2.loop.load_config"),
            mock.patch("loom_tools.daemon_v2.loop.handle_daemon_startup"),
            mock.patch("loom_tools.daemon_v2.loop.handle_daemon_shutdown"),
            mock.patch("loom_tools.daemon_v2.loop.cleanup_on_exit"),
            mock.patch("loom_tools.daemon_v2.loop.check_stop_signal", return_value=False),
            mock.patch("loom_tools.daemon_v2.loop.check_session_conflict", return_value=False),
            mock.patch("loom_tools.daemon_v2.loop.run_iteration", side_effect=mock_run_iteration),
            mock.patch("loom_tools.daemon_v2.loop._responsive_sleep"),
            mock.patch("loom_tools.daemon_v2.loop.CommandPoller") as mock_poller_cls,
        ):
            mock_poller = mock.MagicMock()
            mock_poller.poll.return_value = []
            mock_poller.queue_depth.return_value = 0
            mock_poller_cls.return_value = mock_poller

            ctx.orchestration_active = True

            exit_code = run(ctx)

        # Should crash after MAX_CONSECUTIVE_ERRORS (10)
        assert exit_code == DaemonExitCode.ERROR

    def test_error_counter_resets_on_success(
        self, tmp_path: pathlib.Path
    ) -> None:
        """A successful iteration resets the consecutive error counter."""
        from loom_tools.daemon_v2.loop import run

        config = DaemonConfig(poll_interval=1, timeout_min=0)
        ctx = DaemonContext(config=config, repo_root=tmp_path)

        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir(parents=True)
        signals_dir = loom_dir / "signals"
        signals_dir.mkdir()

        call_count = 0

        def mock_run_iteration(ctx):
            nonlocal call_count
            call_count += 1
            # First 5 calls fail, then succeed, then 5 more fail, then succeed
            # This ensures the counter resets after success
            if call_count <= 5:
                raise FileNotFoundError("transient error")
            if call_count == 6:
                return IterationResult(status="success", summary="ok")
            if call_count <= 11:
                raise FileNotFoundError("transient error again")
            # Stop after the 12th call
            ctx.running = False
            return IterationResult(status="success", summary="done")

        with (
            mock.patch(
                "loom_tools.daemon_v2.loop.check_existing_pid",
                return_value=(False, None),
            ),
            mock.patch("loom_tools.daemon_v2.loop._run_preflight_checks", return_value=[]),
            mock.patch("loom_tools.daemon_v2.loop.write_pid_file"),
            mock.patch("loom_tools.daemon_v2.loop._rotate_state_file"),
            mock.patch("loom_tools.daemon_v2.loop._init_state_file"),
            mock.patch("loom_tools.daemon_v2.loop._init_metrics_file"),
            mock.patch("loom_tools.daemon_v2.loop.clear_stop_signal"),
            mock.patch("loom_tools.daemon_v2.loop._print_header"),
            mock.patch("loom_tools.daemon_v2.loop.load_config"),
            mock.patch("loom_tools.daemon_v2.loop.handle_daemon_startup"),
            mock.patch("loom_tools.daemon_v2.loop.handle_daemon_shutdown"),
            mock.patch("loom_tools.daemon_v2.loop.cleanup_on_exit"),
            mock.patch("loom_tools.daemon_v2.loop.check_stop_signal", return_value=False),
            mock.patch("loom_tools.daemon_v2.loop.check_session_conflict", return_value=False),
            mock.patch("loom_tools.daemon_v2.loop.run_iteration", side_effect=mock_run_iteration),
            mock.patch("loom_tools.daemon_v2.loop._update_metrics"),
            mock.patch("loom_tools.daemon_v2.loop._responsive_sleep"),
            mock.patch("loom_tools.daemon_v2.loop.CommandPoller") as mock_poller_cls,
        ):
            mock_poller = mock.MagicMock()
            mock_poller.poll.return_value = []
            mock_poller.queue_depth.return_value = 0
            mock_poller_cls.return_value = mock_poller

            ctx.orchestration_active = True

            exit_code = run(ctx)

        # Should complete gracefully — the success at call 6 and 12 reset
        # the error counter so we never hit the threshold of 10
        assert exit_code == 0
        assert call_count == 12

    def test_keyboard_interrupt_propagates_through_loop(
        self, tmp_path: pathlib.Path
    ) -> None:
        """KeyboardInterrupt should propagate and not be silently caught."""
        from loom_tools.daemon_v2.loop import run

        config = DaemonConfig(poll_interval=1, timeout_min=0)
        ctx = DaemonContext(config=config, repo_root=tmp_path)

        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir(parents=True)
        signals_dir = loom_dir / "signals"
        signals_dir.mkdir()

        def mock_run_iteration(ctx):
            raise KeyboardInterrupt()

        with (
            mock.patch(
                "loom_tools.daemon_v2.loop.check_existing_pid",
                return_value=(False, None),
            ),
            mock.patch("loom_tools.daemon_v2.loop._run_preflight_checks", return_value=[]),
            mock.patch("loom_tools.daemon_v2.loop.write_pid_file"),
            mock.patch("loom_tools.daemon_v2.loop._rotate_state_file"),
            mock.patch("loom_tools.daemon_v2.loop._init_state_file"),
            mock.patch("loom_tools.daemon_v2.loop._init_metrics_file"),
            mock.patch("loom_tools.daemon_v2.loop.clear_stop_signal"),
            mock.patch("loom_tools.daemon_v2.loop._print_header"),
            mock.patch("loom_tools.daemon_v2.loop.load_config"),
            mock.patch("loom_tools.daemon_v2.loop.handle_daemon_startup"),
            mock.patch("loom_tools.daemon_v2.loop.handle_daemon_shutdown"),
            mock.patch("loom_tools.daemon_v2.loop.cleanup_on_exit"),
            mock.patch("loom_tools.daemon_v2.loop.check_stop_signal", return_value=False),
            mock.patch("loom_tools.daemon_v2.loop.check_session_conflict", return_value=False),
            mock.patch("loom_tools.daemon_v2.loop.run_iteration", side_effect=mock_run_iteration),
            mock.patch("loom_tools.daemon_v2.loop._responsive_sleep"),
            mock.patch("loom_tools.daemon_v2.loop.CommandPoller") as mock_poller_cls,
        ):
            mock_poller = mock.MagicMock()
            mock_poller.poll.return_value = []
            mock_poller.queue_depth.return_value = 0
            mock_poller_cls.return_value = mock_poller

            ctx.orchestration_active = True

            # KeyboardInterrupt inherits from BaseException, not Exception.
            # The inner try/except (KeyboardInterrupt, SystemExit): raise
            # re-raises it. The outer except Exception does NOT catch it.
            # It propagates through finally and out to the caller.
            with pytest.raises(KeyboardInterrupt):
                run(ctx)
