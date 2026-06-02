"""Soft-deprecation warning helper (issue #3376, epic #3372).

Phase 2b of the shepherd/daemon deprecation epic emits warnings — but never
errors — from entry points that are scheduled for removal in the next major
release. The goal is to give downstream consumers (notably the sphere
loom-install pipeline) a clear signal before Phase 3 deletes the code.

Usage:

    from loom_tools.common.deprecation import warn_deprecated

    warn_deprecated(
        "loom-daemon",
        replacement="./.loom/scripts/spawn-loop.sh + GitHub Actions schedules",
    )

Set ``LOOM_SUPPRESS_DEPRECATION=1`` in the environment to silence the warning
(useful for downstream installers that have not yet migrated and want a clean
signal in their logs).

This module is intentionally tiny and dependency-free so it can be imported
at the top of any entry-point script without dragging in the rest of
``loom_tools``.
"""

from __future__ import annotations

import os
import sys

__all__ = ["warn_deprecated"]


def warn_deprecated(component: str, replacement: str, ref: str = "#3372") -> None:
    """Emit a one-shot deprecation warning to stderr.

    Args:
        component: Human-readable name of the deprecated entry point
            (e.g. ``"loom-daemon"``, ``"/shepherd skill"``).
        replacement: One-line description of the replacement path the user
            should migrate to (e.g. spawn-loop + workflows).
        ref: GitHub issue or PR reference for the deprecation rationale.
            Defaults to ``"#3372"`` (the umbrella epic).

    Behaviour:
        - Writes a multi-line ``⚠️  DEPRECATED`` block to ``sys.stderr``.
        - Returns immediately and silently when ``LOOM_SUPPRESS_DEPRECATION=1``.
        - Never raises — the warning must not break the deprecated component
          during the soft-deprecation window.
    """
    if os.environ.get("LOOM_SUPPRESS_DEPRECATION") == "1":
        return

    print(
        f"⚠️  DEPRECATED: {component} is scheduled for removal in the next major release.\n"
        f"    Replacement: {replacement}\n"
        f"    See {ref}.\n"
        f"    Suppress with LOOM_SUPPRESS_DEPRECATION=1.\n",
        file=sys.stderr,
        flush=True,
    )
