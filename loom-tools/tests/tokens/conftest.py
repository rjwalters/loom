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
def _isolate_home_master(monkeypatch: pytest.MonkeyPatch) -> None:
    """Disable the home-dir account master by default (LOOM_ACCOUNTS_ENV="")."""
    monkeypatch.setenv("LOOM_ACCOUNTS_ENV", "")
