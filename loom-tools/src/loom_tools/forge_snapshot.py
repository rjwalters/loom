"""Forge-derived pipeline snapshot for the spawn-loop orchestrator.

Phase 3.2 of the shepherd/daemon deprecation (#3372, #3399): this module
is the kept half of the former ``snapshot.py``.  It contains only the
forge-query orchestration (``collect_pipeline_data``) and the
issue/PR sorting, filtering, and detection helpers that depend exclusively
on forge data.

The daemon-brain half of ``snapshot.py`` (``build_snapshot``,
``SnapshotConfig``, ``SupportRoleState``, ``compute_support_role_state``,
``compute_pipeline_health``, ``compute_systematic_failure_state``,
``compute_recommended_actions``, ``compute_health``, etc.) was deleted in
this PR because the daemon brain (``daemon_v2/``) is gone.

Usage::

    from loom_tools.forge_snapshot import collect_pipeline_data
    pipeline = collect_pipeline_data(repo_root)
"""

from __future__ import annotations

import pathlib
import re
import subprocess
from dataclasses import dataclass
from typing import Any, Sequence

from loom_tools.common.forge import get_forge
from loom_tools.common.github import gh_parallel_queries
from loom_tools.common.issue_failures import load_failure_log, IssueFailureLog
from loom_tools.common.logging import log_info, log_warning

# ---------------------------------------------------------------------------
# Pipeline data collection (parallel gh queries)
# ---------------------------------------------------------------------------

_ISSUE_FIELDS = ["number", "title", "labels", "createdAt", "body"]

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

    # Run CI health check if enabled and CI workflows exist
    ci_status: dict[str, Any] = {"status": "unknown", "message": "CI health check disabled"}
    if ci_health_check_enabled:
        workflows_dir = repo_root / ".github" / "workflows"
        if workflows_dir.is_dir() and any(workflows_dir.iterdir()):
            # Use forge-agnostic CI status (works for both GitHub and Gitea)
            forge = get_forge()
            forge_ci = forge.get_default_branch_ci_status()
            ci_status = {
                "status": forge_ci.status,
                "failed_runs": forge_ci.failed_runs,
                "total_runs": forge_ci.total_runs,
                "message": forge_ci.message,
            }
        else:
            ci_status = {"status": "no_ci", "message": "No CI workflows configured (.github/workflows/ not found)"}

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
        current_iteration: Current iteration number.

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
# Orphaned PR detection
# ---------------------------------------------------------------------------

@dataclass
class OrphanedPR:
    """A PR that needs attention but has no active sweep tracking it."""

    pr_number: int
    needed_role: str  # "judge" or "doctor"

    def to_dict(self) -> dict[str, Any]:
        return {"pr_number": self.pr_number, "needed_role": self.needed_role}


def detect_orphaned_prs(
    review_requested: list[dict[str, Any]],
    changes_requested: list[dict[str, Any]],
    tracked_pr_numbers: set[int] | None = None,
    merge_conflicted: list[dict[str, Any]] | None = None,
) -> list[OrphanedPR]:
    """Detect PRs needing attention that no active sweep is tracking.

    A PR is orphaned when it has ``loom:review-requested`` (needs judge),
    ``loom:changes-requested`` (needs doctor), or ``loom:pr`` +
    ``loom:merge-conflict`` (approved but blocked by conflicts, needs doctor)
    but is not in the *tracked_pr_numbers* set.

    Returns orphaned PRs sorted by PR number ascending (FIFO).

    Args:
        review_requested: PRs with loom:review-requested label.
        changes_requested: PRs with loom:changes-requested label.
        tracked_pr_numbers: Set of PR numbers currently being tracked by
            active sweeps. Pass None or empty set to treat all PRs as orphaned.
        merge_conflicted: PRs with both loom:pr and loom:merge-conflict labels.
    """
    tracked_prs: set[int] = tracked_pr_numbers or set()

    orphaned: list[OrphanedPR] = []

    for pr in review_requested:
        pr_num = pr.get("number")
        if pr_num is not None and pr_num not in tracked_prs:
            orphaned.append(OrphanedPR(pr_number=pr_num, needed_role="judge"))

    for pr in changes_requested:
        pr_num = pr.get("number")
        if pr_num is not None and pr_num not in tracked_prs:
            orphaned.append(OrphanedPR(pr_number=pr_num, needed_role="doctor"))

    # Approved PRs with merge conflicts need doctor attention (issue #3104)
    for pr in merge_conflicted or []:
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
    changes, indicating a build->review->fix->review loop that isn't
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
    """Count the number of review rounds (CHANGES_REQUESTED) on a PR."""
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
    """Extract linked issue number from a PR's body (looks for 'Closes #N')."""
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
