"""Tests for loom_tools.tokens.check (issue #3237).

Mocks Anthropic API responses with ``unittest.mock.patch`` rather than
hitting the live API. Covers:

* Header parsing (resilient suffix match, including renamed prefixes).
* Status assignment for 200/401/429/5xx and timeouts.
* Ranking sort + atomic write semantics.
* ``.bad_tokens`` skip behavior surfaces ``status: blocked``.
* OAuth header selection by token prefix.
"""

from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
import requests

from loom_tools.tokens import check as check_mod
from loom_tools.tokens.check import (
    AccountResult,
    EXHAUSTED_THRESHOLD,
    ProbeReport,
    _build_headers,
    _epoch_to_iso,
    _find_header_by_suffix,
    build_report,
    discover_tokens,
    parse_rate_limit_headers,
    probe_account,
    run_check,
    write_ranking_atomic,
)
from loom_tools.tokens.select import select_token


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _mock_response(
    status_code: int = 200,
    headers: dict[str, str] | None = None,
    text: str = "",
) -> MagicMock:
    """Build a fake ``requests.Response``."""
    resp = MagicMock(spec=requests.Response)
    resp.status_code = status_code
    resp.headers = headers or {}
    resp.text = text
    return resp


@pytest.fixture(autouse=True)
def _reset_first_run_flag():
    """Reset the module-level "have we logged headers yet" flag per-test."""
    check_mod._FIRST_RUN_HEADERS_LOGGED = False
    yield
    check_mod._FIRST_RUN_HEADERS_LOGGED = False


# ---------------------------------------------------------------------------
# Header parser
# ---------------------------------------------------------------------------


class TestHeaderParser:
    def test_suffix_match_canonical(self):
        headers = {
            "anthropic-ratelimit-tokens-5h-utilization": "0.42",
            "anthropic-ratelimit-tokens-7d-utilization": "0.10",
            "anthropic-ratelimit-tokens-7d-reset": "1762070400",
        }
        parsed = parse_rate_limit_headers(headers)
        assert parsed["5h_utilization"] == pytest.approx(0.42)
        assert parsed["7d_utilization"] == pytest.approx(0.10)
        assert parsed["7d_reset"] == "2025-11-02T08:00:00Z"

    def test_suffix_match_after_rename(self):
        # Future rename: prefix becomes ``anthropic-ratelimit-input-tokens-*``.
        # Suffix-only matching still picks it up.
        headers = {
            "anthropic-ratelimit-input-tokens-5h-utilization": "0.55",
            "anthropic-ratelimit-input-tokens-7d-utilization": "0.91",
            "anthropic-ratelimit-input-tokens-7d-reset": "1762070400",
        }
        parsed = parse_rate_limit_headers(headers)
        assert parsed["5h_utilization"] == pytest.approx(0.55)
        assert parsed["7d_utilization"] == pytest.approx(0.91)

    def test_case_insensitive(self):
        headers = {
            "Anthropic-RateLimit-Tokens-5H-Utilization": "0.30",
        }
        assert parse_rate_limit_headers(headers)["5h_utilization"] == pytest.approx(
            0.30
        )

    def test_missing_headers_return_none(self):
        parsed = parse_rate_limit_headers({"x-request-id": "abc"})
        assert parsed["5h_utilization"] is None
        assert parsed["7d_utilization"] is None
        assert parsed["7d_reset"] is None

    def test_unparseable_float(self):
        parsed = parse_rate_limit_headers(
            {"anthropic-ratelimit-tokens-5h-utilization": "not-a-number"}
        )
        assert parsed["5h_utilization"] is None

    def test_iso_reset_passthrough(self):
        # If Anthropic ever sends ISO-8601 instead of epoch, pass through.
        parsed = parse_rate_limit_headers(
            {"anthropic-ratelimit-tokens-7d-reset": "2026-05-09T00:00:00Z"}
        )
        assert parsed["7d_reset"] == "2026-05-09T00:00:00Z"

    def test_find_header_by_suffix_returns_none_when_absent(self):
        assert _find_header_by_suffix({}, "-5h-utilization") is None

    def test_epoch_zero(self):
        # Edge case: explicit "0" reset (lean-genius treats as missing)
        # Our parser converts to epoch 1970-01-01; downstream sort treats
        # this as "very old" which is fine for ranking purposes.
        result = _epoch_to_iso("0")
        assert result == "1970-01-01T00:00:00Z"


# ---------------------------------------------------------------------------
# Auth header selection
# ---------------------------------------------------------------------------


class TestAuthHeaders:
    def test_oauth_token_uses_bearer(self):
        h = _build_headers("sk-ant-oat01-abc123")
        assert h["authorization"] == "Bearer sk-ant-oat01-abc123"
        assert "x-api-key" not in h
        assert h["anthropic-beta"] == check_mod.ANTHROPIC_OAUTH_BETA

    def test_api_key_uses_x_api_key(self):
        h = _build_headers("sk-ant-api03-xyz")
        assert h["x-api-key"] == "sk-ant-api03-xyz"
        assert "authorization" not in h
        assert "anthropic-beta" not in h


# ---------------------------------------------------------------------------
# Probe — status mapping
# ---------------------------------------------------------------------------


def _good_headers(s7d: float = 0.30, s5h: float = 0.10) -> dict[str, str]:
    return {
        "anthropic-ratelimit-tokens-5h-utilization": str(s5h),
        "anthropic-ratelimit-tokens-7d-utilization": str(s7d),
        "anthropic-ratelimit-tokens-7d-reset": "1762070400",
    }


class TestProbeStatuses:
    def test_200_available(self):
        with patch("requests.post", return_value=_mock_response(200, _good_headers())):
            r = probe_account("agent-1", "sk-ant-oat01-x")
        assert r.status == "available"
        assert r.s7d_utilization == pytest.approx(0.30)
        assert r.s7d_reset == "2025-11-02T08:00:00Z"

    def test_200_exhausted_when_7d_high(self):
        with patch(
            "requests.post",
            return_value=_mock_response(200, _good_headers(s7d=0.97)),
        ):
            r = probe_account("agent-1", "sk-ant-oat01-x")
        assert r.status == "exhausted"

    def test_200_at_threshold(self):
        with patch(
            "requests.post",
            return_value=_mock_response(
                200, _good_headers(s7d=EXHAUSTED_THRESHOLD)
            ),
        ):
            r = probe_account("agent-1", "sk-ant-oat01-x")
        assert r.status == "exhausted"

    def test_401_blocked(self):
        with patch("requests.post", return_value=_mock_response(401)):
            r = probe_account("agent-1", "sk-ant-oat01-x")
        assert r.status == "blocked"
        assert r.error == "auth_401"

    def test_429_rate_limited(self):
        with patch(
            "requests.post",
            return_value=_mock_response(429, _good_headers(s7d=0.20)),
        ):
            r = probe_account("agent-1", "sk-ant-oat01-x")
        assert r.status == "rate_limited"
        # Rate-limit headers still captured even on 429.
        assert r.s7d_utilization == pytest.approx(0.20)

    def test_503_error_not_fatal(self):
        with patch("requests.post", return_value=_mock_response(503)):
            r = probe_account("agent-1", "sk-ant-oat01-x")
        assert r.status == "error"
        assert "503" in r.error

    def test_timeout_not_fatal(self):
        with patch("requests.post", side_effect=requests.Timeout()):
            r = probe_account("agent-1", "sk-ant-oat01-x")
        assert r.status == "error"
        assert r.error == "timeout"

    def test_connection_error_not_fatal(self):
        with patch(
            "requests.post", side_effect=requests.ConnectionError("dns failure")
        ):
            r = probe_account("agent-1", "sk-ant-oat01-x")
        assert r.status == "error"
        assert "connection" in r.error

    def test_empty_token_returns_blocked(self):
        # .bad_tokens-listed accounts arrive with empty token strings.
        r = probe_account("agent-bad", "")
        assert r.status == "blocked"
        assert r.error == "bad_token_listed"


# ---------------------------------------------------------------------------
# Token discovery + .bad_tokens
# ---------------------------------------------------------------------------


class TestDiscoverTokens:
    def test_lists_tokens(self, tmp_path: Path):
        (tmp_path / "agent-1.token").write_text("sk-ant-oat01-aaa\n")
        (tmp_path / "agent-2.token").write_text("sk-ant-oat01-bbb\n")
        results = discover_tokens(tmp_path)
        assert {n for n, _ in results} == {"agent-1", "agent-2"}
        assert all(t.startswith("sk-ant-oat01-") for _, t in results)

    def test_skips_bad_tokens(self, tmp_path: Path):
        (tmp_path / "agent-1.token").write_text("sk-ant-oat01-aaa\n")
        (tmp_path / "agent-2.token").write_text("sk-ant-oat01-bbb\n")
        (tmp_path / ".bad_tokens").write_text(
            "# comment line\nagent-2\n\n"  # blank line + comment ignored
        )
        results = discover_tokens(tmp_path)
        names = {n for n, _ in results}
        assert names == {"agent-1", "agent-2"}
        # bad token is surfaced with empty string -> probe returns "blocked"
        agent_2 = next(t for n, t in results if n == "agent-2")
        assert agent_2 == ""

    def test_missing_dir_returns_empty(self, tmp_path: Path):
        assert discover_tokens(tmp_path / "does-not-exist") == []

    def test_empty_token_file_skipped(self, tmp_path: Path):
        (tmp_path / "agent-empty.token").write_text("\n")
        (tmp_path / "agent-real.token").write_text("sk-ant-oat01-x\n")
        results = discover_tokens(tmp_path)
        assert {n for n, _ in results} == {"agent-real"}


# ---------------------------------------------------------------------------
# Ranking sort + atomic write
# ---------------------------------------------------------------------------


class TestRanking:
    def test_sort_available_first_then_by_reset(self):
        results = [
            AccountResult(
                "exhausted-1", "exhausted", s7d_reset="2026-05-09T00:00:00Z"
            ),
            AccountResult(
                "fresh", "available", s7d_reset="2026-05-08T00:00:00Z"
            ),
            AccountResult(
                "older", "available", s7d_reset="2026-05-05T00:00:00Z"
            ),
            AccountResult("blocked-1", "blocked"),
            AccountResult("rl-1", "rate_limited", s7d_reset="2026-05-07T00:00:00Z"),
        ]
        report = build_report(results)
        ordered = [a.name for a in report.accounts]
        assert ordered == ["older", "fresh", "rl-1", "exhausted-1", "blocked-1"]

    def test_atomic_write_creates_file(self, tmp_path: Path):
        report = ProbeReport(
            ranked_at="2026-05-03T00:00:00Z",
            accounts=[AccountResult("a-1", "available")],
        )
        target = tmp_path / "tokens" / ".ranking"
        write_ranking_atomic(report, target)
        assert target.exists()
        data = json.loads(target.read_text())
        assert data["accounts"][0]["name"] == "a-1"
        assert data["ranked_at"] == "2026-05-03T00:00:00Z"

    def test_atomic_write_no_partial_file(self, tmp_path: Path):
        # Verify that the temp file is renamed (no stray .tmp left behind on
        # success). This is the visible side-effect of using Path.replace().
        target = tmp_path / ".ranking"
        report = ProbeReport(ranked_at="2026-05-03T00:00:00Z", accounts=[])
        write_ranking_atomic(report, target)
        assert target.exists()
        assert not target.with_suffix(target.suffix + ".tmp").exists()

    def test_atomic_write_overwrites_existing(self, tmp_path: Path):
        target = tmp_path / ".ranking"
        target.write_text("old contents")
        report = ProbeReport(
            ranked_at="2026-05-03T00:00:00Z",
            accounts=[AccountResult("new", "available")],
        )
        write_ranking_atomic(report, target)
        data = json.loads(target.read_text())
        assert data["accounts"][0]["name"] == "new"


# ---------------------------------------------------------------------------
# End-to-end run_check
# ---------------------------------------------------------------------------


class TestRunCheck:
    def test_skipped_account_propagates_to_ranking(self, tmp_path: Path):
        (tmp_path / "agent-1.token").write_text("sk-ant-oat01-good")
        (tmp_path / "agent-bad.token").write_text("sk-ant-oat01-bad")
        (tmp_path / ".bad_tokens").write_text("agent-bad\n")

        with patch(
            "requests.post",
            return_value=_mock_response(200, _good_headers(s7d=0.20)),
        ):
            report = run_check(tmp_path, write_ranking=True, stagger=False)

        names_status = {a.name: a.status for a in report.accounts}
        assert names_status["agent-1"] == "available"
        assert names_status["agent-bad"] == "blocked"

        ranking = json.loads((tmp_path / ".ranking").read_text())
        assert {a["name"] for a in ranking["accounts"]} == {"agent-1", "agent-bad"}

    def test_one_failure_does_not_kill_run(self, tmp_path: Path):
        # Tokens are probed in sorted-name order (a-good, z-bad).
        (tmp_path / "a-good.token").write_text("sk-ant-oat01-good")
        (tmp_path / "z-bad.token").write_text("sk-ant-oat01-times-out")

        call_count = {"n": 0}

        def fake_post(*args, **kwargs):
            call_count["n"] += 1
            if call_count["n"] == 1:
                return _mock_response(200, _good_headers())
            raise requests.Timeout()

        with patch("requests.post", side_effect=fake_post):
            report = run_check(tmp_path, write_ranking=False, stagger=False)

        statuses = {a.name: a.status for a in report.accounts}
        assert statuses["a-good"] == "available"
        assert statuses["z-bad"] == "error"

    def test_first_run_logs_headers(self, tmp_path: Path, caplog):
        (tmp_path / "a.token").write_text("sk-ant-oat01-a")
        with patch(
            "requests.post",
            return_value=_mock_response(200, _good_headers()),
        ):
            with caplog.at_level("INFO", logger="loom_tools.tokens.check"):
                run_check(tmp_path, write_ranking=False, stagger=False)
        joined = "\n".join(rec.message for rec in caplog.records)
        assert "header set" in joined
        # Subsequent probes do not re-log
        caplog.clear()
        with patch(
            "requests.post",
            return_value=_mock_response(200, _good_headers()),
        ):
            with caplog.at_level("INFO", logger="loom_tools.tokens.check"):
                run_check(tmp_path, write_ranking=False, stagger=False)
        joined2 = "\n".join(rec.message for rec in caplog.records)
        assert "header set" not in joined2

    def test_empty_pool_returns_empty_report(self, tmp_path: Path):
        report = run_check(tmp_path, write_ranking=False, stagger=False)
        assert report.accounts == []


# ---------------------------------------------------------------------------
# --source (claude-monitor ranking consumer, #3697)
# ---------------------------------------------------------------------------


def _fresh_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _setup_pool(tmp_path: Path) -> tuple[Path, Path, Path]:
    """Create a workspace with .loom/tokens/, index.json and .token files.

    Returns (workspace, tokens_dir, monitor_dir).
    """
    workspace = tmp_path / "ws"
    tokens_dir = workspace / ".loom" / "tokens"
    tokens_dir.mkdir(parents=True)
    for name in ("acct-a", "acct-b"):
        (tokens_dir / f"{name}.token").write_text(
            f"sk-ant-oat01-{name}", encoding="utf-8"
        )
    (tokens_dir / "index.json").write_text(
        json.dumps(
            {
                "version": 2,
                "accounts": [
                    {"name": "acct-a", "email": "a@example.com", "file": "acct-a.token"},
                    {"name": "acct-b", "email": "b@example.com", "file": "acct-b.token"},
                ],
            }
        ),
        encoding="utf-8",
    )
    monitor_dir = tmp_path / "cm"
    monitor_dir.mkdir(parents=True)
    return workspace, tokens_dir, monitor_dir


def _write_monitor_ranking(monitor_dir: Path, accounts: list[dict], *, generated_at=None):
    (monitor_dir / "ranking.json").write_text(
        json.dumps(
            {
                "schema": 1,
                "generated_at": generated_at or _fresh_iso(),
                "accounts": accounts,
            }
        ),
        encoding="utf-8",
    )


class TestSourceMonitor:
    def test_probe_source_never_touches_monitor(self, tmp_path: Path, monkeypatch):
        """--source probe ignores claude-monitor and probes (unchanged path)."""
        workspace, tokens_dir, monitor_dir = _setup_pool(tmp_path)
        monkeypatch.setenv("LOOM_CLAUDE_MONITOR_DIR", str(monitor_dir))
        _write_monitor_ranking(
            monitor_dir,
            [{"email": "a@example.com", "status": "available",
              "utilization": {"5h": 0.1, "7d": 0.1}}],
        )
        with patch(
            "requests.post",
            return_value=_mock_response(200, _good_headers(s7d=0.20)),
        ) as post:
            run_check(
                tokens_dir, source="probe", write_ranking=True, stagger=False
            )
        # It probed (network call made) and wrote JSON, not pipe.
        assert post.called
        data = json.loads((tokens_dir / ".ranking").read_text())
        assert "accounts" in data

    def test_monitor_source_writes_pipe_format(self, tmp_path: Path, monkeypatch):
        workspace, tokens_dir, monitor_dir = _setup_pool(tmp_path)
        monkeypatch.setenv("LOOM_CLAUDE_MONITOR_DIR", str(monitor_dir))
        _write_monitor_ranking(
            monitor_dir,
            [
                {"email": "b@example.com", "status": "available",
                 "utilization": {"5h": 0.1, "7d": 0.9}},
                {"email": "a@example.com", "status": "available",
                 "utilization": {"5h": 0.1, "7d": 0.1}},
            ],
        )
        with patch("requests.post") as post:
            run_check(
                tokens_dir, source="monitor", write_ranking=True, stagger=False
            )
        assert not post.called  # monitor path never probes
        text = (tokens_dir / ".ranking").read_text()
        # pipe format, ordered by util_7d ascending (a before b)
        assert text == "acct-a|available\nacct-b|available\n"

    def test_monitor_source_no_data_returns_empty_no_probe(
        self, tmp_path: Path, monkeypatch
    ):
        workspace, tokens_dir, monitor_dir = _setup_pool(tmp_path)
        monkeypatch.setenv("LOOM_CLAUDE_MONITOR_DIR", str(monitor_dir))
        # No ranking.json in monitor_dir.
        with patch("requests.post") as post:
            report = run_check(
                tokens_dir, source="monitor", write_ranking=True, stagger=False
            )
        assert not post.called
        assert report.accounts == []
        # No .ranking written (nothing to rank).
        assert not (tokens_dir / ".ranking").exists()

    def test_auto_uses_monitor_when_fresh(self, tmp_path: Path, monkeypatch):
        workspace, tokens_dir, monitor_dir = _setup_pool(tmp_path)
        monkeypatch.setenv("LOOM_CLAUDE_MONITOR_DIR", str(monitor_dir))
        _write_monitor_ranking(
            monitor_dir,
            [{"email": "a@example.com", "status": "available",
              "utilization": {"5h": 0.1, "7d": 0.1}}],
        )
        with patch("requests.post") as post:
            run_check(tokens_dir, source="auto", write_ranking=True, stagger=False)
        assert not post.called
        text = (tokens_dir / ".ranking").read_text()
        assert "acct-a|available" in text

    def test_auto_falls_back_to_probe_when_absent(self, tmp_path: Path, monkeypatch):
        workspace, tokens_dir, monitor_dir = _setup_pool(tmp_path)
        # Point at an empty monitor dir (no ranking.json).
        monkeypatch.setenv("LOOM_CLAUDE_MONITOR_DIR", str(monitor_dir))
        with patch(
            "requests.post",
            return_value=_mock_response(200, _good_headers(s7d=0.20)),
        ) as post:
            run_check(tokens_dir, source="auto", write_ranking=True, stagger=False)
        assert post.called  # fell back to probing
        # Probe path writes JSON.
        data = json.loads((tokens_dir / ".ranking").read_text())
        assert "accounts" in data

    def test_roundtrip_monitor_output_read_by_selector(
        self, tmp_path: Path, monkeypatch
    ):
        """The .ranking emitted by the monitor path is consumed by select_token.

        This is the format-of-record proof: select.py:_read_ranking parses
        pipe-delimited name|status, so the monitor writer must emit that.
        """
        workspace, tokens_dir, monitor_dir = _setup_pool(tmp_path)
        monkeypatch.setenv("LOOM_CLAUDE_MONITOR_DIR", str(monitor_dir))
        # acct-b exhausted, acct-a available -> selector must pick acct-a.
        _write_monitor_ranking(
            monitor_dir,
            [
                {"email": "b@example.com", "status": "exhausted",
                 "utilization": {"5h": 0.99, "7d": 0.99}},
                {"email": "a@example.com", "status": "available",
                 "utilization": {"5h": 0.1, "7d": 0.1}},
            ],
        )
        run_check(tokens_dir, source="monitor", write_ranking=True, stagger=False)

        selected = select_token(workspace)
        assert selected.mode == "ranked"
        assert selected.name == "acct-a"
