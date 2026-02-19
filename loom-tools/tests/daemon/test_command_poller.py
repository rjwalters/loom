"""Tests for loom_tools.daemon.command_poller.CommandPoller."""

from __future__ import annotations

import json
import pathlib
import tempfile
import threading
import time

import pytest

from loom_tools.daemon_v2.command_poller import CommandPoller


@pytest.fixture()
def workspace(tmp_path: pathlib.Path) -> pathlib.Path:
    """Return a temporary workspace directory."""
    return tmp_path


@pytest.fixture()
def poller(workspace: pathlib.Path) -> CommandPoller:
    """Return a CommandPoller pointed at a fresh workspace."""
    return CommandPoller(workspace)


class TestCommandPollerInit:
    def test_signals_dir_created(self, workspace: pathlib.Path) -> None:
        poller = CommandPoller(workspace)
        assert poller.signals_dir.is_dir()

    def test_signals_dir_path(self, workspace: pathlib.Path) -> None:
        poller = CommandPoller(workspace)
        assert poller.signals_dir == workspace / ".loom" / "signals"

    def test_nested_workspace_creates_dirs(self, tmp_path: pathlib.Path) -> None:
        deep = tmp_path / "a" / "b" / "c"
        poller = CommandPoller(deep)
        assert poller.signals_dir.is_dir()


class TestPollEmpty:
    def test_empty_returns_empty_list(self, poller: CommandPoller) -> None:
        assert poller.poll() == []

    def test_poll_is_idempotent_on_empty(self, poller: CommandPoller) -> None:
        for _ in range(3):
            assert poller.poll() == []


class TestPollConsumesFiles:
    def _write_signal(
        self,
        signals_dir: pathlib.Path,
        name: str,
        payload: dict,
    ) -> pathlib.Path:
        path = signals_dir / name
        path.write_text(json.dumps(payload))
        return path

    def test_single_command_returned(self, poller: CommandPoller) -> None:
        self._write_signal(
            poller.signals_dir,
            "cmd-001.json",
            {"action": "spawn_shepherd", "issue": 42},
        )
        result = poller.poll()
        assert len(result) == 1
        assert result[0]["action"] == "spawn_shepherd"
        assert result[0]["issue"] == 42

    def test_file_deleted_after_poll(self, poller: CommandPoller) -> None:
        sig = self._write_signal(
            poller.signals_dir, "cmd-001.json", {"action": "stop"}
        )
        poller.poll()
        assert not sig.exists()

    def test_second_poll_returns_empty(self, poller: CommandPoller) -> None:
        self._write_signal(
            poller.signals_dir, "cmd-001.json", {"action": "stop"}
        )
        poller.poll()
        assert poller.poll() == []

    def test_multiple_commands_sorted_order(self, poller: CommandPoller) -> None:
        self._write_signal(
            poller.signals_dir, "cmd-003.json", {"action": "c", "seq": 3}
        )
        self._write_signal(
            poller.signals_dir, "cmd-001.json", {"action": "a", "seq": 1}
        )
        self._write_signal(
            poller.signals_dir, "cmd-002.json", {"action": "b", "seq": 2}
        )
        result = poller.poll()
        seqs = [r["seq"] for r in result]
        assert seqs == [1, 2, 3], f"Expected sorted order, got {seqs}"

    def test_all_files_deleted_after_poll(self, poller: CommandPoller) -> None:
        for i in range(5):
            self._write_signal(
                poller.signals_dir,
                f"cmd-{i:03d}.json",
                {"action": "noop", "i": i},
            )
        poller.poll()
        remaining = list(poller.signals_dir.glob("*.json"))
        assert remaining == [], f"Unexpected files remain: {remaining}"


class TestPollHandlesCorruption:
    def test_corrupt_json_skipped(self, poller: CommandPoller) -> None:
        bad = poller.signals_dir / "cmd-corrupt.json"
        bad.write_text("this is not json {{{")
        result = poller.poll()
        assert result == []

    def test_corrupt_file_deleted(self, poller: CommandPoller) -> None:
        bad = poller.signals_dir / "cmd-corrupt.json"
        bad.write_text("not json")
        poller.poll()
        assert not bad.exists()

    def test_non_dict_json_skipped(self, poller: CommandPoller) -> None:
        (poller.signals_dir / "cmd-list.json").write_text("[1, 2, 3]")
        result = poller.poll()
        assert result == []

    def test_good_and_bad_mixed(self, poller: CommandPoller) -> None:
        (poller.signals_dir / "cmd-001.json").write_text(
            json.dumps({"action": "stop"})
        )
        (poller.signals_dir / "cmd-002.json").write_text("bad json {{")
        (poller.signals_dir / "cmd-003.json").write_text(
            json.dumps({"action": "noop"})
        )
        result = poller.poll()
        assert len(result) == 2
        assert result[0]["action"] == "stop"
        assert result[1]["action"] == "noop"


class TestQueueDepth:
    def _write_n(self, poller: CommandPoller, n: int) -> None:
        for i in range(n):
            (poller.signals_dir / f"cmd-{i:04d}.json").write_text(
                json.dumps({"action": "noop"})
            )

    def test_empty_queue_depth(self, poller: CommandPoller) -> None:
        assert poller.queue_depth() == 0

    def test_queue_depth_after_writes(self, poller: CommandPoller) -> None:
        self._write_n(poller, 3)
        assert poller.queue_depth() == 3

    def test_queue_depth_decreases_after_poll(self, poller: CommandPoller) -> None:
        self._write_n(poller, 4)
        poller.poll()
        assert poller.queue_depth() == 0

    def test_queue_depth_non_destructive(self, poller: CommandPoller) -> None:
        self._write_n(poller, 2)
        depth1 = poller.queue_depth()
        depth2 = poller.queue_depth()
        assert depth1 == depth2 == 2


class TestNoConcurrentDoubleConsume:
    """Verify that no command is consumed twice when two pollers race."""

    def test_concurrent_pollers_no_duplicate(
        self, workspace: pathlib.Path
    ) -> None:
        """Two concurrent pollers must together see each command exactly once."""
        poller_a = CommandPoller(workspace)
        poller_b = CommandPoller(workspace)

        # Write 10 commands
        for i in range(10):
            (poller_a.signals_dir / f"cmd-{i:04d}.json").write_text(
                json.dumps({"action": "noop", "seq": i})
            )

        results_a: list[dict] = []
        results_b: list[dict] = []

        # Poll concurrently from two threads
        def poll_a() -> None:
            # Small delay so both threads are likely to overlap
            time.sleep(0.01)
            results_a.extend(poller_a.poll())

        def poll_b() -> None:
            results_b.extend(poller_b.poll())

        t_a = threading.Thread(target=poll_a)
        t_b = threading.Thread(target=poll_b)
        t_a.start()
        t_b.start()
        t_a.join()
        t_b.join()

        all_seqs = sorted(
            [r["seq"] for r in results_a] + [r["seq"] for r in results_b]
        )
        # Each command seen exactly once; no duplicates, no missing
        assert all_seqs == list(range(10)), (
            f"Expected seqs 0..9, got {all_seqs}"
        )
