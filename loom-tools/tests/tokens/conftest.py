"""Shared fixtures for the token-pool tests.

The #3695 home-dir master is **opt-in only** since #3704: ``bootstrap_tokens``
reads it solely when ``LOOM_ACCOUNTS_ENV`` points at a file (there is no default
location). The autouse fixture below still pins ``LOOM_ACCOUNTS_ENV=""`` as
belt-and-suspenders so a test that ``delenv``s then re-``setenv``s the var can
never pick up a developer's or CI runner's real home file, and it isolates the
claude-monitor directory the same way. Tests opt in by setting
``LOOM_ACCOUNTS_ENV`` to a fixture path or passing ``home_env_path=`` explicitly
to ``bootstrap_tokens``.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

import pytest

# --------------------------------------------------------------------------
# claude-monitor usage.db fixtures (shared by test_monitor_db and
# test_bootstrap). A minimal stand-in for claude-monitor's SQLite store, kept
# here so both modules build fake stores through one builder rather than two
# hand-rolled copies drifting apart.
# --------------------------------------------------------------------------

_SCHEMA = """
CREATE TABLE accounts (
    id TEXT PRIMARY KEY,
    account_name TEXT,
    email TEXT,
    plan TEXT
);
CREATE TABLE oauth_credentials (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id TEXT,
    label TEXT NOT NULL,
    access_token TEXT,
    expires_at INTEGER,
    is_active INTEGER DEFAULT 1
);
"""


def make_monitor_db(path: Path, rows: list[dict]) -> Path:
    """Build a usage.db stand-in.

    Each row: ``label``, ``token``; optional ``email`` (creates a joined
    ``accounts`` row), ``is_active`` (default 1), ``expires_at``.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(path)
    conn.executescript(_SCHEMA)
    for n, row in enumerate(rows, start=1):
        account_id = None
        if row.get("email"):
            account_id = f"acct-{n}"
            conn.execute(
                "INSERT INTO accounts (id, email) VALUES (?, ?)",
                (account_id, row["email"]),
            )
        conn.execute(
            "INSERT INTO oauth_credentials "
            "(account_id, label, access_token, expires_at, is_active) "
            "VALUES (?, ?, ?, ?, ?)",
            (
                account_id,
                row["label"],
                row.get("token"),
                row.get("expires_at"),
                row.get("is_active", 1),
            ),
        )
    conn.commit()
    conn.close()
    return path


@pytest.fixture
def monitor_db(tmp_path: Path) -> Path:
    """Two active accounts, one deactivated."""
    return make_monitor_db(
        tmp_path / "monitor" / "usage.db",
        [
            {
                "label": "robb@2amlogic.com's Organization",
                "email": "robb@2amlogic.com",
                "token": "sk-ant-oat01-fresh-robb",
            },
            {
                "label": "agent-1@2amlogic.com",
                "email": "agent-1@2amlogic.com",
                "token": "sk-ant-oat01-fresh-agent1",
            },
            {
                "label": "retired@example.com",
                "email": "retired@example.com",
                "token": "sk-ant-oat01-retired",
                "is_active": 0,
            },
        ],
    )


@pytest.fixture(autouse=True)
def _isolate_home_master(monkeypatch: pytest.MonkeyPatch, tmp_path) -> None:
    """Isolate host-level state so real files never leak into tests.

    * ``LOOM_ACCOUNTS_ENV=""`` disables the #3695 home-dir account master.
      Since #3704 an unset var already means "not read" (no default location),
      so this is belt-and-suspenders — it guards tests that ``delenv`` then
      re-``setenv`` the var.
    * ``LOOM_CLAUDE_MONITOR_DIR`` points the #3697 claude-monitor integration
      at a non-existent tmp path so a developer's or CI runner's real
      ``~/.claude-monitor`` is never consulted. Tests that exercise the
      integration override this with their own ``monkeypatch.setenv``.
    * ``LOOM_SHARED_TOKENS_DIR`` points the #3938 shared-pool fallback at a
      non-existent tmp path so a developer's or CI runner's real
      ``~/.loom/tokens`` never leaks into a test that expects an empty pool.
      Tests that exercise the shared fallback override this with their own
      ``monkeypatch.setenv`` pointing at a materialized pool.
    """
    monkeypatch.setenv("LOOM_ACCOUNTS_ENV", "")
    monkeypatch.setenv("LOOM_CLAUDE_MONITOR_DIR", str(tmp_path / "no-claude-monitor"))
    monkeypatch.setenv("LOOM_SHARED_TOKENS_DIR", str(tmp_path / "no-shared-tokens"))
