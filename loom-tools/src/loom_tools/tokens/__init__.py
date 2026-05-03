"""Multi-account OAuth token pool management for Claude Code rotation.

This subpackage handles bootstrapping the on-disk token pool at
``.loom/tokens/`` from numbered ``ACCOUNT_*_N`` triples in ``.env``.

The token pool is consumed by external rotation logic (see
``scripts/agents/claude-wrapper.sh`` in the lean-genius reference). This
module's sole responsibility is the bootstrap step: turn ``.env`` entries
into per-account ``.token`` files (mode ``0600``) plus an ``index.json``
manifest with sha256 fingerprints (no secret material).
"""

from __future__ import annotations

from loom_tools.tokens.bootstrap import bootstrap_tokens

__all__ = ["bootstrap_tokens"]
