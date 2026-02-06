"""Tests for daemon_cleanup session termination and label revert during shutdown."""

from __future__ import annotations

import json
import pathlib
from unittest import mock

from loom_tools.daemon_cleanup import (
    _revert_shepherd_labels,
    _terminate_active_sessions,
    handle_daemon_shutdown,
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
