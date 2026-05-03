"""Multi-account OAuth token pool for Claude Code rotation.

This subpackage handles two responsibilities:

1. **Bootstrap** (``loom_tools.tokens.bootstrap``) — turn numbered
   ``ACCOUNT_*_N`` triples in ``.env`` into per-account ``.token`` files
   under ``.loom/tokens/`` (mode 0600), plus an ``index.json`` manifest
   with sha256 fingerprints (no secret material).

2. **Selection / bad-token tracking** (``loom_tools.tokens.select`` and
   ``loom_tools.tokens.bad_tokens``) — runtime token rotation. The
   selection algorithm picks a token using a 3-tier strategy
   (ranking -> allowlist -> random), skipping tokens marked bad in
   ``.bad_tokens``. Bad-token writes use ``mkdir``-based atomic locking
   (POSIX, macOS-compatible — ``flock`` is not used).

This module is import-safe: no I/O at import time. The daemon, shell
wrappers (``defaults/scripts/spawn-claude.sh``), and tests all import it
from concurrent processes.

Public API:
    bootstrap_tokens(...)              -- materialize .env -> .token files
    select_token(workspace_path) -> SelectedToken
    mark_bad(workspace_path, name, reason)
    is_bad(workspace_path, name) -> bool
    cleanup_bad_tokens(workspace_path, max_age_seconds)
"""

from __future__ import annotations

from loom_tools.tokens.bad_tokens import cleanup_bad_tokens, is_bad, mark_bad
from loom_tools.tokens.bootstrap import bootstrap_tokens
from loom_tools.tokens.select import (
    EmptyTokenPoolError,
    SelectedToken,
    TokenSelectionError,
    select_token,
)

__all__ = [
    "EmptyTokenPoolError",
    "SelectedToken",
    "TokenSelectionError",
    "bootstrap_tokens",
    "cleanup_bad_tokens",
    "is_bad",
    "mark_bad",
    "select_token",
]
