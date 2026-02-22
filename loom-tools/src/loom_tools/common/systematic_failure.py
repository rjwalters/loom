"""Native Python replacements for detect-systematic-failure.sh and record-blocked-reason.sh.

Provides functions to record blocked issue metadata and detect systematic
failure patterns in ``daemon-state.json``, eliminating the need for shell
subprocess calls from shepherd phase modules.
"""

from __future__ import annotations

import logging
from datetime import datetime, timedelta, timezone
from pathlib import Path

from loom_tools.common.config import env_int
from loom_tools.common.issue_failures import (
    INFRASTRUCTURE_ERROR_CLASSES,
    record_failure as _record_persistent_failure,
)
from loom_tools.common.paths import LoomPaths
from loom_tools.common.state import read_json_file, write_json_file
from loom_tools.models.daemon_state import SystematicFailure

logger = logging.getLogger(__name__)

# Maximum number of recent failures kept in the sliding window
_MAX_RECENT_FAILURES = 20


def _now_iso() -> str:
    """Return current UTC time as ISO 8601 string."""
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _get_threshold() -> int:
    return env_int("LOOM_SYSTEMATIC_FAILURE_THRESHOLD", default=3)


def _get_cooldown() -> int:
    return env_int("LOOM_SYSTEMATIC_FAILURE_COOLDOWN", default=1800)


def record_blocked_reason(
    repo_root: Path,
    issue: int,
    *,
    error_class: str = "unknown",
    phase: str = "",
    details: str = "",
    force_mode: bool = False,
) -> None:
    """Record structured failure metadata when an issue becomes blocked.

    Updates ``blocked_issue_retries`` and appends to ``recent_failures``
    in ``daemon-state.json``.  This is the Python equivalent of
    ``record-blocked-reason.sh``.

    When ``force_mode=True``, the failure is tagged so that systematic
    failure detection ignores it.  Force-mode runs represent deliberate
    user retries of known-failing issues; they should not count against
    the systematic failure budget.  See issue #2897.

    Args:
        repo_root: Repository root path.
        issue: Issue number being blocked.
        error_class: Classification of the error.
        phase: Shepherd phase where the error occurred.
        details: Human-readable description of the failure.
        force_mode: If True, tag the failure so it is excluded from
            systematic failure detection.
    """
    paths = LoomPaths(repo_root)
    state_file = paths.daemon_state_file

    if not state_file.is_file():
        return

    data = read_json_file(state_file)
    if not isinstance(data, dict):
        return

    now = _now_iso()
    issue_key = str(issue)

    # Update or create blocked_issue_retries entry
    retries = data.setdefault("blocked_issue_retries", {})
    existing = retries.get(issue_key, {})
    existing.setdefault("retry_count", 0)
    existing.setdefault("last_retry_at", None)
    existing.setdefault("retry_exhausted", False)
    existing["error_class"] = error_class
    existing["last_blocked_at"] = now
    existing["last_blocked_phase"] = phase
    existing["last_blocked_details"] = details
    retries[issue_key] = existing

    # Append to recent_failures sliding window.
    # Force-mode failures are tagged so detect_systematic_failure can
    # exclude them from the consecutive-failure count.
    failures: list[dict] = data.setdefault("recent_failures", [])
    entry: dict = {
        "issue": issue,
        "error_class": error_class,
        "phase": phase,
        "timestamp": now,
    }
    if force_mode:
        entry["force_mode"] = True
    failures.append(entry)
    # Keep only last N failures
    data["recent_failures"] = failures[-_MAX_RECENT_FAILURES:]

    write_json_file(state_file, data)

    # Also write to persistent cross-session failure log
    try:
        _record_persistent_failure(
            repo_root,
            issue,
            error_class=error_class,
            phase=phase,
            details=details,
        )
    except Exception:
        logger.warning("Failed to write to persistent failure log for issue #%d", issue)

    if force_mode:
        logger.info(
            "Recorded force-mode failure for issue #%d (class=%s) — "
            "excluded from systematic failure detection",
            issue,
            error_class,
        )


def detect_systematic_failure(
    repo_root: Path,
    *,
    update: bool = True,
) -> SystematicFailure | None:
    """Analyse recent failures for systematic patterns and optionally update state.

    Checks whether the last *threshold* failures share the same
    ``error_class``.  When detected and *update* is ``True``, writes the
    ``systematic_failure`` field to ``daemon-state.json``.

    This is the Python equivalent of ``detect-systematic-failure.sh --update``.

    Args:
        repo_root: Repository root path.
        update: If ``True``, update the daemon state file.

    Returns:
        A :class:`SystematicFailure` if a pattern is detected, ``None``
        otherwise.
    """
    paths = LoomPaths(repo_root)
    state_file = paths.daemon_state_file

    if not state_file.is_file():
        return None

    data = read_json_file(state_file)
    if not isinstance(data, dict):
        return None

    threshold = _get_threshold()
    cooldown = _get_cooldown()

    failures_raw: list[dict] = data.get("recent_failures", [])

    # Filter out infrastructure failures — they indicate environment issues
    # (MCP server down, auth timeout), not issue-specific problems, and should
    # not trigger systematic failure escalation.  See issue #2772.
    #
    # Also filter out force-mode failures — these represent deliberate user
    # retries of known-failing issues and should not count against the
    # systematic failure budget.  See issue #2897.
    non_infra = [
        f for f in failures_raw
        if f.get("error_class", "unknown") not in INFRASTRUCTURE_ERROR_CLASSES
        and not f.get("force_mode", False)
    ]

    if len(non_infra) < threshold:
        if update:
            data["systematic_failure"] = {}
            write_json_file(state_file, data)
        return None

    # Check the last N non-infrastructure failures for the same error class
    last_n = non_infra[-threshold:]
    classes = {f.get("error_class", "unknown") for f in last_n}

    if len(classes) != 1:
        if update:
            data["systematic_failure"] = {}
            write_json_file(state_file, data)
        return None

    # Systematic failure detected
    pattern = classes.pop()
    now = _now_iso()
    now_dt = datetime.now(timezone.utc)
    cooldown_until_dt = now_dt + timedelta(seconds=cooldown)
    cooldown_until = cooldown_until_dt.strftime("%Y-%m-%dT%H:%M:%SZ")

    sf = SystematicFailure(
        active=True,
        pattern=pattern,
        count=threshold,
        detected_at=now,
        cooldown_until=cooldown_until,
        probe_count=0,
    )

    if update:
        data["systematic_failure"] = sf.to_dict()
        write_json_file(state_file, data)

    logger.warning(
        "Systematic failure detected: pattern=%s, threshold=%d",
        pattern,
        threshold,
    )
    return sf


def clear_systematic_failure(repo_root: Path) -> None:
    """Clear systematic failure state and recent failures.

    Equivalent to ``detect-systematic-failure.sh --clear`` and
    ``--probe-success``.

    Args:
        repo_root: Repository root path.
    """
    paths = LoomPaths(repo_root)
    state_file = paths.daemon_state_file

    if not state_file.is_file():
        return

    data = read_json_file(state_file)
    if not isinstance(data, dict):
        return

    data["systematic_failure"] = {}
    data["recent_failures"] = []
    write_json_file(state_file, data)


def clear_failures_for_issue(repo_root: Path, issue: int) -> int:
    """Clear failure history for a specific issue from daemon state.

    Removes entries for *issue* from ``recent_failures`` and resets the
    ``blocked_issue_retries`` entry for this issue.  After clearing, the
    ``systematic_failure`` field is re-evaluated: if the cleared entries were
    the ones triggering systematic failure detection, the systematic failure
    state is cleared too.

    This is called at shepherd startup in force mode so that a retried issue
    gets a full failure window instead of inheriting stale failures from
    prior runs.

    Args:
        repo_root: Repository root path.
        issue: Issue number whose failures should be cleared.

    Returns:
        The number of failure entries removed from ``recent_failures``.
    """
    paths = LoomPaths(Path(repo_root))
    state_file = paths.daemon_state_file

    if not state_file.is_file():
        return 0

    data = read_json_file(state_file)
    if not isinstance(data, dict):
        return 0

    # Filter recent_failures to remove entries for this issue
    failures: list[dict] = data.get("recent_failures", [])
    original_count = len(failures)
    data["recent_failures"] = [f for f in failures if f.get("issue") != issue]
    cleared_count = original_count - len(data["recent_failures"])

    # Reset blocked_issue_retries for this issue
    retries: dict = data.get("blocked_issue_retries", {})
    issue_key = str(issue)
    if issue_key in retries:
        retries[issue_key] = {
            "retry_count": 0,
            "retry_exhausted": False,
            "last_retry_at": None,
        }

    # Re-evaluate systematic_failure state after clearing
    # Use the same detection logic but inline to avoid a second file read/write
    threshold = _get_threshold()
    remaining = data["recent_failures"]
    non_infra = [
        f for f in remaining
        if f.get("error_class", "unknown") not in INFRASTRUCTURE_ERROR_CLASSES
        and not f.get("force_mode", False)
    ]

    if len(non_infra) < threshold:
        data["systematic_failure"] = {}
    else:
        last_n = non_infra[-threshold:]
        classes = {f.get("error_class", "unknown") for f in last_n}
        if len(classes) != 1:
            data["systematic_failure"] = {}
        # else: systematic failure still valid from remaining failures — leave it

    write_json_file(state_file, data)

    if cleared_count > 0:
        logger.info(
            "Cleared %d failure entries for issue #%d from recent_failures",
            cleared_count,
            issue,
        )

    return cleared_count


def probe_started(repo_root: Path) -> int:
    """Increment probe count and extend cooldown with exponential backoff.

    Equivalent to ``detect-systematic-failure.sh --probe-started``.

    Args:
        repo_root: Repository root path.

    Returns:
        The new probe count.
    """
    paths = LoomPaths(repo_root)
    state_file = paths.daemon_state_file

    if not state_file.is_file():
        return 0

    data = read_json_file(state_file)
    if not isinstance(data, dict):
        return 0

    cooldown_base = _get_cooldown()

    sf_raw: dict = data.get("systematic_failure", {})
    current_count = sf_raw.get("probe_count", 0)
    new_count = current_count + 1

    # Exponential backoff: base * 2^probe_count
    effective_cooldown = cooldown_base * (1 << new_count)

    now_dt = datetime.now(timezone.utc)
    cooldown_until_dt = now_dt + timedelta(seconds=effective_cooldown)
    cooldown_until = cooldown_until_dt.strftime("%Y-%m-%dT%H:%M:%SZ")

    sf_raw["probe_count"] = new_count
    sf_raw["cooldown_until"] = cooldown_until
    data["systematic_failure"] = sf_raw
    write_json_file(state_file, data)

    return new_count


