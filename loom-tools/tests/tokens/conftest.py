"""Shared fixtures for the token-pool tests.

The #3695 home-dir master (``~/.loom/accounts.env``, overridable via
``LOOM_ACCOUNTS_ENV``) is read by ``bootstrap_tokens`` **by default**. Without
isolation, a developer's or CI runner's real home master would leak into these
tests. The autouse fixture below disables the master for every test unless a
test opts back in (by setting ``LOOM_ACCOUNTS_ENV`` to a fixture path or
passing ``home_env_path=`` explicitly to ``bootstrap_tokens``).
"""

from __future__ import annotations

import pytest


@pytest.fixture(autouse=True)
def _isolate_home_master(monkeypatch: pytest.MonkeyPatch, tmp_path) -> None:
    """Isolate host-level state so real files never leak into tests.

    * ``LOOM_ACCOUNTS_ENV=""`` disables the #3695 home-dir account master.
    * ``LOOM_CLAUDE_MONITOR_DIR`` points the #3697 claude-monitor integration
      at a non-existent tmp path so a developer's or CI runner's real
      ``~/.claude-monitor`` is never consulted. Tests that exercise the
      integration override this with their own ``monkeypatch.setenv``.
    """
    monkeypatch.setenv("LOOM_ACCOUNTS_ENV", "")
    monkeypatch.setenv("LOOM_CLAUDE_MONITOR_DIR", str(tmp_path / "no-claude-monitor"))
