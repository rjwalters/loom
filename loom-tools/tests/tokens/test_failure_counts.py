"""Tests for loom_tools.tokens.failure_counts."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

from loom_tools.tokens import failure_counts as fc


@pytest.fixture
def workspace(tmp_path: Path) -> Path:
    (tmp_path / ".loom" / "tokens").mkdir(parents=True)
    return tmp_path


def _state_file(workspace: Path) -> Path:
    return workspace / ".loom" / "tokens" / ".failure_counts"


def test_record_failure_increments(workspace: Path) -> None:
    assert fc.record_failure(workspace, "agent-1") == 1
    assert fc.record_failure(workspace, "agent-1") == 2
    assert fc.record_failure(workspace, "agent-1") == 3


def test_record_failure_caps_at_threshold(workspace: Path) -> None:
    for _ in range(7):
        fc.record_failure(workspace, "agent-1", threshold=5)
    assert fc.get_count(workspace, "agent-1") == 5


def test_record_failure_independent_per_account(workspace: Path) -> None:
    fc.record_failure(workspace, "agent-1")
    fc.record_failure(workspace, "agent-1")
    fc.record_failure(workspace, "agent-2")
    assert fc.get_count(workspace, "agent-1") == 2
    assert fc.get_count(workspace, "agent-2") == 1


def test_record_failure_writes_iso_timestamp(workspace: Path) -> None:
    fc.record_failure(workspace, "agent-1")
    raw = json.loads(_state_file(workspace).read_text())
    last = raw["agent-1"]["last_failure"]
    # YYYY-MM-DDTHH:MM:SSZ format
    assert len(last) == 20
    assert last.endswith("Z")
    assert last[4] == "-" and last[10] == "T"


def test_record_success_drops_account(workspace: Path) -> None:
    fc.record_failure(workspace, "agent-1")
    fc.record_failure(workspace, "agent-2")
    fc.record_success(workspace, "agent-1")
    assert fc.get_count(workspace, "agent-1") == 0
    assert fc.get_count(workspace, "agent-2") == 1


def test_record_success_unlinks_when_empty(workspace: Path) -> None:
    fc.record_failure(workspace, "agent-1")
    fc.record_success(workspace, "agent-1")
    assert not _state_file(workspace).exists()


def test_record_success_noop_when_absent(workspace: Path) -> None:
    fc.record_success(workspace, "agent-1")  # state file does not exist
    fc.record_failure(workspace, "agent-2")
    fc.record_success(workspace, "agent-1")  # not in state
    assert fc.get_count(workspace, "agent-2") == 1


def test_reset_all_drops_file(workspace: Path) -> None:
    fc.record_failure(workspace, "agent-1")
    fc.record_failure(workspace, "agent-2")
    fc.reset_all(workspace)
    assert not _state_file(workspace).exists()
    assert fc.get_count(workspace, "agent-1") == 0


def test_reset_all_noop_when_missing(workspace: Path) -> None:
    fc.reset_all(workspace)  # no file exists
    assert not _state_file(workspace).exists()


def test_get_count_zero_when_missing(workspace: Path) -> None:
    assert fc.get_count(workspace, "agent-1") == 0


def test_threshold_reached_below_and_at(workspace: Path) -> None:
    for _ in range(4):
        fc.record_failure(workspace, "agent-1", threshold=5)
    assert not fc.threshold_reached(workspace, "agent-1", threshold=5)
    fc.record_failure(workspace, "agent-1", threshold=5)
    assert fc.threshold_reached(workspace, "agent-1", threshold=5)


def test_threshold_reached_custom_threshold(workspace: Path) -> None:
    for _ in range(3):
        fc.record_failure(workspace, "agent-1", threshold=3)
    assert fc.threshold_reached(workspace, "agent-1", threshold=3)
    assert not fc.threshold_reached(workspace, "agent-1", threshold=5)


def test_all_counts_snapshot(workspace: Path) -> None:
    fc.record_failure(workspace, "agent-1")
    fc.record_failure(workspace, "agent-2")
    snap = fc.all_counts(workspace)
    assert set(snap.keys()) == {"agent-1", "agent-2"}
    assert snap["agent-1"]["count"] == 1
    # Mutating the snapshot does not affect the file
    snap["agent-3"] = {"count": 99, "last_failure": ""}
    fresh = fc.all_counts(workspace)
    assert "agent-3" not in fresh


def test_all_counts_empty_when_missing(workspace: Path) -> None:
    assert fc.all_counts(workspace) == {}


def test_malformed_json_treated_as_empty(workspace: Path) -> None:
    state_path = _state_file(workspace)
    state_path.write_text("not json")
    # Should not raise; should treat as empty and overwrite cleanly.
    assert fc.record_failure(workspace, "agent-1") == 1
    assert fc.get_count(workspace, "agent-1") == 1


def test_filters_negative_counts(workspace: Path) -> None:
    state_path = _state_file(workspace)
    state_path.write_text(json.dumps({"agent-1": {"count": -3, "last_failure": ""}}))
    # Negative count is invalid; treated as missing.
    assert fc.get_count(workspace, "agent-1") == 0


def test_filters_non_dict_entries(workspace: Path) -> None:
    state_path = _state_file(workspace)
    state_path.write_text(json.dumps({"agent-1": "garbage", "agent-2": {"count": 2, "last_failure": ""}}))
    assert fc.get_count(workspace, "agent-1") == 0
    assert fc.get_count(workspace, "agent-2") == 2


def test_atomic_write_no_partial_files(workspace: Path) -> None:
    tokens_dir = workspace / ".loom" / "tokens"
    fc.record_failure(workspace, "agent-1")
    fc.record_failure(workspace, "agent-2")
    leftovers = [
        p for p in tokens_dir.iterdir() if p.name.startswith(".failure_counts.") and p.suffix == ".tmp"
    ]
    assert leftovers == []


def test_concurrent_record_failure_no_loss(workspace: Path) -> None:
    """20 concurrent record_failure calls across 4 accounts → counts add up."""
    src = textwrap.dedent(
        """
        import sys
        from pathlib import Path
        sys.path.insert(0, "{src}")
        from loom_tools.tokens.failure_counts import record_failure
        record_failure(Path("{ws}"), sys.argv[1])
        """
    ).format(
        src=str(Path(__file__).parent.parent.parent / "src"),
        ws=str(workspace),
    )
    script = workspace / "_run.py"
    script.write_text(src)

    procs = []
    accounts = ["agent-1", "agent-2", "agent-3", "agent-4"]
    iterations = 5  # 5 * 4 = 20 total writes
    for _ in range(iterations):
        for name in accounts:
            procs.append(
                subprocess.Popen([sys.executable, str(script), name])
            )
    for p in procs:
        rc = p.wait()
        assert rc == 0

    # Default threshold is 5, but we capped each account at 5 increments,
    # so each account should be at exactly 5 (which matches the cap).
    # If concurrency lost any writes, we'd see < 5.
    for name in accounts:
        assert fc.get_count(workspace, name) == iterations, (
            f"{name} expected {iterations}, got {fc.get_count(workspace, name)}"
        )


def test_state_file_in_correct_location(workspace: Path) -> None:
    fc.record_failure(workspace, "agent-1")
    expected = workspace / ".loom" / "tokens" / ".failure_counts"
    assert expected.is_file()


def test_lock_file_cleanup(workspace: Path) -> None:
    fc.record_failure(workspace, "agent-1")
    lock_dir = workspace / ".loom" / "tokens" / ".failure_counts.lock"
    # _MkdirLock should have removed the lock dir on exit.
    assert not lock_dir.exists()
