"""Tests for loom_tools.tokens.monitor (issue #3697).

Covers the claude-monitor ranking consumer in isolation: directory
resolution, schema validation, freshness gating, the email->name join via
index.json, ordering policy, unknown-field tolerance, and the pipe-format
writer that the selector consumes.

The claude-monitor directory is always pointed at a tmp path via
``LOOM_CLAUDE_MONITOR_DIR`` so no test ever touches a real ~/.claude-monitor.
"""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

from loom_tools.tokens.monitor import (
    MONITOR_FRESH_SECONDS,
    MonitorAccount,
    build_monitor_accounts,
    claude_monitor_dir,
    format_ranking_lines,
    load_index_email_map,
    write_monitor_ranking_atomic,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _iso(dt: datetime) -> str:
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


def _fresh_now() -> str:
    return _iso(datetime.now(timezone.utc))


def _write_index(tokens_dir: Path, accounts: list[dict]) -> None:
    tokens_dir.mkdir(parents=True, exist_ok=True)
    (tokens_dir / "index.json").write_text(
        json.dumps({"version": 2, "accounts": accounts}, indent=2),
        encoding="utf-8",
    )


def _write_ranking(monitor_dir: Path, payload: dict) -> None:
    monitor_dir.mkdir(parents=True, exist_ok=True)
    (monitor_dir / "ranking.json").write_text(
        json.dumps(payload, indent=2), encoding="utf-8"
    )


# ---------------------------------------------------------------------------
# Directory resolution
# ---------------------------------------------------------------------------


class TestClaudeMonitorDir:
    def test_env_override(self, tmp_path: Path, monkeypatch) -> None:
        monkeypatch.setenv("LOOM_CLAUDE_MONITOR_DIR", str(tmp_path / "cm"))
        assert claude_monitor_dir() == tmp_path / "cm"

    def test_default_when_unset(self, monkeypatch) -> None:
        monkeypatch.delenv("LOOM_CLAUDE_MONITOR_DIR", raising=False)
        assert claude_monitor_dir() == Path("~/.claude-monitor").expanduser()

    def test_blank_env_falls_back_to_default(self, monkeypatch) -> None:
        monkeypatch.setenv("LOOM_CLAUDE_MONITOR_DIR", "   ")
        assert claude_monitor_dir() == Path("~/.claude-monitor").expanduser()


# ---------------------------------------------------------------------------
# index.json email map
# ---------------------------------------------------------------------------


class TestLoadIndexEmailMap:
    def test_join_is_case_insensitive_on_email(self, tmp_path: Path) -> None:
        _write_index(
            tmp_path,
            [{"name": "u1", "email": "User@Example.com", "file": "u1.token"}],
        )
        assert load_index_email_map(tmp_path) == {"user@example.com": "u1"}

    def test_missing_index_is_empty(self, tmp_path: Path) -> None:
        assert load_index_email_map(tmp_path) == {}

    def test_malformed_index_is_empty(self, tmp_path: Path) -> None:
        tmp_path.mkdir(parents=True, exist_ok=True)
        (tmp_path / "index.json").write_text("{not json", encoding="utf-8")
        assert load_index_email_map(tmp_path) == {}


# ---------------------------------------------------------------------------
# build_monitor_accounts — join + ordering + freshness + schema
# ---------------------------------------------------------------------------


class TestBuildMonitorAccounts:
    def _setup(self, tmp_path: Path) -> tuple[Path, Path]:
        tokens_dir = tmp_path / ".loom" / "tokens"
        monitor_dir = tmp_path / "cm"
        _write_index(
            tokens_dir,
            [
                {"name": "acct-a", "email": "a@example.com", "file": "acct-a.token"},
                {"name": "acct-b", "email": "b@example.com", "file": "acct-b.token"},
            ],
        )
        return tokens_dir, monitor_dir

    def test_join_and_order_by_status_then_util(self, tmp_path: Path) -> None:
        tokens_dir, monitor_dir = self._setup(tmp_path)
        _write_ranking(
            monitor_dir,
            {
                "schema": 1,
                "generated_at": _fresh_now(),
                "accounts": [
                    {
                        "email": "b@example.com",
                        "status": "available",
                        "utilization": {"5h": 0.10, "7d": 0.80},
                    },
                    {
                        "email": "a@example.com",
                        "status": "available",
                        "utilization": {"5h": 0.50, "7d": 0.20},
                    },
                ],
            },
        )
        accounts = build_monitor_accounts(tokens_dir, monitor_dir=monitor_dir)
        assert accounts is not None
        # Both available -> order by util_7d ascending: a (0.20) before b (0.80).
        assert [a.name for a in accounts] == ["acct-a", "acct-b"]

    def test_status_rank_beats_utilization(self, tmp_path: Path) -> None:
        tokens_dir, monitor_dir = self._setup(tmp_path)
        _write_ranking(
            monitor_dir,
            {
                "schema": 1,
                "generated_at": _fresh_now(),
                "accounts": [
                    {
                        "email": "a@example.com",
                        "status": "exhausted",
                        "utilization": {"5h": 0.01, "7d": 0.01},
                    },
                    {
                        "email": "b@example.com",
                        "status": "available",
                        "utilization": {"5h": 0.99, "7d": 0.99},
                    },
                ],
            },
        )
        accounts = build_monitor_accounts(tokens_dir, monitor_dir=monitor_dir)
        assert accounts is not None
        # available sorts before exhausted regardless of utilization.
        assert [a.name for a in accounts] == ["acct-b", "acct-a"]

    def test_unmatched_email_dropped(self, tmp_path: Path) -> None:
        tokens_dir, monitor_dir = self._setup(tmp_path)
        _write_ranking(
            monitor_dir,
            {
                "schema": 1,
                "generated_at": _fresh_now(),
                "accounts": [
                    {
                        "email": "a@example.com",
                        "status": "available",
                        "utilization": {"5h": 0.1, "7d": 0.1},
                    },
                    {
                        "email": "ghost@example.com",  # not in index.json
                        "status": "available",
                        "utilization": {"5h": 0.1, "7d": 0.1},
                    },
                ],
            },
        )
        accounts = build_monitor_accounts(tokens_dir, monitor_dir=monitor_dir)
        assert accounts is not None
        names = {a.name for a in accounts}
        # ghost dropped; acct-b (no monitor entry) still represented.
        assert "acct-a" in names
        assert "acct-b" in names
        assert len(accounts) == 2

    def test_unmatched_manifest_account_represented_last(
        self, tmp_path: Path
    ) -> None:
        tokens_dir, monitor_dir = self._setup(tmp_path)
        _write_ranking(
            monitor_dir,
            {
                "schema": 1,
                "generated_at": _fresh_now(),
                "accounts": [
                    {
                        "email": "a@example.com",
                        "status": "available",
                        "utilization": {"5h": 0.1, "7d": 0.1},
                    }
                ],
            },
        )
        accounts = build_monitor_accounts(tokens_dir, monitor_dir=monitor_dir)
        assert accounts is not None
        # acct-b has no monitor entry -> empty status, sorts last.
        assert accounts[0].name == "acct-a"
        assert accounts[-1].name == "acct-b"
        assert accounts[-1].status == ""

    def test_stale_returns_none(self, tmp_path: Path) -> None:
        tokens_dir, monitor_dir = self._setup(tmp_path)
        old = datetime.now(timezone.utc) - timedelta(
            seconds=MONITOR_FRESH_SECONDS + 60
        )
        _write_ranking(
            monitor_dir,
            {
                "schema": 1,
                "generated_at": _iso(old),
                "accounts": [
                    {"email": "a@example.com", "status": "available"}
                ],
            },
        )
        assert build_monitor_accounts(tokens_dir, monitor_dir=monitor_dir) is None

    def test_freshness_boundary_just_inside(self, tmp_path: Path) -> None:
        tokens_dir, monitor_dir = self._setup(tmp_path)
        gen = datetime.now(timezone.utc) - timedelta(
            seconds=MONITOR_FRESH_SECONDS - 30
        )
        _write_ranking(
            monitor_dir,
            {
                "schema": 1,
                "generated_at": _iso(gen),
                "accounts": [
                    {"email": "a@example.com", "status": "available"}
                ],
            },
        )
        accounts = build_monitor_accounts(tokens_dir, monitor_dir=monitor_dir)
        assert accounts is not None

    def test_wrong_schema_returns_none(self, tmp_path: Path) -> None:
        tokens_dir, monitor_dir = self._setup(tmp_path)
        _write_ranking(
            monitor_dir,
            {
                "schema": 2,
                "generated_at": _fresh_now(),
                "accounts": [{"email": "a@example.com", "status": "available"}],
            },
        )
        assert build_monitor_accounts(tokens_dir, monitor_dir=monitor_dir) is None

    def test_missing_ranking_returns_none(self, tmp_path: Path) -> None:
        tokens_dir, monitor_dir = self._setup(tmp_path)
        # monitor_dir has no ranking.json
        assert build_monitor_accounts(tokens_dir, monitor_dir=monitor_dir) is None

    def test_malformed_ranking_returns_none(self, tmp_path: Path) -> None:
        tokens_dir, monitor_dir = self._setup(tmp_path)
        monitor_dir.mkdir(parents=True, exist_ok=True)
        (monitor_dir / "ranking.json").write_text("{oops", encoding="utf-8")
        assert build_monitor_accounts(tokens_dir, monitor_dir=monitor_dir) is None

    def test_unknown_fields_ignored(self, tmp_path: Path) -> None:
        tokens_dir, monitor_dir = self._setup(tmp_path)
        _write_ranking(
            monitor_dir,
            {
                "schema": 1,
                "generated_at": _fresh_now(),
                "future_top_level": {"anything": 1},
                "accounts": [
                    {
                        "email": "a@example.com",
                        "status": "available",
                        "utilization": {"5h": 0.1, "7d": 0.2},
                        "models": {"fable": {"utilization": 0.3}},
                        "brand_new_field": "ignore me",
                    }
                ],
            },
        )
        accounts = build_monitor_accounts(tokens_dir, monitor_dir=monitor_dir)
        assert accounts is not None
        assert accounts[0].name == "acct-a"
        assert accounts[0].util_7d == 0.2

    def test_no_index_returns_none(self, tmp_path: Path) -> None:
        # No index.json -> cannot join -> None (degrade to probe under auto).
        monitor_dir = tmp_path / "cm"
        tokens_dir = tmp_path / ".loom" / "tokens"
        tokens_dir.mkdir(parents=True)
        _write_ranking(
            monitor_dir,
            {
                "schema": 1,
                "generated_at": _fresh_now(),
                "accounts": [{"email": "a@example.com", "status": "available"}],
            },
        )
        assert build_monitor_accounts(tokens_dir, monitor_dir=monitor_dir) is None


# ---------------------------------------------------------------------------
# pipe-format writer
# ---------------------------------------------------------------------------


class TestWriter:
    def test_format_lines(self) -> None:
        accts = [
            MonitorAccount("a", "available", 0.1, 0.2),
            MonitorAccount("b", "exhausted", 0.9, 0.9),
        ]
        assert format_ranking_lines(accts) == "a|available\nb|exhausted\n"

    def test_empty_writes_empty(self) -> None:
        assert format_ranking_lines([]) == ""

    def test_atomic_write_no_partial(self, tmp_path: Path) -> None:
        target = tmp_path / ".ranking"
        write_monitor_ranking_atomic(
            [MonitorAccount("a", "available", None, None)], target
        )
        assert target.read_text() == "a|available\n"
        assert not target.with_suffix(target.suffix + ".tmp").exists()
