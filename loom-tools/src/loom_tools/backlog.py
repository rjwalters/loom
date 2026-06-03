"""Backlog management utilities for the Loom backlog of failed issues.

Provides tools for bulk-triaging blocked issues, applying the tiered retry
policy retroactively, and escalating permanently-stuck issues to a
human-review queue.

**Source of truth.** Reads exclusively from ``.loom/issue-failures.json``
(the durable cross-session failure log) — the ephemeral
``.loom/daemon-state.json::blocked_issue_retries`` read path that this
module used previously has been removed as part of the Phase 3
shepherd/daemon deprecation (epic #3372, tracker #3378). Unlike sibling
Phase 3.1.x CLI ports, this consumer does **not** keep a fallback to
``daemon-state.json``.

**Escalation persistence.** Without ``daemon-state.json::needs_human_input``
to coordinate against, escalated issues are marked on the forge instead
via the existing ``loom:blocked`` label. The label acts as the
deduplication signal — re-running ``prune`` will skip issues that already
carry ``loom:blocked``. The optional ``--comment`` flag still posts a
human-readable comment alongside the label.

Usage::

    loom-backlog list                            # list blocked issues with policy info
    loom-backlog prune [--dry-run] [--comment]   # triage and escalate exhausted backlog
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

from loom_tools.common.github import gh_run
from loom_tools.common.issue_failures import IssueFailureEntry, load_failure_log
from loom_tools.common.logging import log_info, log_warning
from loom_tools.common.repo import find_repo_root
from loom_tools.common.time_utils import now_utc


# ---------------------------------------------------------------------------
# Tiered retry policy by error class
#
# Inlined here (copied from the historical ``snapshot.get_retry_policy``)
# so that ``loom-backlog`` has no runtime dependency on ``snapshot.py``,
# which is slated for deletion in Phase 3.2. Keep this table in sync with
# any other consumer that may still reference ``_ERROR_CLASS_POLICIES``
# until the daemon brain is removed.
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class RetryPolicy:
    """Per-error-class retry configuration.

    Attributes:
        cooldown: Fixed cooldown in seconds between retry attempts (no
            exponential backoff).
        max_retries: Maximum number of retry attempts before escalating.
        escalate: Whether to add to the human-review queue when the retry
            budget is exhausted.
    """

    cooldown: int
    max_retries: int
    escalate: bool


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

# Safe default for unknown error classes (mirrors the historical
# ``snapshot.get_retry_policy`` fallback for a ``None`` ``SnapshotConfig``).
_DEFAULT_POLICY = RetryPolicy(cooldown=1800, max_retries=3, escalate=True)


def get_retry_policy(error_class: str) -> RetryPolicy:
    """Return the retry policy for *error_class*.

    Known error classes use fixed per-class policies; unknown classes fall
    back to ``_DEFAULT_POLICY``.
    """
    return _ERROR_CLASS_POLICIES.get(error_class, _DEFAULT_POLICY)


# ---------------------------------------------------------------------------
# Forge helpers
# ---------------------------------------------------------------------------


_ESCALATION_LABEL = "loom:blocked"


def _fetch_escalated_issue_numbers() -> set[int]:
    """Return the set of issue numbers already carrying the escalation label.

    Uses a single ``gh issue list`` call so we do not pay one API round-trip
    per issue. Returns an empty set on any failure (callers fall back to
    "assume not yet escalated" semantics so the operator can still escalate).
    """
    try:
        result = gh_run(
            [
                "issue",
                "list",
                "--label",
                _ESCALATION_LABEL,
                "--state",
                "all",
                "--limit",
                "1000",
                "--json",
                "number",
            ],
            check=False,
        )
    except Exception:
        log_warning(f"Failed to list issues with label '{_ESCALATION_LABEL}'")
        return set()

    if getattr(result, "returncode", 1) != 0:
        return set()

    try:
        payload = json.loads(result.stdout or "[]")
    except (json.JSONDecodeError, AttributeError):
        return set()

    return {int(item["number"]) for item in payload if "number" in item}


def _apply_escalation_label(issue_num: int) -> bool:
    """Idempotently apply the escalation label to *issue_num*.

    Returns ``True`` on success, ``False`` otherwise. ``gh issue edit
    --add-label`` is itself idempotent (no-op if the label is already
    present), so we do not need to recheck.
    """
    try:
        result = gh_run(
            ["issue", "edit", str(issue_num), "--add-label", _ESCALATION_LABEL],
            check=False,
        )
    except Exception:
        log_warning(f"Failed to apply '{_ESCALATION_LABEL}' to issue #{issue_num}")
        return False
    return getattr(result, "returncode", 1) == 0


# ---------------------------------------------------------------------------
# Table rendering
# ---------------------------------------------------------------------------


def _print_table(rows: list[dict], columns: list[str]) -> None:
    """Print a simple ASCII table."""
    if not rows:
        return
    widths = {col: len(col) for col in columns}
    for row in rows:
        for col in columns:
            widths[col] = max(widths[col], len(str(row.get(col, ""))))

    header = "  ".join(col.ljust(widths[col]) for col in columns)
    separator = "  ".join("-" * widths[col] for col in columns)
    print(header)
    print(separator)
    for row in rows:
        print("  ".join(str(row.get(col, "")).ljust(widths[col]) for col in columns))


def _build_entry_row(
    issue_key: str,
    entry: IssueFailureEntry,
    escalated_numbers: set[int],
) -> dict:
    """Build a display row for an ``issue-failures.json`` entry.

    ``escalated_numbers`` is the set of issue numbers already carrying
    the escalation label on the forge; entries in this set are reported
    as ``escalated``.
    """
    policy = get_retry_policy(entry.error_class)
    retries_left = max(0, policy.max_retries - entry.total_failures)
    cooldown_h = policy.cooldown // 3600
    cooldown_str = (
        f"{cooldown_h}h"
        if cooldown_h > 0
        else (f"{policy.cooldown // 60}m" if policy.cooldown > 0 else "0")
    )
    exhausted = entry.total_failures >= policy.max_retries
    is_escalated = entry.issue in escalated_numbers
    if is_escalated:
        status = "escalated"
    elif exhausted:
        status = "exhausted"
    else:
        status = "retryable"
    return {
        "issue": f"#{issue_key}",
        "error_class": entry.error_class,
        "retry_count": entry.total_failures,
        "max_retries": policy.max_retries,
        "retries_left": retries_left,
        "cooldown": cooldown_str,
        "escalate": "yes" if policy.escalate else "no",
        "status": status,
    }


# ---------------------------------------------------------------------------
# Subcommands
# ---------------------------------------------------------------------------


def _load_entries(repo_root: Path) -> dict[str, IssueFailureEntry]:
    """Load issue-failures entries with decay-on-main-advance disabled.

    The CLI is meant to be a read-mostly tool; suppress the natural decay
    side effect (which rewrites the file) by passing ``_main_sha=None``.
    """
    log = load_failure_log(repo_root, _main_sha=None)
    return log.entries


def cmd_list(repo_root: Path) -> int:
    """List all tracked failure entries with their tiered retry policy info."""
    failures_file = repo_root / ".loom" / "issue-failures.json"

    if not failures_file.exists():
        print("No issue-failures.json found.")
        return 1

    entries = _load_entries(repo_root)

    if not entries:
        print("No tracked issue failures.")
        return 0

    escalated_numbers = _fetch_escalated_issue_numbers()

    rows = []
    for issue_key, entry in sorted(entries.items(), key=lambda x: int(x[0])):
        rows.append(_build_entry_row(issue_key, entry, escalated_numbers))

    total = len(rows)
    escalated = sum(1 for r in rows if r["status"] == "escalated")
    exhausted = sum(1 for r in rows if r["status"] == "exhausted")
    retryable = sum(1 for r in rows if r["status"] == "retryable")

    print(
        f"Tracked issue failures: {total} total  "
        f"({retryable} retryable, {exhausted} exhausted, {escalated} escalated)"
    )
    print()

    columns = [
        "issue",
        "error_class",
        "retry_count",
        "max_retries",
        "cooldown",
        "escalate",
        "status",
    ]
    _print_table(rows, columns)
    return 0


def cmd_prune(
    repo_root: Path,
    *,
    dry_run: bool = False,
    add_comment: bool = False,
) -> int:
    """Apply tiered retry policy retroactively to all tracked failures.

    Issues that have exceeded their per-class retry budget and whose error
    class has ``escalate=True`` will be:

    - Labelled with ``loom:blocked`` on the forge (idempotent).
    - Optionally commented on (with ``--comment``).

    Pass ``--dry-run`` to preview without touching the forge.

    The ``loom:blocked`` label acts as the deduplication signal: issues
    that already carry the label are reported as "already escalated" and
    not re-touched.
    """
    failures_file = repo_root / ".loom" / "issue-failures.json"

    if not failures_file.exists():
        print("No issue-failures.json found.")
        return 1

    entries = _load_entries(repo_root)

    if not entries:
        print("No tracked issue failures.")
        return 0

    escalated_numbers = _fetch_escalated_issue_numbers()
    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

    # Categorise entries by action needed.
    to_escalate: list[tuple[str, IssueFailureEntry, str]] = []
    already_escalated: list[str] = []
    still_retryable: list[str] = []
    transient_exhausted: list[str] = []

    for issue_key, entry in sorted(entries.items(), key=lambda x: int(x[0])):
        policy = get_retry_policy(entry.error_class)

        if entry.issue in escalated_numbers:
            already_escalated.append(issue_key)
            continue

        exhausted = entry.total_failures >= policy.max_retries
        if exhausted:
            if policy.escalate:
                reason = (
                    f"Exceeded {policy.max_retries} retries for {entry.error_class}"
                    if policy.max_retries > 0
                    else f"Error class {entry.error_class} requires immediate human review"
                )
                to_escalate.append((issue_key, entry, reason))
            else:
                transient_exhausted.append(issue_key)
        else:
            still_retryable.append(issue_key)

    # Summary
    print(f"Backlog prune summary ({timestamp}):")
    print(f"  Total tracked failures:   {len(entries)}")
    print(f"  Already escalated:        {len(already_escalated)}  (carrying '{_ESCALATION_LABEL}')")
    print(f"  To escalate (this run):   {len(to_escalate)}")
    print(
        f"  Transient exhausted:      {len(transient_exhausted)} "
        f"(no escalation, error class non-critical)"
    )
    print(f"  Still retryable:          {len(still_retryable)}")
    print()

    if not to_escalate:
        print("Nothing to escalate.")
        return 0

    print("Issues to escalate:")
    for issue_key, entry, reason in to_escalate:
        print(
            f"  #{issue_key}  {entry.error_class} "
            f"(retry_count={entry.total_failures})  {reason}"
        )

    if dry_run:
        print("\n[dry-run] No changes made.")
        return 0

    print()

    escalated_count = 0
    for issue_key, entry, reason in to_escalate:
        issue_num = int(issue_key)

        if not _apply_escalation_label(issue_num):
            log_warning(f"Skipping issue #{issue_num}: failed to apply escalation label")
            continue

        if add_comment:
            comment = (
                f"**Blocked Issue: Human Review Required (backlog prune)**\n\n"
                f"This issue has exceeded its automatic retry budget and was identified "
                f"during a backlog triage run.\n\n"
                f"**Error class**: `{entry.error_class}`\n"
                f"**Retry attempts**: {entry.total_failures}\n"
                f"**Reason**: {reason}\n\n"
                f"Please review this issue and either:\n"
                f"- Fix the underlying problem and remove the `{_ESCALATION_LABEL}` label to re-queue it, or\n"
                f"- Close the issue if it is no longer valid\n\n"
                f"---\n"
                f"*Escalated by `loom-backlog prune` at {timestamp}*"
            )
            try:
                gh_run(
                    ["issue", "comment", str(issue_num), "--body", comment],
                    check=False,
                )
                log_info(f"Commented on issue #{issue_num}")
            except Exception:
                log_warning(f"Failed to comment on issue #{issue_num}")

        escalated_count += 1

    print(
        f"Escalated {escalated_count} issue(s) via '{_ESCALATION_LABEL}' label."
    )
    if add_comment:
        print("GitHub comments added.")
    else:
        print("(Use --comment to also post GitHub comments.)")

    return 0


def main(argv: Sequence[str] | None = None) -> int:
    """Entry point for the loom-backlog command."""
    parser = argparse.ArgumentParser(
        prog="loom-backlog",
        description=(
            "Backlog management for tracked issue failures "
            "(reads .loom/issue-failures.json)"
        ),
    )
    subparsers = parser.add_subparsers(dest="command", help="Subcommand")

    subparsers.add_parser(
        "list",
        help="List tracked failures with tiered retry policy info",
    )

    prune_parser = subparsers.add_parser(
        "prune",
        help=(
            "Apply tiered retry policy retroactively; label exhausted issues "
            "with 'loom:blocked' for human review"
        ),
    )
    prune_parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Preview changes without touching the forge",
    )
    prune_parser.add_argument(
        "--comment",
        action="store_true",
        help="Also post GitHub comments on escalated issues",
    )

    args = parser.parse_args(argv)

    if not args.command:
        parser.print_help()
        return 1

    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        print("Error: not in a git repository with .loom directory", file=sys.stderr)
        return 1

    if args.command == "list":
        return cmd_list(repo_root)
    elif args.command == "prune":
        return cmd_prune(repo_root, dry_run=args.dry_run, add_comment=args.comment)
    else:
        parser.print_help()
        return 1


if __name__ == "__main__":
    sys.exit(main())
