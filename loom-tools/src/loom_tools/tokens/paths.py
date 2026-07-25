"""Shared resolution of the effective token-pool directory (issue #3938).

The per-repo pool at ``<repo>/.loom/tokens/`` is the primary location. In the
one-daemon-many-repos architecture (#3926/#3835), a freshly-installed consumer
repo has **no** bootstrapped pool — only the daemon's primary workspace does. So
a cross-repo sweep dispatched by the multi-workspace work finder runs
``spawn-claude.sh`` with its ``current_dir`` set to the consumer repo, resolves
that repo's empty ``.loom/tokens/``, and hard-fails with ``EX_CONFIG`` — the
symptom that motivated this module.

To fix it, token **selection** and **all pool-state bookkeeping** fall back to a
shared machine-level pool at ``~/.loom/tokens/`` (override with
``LOOM_SHARED_TOKENS_DIR``) whenever the per-repo pool is absent or empty.

Critically, *every* pool consumer resolves through :func:`resolve_tokens_dir`,
so the state files (``.bad_tokens``, ``.failure_counts``, ``.ranking``,
``.allowlist``) are always read/written in the *same* directory the ``*.token``
files were selected from — pool state is never forked per repo, and the
token-capacity backpressure accounting (#3907/#3930) sees one truth.

This module is import-safe: no I/O occurs at import time.
"""

from __future__ import annotations

import os
from pathlib import Path

# Env override for the shared machine-level pool location.
#
#   * A non-empty value names the shared pool directory (``~`` is expanded).
#   * An explicitly empty value (``LOOM_SHARED_TOKENS_DIR=``) DISABLES the
#     shared fallback entirely — selection stays per-repo only.
#   * Unset -> the default ``~/.loom/tokens``.
SHARED_TOKENS_DIR_ENV = "LOOM_SHARED_TOKENS_DIR"


def per_repo_tokens_dir(workspace: Path | str) -> Path:
    """Return the canonical per-repo pool dir ``<workspace>/.loom/tokens``."""
    return Path(workspace) / ".loom" / "tokens"


def shared_tokens_dir() -> Path | None:
    """Return the shared machine-level pool dir, or ``None`` when disabled.

    Resolution precedence (highest first):

        1. ``LOOM_SHARED_TOKENS_DIR`` env var — a non-empty path names the
           directory (``~`` expanded); an explicitly empty value disables the
           shared fallback (returns ``None``).
        2. Default: ``~/.loom/tokens``.

    Returns ``None`` only when the env var is set to an empty string (operator
    opt-out) or the home directory cannot be resolved.
    """
    env = os.environ.get(SHARED_TOKENS_DIR_ENV)
    if env is not None:
        stripped = env.strip()
        if not stripped:
            return None
        return Path(stripped).expanduser()
    try:
        return Path.home() / ".loom" / "tokens"
    except (RuntimeError, OSError):
        # Path.home() can raise when HOME is unset and the password DB lookup
        # fails — degrade to "no shared pool" rather than crash selection.
        return None


def has_token_files(tokens_dir: Path) -> bool:
    """Return True iff ``tokens_dir`` exists and holds at least one ``*.token``."""
    if not tokens_dir.is_dir():
        return False
    try:
        return any(tokens_dir.glob("*.token"))
    except OSError:
        return False


def resolve_tokens_dir(workspace: Path | str) -> Path:
    """Resolve the effective pool dir for ``workspace`` (issue #3938).

    Returns:
        * the **per-repo** pool (``<workspace>/.loom/tokens``) when it holds
          ``*.token`` files (the common, unchanged case);
        * otherwise the **shared** machine-level pool when *it* holds token
          files (the cross-repo fallback);
        * otherwise the **per-repo** path unchanged — so callers surface a
          sensible "run ``loom-tokens bootstrap``" error against the repo they
          were invoked from, exactly as before this module existed.

    Because both selection and every state-file writer resolve through this one
    function, the ``.bad_tokens`` / ``.failure_counts`` / ``.ranking`` /
    ``.allowlist`` files always land beside the selected ``*.token`` files —
    pool state is never split across the per-repo and shared directories.
    """
    repo_dir = per_repo_tokens_dir(workspace)
    if has_token_files(repo_dir):
        return repo_dir
    shared = shared_tokens_dir()
    if shared is not None and has_token_files(shared):
        return shared
    return repo_dir
