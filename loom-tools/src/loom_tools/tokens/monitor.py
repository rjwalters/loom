"""Optional, auto-detected claude-monitor integration for token selection.

A companion tool, **claude-monitor**, maintains richer per-account
utilization data (5h/7d, per-model, reset times, freshness) than Loom's own
rate-limit probe. When present, Loom can consume ``ranking.json`` to produce
its spawn-time ``.ranking`` file **without** taking a hard dependency: this
module is pure file detection under ``~/.claude-monitor/``. When the file is
absent, malformed, stale, or ``schema != 1``, callers fall back to probing so
behavior collapses to today's path byte-for-byte.

Two separate surfaces are never mixed (secrets vs usage data):

* ``~/.claude-monitor/ranking.json`` — **no secrets** (utilization only);
  consumed here for smarter selection.
* ``~/.claude-monitor/accounts.env`` — secrets (0600); consumed by the
  bootstrap account-sourcing path (separate concern).

**Format-of-record.** The spawn-time selector (``select.py:_read_ranking``)
parses pipe-delimited ``name|status`` lines. This module emits exactly that
format so a monitor-sourced ``.ranking`` is consumed by the selector
identically to any other. (The probe path's ``write_ranking_atomic`` currently
serializes JSON — a latent producer/consumer discrepancy that predates this
feature; reconciling the probe path is intentionally out of scope here.)

**Ordering policy stays Loom's.** claude-monitor is a thin numbers provider;
we sort by ``(status_rank, util_7d, util_5h)`` using the existing
``check._STATUS_RANK`` vocabulary. The email join to Loom account names goes
through the ``index.json`` manifest (#3695).

The claude-monitor directory is overridable via ``LOOM_CLAUDE_MONITOR_DIR``
so tests never touch a real ``~/.claude-monitor``.
"""

from __future__ import annotations

import json
import logging
import os
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from loom_tools.tokens.check import _STATUS_RANK

logger = logging.getLogger(__name__)

# Environment override for the claude-monitor directory (tests, custom setups).
CLAUDE_MONITOR_DIR_VAR = "LOOM_CLAUDE_MONITOR_DIR"
DEFAULT_CLAUDE_MONITOR_DIR = "~/.claude-monitor"

# Filename of the ranking contract inside the claude-monitor directory.
RANKING_JSON_NAME = "ranking.json"

# The schema version this consumer understands. Unknown fields are ignored
# (forward-compatible); an unexpected top-level schema degrades to probe.
SUPPORTED_SCHEMA = 1

# Freshness window — mirrors ``select._RANKING_FRESH_SECONDS`` (10 min). A
# ``ranking.json`` older than this is treated as stale and ignored.
MONITOR_FRESH_SECONDS = 600


def claude_monitor_dir() -> Path:
    """Resolve the claude-monitor directory.

    Precedence:
        1. ``LOOM_CLAUDE_MONITOR_DIR`` env var (``~`` expanded) — for tests
           and non-default installs.
        2. ``~/.claude-monitor`` (the default location).
    """
    override = os.environ.get(CLAUDE_MONITOR_DIR_VAR)
    if override is not None and override.strip():
        return Path(override).expanduser()
    return Path(DEFAULT_CLAUDE_MONITOR_DIR).expanduser()


def _parse_iso8601(raw: str | None) -> datetime | None:
    """Parse an ISO-8601 timestamp (accepting a trailing ``Z``) to aware UTC."""
    if not raw or not isinstance(raw, str):
        return None
    text = raw.strip()
    if not text:
        return None
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        dt = datetime.fromisoformat(text)
    except ValueError:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt.astimezone(timezone.utc)


def _is_fresh(generated_at: str | None, now: datetime | None) -> bool:
    """Return True when *generated_at* is within the freshness window.

    A missing/unparseable timestamp is treated as **stale** (fail closed):
    the whole point of the gate is to avoid trusting an old file, and we
    cannot establish freshness without a valid timestamp.
    """
    dt = _parse_iso8601(generated_at)
    if dt is None:
        return False
    now = now or datetime.now(timezone.utc)
    age = (now - dt).total_seconds()
    return 0 <= age < MONITOR_FRESH_SECONDS


def load_index_email_map(tokens_dir: Path) -> dict[str, str]:
    """Return ``{email_lower: account_name}`` from the #3695 ``index.json``.

    Missing/malformed manifest yields an empty map (the caller then finds no
    joins and degrades to probe under ``auto``).
    """
    index_path = tokens_dir / "index.json"
    try:
        raw = index_path.read_text(encoding="utf-8")
    except OSError:
        return {}
    try:
        data = json.loads(raw)
    except (ValueError, TypeError):
        logger.debug("monitor: index.json is not valid JSON: %s", index_path)
        return {}
    out: dict[str, str] = {}
    for entry in data.get("accounts", []) or []:
        if not isinstance(entry, dict):
            continue
        email = entry.get("email")
        name = entry.get("name")
        if isinstance(email, str) and isinstance(name, str) and email and name:
            out[email.strip().lower()] = name
    return out


def _load_ranking_json(monitor_dir: Path) -> dict[str, Any] | None:
    """Read + validate ``ranking.json``; return the parsed dict or None.

    Returns None (degrade to probe) when the file is absent, unreadable, not
    valid JSON, not an object, or carries an unsupported ``schema``.
    """
    ranking_path = monitor_dir / RANKING_JSON_NAME
    try:
        raw = ranking_path.read_text(encoding="utf-8")
    except OSError:
        return None
    try:
        data = json.loads(raw)
    except (ValueError, TypeError):
        logger.debug("monitor: %s is not valid JSON", ranking_path)
        return None
    if not isinstance(data, dict):
        logger.debug("monitor: %s is not a JSON object", ranking_path)
        return None
    schema = data.get("schema")
    if schema != SUPPORTED_SCHEMA:
        logger.debug(
            "monitor: %s schema=%r unsupported (want %d); ignoring",
            ranking_path,
            schema,
            SUPPORTED_SCHEMA,
        )
        return None
    return data


def _coerce_float(value: Any) -> float | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value.strip())
        except ValueError:
            return None
    return None


@dataclass(frozen=True)
class MonitorAccount:
    """One account resolved from ranking.json, joined to a Loom name."""

    name: str
    status: str
    util_7d: float | None
    util_5h: float | None


# Utilization sentinel used when a value is absent so such accounts sort after
# accounts with a known (lower) utilization within the same status bucket.
_UTIL_SENTINEL = 2.0


def _order_key(acct: MonitorAccount) -> tuple[int, float, float]:
    """Loom's ordering policy: ``(status_rank, util_7d, util_5h)``."""
    rank = _STATUS_RANK.get(acct.status, 99)
    u7 = acct.util_7d if acct.util_7d is not None else _UTIL_SENTINEL
    u5 = acct.util_5h if acct.util_5h is not None else _UTIL_SENTINEL
    return (rank, u7, u5)


def build_monitor_accounts(
    tokens_dir: Path,
    *,
    monitor_dir: Path | None = None,
    now: datetime | None = None,
) -> list[MonitorAccount] | None:
    """Translate a fresh ``ranking.json`` into ordered :class:`MonitorAccount`s.

    Returns:
        A list ordered by ``(status_rank, util_7d, util_5h)`` when a usable,
        fresh ``ranking.json`` exists; otherwise ``None`` (caller degrades to
        probe under ``auto``, or emits nothing under ``monitor``).

    Join semantics:
        * Monitor entries are joined to Loom account names **by email** via
          ``index.json``. An email with no manifest entry is dropped (debug
          log).
        * Manifest accounts with no monitor entry are still represented (empty
          status, no utilization) so the selector continues to see them; they
          sort last.
    """
    monitor_dir = monitor_dir or claude_monitor_dir()
    data = _load_ranking_json(monitor_dir)
    if data is None:
        return None
    if not _is_fresh(data.get("generated_at"), now):
        logger.debug(
            "monitor: ranking.json stale or undated (generated_at=%r); "
            "falling back",
            data.get("generated_at"),
        )
        return None

    email_to_name = load_index_email_map(tokens_dir)
    if not email_to_name:
        logger.debug("monitor: no index.json email map; cannot join")
        return None

    accounts: list[MonitorAccount] = []
    matched_names: set[str] = set()
    for entry in data.get("accounts", []) or []:
        if not isinstance(entry, dict):
            continue
        email = entry.get("email")
        if not isinstance(email, str) or not email:
            continue
        name = email_to_name.get(email.strip().lower())
        if name is None:
            logger.debug("monitor: email %r not in index.json; dropping", email)
            continue
        status = entry.get("status")
        if not isinstance(status, str):
            status = ""
        util = entry.get("utilization")
        util_7d = util_5h = None
        if isinstance(util, dict):
            util_7d = _coerce_float(util.get("7d"))
            util_5h = _coerce_float(util.get("5h"))
        accounts.append(
            MonitorAccount(
                name=name,
                status=status,
                util_7d=util_7d,
                util_5h=util_5h,
            )
        )
        matched_names.add(name)

    # Represent manifest accounts that the monitor did not mention so the
    # selector still sees them (unknown status -> eligible, sorts last).
    for name in email_to_name.values():
        if name not in matched_names:
            accounts.append(
                MonitorAccount(name=name, status="", util_7d=None, util_5h=None)
            )
            matched_names.add(name)

    if not accounts:
        return None

    accounts.sort(key=_order_key)
    return accounts


def format_ranking_lines(accounts: list[MonitorAccount]) -> str:
    """Serialize ordered accounts to the selector's ``name|status`` format.

    This is the format ``select.py:_read_ranking`` consumes. A trailing
    newline is included so the file ends cleanly.
    """
    lines = [f"{a.name}|{a.status}" for a in accounts]
    return "\n".join(lines) + ("\n" if lines else "")


def write_monitor_ranking_atomic(
    accounts: list[MonitorAccount], ranking_path: Path
) -> None:
    """Write the monitor-sourced ``.ranking`` (pipe format) atomically.

    Write-then-rename in the same directory so readers see either the old or
    the new file, never a partial. Format-of-record matches
    ``select.py:_read_ranking`` (pipe-delimited ``name|status``).
    """
    ranking_path.parent.mkdir(parents=True, exist_ok=True)
    tmp = ranking_path.with_suffix(ranking_path.suffix + ".tmp")
    tmp.write_text(format_ranking_lines(accounts), encoding="utf-8")
    tmp.replace(ranking_path)
