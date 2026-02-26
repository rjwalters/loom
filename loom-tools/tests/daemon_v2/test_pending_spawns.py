"""Tests for spawn_shepherd pending queue behaviour.

When all shepherd slots are full, a spawn_shepherd signal must NOT be silently
dropped.  Instead it should be enqueued in ctx.pending_spawns and retried when
a slot becomes available.
"""

from __future__ import annotations

import pathlib
from unittest import mock

import pytest

import loom_tools.daemon_v2.loop as loop_module
from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.daemon_v2.loop import (
    _find_idle_shepherd_slot,
    _process_commands,
    _retry_pending_spawns,
    _spawn_shepherd_from_signal,
)
from loom_tools.models.daemon_state import DaemonState, ShepherdEntry


# ---------------------------------------------------------------------------
# Module-level fixture: default all issues to OPEN so existing tests don't
# make real gh CLI calls.  Tests that need a closed/missing issue use their
# own @mock.patch which overrides this fixture for that specific test.
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _default_gh_issue_view_open(monkeypatch: pytest.MonkeyPatch) -> None:
    """Patch gh_issue_view to return an open issue for all tests in this module."""
    monkeypatch.setattr(loop_module, "gh_issue_view", lambda *a, **k: {"state": "OPEN"})


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_ctx(
    tmp_path: pathlib.Path,
    *,
    max_shepherds: int = 2,
) -> DaemonContext:
    """Return a DaemonContext with minimal state for unit tests."""
    config = DaemonConfig(max_shepherds=max_shepherds)
    ctx = DaemonContext(config=config, repo_root=tmp_path)
    ctx.state = DaemonState()
    return ctx


def _fill_slots(ctx: DaemonContext) -> None:
    """Mark all shepherd slots as working so no idle slot is available."""
    for i in range(ctx.config.max_shepherds):
        name = f"shepherd-{i + 1}"
        ctx.state.shepherds[name] = ShepherdEntry(status="working", issue=100 + i)


def _free_one_slot(ctx: DaemonContext) -> None:
    """Set the first working shepherd back to idle, freeing one slot."""
    for name, entry in ctx.state.shepherds.items():
        if entry.status == "working":
            entry.status = "idle"
            entry.issue = None
            return


# ---------------------------------------------------------------------------
# Tests: _spawn_shepherd_from_signal enqueues instead of dropping
# ---------------------------------------------------------------------------


class TestSpawnShepherdFromSignalQueuesWhenFull:
    """_spawn_shepherd_from_signal should enqueue pending spawns, not drop."""

    def test_no_slot_enqueues_pending(self, tmp_path: pathlib.Path) -> None:
        """When all slots are full the signal is added to pending_spawns."""
        ctx = _make_ctx(tmp_path)
        _fill_slots(ctx)
        assert len(ctx.pending_spawns) == 0

        _spawn_shepherd_from_signal(ctx, issue=42, mode="default", flags=[])

        assert len(ctx.pending_spawns) == 1
        assert ctx.pending_spawns[0] == {"issue": 42, "mode": "default", "flags": []}

    def test_no_slot_does_not_duplicate_pending(self, tmp_path: pathlib.Path) -> None:
        """Calling with the same issue multiple times should not duplicate entries."""
        ctx = _make_ctx(tmp_path)
        _fill_slots(ctx)

        _spawn_shepherd_from_signal(ctx, issue=42, mode="default", flags=[])
        _spawn_shepherd_from_signal(ctx, issue=42, mode="default", flags=[])

        assert len(ctx.pending_spawns) == 1

    def test_different_issues_both_enqueued(self, tmp_path: pathlib.Path) -> None:
        """Two different issues with no slot should both be enqueued."""
        ctx = _make_ctx(tmp_path)
        _fill_slots(ctx)

        _spawn_shepherd_from_signal(ctx, issue=10, mode="default", flags=[])
        _spawn_shepherd_from_signal(ctx, issue=20, mode="default", flags=[])

        issues = [p["issue"] for p in ctx.pending_spawns]
        assert sorted(issues) == [10, 20]

    @mock.patch("loom_tools.daemon_v2.loop.subprocess.Popen")
    def test_slot_available_does_not_enqueue(
        self, mock_popen: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        """When a slot IS available the signal is spawned, not enqueued."""
        ctx = _make_ctx(tmp_path, max_shepherds=2)
        # Only one working shepherd — one idle slot remains
        ctx.state.shepherds["shepherd-1"] = ShepherdEntry(status="working", issue=100)

        # Patch shepherd script existence check
        shepherd_script = tmp_path / ".loom" / "scripts" / "loom-shepherd.sh"
        shepherd_script.parent.mkdir(parents=True, exist_ok=True)
        shepherd_script.touch()

        proc_mock = mock.MagicMock()
        proc_mock.pid = 12345
        mock_popen.return_value = proc_mock

        _spawn_shepherd_from_signal(ctx, issue=42, mode="default", flags=[])

        # Nothing should be pending
        assert len(ctx.pending_spawns) == 0
        # Popen should have been called
        assert mock_popen.called


# ---------------------------------------------------------------------------
# Tests: _retry_pending_spawns
# ---------------------------------------------------------------------------


class TestRetryPendingSpawns:
    """_retry_pending_spawns should drain the queue when slots open up."""

    def test_empty_queue_is_noop(self, tmp_path: pathlib.Path) -> None:
        """Calling with an empty queue should not raise."""
        ctx = _make_ctx(tmp_path)
        ctx.pending_spawns = []
        _retry_pending_spawns(ctx)  # must not raise
        assert ctx.pending_spawns == []

    @mock.patch("loom_tools.daemon_v2.loop.subprocess.Popen")
    def test_retry_succeeds_when_slot_opens(
        self, mock_popen: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        """Once a slot is freed, _retry_pending_spawns spawns the pending entry."""
        ctx = _make_ctx(tmp_path, max_shepherds=1)
        _fill_slots(ctx)

        # Signal arrives when full — gets queued
        _spawn_shepherd_from_signal(ctx, issue=42, mode="default", flags=[])
        assert len(ctx.pending_spawns) == 1

        # Free the slot
        _free_one_slot(ctx)

        # Set up shepherd script
        shepherd_script = tmp_path / ".loom" / "scripts" / "loom-shepherd.sh"
        shepherd_script.parent.mkdir(parents=True, exist_ok=True)
        shepherd_script.touch()

        proc_mock = mock.MagicMock()
        proc_mock.pid = 99999
        mock_popen.return_value = proc_mock

        _retry_pending_spawns(ctx)

        # Queue should be drained
        assert ctx.pending_spawns == []
        # Spawn should have been called
        assert mock_popen.called

    def test_retry_keeps_items_when_still_full(self, tmp_path: pathlib.Path) -> None:
        """Items stay queued when all slots are still occupied after retry."""
        ctx = _make_ctx(tmp_path, max_shepherds=1)
        _fill_slots(ctx)

        # Queue a pending spawn
        ctx.pending_spawns = [{"issue": 42, "mode": "default", "flags": []}]

        # Retry with still-full slots — nothing should be spawned
        _retry_pending_spawns(ctx)

        # Item should still be in the queue
        assert len(ctx.pending_spawns) == 1
        assert ctx.pending_spawns[0]["issue"] == 42

    @mock.patch("loom_tools.daemon_v2.loop.subprocess.Popen")
    def test_retry_only_drains_as_many_as_available_slots(
        self, mock_popen: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        """Only as many pending spawns as available slots should be drained."""
        ctx = _make_ctx(tmp_path, max_shepherds=2)
        _fill_slots(ctx)

        # Queue 3 pending spawns
        ctx.pending_spawns = [
            {"issue": 10, "mode": "default", "flags": []},
            {"issue": 20, "mode": "default", "flags": []},
            {"issue": 30, "mode": "default", "flags": []},
        ]

        # Free exactly one slot
        _free_one_slot(ctx)

        shepherd_script = tmp_path / ".loom" / "scripts" / "loom-shepherd.sh"
        shepherd_script.parent.mkdir(parents=True, exist_ok=True)
        shepherd_script.touch()

        proc_mock = mock.MagicMock()
        proc_mock.pid = 11111
        mock_popen.return_value = proc_mock

        _retry_pending_spawns(ctx)

        # One spawned, two remain
        assert len(ctx.pending_spawns) == 2
        assert mock_popen.call_count == 1


# ---------------------------------------------------------------------------
# Tests: _process_commands integration — queuing path
# ---------------------------------------------------------------------------


class TestProcessCommandsQueuing:
    """_process_commands should enqueue spawn_shepherd when all slots are full."""

    def test_spawn_shepherd_queued_when_full(self, tmp_path: pathlib.Path) -> None:
        """spawn_shepherd command is queued when no idle slot exists."""
        ctx = _make_ctx(tmp_path)
        _fill_slots(ctx)

        commands = [{"action": "spawn_shepherd", "issue": 55, "mode": "default", "flags": []}]
        _process_commands(ctx, commands)

        assert any(p["issue"] == 55 for p in ctx.pending_spawns)

    def test_spawn_shepherd_without_issue_not_queued(self, tmp_path: pathlib.Path) -> None:
        """spawn_shepherd with no issue field should be skipped, not queued."""
        ctx = _make_ctx(tmp_path)
        _fill_slots(ctx)

        commands = [{"action": "spawn_shepherd", "mode": "default"}]
        _process_commands(ctx, commands)

        assert ctx.pending_spawns == []


# ---------------------------------------------------------------------------
# Tests: closed issue detection
# ---------------------------------------------------------------------------


class TestSpawnShepherdFromSignalClosedIssue:
    """_spawn_shepherd_from_signal should reject closed issues."""

    @mock.patch("loom_tools.daemon_v2.loop.gh_issue_view")
    def test_closed_issue_is_rejected(
        self, mock_gh_view: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        """A closed issue must not be spawned and must not be queued."""
        mock_gh_view.return_value = {"state": "CLOSED"}
        ctx = _make_ctx(tmp_path)
        # Add an idle slot so a slot check is not the cause of rejection
        ctx.state.shepherds["shepherd-1"] = ShepherdEntry(status="idle")

        _spawn_shepherd_from_signal(ctx, issue=42, mode="default", flags=[])

        # Nothing queued, nothing spawned
        assert ctx.pending_spawns == []

    @mock.patch("loom_tools.daemon_v2.loop.gh_issue_view")
    def test_not_found_issue_is_rejected(
        self, mock_gh_view: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        """A non-existent issue must not be spawned and must not be queued."""
        mock_gh_view.return_value = None
        ctx = _make_ctx(tmp_path)
        ctx.state.shepherds["shepherd-1"] = ShepherdEntry(status="idle")

        _spawn_shepherd_from_signal(ctx, issue=99, mode="default", flags=[])

        assert ctx.pending_spawns == []

    @mock.patch("loom_tools.daemon_v2.loop.subprocess.Popen")
    @mock.patch("loom_tools.daemon_v2.loop.gh_issue_view")
    def test_open_issue_proceeds_to_spawn(
        self,
        mock_gh_view: mock.MagicMock,
        mock_popen: mock.MagicMock,
        tmp_path: pathlib.Path,
    ) -> None:
        """An open issue with an available slot should proceed to spawn."""
        mock_gh_view.return_value = {"state": "OPEN"}
        ctx = _make_ctx(tmp_path)
        ctx.state.shepherds["shepherd-1"] = ShepherdEntry(status="idle")

        shepherd_script = tmp_path / ".loom" / "scripts" / "loom-shepherd.sh"
        shepherd_script.parent.mkdir(parents=True, exist_ok=True)
        shepherd_script.touch()

        proc_mock = mock.MagicMock()
        proc_mock.pid = 12345
        mock_popen.return_value = proc_mock

        _spawn_shepherd_from_signal(ctx, issue=42, mode="default", flags=[])

        assert mock_popen.called
        assert ctx.pending_spawns == []


# ---------------------------------------------------------------------------
# Tests: standby-mode spawn (ctx.state is None at call time)
# ---------------------------------------------------------------------------


class TestSpawnShepherdFromSignalStandbyMode:
    """_spawn_shepherd_from_signal should load state on-demand in standby mode."""

    @mock.patch("loom_tools.daemon_v2.loop.subprocess.Popen")
    @mock.patch("loom_tools.daemon_v2.loop.read_daemon_state")
    def test_spawns_when_state_none_but_file_exists(
        self,
        mock_read_state: mock.MagicMock,
        mock_popen: mock.MagicMock,
        tmp_path: pathlib.Path,
    ) -> None:
        """When ctx.state is None but daemon-state.json exists, load it and spawn."""
        # read_daemon_state returns a valid DaemonState with no shepherds
        mock_read_state.return_value = DaemonState()

        ctx = _make_ctx(tmp_path)
        ctx.state = None  # simulate standby mode

        shepherd_script = tmp_path / ".loom" / "scripts" / "loom-shepherd.sh"
        shepherd_script.parent.mkdir(parents=True, exist_ok=True)
        shepherd_script.touch()

        proc_mock = mock.MagicMock()
        proc_mock.pid = 12345
        mock_popen.return_value = proc_mock

        _spawn_shepherd_from_signal(ctx, issue=42, mode="default", flags=[])

        # State should have been populated
        assert ctx.state is not None
        # Shepherd should have been spawned, not re-queued
        assert mock_popen.called
        assert ctx.pending_spawns == []

    @mock.patch("loom_tools.daemon_v2.loop.read_daemon_state")
    def test_requeues_when_state_none_and_file_unreadable(
        self,
        mock_read_state: mock.MagicMock,
        tmp_path: pathlib.Path,
    ) -> None:
        """When ctx.state is None and reading state raises, signal is re-queued."""
        mock_read_state.side_effect = OSError("daemon-state.json not found")

        ctx = _make_ctx(tmp_path)
        ctx.state = None  # simulate standby mode

        mock_poller = mock.MagicMock()
        mock_poller.requeue.return_value = True

        _spawn_shepherd_from_signal(ctx, issue=42, mode="default", flags=[], command_poller=mock_poller)

        # Signal should be re-queued via command_poller.requeue
        assert mock_poller.requeue.call_count == 1
        requeued_cmd = mock_poller.requeue.call_args[0][0]
        assert requeued_cmd == {"action": "spawn_shepherd", "issue": 42, "mode": "default", "flags": []}
        # ctx.state should remain None
        assert ctx.state is None


# ---------------------------------------------------------------------------
# Tests: DaemonContext.pending_spawns field
# ---------------------------------------------------------------------------


class TestDaemonContextPendingSpawns:
    """DaemonContext should have a pending_spawns field that defaults to []."""

    def test_pending_spawns_default_empty(self, tmp_path: pathlib.Path) -> None:
        config = DaemonConfig()
        ctx = DaemonContext(config=config, repo_root=tmp_path)
        assert ctx.pending_spawns == []

    def test_pending_spawns_not_shared_between_instances(
        self, tmp_path: pathlib.Path
    ) -> None:
        """Each DaemonContext gets its own list (no mutable default sharing)."""
        config = DaemonConfig()
        ctx_a = DaemonContext(config=config, repo_root=tmp_path)
        ctx_b = DaemonContext(config=config, repo_root=tmp_path)
        ctx_a.pending_spawns.append({"issue": 1, "mode": "default", "flags": []})
        assert ctx_b.pending_spawns == []
