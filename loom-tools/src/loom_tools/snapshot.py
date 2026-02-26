"""Consolidated daemon state snapshot.

Replaces the former ``daemon-snapshot.sh`` (removed in #1745) with
typed Python objects and ``concurrent.futures`` for parallel GitHub API
queries.

Usage::

    python -m loom_tools.snapshot              # compact JSON
    python -m loom_tools.snapshot --pretty     # indented JSON
    python -m loom_tools.snapshot --help       # show help

The output JSON is schema-compatible with the shell version consumed by
``loom-iteration.md`` and ``health-check.sh``.
"""

from __future__ import annotations

import json
import os
import pathlib
import re
import subprocess
import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any, Sequence

import shutil

from loom_tools.common.github import gh_parallel_queries, gh_get_default_branch_ci_status
from loom_tools.common.issue_failures import load_failure_log, IssueFailureLog
from loom_tools.common.logging import log_info, log_warning
from loom_tools.common.paths import LoomPaths
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import (
    parse_command_output,
    read_daemon_state,
    read_progress_files,
)
from loom_tools.common.time_utils import now_utc, parse_iso_timestamp
from loom_tools.models.daemon_state import DaemonState, SupportRoleEntry
from loom_tools.shepherd.labels import LABEL_EXCLUSION_GROUPS

# ---------------------------------------------------------------------------
# Tiered retry policy by error class
# ---------------------------------------------------------------------------

@dataclass
class RetryPolicy:
    """Per-error-class retry configuration."""

    # Fixed cooldown in seconds between retry attempts (no exponential backoff)
    cooldown: int
    # Maximum number of retry attempts before escalating
    max_retries: int
    # Whether to add to needs_human_input when retry budget is exhausted
    escalate: bool


# Tiered retry policies per error class.
# Transient errors: short cooldown, more retries, no escalation.
# Structural errors: longer cooldown, fewer retries, then escalate.
_ERROR_CLASS_POLICIES: dict[str, RetryPolicy] = {
    # Transient: short cooldown, auto-retry (no human escalation)
    "mcp_infrastructure_failure": RetryPolicy(cooldown=1800, max_retries=5, escalate=False),
    "shepherd_failure": RetryPolicy(cooldown=1800, max_retries=5, escalate=False),
    # Medium: 2h cooldown, max 3 retries, then escalate
    "builder_unknown_failure": RetryPolicy(cooldown=7200, max_retries=3, escalate=True),
    "builder_no_pr": RetryPolicy(cooldown=7200, max_retries=3, escalate=True),
    # Structural: 6h cooldown, max 2 retries, then escalate
    "builder_test_failure": RetryPolicy(cooldown=21600, max_retries=2, escalate=True),
    "judge_exhausted": RetryPolicy(cooldown=21600, max_retries=2, escalate=True),
    # Doctor failures: immediate human escalation, no auto-retry
    "doctor_exhausted": RetryPolicy(cooldown=0, max_retries=0, escalate=True),
    "doctor_no_progress": RetryPolicy(cooldown=0, max_retries=0, escalate=True),
}


def get_retry_policy(error_class: str, cfg: "SnapshotConfig | None" = None) -> RetryPolicy:
    """Return the retry policy for *error_class*.

    Known error classes use fixed per-class policies.
    Unknown classes fall back to the global config defaults (with exponential
    backoff driven by ``cfg``), or a built-in safe default when cfg is None.
    """
    if error_class in _ERROR_CLASS_POLICIES:
        return _ERROR_CLASS_POLICIES[error_class]
    # Default for unknown error classes
    if cfg is not None:
        return RetryPolicy(
            cooldown=cfg.retry_cooldown,
            max_retries=cfg.max_retry_count,
            escalate=True,
        )
    return RetryPolicy(cooldown=1800, max_retries=3, escalate=True)


# ---------------------------------------------------------------------------
# Configuration (18 env vars, same defaults as shell version)
# ---------------------------------------------------------------------------

@dataclass
class SnapshotConfig:
    """Configuration thresholds loaded from environment variables."""

    issue_threshold: int = 3
    max_shepherds: int = 3
    max_proposals: int = 5
    architect_cooldown: int = 1800
    hermit_cooldown: int = 1800
    guide_interval: int = 900
    champion_interval: int = 600
    doctor_interval: int = 300
    auditor_interval: int = 600
    judge_interval: int = 300
    curator_interval: int = 300
    issue_strategy: str = "fifo"
    heartbeat_stale_threshold: int = 120
    tmux_socket: str = "loom"
    # Retry configuration for blocked issues
    max_retry_count: int = 3
    retry_cooldown: int = 1800  # 30 minutes initial cooldown
    retry_backoff_multiplier: int = 2
    retry_max_cooldown: int = 14400  # 4 hours max cooldown
    systematic_failure_threshold: int = 3
    # Systematic failure auto-clear configuration
    systematic_failure_cooldown: int = 1800  # 30 minutes before probe
    systematic_failure_max_probes: int = 3  # Max probes before giving up
    # CI health check configuration
    ci_health_check_enabled: bool = True  # Enable CI status monitoring
    # Heartbeat grace period for newly spawned shepherds
    heartbeat_grace_period: int = 300  # 5 minutes
    # Shorter grace period for shepherds that have already reported heartbeats
    heartbeat_active_grace_period: int = 180  # 3 minutes
    # Spinning issue detection: auto-escalate after N review cycles
    spinning_review_threshold: int = 3

    @classmethod
    def from_env(cls) -> SnapshotConfig:
        """Build config from ``LOOM_*`` environment variables."""
        def _int(key: str, default: int) -> int:
            val = os.environ.get(key, "")
            try:
                return int(val) if val else default
            except ValueError:
                return default

        return cls(
            issue_threshold=_int("LOOM_ISSUE_THRESHOLD", 3),
            max_shepherds=_int("LOOM_MAX_SHEPHERDS", 10),
            max_proposals=_int("LOOM_MAX_PROPOSALS", 5),
            architect_cooldown=_int("LOOM_ARCHITECT_COOLDOWN", 1800),
            hermit_cooldown=_int("LOOM_HERMIT_COOLDOWN", 1800),
            guide_interval=_int("LOOM_GUIDE_INTERVAL", 900),
            champion_interval=_int("LOOM_CHAMPION_INTERVAL", 600),
            doctor_interval=_int("LOOM_DOCTOR_INTERVAL", 300),
            auditor_interval=_int("LOOM_AUDITOR_INTERVAL", 600),
            judge_interval=_int("LOOM_JUDGE_INTERVAL", 300),
            curator_interval=_int("LOOM_CURATOR_INTERVAL", 300),
            issue_strategy=os.environ.get("LOOM_ISSUE_STRATEGY", "fifo"),
            heartbeat_stale_threshold=_int("LOOM_HEARTBEAT_STALE_THRESHOLD", 120),
            tmux_socket=os.environ.get("LOOM_TMUX_SOCKET", "loom"),
            max_retry_count=_int("LOOM_MAX_RETRY_COUNT", 3),
            retry_cooldown=_int("LOOM_RETRY_COOLDOWN", 1800),
            retry_backoff_multiplier=_int("LOOM_RETRY_BACKOFF_MULTIPLIER", 2),
            retry_max_cooldown=_int("LOOM_RETRY_MAX_COOLDOWN", 14400),
            systematic_failure_threshold=_int("LOOM_SYSTEMATIC_FAILURE_THRESHOLD", 3),
            systematic_failure_cooldown=_int("LOOM_SYSTEMATIC_FAILURE_COOLDOWN", 1800),
            systematic_failure_max_probes=_int("LOOM_SYSTEMATIC_FAILURE_MAX_PROBES", 3),
            ci_health_check_enabled=os.environ.get("LOOM_CI_HEALTH_CHECK", "true").lower() not in ("false", "0", "no"),
            heartbeat_grace_period=_int("LOOM_HEARTBEAT_GRACE_PERIOD", 300),
            heartbeat_active_grace_period=_int("LOOM_HEARTBEAT_ACTIVE_GRACE_PERIOD", 180),
            spinning_review_threshold=_int("LOOM_SPINNING_REVIEW_THRESHOLD", 3),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "issue_threshold": self.issue_threshold,
            "max_shepherds": self.max_shepherds,
            "max_proposals": self.max_proposals,
            "issue_strategy": self.issue_strategy,
            "max_retry_count": self.max_retry_count,
            "retry_cooldown": self.retry_cooldown,
            "systematic_failure_threshold": self.systematic_failure_threshold,
            "systematic_failure_cooldown": self.systematic_failure_cooldown,
            "systematic_failure_max_probes": self.systematic_failure_max_probes,
            "spinning_review_threshold": self.spinning_review_threshold,
        }


# ---------------------------------------------------------------------------
# Support role state
# ---------------------------------------------------------------------------

_SUPPORT_ROLES = ("guide", "champion", "doctor", "auditor", "judge", "architect", "hermit", "curator")

# Roles that can have demand-based triggers
_DEMAND_ROLES = frozenset({"champion", "doctor", "judge"})


@dataclass
class SupportRoleState:
    """Computed idle-time and trigger state for a single support role."""

    status: str = "idle"
    idle_seconds: int = 0
    interval: int = 0
    needs_trigger: bool = False
    demand_trigger: bool = False


def _role_interval(role: str, cfg: SnapshotConfig) -> int:
    """Return the interval/cooldown for *role*."""
    return {
        "guide": cfg.guide_interval,
        "champion": cfg.champion_interval,
        "doctor": cfg.doctor_interval,
        "auditor": cfg.auditor_interval,
        "judge": cfg.judge_interval,
        "architect": cfg.architect_cooldown,
        "hermit": cfg.hermit_cooldown,
        "curator": cfg.curator_interval,
    }.get(role, 0)


def compute_support_role_state(
    daemon_state: DaemonState,
    cfg: SnapshotConfig,
    *,
    _now: datetime | None = None,
) -> dict[str, SupportRoleState]:
    """Compute idle times and ``needs_trigger`` for all 7 support roles."""
    now = _now or now_utc()
    result: dict[str, SupportRoleState] = {}

    for role in _SUPPORT_ROLES:
        entry = daemon_state.support_roles.get(role)
        status = entry.status if entry else "idle"
        last_completed = entry.last_completed if entry else None
        interval = _role_interval(role, cfg)

        idle_seconds = 0
        needs_trigger = False

        if last_completed and last_completed != "null":
            try:
                idle_seconds = _elapsed(last_completed, now)
                if status != "running" and idle_seconds > interval:
                    needs_trigger = True
            except (ValueError, OSError):
                pass
        elif status != "running":
            # Never run — needs trigger
            needs_trigger = True

        state = SupportRoleState(
            status=status,
            idle_seconds=idle_seconds,
            interval=interval,
            needs_trigger=needs_trigger,
        )
        result[role] = state

    return result


# ---------------------------------------------------------------------------
# Pipeline data collection (parallel gh queries)
# ---------------------------------------------------------------------------

_ISSUE_FIELDS = ["number", "title", "labels", "createdAt"]

# Labels that indicate an issue has been processed or claimed — used to identify
# issues that still need curator attention.
_CURATED_SKIP_LABELS = frozenset({
    "loom:curated", "loom:curating", "loom:issue", "loom:building", "loom:blocked",
    "external",
})
_PR_FIELDS = ["number", "title", "labels", "headRefName"]


def collect_pipeline_data(
    repo_root: pathlib.Path,
    *,
    ci_health_check_enabled: bool = True,
) -> dict[str, list[dict[str, Any]]]:
    """Run 10 parallel ``gh`` queries and return raw pipeline data.

    Returns a dict with keys:
        ready_issues, building_issues, blocked_issues,
        architect_proposals, hermit_proposals, curated_issues,
        review_requested, changes_requested, ready_to_merge,
        uncurated_issues, usage, ci_status
    """
    issue_field_str = ",".join(_ISSUE_FIELDS)
    pr_field_str = ",".join(_PR_FIELDS)

    queries: list[Sequence[str]] = [
        # 0: ready issues
        ["issue", "list", "--label", "loom:issue", "--state", "open", "--json", issue_field_str],
        # 1: building issues
        ["issue", "list", "--label", "loom:building", "--state", "open", "--json", "number,title,labels"],
        # 2: architect proposals
        ["issue", "list", "--label", "loom:architect", "--state", "open", "--json", "number,title,labels"],
        # 3: hermit proposals
        ["issue", "list", "--label", "loom:hermit", "--state", "open", "--json", "number,title,labels"],
        # 4: curated issues
        ["issue", "list", "--label", "loom:curated", "--state", "open", "--json", "number,title,labels"],
        # 5: blocked issues
        ["issue", "list", "--label", "loom:blocked", "--state", "open", "--json", "number,title,labels"],
        # 6: review-requested PRs
        ["pr", "list", "--label", "loom:review-requested", "--state", "open", "--json", pr_field_str],
        # 7: changes-requested PRs
        ["pr", "list", "--label", "loom:changes-requested", "--state", "open", "--json", pr_field_str],
        # 8: ready-to-merge PRs
        ["pr", "list", "--label", "loom:pr", "--state", "open", "--json", pr_field_str],
        # 9: all open issues (for uncurated count — issues needing curator attention)
        ["issue", "list", "--state", "open", "--json", "number,labels", "--limit", "100"],
    ]

    results = gh_parallel_queries(queries, max_workers=8)

    # Filter curated issues: exclude those also labeled loom:building or loom:issue
    curated_raw = results[4]
    curated_filtered = [
        item for item in curated_raw
        if not _has_label(item, "loom:building") and not _has_label(item, "loom:issue")
    ]

    # Compute uncurated issues: open issues without any of the skip labels
    all_open_issues = results[9]
    uncurated_issues = [
        item for item in all_open_issues
        if not any(lbl["name"] in _CURATED_SKIP_LABELS for lbl in item.get("labels", []))
    ]

    # Run usage check separately (it's a script, not a gh query)
    usage = _collect_usage(repo_root)

    # Run CI health check if enabled
    ci_status: dict[str, Any] = {"status": "unknown", "message": "CI health check disabled"}
    if ci_health_check_enabled:
        ci_status = gh_get_default_branch_ci_status()

    return {
        "ready_issues": results[0],
        "building_issues": results[1],
        "architect_proposals": results[2],
        "hermit_proposals": results[3],
        "curated_issues": curated_filtered,
        "blocked_issues": results[5],
        "review_requested": results[6],
        "changes_requested": results[7],
        "ready_to_merge": results[8],
        "uncurated_issues": uncurated_issues,
        "usage": usage,
        "ci_status": ci_status,
    }


def _has_label(item: dict[str, Any], label: str) -> bool:
    """Check if an issue/PR dict has a specific label."""
    for lbl in item.get("labels", []):
        if lbl.get("name") == label:
            return True
    return False


def _collect_usage(repo_root: pathlib.Path) -> dict[str, Any]:
    """Query Claude API usage via the Anthropic OAuth API."""
    try:
        from loom_tools.common.usage import get_usage

        return get_usage(repo_root)
    except Exception:
        return {"error": "no data"}


# ---------------------------------------------------------------------------
# Issue sorting
# ---------------------------------------------------------------------------

def sort_issues_by_strategy(
    issues: list[dict[str, Any]],
    strategy: str,
) -> list[dict[str, Any]]:
    """Sort issues according to strategy, with ``loom:urgent`` always first.

    Strategies:
        fifo: oldest first (ascending createdAt)
        lifo: newest first (descending createdAt)
        priority: same as fifo (urgent first, then oldest)
        fallback: same as fifo (unknown strategy warning)
    """
    urgent = [i for i in issues if _has_label(i, "loom:urgent")]
    non_urgent = [i for i in issues if not _has_label(i, "loom:urgent")]

    if strategy in ("fifo", "priority"):
        urgent.sort(key=_created_at_key)
        non_urgent.sort(key=_created_at_key)
    elif strategy == "lifo":
        urgent.sort(key=_created_at_key, reverse=True)
        non_urgent.sort(key=_created_at_key, reverse=True)
    else:
        log_warning(f"Unknown issue strategy '{strategy}', falling back to fifo")
        urgent.sort(key=_created_at_key)
        non_urgent.sort(key=_created_at_key)

    return urgent + non_urgent


def _created_at_key(item: dict[str, Any]) -> str:
    """Extract createdAt for sorting (empty string as fallback)."""
    return item.get("createdAt", "")


def filter_issues_by_failure_backoff(
    issues: list[dict[str, Any]],
    failure_log: IssueFailureLog,
    current_iteration: int,
) -> list[dict[str, Any]]:
    """Filter out issues that are in failure backoff.

    Issues with persistent failure history are skipped if the daemon hasn't
    completed enough iterations since the failure was recorded. Issues that
    have hit MAX_FAILURES_BEFORE_BLOCK are also filtered out (they should
    already be labeled loom:blocked, but this provides defense in depth).

    Args:
        issues: Sorted list of ready issues.
        failure_log: Persistent failure log.
        current_iteration: Current daemon iteration number.

    Returns:
        Filtered list of issues not in backoff.
    """
    if not failure_log.entries:
        return issues

    filtered: list[dict[str, Any]] = []
    for issue in issues:
        issue_num = issue.get("number")
        if issue_num is None:
            filtered.append(issue)
            continue

        entry = failure_log.entries.get(str(issue_num))
        if entry is None:
            filtered.append(issue)
            continue

        # Auto-block threshold reached — skip entirely
        if entry.should_auto_block:
            log_warning(
                f"Skipping issue #{issue_num}: {entry.total_failures} failures "
                f"(>= threshold), should be auto-blocked"
            )
            continue

        # Check backoff: skip if not enough iterations have passed
        backoff = entry.backoff_iterations()
        if backoff > 0 and current_iteration % (backoff + 1) != 0:
            log_info(
                f"Skipping issue #{issue_num}: in backoff "
                f"(failures={entry.total_failures}, backoff_iters={backoff})"
            )
            continue

        filtered.append(issue)

    return filtered


# ---------------------------------------------------------------------------
# Shepherd progress and heartbeat staleness
# ---------------------------------------------------------------------------

@dataclass
class EnhancedProgress:
    """Shepherd progress with computed heartbeat fields."""

    raw: dict[str, Any]
    heartbeat_age_seconds: int = -1
    heartbeat_stale: bool = False

    def to_dict(self) -> dict[str, Any]:
        d = dict(self.raw)
        d["heartbeat_age_seconds"] = self.heartbeat_age_seconds
        d["heartbeat_stale"] = self.heartbeat_stale
        return d


def compute_shepherd_progress(
    repo_root: pathlib.Path,
    cfg: SnapshotConfig,
    *,
    _now: datetime | None = None,
) -> list[EnhancedProgress]:
    """Read progress files and compute heartbeat staleness."""
    now = _now or now_utc()
    progress_files = read_progress_files(repo_root)
    results: list[EnhancedProgress] = []

    for sp in progress_files:
        age = -1
        stale = False

        if sp.last_heartbeat:
            try:
                age = _elapsed(sp.last_heartbeat, now)
                if age > cfg.heartbeat_stale_threshold:
                    stale = True
            except (ValueError, OSError):
                pass

        # Grace period: don't flag recently spawned shepherds as stale.
        # Two-tier: shepherds that have already reported heartbeats use a
        # shorter grace period so that deaths are detected faster (~3 min
        # instead of ~10 min).
        if stale and sp.started_at:
            try:
                spawn_age = _elapsed(sp.started_at, now)
                effective_grace = (
                    cfg.heartbeat_active_grace_period
                    if sp.last_heartbeat
                    else cfg.heartbeat_grace_period
                )
                if spawn_age < effective_grace:
                    stale = False
            except (ValueError, OSError):
                pass

        results.append(EnhancedProgress(
            raw=sp.to_dict(),
            heartbeat_age_seconds=age,
            heartbeat_stale=stale,
        ))

    return results


# ---------------------------------------------------------------------------
# Tmux pool detection
# ---------------------------------------------------------------------------

@dataclass
class TmuxPool:
    """Tmux agent pool status."""

    available: bool = False
    sessions: list[str] = field(default_factory=list)
    shepherd_count: int = 0
    total_count: int = 0
    execution_mode: str = "direct"

    def to_dict(self) -> dict[str, Any]:
        return {
            "available": self.available,
            "sessions": self.sessions,
            "shepherd_count": self.shepherd_count,
            "total_count": self.total_count,
            "execution_mode": self.execution_mode,
        }


def detect_tmux_pool(tmux_socket: str = "loom") -> TmuxPool:
    """Detect tmux agent pool status via subprocess calls."""
    try:
        # Check if tmux server is running with the loom socket
        subprocess.run(
            ["tmux", "-L", tmux_socket, "has-session"],
            capture_output=True, timeout=5,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError):
        return TmuxPool()

    try:
        result = subprocess.run(
            ["tmux", "-L", tmux_socket, "list-sessions", "-F", "#{session_name}"],
            capture_output=True, text=True, timeout=5,
        )
        if result.returncode != 0 or not result.stdout.strip():
            return TmuxPool()

        sessions = [s.strip() for s in result.stdout.strip().splitlines() if s.strip()]
        shepherd_count = sum(1 for s in sessions if "shepherd" in s)
        execution_mode = "tmux" if shepherd_count > 0 else "direct"

        return TmuxPool(
            available=True,
            sessions=sessions,
            shepherd_count=shepherd_count,
            total_count=len(sessions),
            execution_mode=execution_mode,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError, OSError):
        return TmuxPool()


# ---------------------------------------------------------------------------
# Task ID validation
# ---------------------------------------------------------------------------

_TASK_ID_RE = re.compile(r"^[a-f0-9]{7}$")


def validate_task_ids(daemon_state: DaemonState) -> list[dict[str, str]]:
    """Check all task IDs in daemon state for ``^[a-f0-9]{7}$`` format.

    Returns a list of ``{location, key, task_id}`` dicts for invalid IDs.
    """
    invalid: list[dict[str, str]] = []

    for key, entry in daemon_state.shepherds.items():
        tid = entry.task_id
        if tid and not _TASK_ID_RE.match(tid):
            invalid.append({"location": "shepherds", "key": key, "task_id": tid})

    for key, entry in daemon_state.support_roles.items():
        tid = entry.task_id
        if tid and not _TASK_ID_RE.match(tid):
            invalid.append({"location": "support_roles", "key": key, "task_id": tid})

    return invalid


# ---------------------------------------------------------------------------
# Orphaned shepherd detection
# ---------------------------------------------------------------------------

def detect_orphaned_shepherds(
    daemon_state: DaemonState,
    building_issues: list[dict[str, Any]],
    shepherd_progress: list[EnhancedProgress],
) -> list[dict[str, Any]]:
    """Detect orphaned shepherds.

    An orphaned shepherd is:
    1. A ``loom:building`` issue not tracked in any daemon-state shepherd
    2. A progress file with stale heartbeat and ``working`` status
    """
    orphaned: list[dict[str, Any]] = []

    # Get issues tracked by active daemon shepherds
    tracked_issues: set[int] = set()
    for entry in daemon_state.shepherds.values():
        if entry.status == "working" and entry.issue is not None:
            tracked_issues.add(entry.issue)

    # Check 1: building issues not tracked in daemon-state
    for item in building_issues:
        issue_num = item.get("number")
        if issue_num is None:
            continue
        if issue_num not in tracked_issues:
            # Check for active (non-stale) progress file
            has_active = any(
                ep.raw.get("issue") == issue_num
                and ep.raw.get("status") == "working"
                and not ep.heartbeat_stale
                for ep in shepherd_progress
            )
            if not has_active:
                orphaned.append({
                    "type": "untracked_building",
                    "issue": issue_num,
                    "reason": "no_daemon_entry",
                })

    # Check 2: progress files with stale heartbeats
    for ep in shepherd_progress:
        if ep.raw.get("status") == "working" and ep.heartbeat_stale:
            orphaned.append({
                "type": "stale_heartbeat",
                "task_id": ep.raw.get("task_id", ""),
                "issue": ep.raw.get("issue", 0),
                "age_seconds": ep.heartbeat_age_seconds,
                "reason": "heartbeat_stale",
            })

    return orphaned


# ---------------------------------------------------------------------------
# Orphaned PR detection
# ---------------------------------------------------------------------------

@dataclass
class OrphanedPR:
    """A PR that needs attention but has no active shepherd tracking it."""

    pr_number: int
    needed_role: str  # "judge" or "doctor"

    def to_dict(self) -> dict[str, Any]:
        return {"pr_number": self.pr_number, "needed_role": self.needed_role}


def detect_orphaned_prs(
    daemon_state: DaemonState,
    review_requested: list[dict[str, Any]],
    changes_requested: list[dict[str, Any]],
) -> list[OrphanedPR]:
    """Detect PRs needing attention that no active shepherd is tracking.

    A PR is orphaned when it has ``loom:review-requested`` (needs judge) or
    ``loom:changes-requested`` (needs doctor) but no working shepherd has it
    as its ``pr_number``.

    Returns orphaned PRs sorted by PR number ascending (FIFO).
    """
    # Collect PR numbers tracked by active shepherds
    tracked_prs: set[int] = set()
    for entry in daemon_state.shepherds.values():
        if entry.status == "working" and entry.pr_number is not None:
            tracked_prs.add(entry.pr_number)

    orphaned: list[OrphanedPR] = []

    for pr in review_requested:
        pr_num = pr.get("number")
        if pr_num is not None and pr_num not in tracked_prs:
            orphaned.append(OrphanedPR(pr_number=pr_num, needed_role="judge"))

    for pr in changes_requested:
        pr_num = pr.get("number")
        if pr_num is not None and pr_num not in tracked_prs:
            orphaned.append(OrphanedPR(pr_number=pr_num, needed_role="doctor"))

    # Sort by PR number ascending (FIFO — oldest PR first)
    orphaned.sort(key=lambda o: o.pr_number)
    return orphaned


# ---------------------------------------------------------------------------
# Spinning PR detection
# ---------------------------------------------------------------------------

@dataclass
class SpinningPR:
    """A PR that has cycled through too many review rounds."""

    pr_number: int
    review_cycles: int
    linked_issue: int | None = None

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "pr_number": self.pr_number,
            "review_cycles": self.review_cycles,
        }
        if self.linked_issue is not None:
            d["linked_issue"] = self.linked_issue
        return d


def detect_spinning_prs(
    changes_requested: list[dict[str, Any]],
    threshold: int = 3,
) -> list[SpinningPR]:
    """Detect PRs stuck in review cycles (changes-requested repeatedly).

    Queries the review count for each changes-requested PR. A PR is
    "spinning" when it has accumulated >= *threshold* reviews requesting
    changes, indicating a build→review→fix→review loop that isn't
    converging.

    Only examines PRs already labeled ``loom:changes-requested`` since those
    are the ones actively stuck in the cycle.
    """
    if not changes_requested:
        return []

    # Build a list of PR numbers to query
    pr_numbers = [pr.get("number") for pr in changes_requested if pr.get("number")]
    if not pr_numbers:
        return []

    spinning: list[SpinningPR] = []

    for pr_num in pr_numbers:
        review_count = _count_review_rounds(pr_num)
        if review_count >= threshold:
            linked_issue = _extract_linked_issue(pr_num)
            spinning.append(SpinningPR(
                pr_number=pr_num,
                review_cycles=review_count,
                linked_issue=linked_issue,
            ))

    return spinning


def _count_review_rounds(pr_number: int) -> int:
    """Count the number of review rounds (CHANGES_REQUESTED) on a PR.

    Uses ``gh api`` to fetch reviews and counts distinct
    CHANGES_REQUESTED submissions.
    """
    try:
        result = subprocess.run(
            [
                "gh", "api",
                f"repos/{{owner}}/{{repo}}/pulls/{pr_number}/reviews",
                "--jq", '[.[] | select(.state == "CHANGES_REQUESTED")] | length',
            ],
            capture_output=True, text=True, timeout=30,
        )
        if result.returncode == 0 and result.stdout.strip():
            return int(result.stdout.strip())
    except (subprocess.TimeoutExpired, OSError, ValueError):
        pass
    return 0


def _extract_linked_issue(pr_number: int) -> int | None:
    """Extract linked issue number from a PR's body (looks for 'Closes #N').

    Returns the issue number or None.
    """
    try:
        result = subprocess.run(
            [
                "gh", "pr", "view", str(pr_number),
                "--json", "body",
                "--jq", ".body",
            ],
            capture_output=True, text=True, timeout=15,
        )
        if result.returncode == 0 and result.stdout:
            body = result.stdout
            match = re.search(r"(?:Closes|Fixes|Resolves)\s+#(\d+)", body, re.IGNORECASE)
            if match:
                return int(match.group(1))
    except (subprocess.TimeoutExpired, OSError, ValueError):
        pass
    return None


# ---------------------------------------------------------------------------
# Pipeline health computation
# ---------------------------------------------------------------------------

@dataclass
class PipelineHealth:
    """Pipeline health classification with retry metadata."""

    status: str = "healthy"  # healthy, degraded, stalled
    stall_reason: str | None = None
    blocked_count: int = 0
    retryable_count: int = 0
    permanent_blocked_count: int = 0
    retryable_issues: list[dict[str, Any]] = field(default_factory=list)
    # Issues that have exhausted their retry budget and need human review.
    # Each entry: {number, error_class, retry_count, reason}
    escalation_needed: list[dict[str, Any]] = field(default_factory=list)


def compute_pipeline_health(
    *,
    ready_count: int,
    building_count: int,
    blocked_count: int,
    total_in_flight: int,
    blocked_issues: list[dict[str, Any]],
    daemon_state: DaemonState,
    cfg: SnapshotConfig,
    now: datetime,
) -> PipelineHealth:
    """Classify pipeline health and identify retryable blocked issues.

    Uses per-error-class retry policies (see ``get_retry_policy``) to determine
    cooldown and max retries for each blocked issue, instead of a single global
    policy.  Issues that exhaust their per-class retry budget and whose class
    has ``escalate=True`` are returned in ``escalation_needed`` so the daemon
    can add them to ``needs_human_input``.
    """
    retryable_count = 0
    permanent_count = 0
    retryable_issues: list[dict[str, Any]] = []
    escalation_needed: list[dict[str, Any]] = []

    for item in blocked_issues:
        blocked_num = item.get("number")
        if blocked_num is None:
            continue

        issue_key = str(blocked_num)
        retry_info = daemon_state.blocked_issue_retries.get(issue_key)

        error_class = retry_info.error_class if retry_info else "unknown"
        policy = get_retry_policy(error_class, cfg)
        retry_count = retry_info.retry_count if retry_info else 0

        # Check if retry budget is exhausted for this error class
        if retry_info and (retry_info.retry_exhausted or retry_count >= policy.max_retries):
            permanent_count += 1
            # Identify issues that need human escalation (only once per issue)
            if policy.escalate and not retry_info.escalated_to_human:
                escalation_needed.append({
                    "number": blocked_num,
                    "error_class": error_class,
                    "retry_count": retry_count,
                    "reason": (
                        f"Exceeded {policy.max_retries} retries for {error_class}"
                        if policy.max_retries > 0
                        else f"Error class {error_class} requires immediate human review"
                    ),
                })
            continue

        # Check per-class cooldown
        last_retry = retry_info.last_retry_at if retry_info else None
        cooldown_elapsed = True

        if last_retry and policy.cooldown > 0:
            try:
                elapsed = _elapsed(last_retry, now)
                # Known error classes use fixed cooldown; unknown classes use
                # exponential backoff with the global config multiplier.
                if error_class in _ERROR_CLASS_POLICIES:
                    effective_cooldown = policy.cooldown
                else:
                    effective_cooldown = policy.cooldown * (cfg.retry_backoff_multiplier ** retry_count)
                    if effective_cooldown > cfg.retry_max_cooldown:
                        effective_cooldown = cfg.retry_max_cooldown
                if elapsed < effective_cooldown:
                    cooldown_elapsed = False
            except (ValueError, OSError):
                pass

        if cooldown_elapsed:
            retryable_count += 1
            retryable_issues.append({"number": blocked_num, "retry_count": retry_count})
        else:
            permanent_count += 1

    # Determine status
    status = "healthy"
    stall_reason: str | None = None

    if ready_count == 0 and blocked_count > 0 and building_count == 0:
        status = "stalled"
        stall_reason = "all_issues_blocked"
    elif ready_count == 0 and blocked_count == 0 and building_count == 0 and total_in_flight == 0:
        status = "stalled"
        stall_reason = "no_ready_issues"
    elif blocked_count > 0 and blocked_count > ready_count:
        status = "degraded"

    return PipelineHealth(
        status=status,
        stall_reason=stall_reason,
        blocked_count=blocked_count,
        retryable_count=retryable_count,
        permanent_blocked_count=permanent_count,
        retryable_issues=retryable_issues,
        escalation_needed=escalation_needed,
    )


# ---------------------------------------------------------------------------
# Systematic failure state computation
# ---------------------------------------------------------------------------

@dataclass
class SystematicFailureState:
    """Computed systematic failure state with cooldown information."""

    active: bool = False
    pattern: str = ""
    probe_count: int = 0
    cooldown_elapsed: bool = False
    cooldown_remaining_seconds: int = 0
    probes_exhausted: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "active": self.active,
            "pattern": self.pattern,
            "probe_count": self.probe_count,
            "cooldown_elapsed": self.cooldown_elapsed,
            "cooldown_remaining_seconds": self.cooldown_remaining_seconds,
            "probes_exhausted": self.probes_exhausted,
        }


def compute_systematic_failure_state(
    daemon_state: DaemonState,
    cfg: SnapshotConfig,
    *,
    _now: datetime | None = None,
) -> SystematicFailureState:
    """Compute systematic failure state with cooldown and probe information.

    Returns a SystematicFailureState with:
    - cooldown_elapsed: True if enough time has passed for a probe attempt
    - probes_exhausted: True if max probes reached (require manual intervention)
    """
    sf = daemon_state.systematic_failure
    now = _now or now_utc()

    if not sf.active:
        return SystematicFailureState()

    # Check if probes are exhausted
    probes_exhausted = sf.probe_count >= cfg.systematic_failure_max_probes

    # Compute cooldown with exponential backoff based on probe count
    # First probe: 30min, second: 60min, third: 120min
    effective_cooldown = cfg.systematic_failure_cooldown * (2 ** sf.probe_count)

    # Check if cooldown has elapsed
    cooldown_elapsed = False
    cooldown_remaining = 0

    # Use cooldown_until if set, otherwise fall back to detected_at + cooldown
    cooldown_reference = sf.cooldown_until or sf.detected_at
    if cooldown_reference:
        try:
            elapsed = _elapsed(cooldown_reference, now)
            if sf.cooldown_until:
                # cooldown_until is the target time, so elapsed > 0 means we've passed it
                cooldown_elapsed = elapsed >= 0
                cooldown_remaining = max(0, -elapsed)
            else:
                # detected_at + effective_cooldown
                cooldown_elapsed = elapsed >= effective_cooldown
                cooldown_remaining = max(0, effective_cooldown - elapsed)
        except (ValueError, OSError):
            # On parse error, assume cooldown elapsed to allow recovery attempt
            cooldown_elapsed = True

    return SystematicFailureState(
        active=sf.active,
        pattern=sf.pattern,
        probe_count=sf.probe_count,
        cooldown_elapsed=cooldown_elapsed,
        cooldown_remaining_seconds=cooldown_remaining,
        probes_exhausted=probes_exhausted,
    )


# ---------------------------------------------------------------------------
# Recommended actions engine
# ---------------------------------------------------------------------------

def compute_recommended_actions(
    *,
    ready_count: int,
    building_count: int,
    blocked_count: int,
    total_proposals: int,
    architect_count: int,
    hermit_count: int,
    review_count: int,
    changes_count: int,
    merge_count: int,
    available_shepherd_slots: int,
    needs_work_generation: bool,
    architect_cooldown_ok: bool,
    hermit_cooldown_ok: bool,
    support_roles: dict[str, SupportRoleState],
    orphaned_count: int,
    invalid_task_id_count: int,
    systematic_failure_active: bool = False,
    systematic_failure_state: SystematicFailureState | None = None,
    pipeline_health: PipelineHealth | None = None,
    orphaned_prs: list[OrphanedPR] | None = None,
    spinning_prs: list[SpinningPR] | None = None,
    curated_count: int = 0,
    uncurated_count: int = 0,
) -> tuple[list[str], dict[str, Any]]:
    """Compute recommended actions and demand flags.

    Returns ``(actions, demand_flags)`` where demand_flags has keys
    ``champion_demand``, ``doctor_demand``, ``judge_demand``,
    and optionally ``doctor_targeted_prs`` and ``judge_targeted_prs``.
    """
    actions: list[str] = []
    demand: dict[str, Any] = {"champion_demand": False, "doctor_demand": False, "judge_demand": False}

    # Action: promote proposals (for force mode)
    if total_proposals > 0:
        actions.append("promote_proposals")

    # Handle systematic failure state
    sf_state = systematic_failure_state or SystematicFailureState()
    should_suppress_spawning = systematic_failure_active

    if systematic_failure_active and sf_state.cooldown_elapsed:
        if sf_state.probes_exhausted:
            # Probes exhausted - require manual intervention
            # Keep suppressing and add a warning action
            actions.append("systematic_failure_manual_intervention")
        else:
            # Cooldown elapsed and probes available - recommend probe
            actions.append("probe_systematic_failure")
            # Allow spawning a single probe shepherd
            should_suppress_spawning = False

    # Action: spawn shepherds (suppressed during systematic failure unless probing)
    if ready_count > 0 and available_shepherd_slots > 0 and not should_suppress_spawning:
        actions.append("spawn_shepherds")

    # Action: trigger architect
    architect_state = support_roles.get("architect")
    architect_status = architect_state.status if architect_state else "idle"
    if (needs_work_generation and architect_cooldown_ok
            and architect_count < 2 and architect_status != "running"):
        actions.append("trigger_architect")

    # Action: trigger hermit
    hermit_state = support_roles.get("hermit")
    hermit_status = hermit_state.status if hermit_state else "idle"
    if (needs_work_generation and hermit_cooldown_ok
            and hermit_count < 2 and hermit_status != "running"):
        actions.append("trigger_hermit")

    # Action: check stuck
    if building_count > 0:
        actions.append("check_stuck")

    # Demand-based spawning
    # Targeted dispatch takes precedence over generic demand: if orphaned PRs
    # exist, dispatch the first one (FIFO) with the PR number so the agent
    # can target it directly.
    _orphaned = orphaned_prs or []
    doctor_targeted = [o for o in _orphaned if o.needed_role == "doctor"]
    judge_targeted = [o for o in _orphaned if o.needed_role == "judge"]

    champion_status = support_roles.get("champion", SupportRoleState()).status
    if merge_count > 0 and champion_status != "running":
        actions.append("spawn_champion_demand")
        demand["champion_demand"] = True

    doctor_status = support_roles.get("doctor", SupportRoleState()).status
    if changes_count > 0 and doctor_status != "running":
        if doctor_targeted:
            actions.append("spawn_doctor_targeted")
            demand["doctor_targeted_prs"] = [o.pr_number for o in doctor_targeted]
        else:
            actions.append("spawn_doctor_demand")
        demand["doctor_demand"] = True

    judge_status = support_roles.get("judge", SupportRoleState()).status
    if review_count > 0 and judge_status != "running":
        if judge_targeted:
            actions.append("spawn_judge_targeted")
            demand["judge_targeted_prs"] = [o.pr_number for o in judge_targeted]
        else:
            actions.append("spawn_judge_demand")
        demand["judge_demand"] = True

    # Interval-based triggers (skip if demand trigger handles it)
    guide_state = support_roles.get("guide", SupportRoleState())
    if guide_state.needs_trigger:
        actions.append("trigger_guide")

    champion_state = support_roles.get("champion", SupportRoleState())
    if champion_state.needs_trigger and not demand["champion_demand"]:
        actions.append("trigger_champion")

    doctor_state_obj = support_roles.get("doctor", SupportRoleState())
    if doctor_state_obj.needs_trigger and not demand["doctor_demand"]:
        actions.append("trigger_doctor")

    auditor_state = support_roles.get("auditor", SupportRoleState())
    if auditor_state.needs_trigger:
        actions.append("trigger_auditor")

    judge_state = support_roles.get("judge", SupportRoleState())
    if judge_state.needs_trigger and not demand["judge_demand"]:
        actions.append("trigger_judge")

    curator_state = support_roles.get("curator", SupportRoleState())
    if curator_state.needs_trigger and uncurated_count > 0:
        actions.append("trigger_curator")

    # Recover orphans
    if orphaned_count > 0:
        actions.append("recover_orphans")

    # Validate state
    if invalid_task_id_count > 0:
        actions.append("validate_state")

    # Retry blocked issues when pipeline is stalled and retryable issues exist
    if pipeline_health and pipeline_health.status == "stalled" and pipeline_health.retryable_count > 0:
        actions.append("retry_blocked_issues")

    # Escalate spinning PRs (review cycle loop detected)
    _spinning = spinning_prs or []
    if _spinning:
        actions.append("escalate_spinning_issues")

    # Wait fallback
    if not actions or (len(actions) == 1 and actions[0] == "check_stuck"):
        actions.append("wait")

    # Detect human-input-needed blockers when pipeline has no ready work.
    # We check ready_count == 0 rather than "wait" in actions because
    # "promote_proposals" may be present (only applies in force mode) and
    # would mask the fact that the pipeline is effectively idle.
    human_input_blockers: list[dict[str, Any]] = []
    pipeline_idle = ready_count == 0 and building_count == 0
    if pipeline_idle:
        if curated_count > 0:
            human_input_blockers.append({
                "type": "approval_needed",
                "count": curated_count,
                "description": f"{curated_count} curated issue(s) awaiting human approval to become loom:issue",
            })
        if architect_count > 0:
            human_input_blockers.append({
                "type": "proposal_review",
                "count": architect_count,
                "description": f"{architect_count} architect proposal(s) awaiting human review",
            })
        if hermit_count > 0:
            human_input_blockers.append({
                "type": "proposal_review",
                "count": hermit_count,
                "description": f"{hermit_count} hermit proposal(s) awaiting human review",
            })
        if blocked_count > 0:
            human_input_blockers.append({
                "type": "blocked",
                "count": blocked_count,
                "description": f"{blocked_count} issue(s) blocked — may need human intervention",
            })
        if human_input_blockers:
            actions.append("needs_human_input")
    demand["human_input_blockers"] = human_input_blockers

    return actions, demand


# ---------------------------------------------------------------------------
# Contradictory label detection
# ---------------------------------------------------------------------------

def detect_contradictory_labels(
    pipeline_data: dict[str, list[dict[str, Any]]],
) -> list[dict[str, Any]]:
    """Detect entities with contradictory labels from exclusion groups.

    Uses the pipeline data (which queries PRs/issues by label in parallel)
    to find entities appearing in multiple mutually-exclusive result sets.

    Returns a list of dicts with:
        entity_type: "pr" or "issue"
        number: entity number
        conflicting_labels: list of conflicting label names
    """
    conflicts: list[dict[str, Any]] = []

    # PR exclusion group: loom:pr, loom:changes-requested, loom:review-requested
    # These map to pipeline keys: ready_to_merge, changes_requested, review_requested
    pr_label_map: dict[str, str] = {
        "review_requested": "loom:review-requested",
        "changes_requested": "loom:changes-requested",
        "ready_to_merge": "loom:pr",
    }

    # Build a dict of PR number -> set of labels found
    pr_labels: dict[int, set[str]] = {}
    for key, label in pr_label_map.items():
        for item in pipeline_data.get(key, []):
            num = item.get("number")
            if num is not None:
                pr_labels.setdefault(num, set()).add(label)

    for num, labels in pr_labels.items():
        if len(labels) > 1:
            conflicts.append({
                "entity_type": "pr",
                "number": num,
                "conflicting_labels": sorted(labels),
            })

    # Issue exclusion group: loom:issue, loom:building, loom:blocked
    # These map to pipeline keys: ready_issues, building_issues, blocked_issues
    issue_label_map: dict[str, str] = {
        "ready_issues": "loom:issue",
        "building_issues": "loom:building",
        "blocked_issues": "loom:blocked",
    }

    issue_labels: dict[int, set[str]] = {}
    for key, label in issue_label_map.items():
        for item in pipeline_data.get(key, []):
            num = item.get("number")
            if num is not None:
                issue_labels.setdefault(num, set()).add(label)

    for num, labels in issue_labels.items():
        if len(labels) > 1:
            conflicts.append({
                "entity_type": "issue",
                "number": num,
                "conflicting_labels": sorted(labels),
            })

    return conflicts


# ---------------------------------------------------------------------------
# Health warnings and status
# ---------------------------------------------------------------------------

def compute_health(
    *,
    ready_count: int,
    building_count: int,
    blocked_count: int,
    total_proposals: int,
    stale_heartbeat_count: int,
    orphaned_count: int,
    usage_healthy: bool,
    session_percent: float,
    ci_status: dict[str, Any] | None = None,
    contradictory_labels: list[dict[str, Any]] | None = None,
    spinning_prs: list[SpinningPR] | None = None,
    curated_count: int = 0,
    architect_count: int = 0,
    hermit_count: int = 0,
) -> tuple[str, list[dict[str, str]]]:
    """Compute health status and warnings.

    Returns ``(health_status, health_warnings)``.
    """
    warnings: list[dict[str, str]] = []

    # contradictory_labels — entities with mutually exclusive labels
    if contradictory_labels:
        for conflict in contradictory_labels:
            entity_type = conflict.get("entity_type", "entity")
            number = conflict.get("number", "?")
            labels = ", ".join(conflict.get("conflicting_labels", []))
            warnings.append({
                "code": "contradictory_labels",
                "level": "warning",
                "message": f"{entity_type} #{number} has contradictory labels: {labels}",
            })

    # pipeline_stalled
    if ready_count == 0 and building_count == 0 and blocked_count > 0:
        warnings.append({
            "code": "pipeline_stalled",
            "level": "warning",
            "message": f"0 ready, {blocked_count} blocked, 0 building — pipeline has no actionable work",
        })

    # proposal_backlog
    if ready_count == 0 and building_count == 0 and total_proposals > 0:
        warnings.append({
            "code": "proposal_backlog",
            "level": "info",
            "message": f"{total_proposals} proposals awaiting approval, pipeline empty",
        })

    # needs_human_input — actionable warning when pipeline is idle and human
    # action can unblock it (curated issues need approval, proposals need review)
    human_input_items = curated_count + architect_count + hermit_count
    if ready_count == 0 and building_count == 0 and human_input_items > 0:
        parts = []
        if curated_count > 0:
            parts.append(f"{curated_count} curated issue(s) need approval")
        if architect_count > 0:
            parts.append(f"{architect_count} architect proposal(s) need review")
        if hermit_count > 0:
            parts.append(f"{hermit_count} hermit proposal(s) need review")
        warnings.append({
            "code": "needs_human_input",
            "level": "warning",
            "message": f"Pipeline blocked on human input: {', '.join(parts)}",
        })

    # no_work_available
    if ready_count == 0 and building_count == 0 and blocked_count == 0 and total_proposals == 0:
        warnings.append({
            "code": "no_work_available",
            "level": "info",
            "message": "No ready, building, blocked, or proposed issues — pipeline is empty",
        })

    # stale_heartbeats
    if stale_heartbeat_count > 0:
        warnings.append({
            "code": "stale_heartbeats",
            "level": "warning",
            "message": f"{stale_heartbeat_count} shepherd(s) with stale heartbeats — may be stuck",
        })

    # orphaned_issues
    if orphaned_count > 0:
        warnings.append({
            "code": "orphaned_issues",
            "level": "warning",
            "message": f"{orphaned_count} orphaned shepherd(s) detected — recovery needed",
        })

    # session_budget_low
    if not usage_healthy:
        warnings.append({
            "code": "session_budget_low",
            "level": "warning",
            "message": f"Session usage at {session_percent}% — nearing budget limit",
        })

    # spinning_prs — PRs stuck in review cycles
    _spinning = spinning_prs or []
    if _spinning:
        pr_nums = ", ".join(f"#{s.pr_number}" for s in _spinning)
        warnings.append({
            "code": "spinning_prs",
            "level": "warning",
            "message": f"{len(_spinning)} PR(s) stuck in review cycles: {pr_nums}",
        })

    # ci_failing - CI is broken on the default branch
    if ci_status and ci_status.get("status") == "failing":
        failed_runs = ci_status.get("failed_runs", [])
        warnings.append({
            "code": "ci_failing",
            "level": "info",
            "message": ci_status.get("message", f"CI failing on main: {len(failed_runs)} workflow(s) failed"),
        })

    # Derive status
    has_warning = any(w["level"] == "warning" for w in warnings)
    if has_warning:
        status = "stalled"
    elif warnings:
        status = "degraded"
    else:
        status = "healthy"

    return status, warnings


# ---------------------------------------------------------------------------
# Pre-flight environment checks
# ---------------------------------------------------------------------------

def run_preflight_checks(
    repo_root: pathlib.Path,
    *,
    _check_import: bool | None = None,
    _check_gh: bool | None = None,
) -> dict[str, Any]:
    """Run pre-flight environment checks and return status dict.

    Parameters starting with ``_`` are for testing injection.

    Returns a dict with keys like ``loom_tools_available``,
    ``gh_authenticated``, ``python_available``, each mapping to a bool.
    """
    result: dict[str, Any] = {}

    # Check 1: Python interpreter available
    result["python_available"] = shutil.which("python3") is not None or shutil.which("python") is not None

    # Check 2: loom_tools importable
    if _check_import is not None:
        result["loom_tools_available"] = _check_import
    else:
        try:
            proc = subprocess.run(
                [sys.executable, "-c", "import loom_tools"],
                capture_output=True,
                timeout=10,
            )
            result["loom_tools_available"] = proc.returncode == 0
        except (subprocess.TimeoutExpired, FileNotFoundError, OSError):
            result["loom_tools_available"] = False

    # Check 3: gh CLI available and authenticated
    if _check_gh is not None:
        result["gh_authenticated"] = _check_gh
    else:
        if not shutil.which("gh"):
            result["gh_authenticated"] = False
        else:
            try:
                proc = subprocess.run(
                    ["gh", "auth", "status"],
                    capture_output=True,
                    timeout=10,
                )
                result["gh_authenticated"] = proc.returncode == 0
            except (subprocess.TimeoutExpired, FileNotFoundError, OSError):
                result["gh_authenticated"] = False

    # Check 4: claude CLI available
    result["claude_cli_available"] = shutil.which("claude") is not None

    # Check 5: loom-tools install path exists
    loom_tools_dir = repo_root / "loom-tools"
    result["loom_tools_dir_exists"] = loom_tools_dir.exists()

    return result


# ---------------------------------------------------------------------------
# Top-level orchestrator
# ---------------------------------------------------------------------------

def build_snapshot(
    *,
    cfg: SnapshotConfig | None = None,
    repo_root: pathlib.Path | None = None,
    _now: datetime | None = None,
    _pipeline_data: dict[str, Any] | None = None,
    _tmux_pool: TmuxPool | None = None,
    _preflight: dict[str, Any] | None = None,
    _current_iteration: int = 0,
) -> dict[str, Any]:
    """Build the full snapshot dict.

    Parameters marked with ``_`` are for testing injection; live calls
    leave them as ``None``.
    """
    if cfg is None:
        cfg = SnapshotConfig.from_env()
    if repo_root is None:
        repo_root = find_repo_root()

    now = _now or now_utc()
    timestamp = now.strftime("%Y-%m-%dT%H:%M:%SZ")

    # 1. Collect pipeline data
    pipeline = _pipeline_data if _pipeline_data is not None else collect_pipeline_data(
        repo_root, ci_health_check_enabled=cfg.ci_health_check_enabled
    )

    # 2. Sort ready issues and filter by failure backoff
    ready_issues = sort_issues_by_strategy(pipeline["ready_issues"], cfg.issue_strategy)
    failure_log = load_failure_log(repo_root)
    ready_issues = filter_issues_by_failure_backoff(
        ready_issues, failure_log, _current_iteration
    )
    building_issues = pipeline["building_issues"]
    blocked_issues = pipeline["blocked_issues"]
    architect_proposals = pipeline["architect_proposals"]
    hermit_proposals = pipeline["hermit_proposals"]
    curated_issues = pipeline["curated_issues"]
    review_requested = pipeline["review_requested"]
    changes_requested = pipeline["changes_requested"]
    ready_to_merge = pipeline["ready_to_merge"]
    uncurated_issues = pipeline.get("uncurated_issues", [])
    usage = pipeline.get("usage", {"error": "no data"})
    ci_status = pipeline.get("ci_status", {"status": "unknown", "message": "CI health check not available"})

    # 3. Read daemon state
    daemon_state = read_daemon_state(repo_root)

    # 4. Compute counts
    ready_count = len(ready_issues)
    building_count = len(building_issues)
    blocked_count = len(blocked_issues)
    architect_count = len(architect_proposals)
    hermit_count = len(hermit_proposals)
    curated_count = len(curated_issues)
    uncurated_count = len(uncurated_issues)
    review_count = len(review_requested)
    changes_count = len(changes_requested)
    merge_count = len(ready_to_merge)

    total_proposals = architect_count + hermit_count + curated_count
    total_in_flight = building_count + review_count + changes_count + merge_count

    # Active shepherds
    active_shepherds = sum(
        1 for e in daemon_state.shepherds.values() if e.status == "working"
    )
    available_shepherd_slots = max(0, cfg.max_shepherds - active_shepherds)

    # 5. Needs work generation
    needs_work_gen = (ready_count < cfg.issue_threshold and total_proposals < cfg.max_proposals)

    # 6. Support role state
    sr_state = compute_support_role_state(daemon_state, cfg, _now=now)

    # 7. Cooldown status
    architect_cooldown_ok = _check_cooldown(
        daemon_state.support_roles.get("architect"),
        cfg.architect_cooldown,
        now,
    )
    hermit_cooldown_ok = _check_cooldown(
        daemon_state.support_roles.get("hermit"),
        cfg.hermit_cooldown,
        now,
    )

    # 8. Shepherd progress
    shepherd_progress = compute_shepherd_progress(repo_root, cfg, _now=now)
    stale_heartbeat_count = sum(
        1 for ep in shepherd_progress
        if ep.heartbeat_stale and ep.raw.get("status") == "working"
    )

    # 9. Tmux pool
    tmux_pool = _tmux_pool if _tmux_pool is not None else detect_tmux_pool(cfg.tmux_socket)

    # 10. Task ID validation
    invalid_task_ids = validate_task_ids(daemon_state)
    invalid_task_id_count = len(invalid_task_ids)

    # 11. Orphaned shepherds
    orphaned = detect_orphaned_shepherds(daemon_state, building_issues, shepherd_progress)
    orphaned_count = len(orphaned)

    # 12. Orphaned PRs (PRs needing attention without an active shepherd)
    orphaned_prs = detect_orphaned_prs(daemon_state, review_requested, changes_requested)

    # 13. Pipeline health (retry/backoff classification)
    p_health = compute_pipeline_health(
        ready_count=ready_count,
        building_count=building_count,
        blocked_count=blocked_count,
        total_in_flight=total_in_flight,
        blocked_issues=blocked_issues,
        daemon_state=daemon_state,
        cfg=cfg,
        now=now,
    )

    # 14. Systematic failure detection and state computation
    sf = daemon_state.systematic_failure
    sf_state = compute_systematic_failure_state(daemon_state, cfg, _now=now)

    # 14b. Spinning PR detection
    spinning_prs = detect_spinning_prs(changes_requested, threshold=cfg.spinning_review_threshold)

    # 15. Recommended actions
    actions, demand = compute_recommended_actions(
        ready_count=ready_count,
        building_count=building_count,
        blocked_count=blocked_count,
        total_proposals=total_proposals,
        architect_count=architect_count,
        hermit_count=hermit_count,
        review_count=review_count,
        changes_count=changes_count,
        merge_count=merge_count,
        available_shepherd_slots=available_shepherd_slots,
        needs_work_generation=needs_work_gen,
        architect_cooldown_ok=architect_cooldown_ok,
        hermit_cooldown_ok=hermit_cooldown_ok,
        support_roles=sr_state,
        orphaned_count=orphaned_count,
        invalid_task_id_count=invalid_task_id_count,
        systematic_failure_active=sf.active,
        systematic_failure_state=sf_state,
        pipeline_health=p_health,
        orphaned_prs=orphaned_prs,
        spinning_prs=spinning_prs,
        curated_count=curated_count,
        uncurated_count=uncurated_count,
    )

    # 16. Promotable proposals
    promotable_proposals = (
        [i["number"] for i in architect_proposals]
        + [i["number"] for i in hermit_proposals]
        + [i["number"] for i in curated_issues]
    )

    # 16. Usage health
    session_percent = _extract_session_percent(usage)
    usage_healthy = session_percent < 97 if isinstance(session_percent, (int, float)) else True

    # 16b. Contradictory label detection
    contradictory = detect_contradictory_labels(pipeline)

    # 17. Health status
    health_status, health_warnings = compute_health(
        ready_count=ready_count,
        building_count=building_count,
        blocked_count=blocked_count,
        total_proposals=total_proposals,
        stale_heartbeat_count=stale_heartbeat_count,
        orphaned_count=orphaned_count,
        usage_healthy=usage_healthy,
        session_percent=session_percent,
        ci_status=ci_status,
        contradictory_labels=contradictory,
        spinning_prs=spinning_prs,
        curated_count=curated_count,
        architect_count=architect_count,
        hermit_count=hermit_count,
    )

    # 18. Build support_roles output (schema matches shell)
    support_roles_out: dict[str, Any] = {}
    for role in _SUPPORT_ROLES:
        state = sr_state[role]
        d: dict[str, Any] = {
            "status": state.status,
            "idle_seconds": state.idle_seconds,
            "interval": state.interval,
            "needs_trigger": state.needs_trigger,
        }
        if role in _DEMAND_ROLES:
            d["demand_trigger"] = demand.get(f"{role}_demand", False)
        support_roles_out[role] = d

    # 19. Pre-flight environment checks
    preflight = _preflight if _preflight is not None else run_preflight_checks(repo_root)

    # 20. Build final output (exact schema match with shell)
    usage_out = dict(usage) if isinstance(usage, dict) else {"error": "no data"}
    usage_out["healthy"] = usage_healthy

    return {
        "timestamp": timestamp,
        "pipeline": {
            "ready_issues": ready_issues,
            "building_issues": building_issues,
            "blocked_issues": blocked_issues,
        },
        "proposals": {
            "architect": architect_proposals,
            "hermit": hermit_proposals,
            "curated": curated_issues,
        },
        "prs": {
            "review_requested": review_requested,
            "changes_requested": changes_requested,
            "ready_to_merge": ready_to_merge,
            "orphaned": [o.to_dict() for o in orphaned_prs],
            "orphaned_count": len(orphaned_prs),
            "spinning": [s.to_dict() for s in spinning_prs],
            "spinning_count": len(spinning_prs),
        },
        "shepherds": {
            "progress": [ep.to_dict() for ep in shepherd_progress],
            "stale_heartbeat_count": stale_heartbeat_count,
            "orphaned": orphaned,
            "orphaned_count": orphaned_count,
        },
        "validation": {
            "invalid_task_ids": invalid_task_ids,
            "invalid_task_id_count": invalid_task_id_count,
            "contradictory_labels": contradictory,
            "contradictory_label_count": len(contradictory),
        },
        "support_roles": support_roles_out,
        "pipeline_health": {
            "status": p_health.status,
            "stall_reason": p_health.stall_reason,
            "blocked_count": p_health.blocked_count,
            "retryable_count": p_health.retryable_count,
            "permanent_blocked_count": p_health.permanent_blocked_count,
            "retryable_issues": p_health.retryable_issues,
        },
        "systematic_failure": {
            "active": sf.active,
            "pattern": sf.pattern,
            "count": sf.count,
            "probe_count": sf.probe_count,
            "cooldown_elapsed": sf_state.cooldown_elapsed,
            "cooldown_remaining_seconds": sf_state.cooldown_remaining_seconds,
            "probes_exhausted": sf_state.probes_exhausted,
        },
        "preflight": preflight,
        "usage": usage_out,
        "ci_status": ci_status,
        "tmux_pool": tmux_pool.to_dict(),
        "computed": {
            "total_ready": ready_count,
            "total_building": building_count,
            "total_blocked": blocked_count,
            "total_uncurated": uncurated_count,
            "total_proposals": total_proposals,
            "total_in_flight": total_in_flight,
            "active_shepherds": active_shepherds,
            "available_shepherd_slots": available_shepherd_slots,
            "needs_work_generation": needs_work_gen,
            "architect_cooldown_ok": architect_cooldown_ok,
            "hermit_cooldown_ok": hermit_cooldown_ok,
            "promotable_proposals": promotable_proposals,
            "recommended_actions": actions,
            "stale_heartbeat_count": stale_heartbeat_count,
            "orphaned_count": orphaned_count,
            "prs_awaiting_review": review_count,
            "prs_needing_fixes": changes_count,
            "prs_ready_to_merge": merge_count,
            "champion_demand": demand["champion_demand"],
            "doctor_demand": demand["doctor_demand"],
            "judge_demand": demand["judge_demand"],
            "doctor_targeted_prs": demand.get("doctor_targeted_prs", []),
            "judge_targeted_prs": demand.get("judge_targeted_prs", []),
            "execution_mode": tmux_pool.execution_mode,
            "tmux_available": tmux_pool.available,
            "tmux_shepherd_count": tmux_pool.shepherd_count,
            "pipeline_health_status": p_health.status,
            "systematic_failure_active": sf.active,
            "health_status": health_status,
            "health_warnings": health_warnings,
            "needs_human_input": demand.get("human_input_blockers", []),
            "ci_failing": ci_status.get("status") == "failing",
        },
        "config": cfg.to_dict(),
    }


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _elapsed(ts: str, now: datetime) -> int:
    """Seconds since ISO timestamp *ts* relative to *now*."""
    dt = parse_iso_timestamp(ts)
    return int((now - dt).total_seconds())


def _check_cooldown(
    entry: SupportRoleEntry | None,
    cooldown: int,
    now: datetime,
) -> bool:
    """Check if a role's cooldown has elapsed (True = OK to trigger)."""
    if entry is None:
        return True
    last_completed = entry.last_completed
    if not last_completed or last_completed == "null":
        return True
    try:
        elapsed = _elapsed(last_completed, now)
        return elapsed > cooldown
    except (ValueError, OSError):
        return True


def _extract_session_percent(usage: dict[str, Any] | Any) -> float:
    """Extract session_percent from usage data, defaulting to 0."""
    if not isinstance(usage, dict):
        return 0.0
    val = usage.get("session_percent", 0)
    if val is None or val == "null":
        return 0.0
    try:
        return float(val)
    except (ValueError, TypeError):
        return 0.0


# ---------------------------------------------------------------------------
# CLI entry point
# ---------------------------------------------------------------------------

_HELP_TEXT = """\
daemon-snapshot.py - Consolidated daemon state snapshot (Python)

USAGE:
    python -m loom_tools.snapshot              Output JSON snapshot (compact)
    python -m loom_tools.snapshot --pretty     Output pretty-printed JSON
    python -m loom_tools.snapshot --help       Show this help

DESCRIPTION:
    Consolidates all daemon state queries into a single JSON output.
    Runs GitHub API queries in parallel for efficiency.

    Produces the same output schema as the former daemon-snapshot.sh.

ENVIRONMENT VARIABLES:
    LOOM_ISSUE_THRESHOLD     Threshold for work generation (default: 3)
    LOOM_MAX_SHEPHERDS       Maximum concurrent shepherds (default: 10)
    LOOM_MAX_PROPOSALS       Maximum pending proposals (default: 5)
    LOOM_ARCHITECT_COOLDOWN  Architect trigger cooldown in seconds (default: 1800)
    LOOM_HERMIT_COOLDOWN     Hermit trigger cooldown in seconds (default: 1800)
    LOOM_GUIDE_INTERVAL      Guide re-trigger interval in seconds (default: 900)
    LOOM_CHAMPION_INTERVAL   Champion re-trigger interval in seconds (default: 600)
    LOOM_DOCTOR_INTERVAL     Doctor re-trigger interval in seconds (default: 300)
    LOOM_AUDITOR_INTERVAL    Auditor re-trigger interval in seconds (default: 600)
    LOOM_JUDGE_INTERVAL      Judge re-trigger interval in seconds (default: 300)
    LOOM_ISSUE_STRATEGY      Issue selection strategy (default: fifo)
    LOOM_HEARTBEAT_STALE_THRESHOLD  Heartbeat staleness threshold (default: 120)
    LOOM_HEARTBEAT_GRACE_PERIOD     Grace period for new shepherds (default: 300s)
    LOOM_HEARTBEAT_ACTIVE_GRACE_PERIOD  Shorter grace for active shepherds (default: 180)
    LOOM_TMUX_SOCKET         Tmux socket name (default: loom)
    LOOM_MAX_RETRY_COUNT     Max retries for blocked issues (default: 3)
    LOOM_RETRY_COOLDOWN      Initial retry cooldown in seconds (default: 1800)
    LOOM_RETRY_BACKOFF_MULTIPLIER  Backoff multiplier (default: 2)
    LOOM_RETRY_MAX_COOLDOWN  Maximum retry cooldown in seconds (default: 14400)
    LOOM_SYSTEMATIC_FAILURE_THRESHOLD  Consecutive failures to suppress (default: 3)
    LOOM_SYSTEMATIC_FAILURE_COOLDOWN   Seconds before first probe attempt (default: 1800)
    LOOM_SYSTEMATIC_FAILURE_MAX_PROBES Maximum probe attempts (default: 3)
    LOOM_CI_HEALTH_CHECK               Enable CI status monitoring (default: true)
"""


def main(argv: Sequence[str] | None = None) -> None:
    """CLI entry point."""
    args = list(argv if argv is not None else sys.argv[1:])

    pretty = False
    for arg in args:
        if arg in ("--help", "-h"):
            print(_HELP_TEXT)
            sys.exit(0)
        elif arg == "--pretty":
            pretty = True
        else:
            print(f"Unknown option: {arg}", file=sys.stderr)
            print("Run 'python -m loom_tools.snapshot --help' for usage", file=sys.stderr)
            sys.exit(1)

    snapshot = build_snapshot()

    if pretty:
        print(json.dumps(snapshot, indent=2))
    else:
        print(json.dumps(snapshot, separators=(",", ":")))


if __name__ == "__main__":
    main()
