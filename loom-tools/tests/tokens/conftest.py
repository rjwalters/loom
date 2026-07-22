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

import pytest


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
    """
    monkeypatch.setenv("LOOM_ACCOUNTS_ENV", "")
    monkeypatch.setenv("LOOM_CLAUDE_MONITOR_DIR", str(tmp_path / "no-claude-monitor"))
