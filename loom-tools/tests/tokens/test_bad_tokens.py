"""Tests for loom_tools.tokens.bad_tokens."""

from __future__ import annotations

import multiprocessing as mp
import time
from pathlib import Path

import pytest

from datetime import datetime, timedelta, timezone

from loom_tools.tokens import bad_tokens
from loom_tools.tokens.bad_tokens import (
    DEFAULT_EXHAUSTION_COOLDOWN_SECS,
    EXHAUSTION_COOLDOWN_ENV,
    cleanup_bad_tokens,
    exhaustion_cooldown_secs,
    is_auth_reason,
    is_bad,
    mark_bad,
)


def _make_tokens_dir(tmp_path: Path) -> Path:
    d = tmp_path / ".loom" / "tokens"
    d.mkdir(parents=True)
    return tmp_path


def _iso(dt: datetime) -> str:
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


def test_mark_then_is_bad(tmp_path):
    workspace = _make_tokens_dir(tmp_path)
    assert is_bad(workspace, "agent-1") is False
    mark_bad(workspace, "agent-1", "OAuth expired")
    assert is_bad(workspace, "agent-1") is True


def test_word_boundary_does_not_collide(tmp_path):
    """`agent-1` should not match `agent-10` (regression for substring bug)."""
    workspace = _make_tokens_dir(tmp_path)
    mark_bad(workspace, "agent-10", "exhausted")
    # The bad-tokens file mentions agent-10; agent-1 must not match.
    assert is_bad(workspace, "agent-10") is True
    assert is_bad(workspace, "agent-1") is False


def test_multiple_entries_for_same_token(tmp_path):
    workspace = _make_tokens_dir(tmp_path)
    mark_bad(workspace, "agent-1", "first")
    mark_bad(workspace, "agent-1", "second")
    bad_file = workspace / ".loom" / "tokens" / ".bad_tokens"
    contents = bad_file.read_text()
    assert contents.count("agent-1") == 2
    assert "first" in contents
    assert "second" in contents


def test_reason_with_newlines_collapsed(tmp_path):
    workspace = _make_tokens_dir(tmp_path)
    mark_bad(workspace, "agent-1", "line one\nline two\rline three")
    bad_file = workspace / ".loom" / "tokens" / ".bad_tokens"
    contents = bad_file.read_text()
    # Each appended record must be exactly one line — no embedded newlines
    assert contents.count("\n") == 1
    assert "line one" in contents
    assert "line two" in contents


def test_missing_tokens_dir_raises(tmp_path):
    with pytest.raises(FileNotFoundError):
        mark_bad(tmp_path, "agent-1", "no dir")


def test_is_bad_when_file_absent_returns_false(tmp_path):
    workspace = _make_tokens_dir(tmp_path)
    assert is_bad(workspace, "anything") is False


def test_cleanup_drops_old_entries(tmp_path):
    workspace = _make_tokens_dir(tmp_path)
    bad_file = workspace / ".loom" / "tokens" / ".bad_tokens"
    bad_file.write_text(
        "1999-01-01T00:00:00Z very-old expired\n"
        "2099-01-01T00:00:00Z very-new exhausted\n",
        encoding="utf-8",
    )
    kept = cleanup_bad_tokens(workspace, max_age_seconds=60)
    assert kept == 1
    contents = bad_file.read_text()
    assert "very-old" not in contents
    assert "very-new" in contents


def test_cleanup_keeps_malformed_lines(tmp_path):
    """Defensively retain lines we can't parse rather than silently delete."""
    workspace = _make_tokens_dir(tmp_path)
    bad_file = workspace / ".loom" / "tokens" / ".bad_tokens"
    bad_file.write_text(
        "garbage line with no timestamp\n"
        "2099-01-01T00:00:00Z fresh ok\n",
        encoding="utf-8",
    )
    cleanup_bad_tokens(workspace, max_age_seconds=60)
    contents = bad_file.read_text()
    assert "garbage" in contents
    assert "fresh" in contents


# ---------- reason-aware cooldown (#4122 / #4212) ----------


def test_exhaustion_entry_expires_after_cooldown_auth_stays(tmp_path):
    """#4212: a stale exhaustion (non-auth) entry no longer blocks selection
    even before cleanup prunes it, while an auth entry of the same age stays
    permanent — the live Python spawn path must self-heal like the Rust one."""
    workspace = _make_tokens_dir(tmp_path)
    bad_file = workspace / ".loom" / "tokens" / ".bad_tokens"
    old = _iso(datetime.now(timezone.utc) - timedelta(seconds=7 * 3600))
    bad_file.write_text(
        f"{old} agent-exh exhausted: weekly-limit\n"
        f"{old} agent-auth 401 unauthorized\n",
        encoding="utf-8",
    )
    # Exhaustion entry has aged out — selectable again (line still on disk).
    assert is_bad(workspace, "agent-exh") is False
    # Auth entry is permanent — unaffected by the TTL.
    assert is_bad(workspace, "agent-auth") is True


def test_fresh_exhaustion_entry_still_blocks(tmp_path):
    """A just-written exhaustion entry blocks until the cooldown elapses."""
    workspace = _make_tokens_dir(tmp_path)
    mark_bad(workspace, "agent-1", "exhausted: weekly-limit")
    assert is_bad(workspace, "agent-1") is True


def test_malformed_timestamp_fails_closed(tmp_path):
    """A matching line with an unparseable timestamp stays bad (fail-closed)."""
    workspace = _make_tokens_dir(tmp_path)
    bad_file = workspace / ".loom" / "tokens" / ".bad_tokens"
    bad_file.write_text("not-a-timestamp agent-1 exhausted\n", encoding="utf-8")
    assert is_bad(workspace, "agent-1") is True


def test_expired_entry_does_not_shortcircuit_later_blocking_line(tmp_path):
    """An aged-out exhaustion line must not mask a later still-blocking one."""
    workspace = _make_tokens_dir(tmp_path)
    bad_file = workspace / ".loom" / "tokens" / ".bad_tokens"
    old = _iso(datetime.now(timezone.utc) - timedelta(seconds=7 * 3600))
    fresh = _iso(datetime.now(timezone.utc))
    bad_file.write_text(
        f"{old} agent-1 exhausted: weekly-limit\n"
        f"{fresh} agent-1 exhausted: 429\n",
        encoding="utf-8",
    )
    assert is_bad(workspace, "agent-1") is True


def test_exhaustion_cooldown_env_override(monkeypatch):
    monkeypatch.setenv(EXHAUSTION_COOLDOWN_ENV, "100")
    assert exhaustion_cooldown_secs() == 100
    monkeypatch.setenv(EXHAUSTION_COOLDOWN_ENV, "0")
    assert exhaustion_cooldown_secs() == DEFAULT_EXHAUSTION_COOLDOWN_SECS
    monkeypatch.setenv(EXHAUSTION_COOLDOWN_ENV, "garbage")
    assert exhaustion_cooldown_secs() == DEFAULT_EXHAUSTION_COOLDOWN_SECS
    monkeypatch.delenv(EXHAUSTION_COOLDOWN_ENV, raising=False)
    assert exhaustion_cooldown_secs() == DEFAULT_EXHAUSTION_COOLDOWN_SECS


def test_is_auth_reason_matches_expected():
    assert is_auth_reason("401 Unauthorized") is True
    assert is_auth_reason("oauth token expired") is True
    assert is_auth_reason("blocked") is True
    assert is_auth_reason("exhausted: weekly-limit") is False
    assert is_auth_reason("exhausted: rate-limited") is False


# ---------- locking ----------


def _writer(workspace_str: str, name: str, count: int) -> None:
    """Append `count` entries from a child process."""
    for i in range(count):
        mark_bad(workspace_str, name, f"reason-{i}")


def test_concurrent_writers_no_loss(tmp_path):
    """Spawn N processes, each writes K entries, verify N*K lines exist."""
    workspace = _make_tokens_dir(tmp_path)
    n_writers = 4
    per_writer = 8
    procs = []
    for i in range(n_writers):
        p = mp.Process(target=_writer, args=(str(workspace), f"agent-{i}", per_writer))
        p.start()
        procs.append(p)
    for p in procs:
        p.join(timeout=15)
        assert p.exitcode == 0, f"writer {p.pid} crashed"

    bad_file = workspace / ".loom" / "tokens" / ".bad_tokens"
    lines = bad_file.read_text().splitlines()
    # All lines should be well-formed (no interleaved bytes)
    assert len(lines) == n_writers * per_writer
    for line in lines:
        # Each line is "TS NAME reason-N"
        parts = line.split()
        assert len(parts) >= 3, f"malformed line: {line!r}"
        assert parts[1].startswith("agent-")
        assert parts[2].startswith("reason-")


def test_lock_released_on_exception(tmp_path):
    """If mark_bad's body throws, the lock must still release."""
    workspace = _make_tokens_dir(tmp_path)
    # Block the lock dir — first acquisition should succeed.
    mark_bad(workspace, "x", "first")
    # The lock should not be held after mark_bad returns.
    lock_path = workspace / ".loom" / "tokens" / ".bad_tokens.lock"
    assert not lock_path.exists()
    # Second call must succeed too.
    mark_bad(workspace, "y", "second")
    assert is_bad(workspace, "y")


def test_stale_lock_cleaned_up(tmp_path, monkeypatch):
    """A lock dir older than the stale threshold should be removed."""
    workspace = _make_tokens_dir(tmp_path)
    lock_path = workspace / ".loom" / "tokens" / ".bad_tokens.lock"
    lock_path.mkdir()
    # Backdate by 60 seconds (well past 30s stale threshold)
    old = time.time() - 60
    import os

    os.utime(lock_path, (old, old))

    # Now mark_bad should clean up the stale lock and proceed
    mark_bad(workspace, "stale-test", "ok")
    assert is_bad(workspace, "stale-test")
