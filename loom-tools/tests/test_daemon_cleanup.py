"""Tests for daemon_cleanup session termination and label revert during shutdown."""

from __future__ import annotations

import json
import pathlib
import threading
import time
from unittest import mock

from loom_tools.daemon_cleanup import (
    _revert_shepherd_labels,
    _run_orphan_recovery,
    _terminate_active_sessions,
    cleanup_stale_signal_files,
    handle_daemon_shutdown,
    handle_daemon_startup,
    load_config,
)


class TestTerminateActiveSessions:
    """Tests for _terminate_active_sessions() called during daemon shutdown."""

    def _write_state(self, state_path: pathlib.Path, data: dict) -> None:
        state_path.parent.mkdir(parents=True, exist_ok=True)
        state_path.write_text(json.dumps(data))

    def test_kills_working_shepherds(self, tmp_path: pathlib.Path) -> None:
        """Working shepherds should be terminated during shutdown."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {
                "shepherd-1": {"status": "working", "issue": 42},
                "shepherd-2": {"status": "working", "issue": 43},
            },
            "support_roles": {},
        })

        with mock.patch("loom_tools.daemon_cleanup.session_exists", return_value=True) as m_exists, \
             mock.patch("loom_tools.daemon_cleanup.kill_stuck_session") as m_kill:
            _terminate_active_sessions(state_path)

        assert m_exists.call_count == 2
        assert m_kill.call_count == 2
        m_kill.assert_any_call("shepherd-1")
        m_kill.assert_any_call("shepherd-2")

    def test_kills_errored_shepherds(self, tmp_path: pathlib.Path) -> None:
        """Errored shepherds should also be terminated."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {
                "shepherd-1": {"status": "errored", "issue": 42},
            },
            "support_roles": {},
        })

        with mock.patch("loom_tools.daemon_cleanup.session_exists", return_value=True), \
             mock.patch("loom_tools.daemon_cleanup.kill_stuck_session") as m_kill:
            _terminate_active_sessions(state_path)

        m_kill.assert_called_once_with("shepherd-1")

    def test_skips_idle_shepherds(self, tmp_path: pathlib.Path) -> None:
        """Idle shepherds should not be terminated."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {
                "shepherd-1": {"status": "idle", "issue": None},
            },
            "support_roles": {},
        })

        with mock.patch("loom_tools.daemon_cleanup.session_exists") as m_exists, \
             mock.patch("loom_tools.daemon_cleanup.kill_stuck_session") as m_kill:
            _terminate_active_sessions(state_path)

        m_exists.assert_not_called()
        m_kill.assert_not_called()

    def test_kills_running_support_roles(self, tmp_path: pathlib.Path) -> None:
        """Running support roles should be terminated."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {},
            "support_roles": {
                "champion": {"status": "running", "tmux_session": "loom-champion"},
                "doctor": {"status": "running", "tmux_session": "loom-doctor"},
            },
        })

        with mock.patch("loom_tools.daemon_cleanup.session_exists", return_value=True) as m_exists, \
             mock.patch("loom_tools.daemon_cleanup.kill_stuck_session") as m_kill:
            _terminate_active_sessions(state_path)

        assert m_kill.call_count == 2
        m_kill.assert_any_call("champion")
        m_kill.assert_any_call("doctor")

    def test_support_role_without_tmux_session_field(self, tmp_path: pathlib.Path) -> None:
        """Support roles missing tmux_session field should fall back to role name."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {},
            "support_roles": {
                "guide": {"status": "running"},
            },
        })

        with mock.patch("loom_tools.daemon_cleanup.session_exists", return_value=True), \
             mock.patch("loom_tools.daemon_cleanup.kill_stuck_session") as m_kill:
            _terminate_active_sessions(state_path)

        m_kill.assert_called_once_with("guide")

    def test_skips_idle_support_roles(self, tmp_path: pathlib.Path) -> None:
        """Idle support roles should not be terminated."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {},
            "support_roles": {
                "champion": {"status": "idle"},
            },
        })

        with mock.patch("loom_tools.daemon_cleanup.session_exists") as m_exists, \
             mock.patch("loom_tools.daemon_cleanup.kill_stuck_session") as m_kill:
            _terminate_active_sessions(state_path)

        m_exists.assert_not_called()
        m_kill.assert_not_called()

    def test_shepherds_killed_before_support_roles(self, tmp_path: pathlib.Path) -> None:
        """Shepherds should be terminated before support roles."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {
                "shepherd-1": {"status": "working", "issue": 42},
            },
            "support_roles": {
                "champion": {"status": "running", "tmux_session": "loom-champion"},
            },
        })

        call_order: list[str] = []

        def track_kill(name: str) -> None:
            call_order.append(name)

        with mock.patch("loom_tools.daemon_cleanup.session_exists", return_value=True), \
             mock.patch("loom_tools.daemon_cleanup.kill_stuck_session", side_effect=track_kill):
            _terminate_active_sessions(state_path)

        assert call_order == ["shepherd-1", "champion"]

    def test_no_active_sessions(self, tmp_path: pathlib.Path) -> None:
        """Shutdown with no active sessions should complete cleanly."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {
                "shepherd-1": {"status": "idle"},
            },
            "support_roles": {
                "champion": {"status": "idle"},
            },
        })

        with mock.patch("loom_tools.daemon_cleanup.session_exists") as m_exists, \
             mock.patch("loom_tools.daemon_cleanup.kill_stuck_session") as m_kill:
            _terminate_active_sessions(state_path)

        m_exists.assert_not_called()
        m_kill.assert_not_called()

    def test_missing_state_file(self, tmp_path: pathlib.Path) -> None:
        """Should not error when state file doesn't exist."""
        state_path = tmp_path / ".loom" / "daemon-state.json"

        with mock.patch("loom_tools.daemon_cleanup.kill_stuck_session") as m_kill:
            _terminate_active_sessions(state_path)

        m_kill.assert_not_called()

    def test_corrupt_state_file(self, tmp_path: pathlib.Path) -> None:
        """Should handle corrupt/non-dict state file gracefully."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        state_path.parent.mkdir(parents=True, exist_ok=True)
        state_path.write_text("not valid json {{")

        with mock.patch("loom_tools.daemon_cleanup.kill_stuck_session") as m_kill:
            _terminate_active_sessions(state_path)

        m_kill.assert_not_called()

    def test_session_already_dead(self, tmp_path: pathlib.Path) -> None:
        """Should skip sessions that already died (session_exists returns False)."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {
                "shepherd-1": {"status": "working", "issue": 42},
            },
            "support_roles": {},
        })

        with mock.patch("loom_tools.daemon_cleanup.session_exists", return_value=False), \
             mock.patch("loom_tools.daemon_cleanup.kill_stuck_session") as m_kill:
            _terminate_active_sessions(state_path)

        m_kill.assert_not_called()

    def test_dry_run_does_not_kill(self, tmp_path: pathlib.Path) -> None:
        """Dry run should log but not terminate sessions."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {
                "shepherd-1": {"status": "working", "issue": 42},
            },
            "support_roles": {
                "champion": {"status": "running", "tmux_session": "loom-champion"},
            },
        })

        with mock.patch("loom_tools.daemon_cleanup.session_exists") as m_exists, \
             mock.patch("loom_tools.daemon_cleanup.kill_stuck_session") as m_kill:
            _terminate_active_sessions(state_path, dry_run=True)

        m_exists.assert_not_called()
        m_kill.assert_not_called()

    def test_mixed_shepherd_statuses(self, tmp_path: pathlib.Path) -> None:
        """Only working/errored shepherds should be killed, not idle/paused ones."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {
                "shepherd-1": {"status": "working", "issue": 42},
                "shepherd-2": {"status": "idle"},
                "shepherd-3": {"status": "errored", "issue": 43},
                "shepherd-4": {"status": "paused"},
            },
            "support_roles": {},
        })

        with mock.patch("loom_tools.daemon_cleanup.session_exists", return_value=True), \
             mock.patch("loom_tools.daemon_cleanup.kill_stuck_session") as m_kill:
            _terminate_active_sessions(state_path)

        assert m_kill.call_count == 2
        m_kill.assert_any_call("shepherd-1")
        m_kill.assert_any_call("shepherd-3")

    def test_support_role_tmux_session_without_prefix(self, tmp_path: pathlib.Path) -> None:
        """Support role with tmux_session not starting with loom- should use as-is."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {},
            "support_roles": {
                "custom": {"status": "running", "tmux_session": "custom-session"},
            },
        })

        with mock.patch("loom_tools.daemon_cleanup.session_exists", return_value=True), \
             mock.patch("loom_tools.daemon_cleanup.kill_stuck_session") as m_kill:
            _terminate_active_sessions(state_path)

        # Since "custom-session" doesn't start with "loom-",
        # removeprefix("loom-") returns it unchanged
        m_kill.assert_called_once_with("custom-session")


class TestRevertShepherdLabels:
    """Tests for _revert_shepherd_labels() during daemon shutdown."""

    def _write_state(self, state_path: pathlib.Path, data: dict) -> None:
        state_path.parent.mkdir(parents=True, exist_ok=True)
        state_path.write_text(json.dumps(data))

    def test_reverts_labels_for_working_shepherds(self, tmp_path: pathlib.Path) -> None:
        """Working shepherds with issues should have labels reverted."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {
                "shepherd-1": {"status": "working", "issue": 42},
                "shepherd-2": {"status": "working", "issue": 43},
            },
        })

        with mock.patch("loom_tools.common.github.gh_run") as m_gh:
            _revert_shepherd_labels(state_path)

        assert m_gh.call_count == 2
        m_gh.assert_any_call([
            "issue", "edit", "42",
            "--remove-label", "loom:building",
            "--add-label", "loom:issue",
        ])
        m_gh.assert_any_call([
            "issue", "edit", "43",
            "--remove-label", "loom:building",
            "--add-label", "loom:issue",
        ])

    def test_skips_non_working_shepherds(self, tmp_path: pathlib.Path) -> None:
        """Shepherds with status != 'working' should not have labels reverted."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {
                "shepherd-1": {"status": "idle", "issue": None},
                "shepherd-2": {"status": "errored", "issue": 50},
                "shepherd-3": {"status": "paused", "issue": 60},
            },
        })

        with mock.patch("loom_tools.common.github.gh_run") as m_gh:
            _revert_shepherd_labels(state_path)

        m_gh.assert_not_called()

    def test_skips_working_shepherd_with_null_issue(self, tmp_path: pathlib.Path) -> None:
        """Working shepherds with no issue should not trigger label revert."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {
                "shepherd-1": {"status": "working", "issue": None},
            },
        })

        with mock.patch("loom_tools.common.github.gh_run") as m_gh:
            _revert_shepherd_labels(state_path)

        m_gh.assert_not_called()

    def test_continues_on_gh_failure(self, tmp_path: pathlib.Path) -> None:
        """If gh_run raises an exception, shutdown should continue gracefully."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {
                "shepherd-1": {"status": "working", "issue": 42},
                "shepherd-2": {"status": "working", "issue": 43},
            },
        })

        # First call raises, second succeeds
        with mock.patch("loom_tools.common.github.gh_run") as m_gh:
            m_gh.side_effect = [Exception("API error"), None]
            _revert_shepherd_labels(state_path)

        # Both calls were attempted despite the first failing
        assert m_gh.call_count == 2

    def test_no_shepherds(self, tmp_path: pathlib.Path) -> None:
        """Shutdown with zero shepherds should complete without errors."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {},
        })

        with mock.patch("loom_tools.common.github.gh_run") as m_gh:
            _revert_shepherd_labels(state_path)

        m_gh.assert_not_called()

    def test_missing_state_file(self, tmp_path: pathlib.Path) -> None:
        """Should not error when state file doesn't exist."""
        state_path = tmp_path / ".loom" / "daemon-state.json"

        with mock.patch("loom_tools.common.github.gh_run") as m_gh:
            _revert_shepherd_labels(state_path)

        m_gh.assert_not_called()

    def test_dry_run_does_not_call_gh(self, tmp_path: pathlib.Path) -> None:
        """Dry run should log but not call GitHub API."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {
                "shepherd-1": {"status": "working", "issue": 42},
            },
        })

        with mock.patch("loom_tools.common.github.gh_run") as m_gh:
            _revert_shepherd_labels(state_path, dry_run=True)

        m_gh.assert_not_called()

    def test_mixed_shepherds_only_reverts_working_with_issue(
        self, tmp_path: pathlib.Path
    ) -> None:
        """Only working shepherds with a non-null issue should be reverted."""
        state_path = tmp_path / ".loom" / "daemon-state.json"
        self._write_state(state_path, {
            "shepherds": {
                "shepherd-1": {"status": "working", "issue": 42},
                "shepherd-2": {"status": "idle", "issue": None},
                "shepherd-3": {"status": "working", "issue": None},
                "shepherd-4": {"status": "errored", "issue": 50},
            },
        })

        with mock.patch("loom_tools.common.github.gh_run") as m_gh:
            _revert_shepherd_labels(state_path)

        m_gh.assert_called_once_with([
            "issue", "edit", "42",
            "--remove-label", "loom:building",
            "--add-label", "loom:issue",
        ])


class TestHandleDaemonShutdownLabelRevert:
    """Integration test: handle_daemon_shutdown calls label revert."""

    def _write_state(self, state_path: pathlib.Path, data: dict) -> None:
        state_path.parent.mkdir(parents=True, exist_ok=True)
        state_path.write_text(json.dumps(data))

    def test_shutdown_reverts_labels_before_resetting_state(
        self, tmp_path: pathlib.Path
    ) -> None:
        """handle_daemon_shutdown should revert labels before resetting shepherds to idle."""
        repo_root = tmp_path
        loom_dir = repo_root / ".loom"
        state_path = loom_dir / "daemon-state.json"
        self._write_state(state_path, {
            "running": True,
            "shepherds": {
                "shepherd-1": {"status": "working", "issue": 42},
            },
            "support_roles": {},
        })

        config = load_config()

        with mock.patch("loom_tools.daemon_cleanup._run_archive_logs"), \
             mock.patch("loom_tools.daemon_cleanup.session_exists", return_value=False), \
             mock.patch("loom_tools.daemon_cleanup.kill_stuck_session"), \
             mock.patch("loom_tools.common.github.gh_run") as m_gh, \
             mock.patch("loom_tools.daemon_cleanup.find_repo_root", return_value=repo_root):
            handle_daemon_shutdown(repo_root, config)

        # Label revert should have been called
        m_gh.assert_called_once_with([
            "issue", "edit", "42",
            "--remove-label", "loom:building",
            "--add-label", "loom:issue",
        ])

        # State should be finalized (shepherd reset to idle)
        with open(state_path) as f:
            final_state = json.load(f)
        assert final_state["running"] is False
        assert final_state["shepherds"]["shepherd-1"]["status"] == "idle"
        assert final_state["shepherds"]["shepherd-1"]["issue"] is None


class TestRunOrphanRecoveryBackground:
    """Tests for _run_orphan_recovery background execution mode (issue #2973)."""

    def test_run_background_false_runs_synchronously(self, tmp_path: pathlib.Path) -> None:
        """With run_background=False, recovery runs in the calling thread (synchronous)."""
        called_in_thread: list[str] = []

        def fake_recovery(repo_root, *, recover, verbose):
            called_in_thread.append(threading.current_thread().name)

        with mock.patch(
            "loom_tools.orphan_recovery.run_orphan_recovery",
            side_effect=fake_recovery,
        ):
            _run_orphan_recovery(tmp_path, recover=True, verbose=False, run_background=False)

        assert len(called_in_thread) == 1
        # Synchronous: called in the main thread (not a background thread)
        assert called_in_thread[0] == threading.current_thread().name

    def test_run_background_true_returns_immediately(self, tmp_path: pathlib.Path) -> None:
        """With run_background=True, _run_orphan_recovery returns before recovery finishes."""
        recovery_started = threading.Event()
        recovery_can_finish = threading.Event()

        def slow_recovery(repo_root, *, recover, verbose):
            recovery_started.set()
            # Block until the test allows completion
            recovery_can_finish.wait(timeout=5.0)

        with mock.patch(
            "loom_tools.orphan_recovery.run_orphan_recovery",
            side_effect=slow_recovery,
        ):
            start = time.monotonic()
            _run_orphan_recovery(tmp_path, recover=True, verbose=False, run_background=True)
            elapsed = time.monotonic() - start

        # The call should return quickly (well before recovery has a chance to finish)
        assert elapsed < 1.0, f"Expected immediate return but took {elapsed:.2f}s"

        # Recovery should have started in the background
        assert recovery_started.wait(timeout=2.0), "Background recovery never started"

        # Allow the background thread to finish cleanly
        recovery_can_finish.set()

    def test_run_background_true_runs_in_separate_thread(self, tmp_path: pathlib.Path) -> None:
        """With run_background=True, recovery runs in a thread named 'orphan-recovery'."""
        background_thread_names: list[str] = []
        recovery_done = threading.Event()

        def capture_thread(repo_root, *, recover, verbose):
            background_thread_names.append(threading.current_thread().name)
            recovery_done.set()

        with mock.patch(
            "loom_tools.orphan_recovery.run_orphan_recovery",
            side_effect=capture_thread,
        ):
            _run_orphan_recovery(tmp_path, recover=True, verbose=False, run_background=True)

        assert recovery_done.wait(timeout=3.0), "Background recovery did not complete"
        assert len(background_thread_names) == 1
        assert background_thread_names[0] == "orphan-recovery"

    def test_run_background_thread_is_daemon(self, tmp_path: pathlib.Path) -> None:
        """The background thread must be a daemon thread so it does not block process exit."""
        thread_is_daemon: list[bool] = []
        recovery_done = threading.Event()

        def capture_daemon_status(repo_root, *, recover, verbose):
            thread_is_daemon.append(threading.current_thread().daemon)
            recovery_done.set()

        with mock.patch(
            "loom_tools.orphan_recovery.run_orphan_recovery",
            side_effect=capture_daemon_status,
        ):
            _run_orphan_recovery(tmp_path, recover=True, verbose=False, run_background=True)

        assert recovery_done.wait(timeout=3.0), "Background recovery did not complete"
        assert thread_is_daemon == [True], "Background orphan recovery thread must be a daemon thread"

    def test_background_exception_does_not_propagate(self, tmp_path: pathlib.Path) -> None:
        """Exceptions in the background thread are caught and logged (not propagated)."""
        recovery_done = threading.Event()

        def failing_recovery(repo_root, *, recover, verbose):
            recovery_done.set()
            raise RuntimeError("Recovery failed!")

        with mock.patch(
            "loom_tools.orphan_recovery.run_orphan_recovery",
            side_effect=failing_recovery,
        ), mock.patch("loom_tools.daemon_cleanup.log_warning") as m_warn:
            _run_orphan_recovery(tmp_path, recover=True, verbose=False, run_background=True)

        assert recovery_done.wait(timeout=3.0), "Background thread never ran"
        # Wait a moment for the exception handler to run after recovery_done is set
        time.sleep(0.1)
        # Warning should have been logged
        assert m_warn.called, "Expected log_warning to be called for background exception"

    def test_dry_run_startup_runs_synchronously(self, tmp_path: pathlib.Path) -> None:
        """handle_daemon_startup with dry_run=True runs orphan recovery synchronously.

        Dry-run mode must be synchronous so callers can observe side effects
        without a race condition.
        """
        called_in_thread: list[str] = []

        def fake_recovery(repo_root, *, recover, verbose):
            called_in_thread.append(threading.current_thread().name)

        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()

        config = load_config()

        with mock.patch("loom_tools.daemon_cleanup._run_archive_logs"), \
             mock.patch("loom_tools.daemon_cleanup._run_loom_clean"), \
             mock.patch("loom_tools.daemon_cleanup.cleanup_stale_progress_files"), \
             mock.patch("loom_tools.orphan_recovery.run_orphan_recovery", side_effect=fake_recovery):
            handle_daemon_startup(tmp_path, config, dry_run=True)

        assert len(called_in_thread) == 1
        # Must have run in the main thread, not a background thread
        assert called_in_thread[0] == threading.current_thread().name

    def test_normal_startup_runs_recovery_in_background(self, tmp_path: pathlib.Path) -> None:
        """handle_daemon_startup without dry_run runs orphan recovery in a background thread."""
        background_thread_names: list[str] = []
        recovery_done = threading.Event()

        def fake_recovery(repo_root, *, recover, verbose):
            background_thread_names.append(threading.current_thread().name)
            recovery_done.set()

        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()

        config = load_config()

        with mock.patch("loom_tools.daemon_cleanup._run_archive_logs"), \
             mock.patch("loom_tools.daemon_cleanup._run_loom_clean"), \
             mock.patch("loom_tools.daemon_cleanup.cleanup_stale_progress_files"), \
             mock.patch("loom_tools.claim.cleanup_claims"), \
             mock.patch("loom_tools.orphan_recovery.run_orphan_recovery", side_effect=fake_recovery):
            handle_daemon_startup(tmp_path, config)

        # Wait for background recovery to complete
        assert recovery_done.wait(timeout=3.0), "Background orphan recovery never ran"

        assert len(background_thread_names) == 1
        # Must have run in the background thread, not the main thread
        assert background_thread_names[0] != threading.current_thread().name
        assert background_thread_names[0] == "orphan-recovery"

    def test_startup_proceeds_without_waiting_for_recovery(self, tmp_path: pathlib.Path) -> None:
        """handle_daemon_startup returns before background orphan recovery completes.

        This is the key correctness property: the daemon must not block on orphan
        recovery when starting up with many orphaned issues.
        """
        recovery_started = threading.Event()
        recovery_can_finish = threading.Event()
        startup_returned = threading.Event()

        def slow_recovery(repo_root, *, recover, verbose):
            recovery_started.set()
            recovery_can_finish.wait(timeout=5.0)

        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()

        config = load_config()

        def run_startup():
            with mock.patch("loom_tools.daemon_cleanup._run_archive_logs"), \
                 mock.patch("loom_tools.daemon_cleanup._run_loom_clean"), \
                 mock.patch("loom_tools.daemon_cleanup.cleanup_stale_progress_files"), \
                 mock.patch("loom_tools.claim.cleanup_claims"), \
                 mock.patch(
                     "loom_tools.orphan_recovery.run_orphan_recovery",
                     side_effect=slow_recovery,
                 ):
                handle_daemon_startup(tmp_path, config)
            startup_returned.set()

        startup_thread = threading.Thread(target=run_startup)
        startup_thread.start()

        # handle_daemon_startup should return BEFORE recovery finishes
        assert startup_returned.wait(timeout=3.0), "handle_daemon_startup blocked on recovery"

        # But recovery should still be running in the background
        assert recovery_started.is_set(), "Recovery never started in background"

        # Now let recovery finish
        recovery_can_finish.set()
        startup_thread.join(timeout=3.0)


class TestCleanupStaleSignalFiles:
    """Tests for cleanup_stale_signal_files()."""

    def _write_signal(self, signals_dir: pathlib.Path, name: str, payload: dict | None = None) -> pathlib.Path:
        signals_dir.mkdir(parents=True, exist_ok=True)
        path = signals_dir / name
        path.write_text(json.dumps(payload or {"action": "spawn_shepherd", "issue": 1}))
        return path

    def _backdate(self, path: pathlib.Path, hours: float) -> None:
        import os
        old_time = time.time() - hours * 3600
        os.utime(path, (old_time, old_time))

    def test_returns_zero_when_no_signals_dir(self, tmp_path: pathlib.Path) -> None:
        count = cleanup_stale_signal_files(tmp_path, stale_hours=1)
        assert count == 0

    def test_returns_zero_on_empty_signals_dir(self, tmp_path: pathlib.Path) -> None:
        (tmp_path / ".loom" / "signals").mkdir(parents=True)
        count = cleanup_stale_signal_files(tmp_path, stale_hours=1)
        assert count == 0

    def test_fresh_signals_not_deleted(self, tmp_path: pathlib.Path) -> None:
        signals_dir = tmp_path / ".loom" / "signals"
        sig = self._write_signal(signals_dir, "cmd-001.json")
        count = cleanup_stale_signal_files(tmp_path, stale_hours=1)
        assert count == 0
        assert sig.exists()

    def test_stale_signal_deleted(self, tmp_path: pathlib.Path) -> None:
        signals_dir = tmp_path / ".loom" / "signals"
        sig = self._write_signal(signals_dir, "cmd-001.json")
        self._backdate(sig, hours=2)  # 2h old, exceeds 1h threshold
        count = cleanup_stale_signal_files(tmp_path, stale_hours=1)
        assert count == 1
        assert not sig.exists()

    def test_multiple_stale_signals_all_deleted(self, tmp_path: pathlib.Path) -> None:
        signals_dir = tmp_path / ".loom" / "signals"
        stale = []
        for i in range(3):
            sig = self._write_signal(signals_dir, f"cmd-{i:03d}.json")
            self._backdate(sig, hours=5)
            stale.append(sig)
        count = cleanup_stale_signal_files(tmp_path, stale_hours=1)
        assert count == 3
        for sig in stale:
            assert not sig.exists()

    def test_mixed_fresh_and_stale(self, tmp_path: pathlib.Path) -> None:
        signals_dir = tmp_path / ".loom" / "signals"
        fresh = self._write_signal(signals_dir, "cmd-fresh.json")
        stale = self._write_signal(signals_dir, "cmd-stale.json")
        self._backdate(stale, hours=2)
        count = cleanup_stale_signal_files(tmp_path, stale_hours=1)
        assert count == 1
        assert fresh.exists()
        assert not stale.exists()

    def test_dry_run_does_not_delete(self, tmp_path: pathlib.Path) -> None:
        signals_dir = tmp_path / ".loom" / "signals"
        sig = self._write_signal(signals_dir, "cmd-001.json")
        self._backdate(sig, hours=5)
        count = cleanup_stale_signal_files(tmp_path, stale_hours=1, dry_run=True)
        assert count == 1
        assert sig.exists()

    def test_dry_run_returns_count_of_would_be_deleted(self, tmp_path: pathlib.Path) -> None:
        signals_dir = tmp_path / ".loom" / "signals"
        for i in range(4):
            sig = self._write_signal(signals_dir, f"cmd-{i:03d}.json")
            self._backdate(sig, hours=3)
        count = cleanup_stale_signal_files(tmp_path, stale_hours=1, dry_run=True)
        assert count == 4

    def test_only_json_files_considered(self, tmp_path: pathlib.Path) -> None:
        signals_dir = tmp_path / ".loom" / "signals"
        signals_dir.mkdir(parents=True)
        txt_file = signals_dir / "old-file.txt"
        txt_file.write_text("not a signal")
        import os
        old_time = time.time() - 7200
        os.utime(txt_file, (old_time, old_time))
        count = cleanup_stale_signal_files(tmp_path, stale_hours=1)
        assert count == 0
        assert txt_file.exists()


class TestHandleDaemonStartupCleanupSignals:
    """Tests that handle_daemon_startup invokes cleanup_stale_signal_files."""

    def test_startup_calls_cleanup_stale_signal_files(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        config = load_config()

        with mock.patch("loom_tools.daemon_cleanup._run_archive_logs"), \
             mock.patch("loom_tools.daemon_cleanup._run_loom_clean"), \
             mock.patch("loom_tools.daemon_cleanup.cleanup_stale_progress_files"), \
             mock.patch("loom_tools.daemon_cleanup.cleanup_stale_signal_files") as mock_cleanup, \
             mock.patch("loom_tools.daemon_cleanup._run_orphan_recovery"):
            handle_daemon_startup(tmp_path, config, dry_run=True)

        mock_cleanup.assert_called_once()
        call_args = mock_cleanup.call_args
        assert call_args.args[0] == tmp_path
        assert call_args.args[1] == config.signal_stale_hours

    def test_startup_dry_run_passes_dry_run_to_cleanup(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        config = load_config()

        with mock.patch("loom_tools.daemon_cleanup._run_archive_logs"), \
             mock.patch("loom_tools.daemon_cleanup._run_loom_clean"), \
             mock.patch("loom_tools.daemon_cleanup.cleanup_stale_progress_files"), \
             mock.patch("loom_tools.daemon_cleanup.cleanup_stale_signal_files") as mock_cleanup, \
             mock.patch("loom_tools.daemon_cleanup._run_orphan_recovery"):
            handle_daemon_startup(tmp_path, config, dry_run=True)

        mock_cleanup.assert_called_once()
        assert mock_cleanup.call_args.kwargs.get("dry_run") is True
