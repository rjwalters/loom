"""Tests for loom_tools.tokens.allowlist."""

from __future__ import annotations

import multiprocessing as mp
from pathlib import Path

import pytest

from loom_tools.tokens import allowlist
from loom_tools.tokens.allowlist import (
    UnknownAccountError,
    add_to_allowlist,
    clear_allowlist,
    list_accounts,
    read_allowlist,
    remove_from_allowlist,
    write_allowlist,
)


def _make_workspace(tmp_path: Path, names: list[str]) -> Path:
    d = tmp_path / ".loom" / "tokens"
    d.mkdir(parents=True)
    for name in names:
        (d / f"{name}.token").write_text("dummy", encoding="utf-8")
    return tmp_path


def test_list_accounts_empty(tmp_path):
    ws = _make_workspace(tmp_path, [])
    assert list_accounts(ws) == []


def test_list_accounts_sorted(tmp_path):
    ws = _make_workspace(tmp_path, ["alpha", "agent-1", "agent-10", "agent-2"])
    assert list_accounts(ws) == ["agent-1", "agent-10", "agent-2", "alpha"]


def test_list_accounts_no_dir(tmp_path):
    # Nothing in tmp_path — no .loom/tokens/ dir.
    assert list_accounts(tmp_path) == []


def test_read_allowlist_missing_returns_empty(tmp_path):
    ws = _make_workspace(tmp_path, ["agent-1"])
    assert read_allowlist(ws) == []


def test_write_allowlist_exact_match(tmp_path):
    ws = _make_workspace(tmp_path, ["agent-1", "agent-2", "agent-3"])
    written = write_allowlist(ws, ["agent-2", "agent-1"])
    assert written == ["agent-2", "agent-1"]
    assert read_allowlist(ws) == ["agent-2", "agent-1"]


def test_write_rejects_unknown_account(tmp_path):
    ws = _make_workspace(tmp_path, ["agent-1"])
    with pytest.raises(UnknownAccountError) as exc:
        write_allowlist(ws, ["agent-1", "ghost"])
    assert "ghost" in str(exc.value)


def test_write_rejects_substring_match(tmp_path):
    """EXACT match required — partial 'agent' must not resolve to agent-1."""
    ws = _make_workspace(tmp_path, ["agent-1", "agent-2"])
    with pytest.raises(UnknownAccountError):
        write_allowlist(ws, ["agent"])


def test_write_dedupes(tmp_path):
    ws = _make_workspace(tmp_path, ["agent-1", "agent-2"])
    written = write_allowlist(ws, ["agent-1", "agent-1", "agent-2"])
    assert written == ["agent-1", "agent-2"]


def test_write_empty_clears_file(tmp_path):
    ws = _make_workspace(tmp_path, ["agent-1"])
    write_allowlist(ws, ["agent-1"])
    assert (ws / ".loom" / "tokens" / ".allowlist").is_file()
    write_allowlist(ws, [])
    assert not (ws / ".loom" / "tokens" / ".allowlist").exists()


def test_add_new_and_existing(tmp_path):
    ws = _make_workspace(tmp_path, ["a", "b", "c"])
    write_allowlist(ws, ["a"])
    added, skipped = add_to_allowlist(ws, ["a", "b"])
    assert added == ["b"]
    assert skipped == ["a"]
    assert read_allowlist(ws) == ["a", "b"]


def test_add_unknown_raises(tmp_path):
    ws = _make_workspace(tmp_path, ["a"])
    with pytest.raises(UnknownAccountError):
        add_to_allowlist(ws, ["ghost"])


def test_remove_some(tmp_path):
    ws = _make_workspace(tmp_path, ["a", "b", "c"])
    write_allowlist(ws, ["a", "b", "c"])
    removed, skipped = remove_from_allowlist(ws, ["b"])
    assert removed == ["b"]
    assert skipped == []
    assert read_allowlist(ws) == ["a", "c"]


def test_remove_last_drops_file(tmp_path):
    ws = _make_workspace(tmp_path, ["a"])
    write_allowlist(ws, ["a"])
    remove_from_allowlist(ws, ["a"])
    assert not (ws / ".loom" / "tokens" / ".allowlist").exists()


def test_remove_unknown_raises(tmp_path):
    ws = _make_workspace(tmp_path, ["a"])
    write_allowlist(ws, ["a"])
    with pytest.raises(UnknownAccountError):
        remove_from_allowlist(ws, ["ghost"])


def test_remove_known_but_not_in_allowlist(tmp_path):
    ws = _make_workspace(tmp_path, ["a", "b"])
    write_allowlist(ws, ["a"])
    removed, skipped = remove_from_allowlist(ws, ["b"])
    # b is a real account, but wasn't in the allowlist
    assert removed == []
    assert skipped == ["b"]


def test_clear_allowlist_returns_status(tmp_path):
    ws = _make_workspace(tmp_path, ["a"])
    assert clear_allowlist(ws) is False  # no file
    write_allowlist(ws, ["a"])
    assert clear_allowlist(ws) is True
    assert clear_allowlist(ws) is False  # already gone


def test_read_handles_comments_and_blanks(tmp_path):
    ws = _make_workspace(tmp_path, ["a", "b"])
    f = ws / ".loom" / "tokens" / ".allowlist"
    f.write_text("# header\n\na\n  b  # inline\n# trailing\n", encoding="utf-8")
    assert read_allowlist(ws) == ["a", "b"]


def test_read_dedupes(tmp_path):
    ws = _make_workspace(tmp_path, ["a", "b"])
    f = ws / ".loom" / "tokens" / ".allowlist"
    f.write_text("a\nb\na\n", encoding="utf-8")
    assert read_allowlist(ws) == ["a", "b"]


def test_missing_tokens_dir_raises(tmp_path):
    with pytest.raises(FileNotFoundError):
        write_allowlist(tmp_path, ["a"])


# ---------- concurrent locking ----------


def _add_worker(workspace_str: str, names: list[str]) -> None:
    add_to_allowlist(workspace_str, names)


def test_concurrent_add_no_loss(tmp_path):
    """N processes adding distinct names should produce union, no drops."""
    accounts = [f"a{i}" for i in range(10)]
    ws = _make_workspace(tmp_path, accounts)
    procs = []
    for name in accounts:
        p = mp.Process(target=_add_worker, args=(str(ws), [name]))
        p.start()
        procs.append(p)
    for p in procs:
        p.join(timeout=15)
        assert p.exitcode == 0

    final = set(read_allowlist(ws))
    assert final == set(accounts)


def test_lock_released_after_write(tmp_path):
    ws = _make_workspace(tmp_path, ["a"])
    write_allowlist(ws, ["a"])
    lock = ws / ".loom" / "tokens" / ".allowlist.lock"
    assert not lock.exists()


def test_atomic_write_no_partial_files(tmp_path):
    """No leftover ``.tmp`` files after a successful write."""
    ws = _make_workspace(tmp_path, ["a", "b"])
    write_allowlist(ws, ["a", "b"])
    tokens_dir = ws / ".loom" / "tokens"
    leftovers = list(tokens_dir.glob(".allowlist.*.tmp"))
    assert leftovers == []


def test_module_exports():
    # Ensure the public API stays in __all__-style use.
    assert hasattr(allowlist, "write_allowlist")
    assert hasattr(allowlist, "read_allowlist")
    assert hasattr(allowlist, "list_accounts")
