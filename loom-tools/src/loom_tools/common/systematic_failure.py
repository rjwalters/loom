"""Deprecated systematic failure module — Phase 3.2 stub.

The Python daemon brain (``daemon_v2/``) was deleted in Phase 3.2 (#3399).
This module is kept as a minimal stub so that ``shepherd/cli.py`` (which is
deleted in Phase 3.3, #3400) continues to import without error during the
Phase 3.2 → Phase 3.3 window.

All functions are no-ops that safely return their "nothing happened" defaults.
Phase 3.3 deletes the shepherd brain and this stub along with it.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any


def record_blocked_reason(
    repo_root: Path,
    issue: int,
    *,
    error_class: str = "unknown",
    phase: str = "",
    details: str = "",
    force_mode: bool = False,
) -> None:
    """Stub: no-op (daemon-state.json producer is deleted)."""
    return


def detect_systematic_failure(
    repo_root: Path,
    *,
    update: bool = True,
) -> None:
    """Stub: always returns None (no systematic failure state)."""
    return None


def clear_systematic_failure(repo_root: Path) -> None:
    """Stub: no-op."""
    return


def clear_failures_for_issue(repo_root: Path, issue: int) -> int:
    """Stub: always returns 0 (no failures cleared)."""
    return 0


def probe_started(repo_root: Path) -> int:
    """Stub: always returns 0."""
    return 0
