"""Backlog management utilities for the Loom daemon.

Provides tools for bulk-triaging blocked issues, applying the tiered retry
policy retroactively, and escalating permanently-stuck issues to the human
input queue.

Usage::

    loom-backlog prune [--dry-run] [--comment]   # triage blocked backlog
    loom-backlog list                             # list blocked issues with policy info
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Sequence

from loom_tools.common.github import gh_run
from loom_tools.common.logging import log_info, log_warning
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_daemon_state, write_json_file
from loom_tools.common.time_utils import now_utc
from loom_tools.models.daemon_state import BlockedIssueRetry
from loom_tools.snapshot import get_retry_policy


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


def _build_entry_row(issue_key: str, entry: BlockedIssueRetry) -> dict:
    """Build a display row for a blocked issue entry."""
    policy = get_retry_policy(entry.error_class)
    retries_left = max(0, policy.max_retries - entry.retry_count)
    cooldown_h = policy.cooldown // 3600
    cooldown_str = f"{cooldown_h}h" if cooldown_h > 0 else (f"{policy.cooldown // 60}m" if policy.cooldown > 0 else "0")
    status = "escalated" if entry.escalated_to_human else (
        "exhausted" if (entry.retry_exhausted or entry.retry_count >= policy.max_retries) else "retryable"
    )
    return {
        "issue": f"#{issue_key}",
        "error_class": entry.error_class,
        "retry_count": entry.retry_count,
        "max_retries": policy.max_retries,
        "retries_left": retries_left,
        "cooldown": cooldown_str,
        "escalate": "yes" if policy.escalate else "no",
        "status": status,
    }


def cmd_list(repo_root: Path) -> int:
    """List all blocked issues with their tiered retry policy info."""
    loom_dir = repo_root / ".loom"
    state_file = loom_dir / "daemon-state.json"

    if not state_file.exists():
        print("No daemon-state.json found.")
        return 1

    state = read_daemon_state(repo_root)
    retries = state.blocked_issue_retries

    if not retries:
        print("No blocked issues in daemon state.")
        return 0

    rows = []
    for issue_key, entry in sorted(retries.items(), key=lambda x: int(x[0])):
        rows.append(_build_entry_row(issue_key, entry))

    # Summary stats
    total = len(rows)
    escalated = sum(1 for r in rows if r["status"] == "escalated")
    exhausted = sum(1 for r in rows if r["status"] == "exhausted")
    retryable = sum(1 for r in rows if r["status"] == "retryable")

    print(f"Blocked issues: {total} total  ({retryable} retryable, {exhausted} exhausted, {escalated} escalated)")
    print()

    columns = ["issue", "error_class", "retry_count", "max_retries", "cooldown", "escalate", "status"]
    _print_table(rows, columns)
    return 0


def cmd_prune(
    repo_root: Path,
    *,
    dry_run: bool = False,
    add_comment: bool = False,
) -> int:
    """Apply tiered retry policy retroactively to all blocked issues.

    Issues that have exceeded their per-class retry budget will be:
    - Marked ``escalated_to_human=True`` in daemon state
    - Added to ``needs_human_input``
    - Optionally commented on GitHub (with ``--comment``)

    Pass ``--dry-run`` to preview changes without modifying daemon state.
    """
    loom_dir = repo_root / ".loom"
    state_file = loom_dir / "daemon-state.json"

    if not state_file.exists():
        print("No daemon-state.json found.")
        return 1

    state = read_daemon_state(repo_root)
    retries = state.blocked_issue_retries

    if not retries:
        print("No blocked issues in daemon state.")
        return 0

    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

    # Categorise issues by action needed
    to_escalate: list[tuple[str, BlockedIssueRetry, str]] = []  # (key, entry, reason)
    already_escalated: list[str] = []
    still_retryable: list[str] = []
    transient_exhausted: list[str] = []

    for issue_key, entry in sorted(retries.items(), key=lambda x: int(x[0])):
        policy = get_retry_policy(entry.error_class)

        if entry.escalated_to_human:
            already_escalated.append(issue_key)
            continue

        exhausted = entry.retry_exhausted or entry.retry_count >= policy.max_retries
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

    # Print summary
    print(f"Backlog prune summary ({timestamp}):")
    print(f"  Total blocked issues:     {len(retries)}")
    print(f"  Already escalated:        {len(already_escalated)}")
    print(f"  To escalate (this run):   {len(to_escalate)}")
    print(f"  Transient exhausted:      {len(transient_exhausted)} (no escalation, error class non-critical)")
    print(f"  Still retryable:          {len(still_retryable)}")
    print()

    if not to_escalate:
        print("Nothing to escalate.")
        return 0

    print("Issues to escalate:")
    for issue_key, entry, reason in to_escalate:
        print(f"  #{issue_key}  {entry.error_class} (retry_count={entry.retry_count})  {reason}")

    if dry_run:
        print("\n[dry-run] No changes made.")
        return 0

    print()

    # Apply escalations
    escalated_count = 0
    for issue_key, entry, reason in to_escalate:
        issue_num = int(issue_key)

        # Mark escalated in state
        entry.escalated_to_human = True

        # Add to needs_human_input
        human_entry = {
            "type": "exhausted_retry",
            "issue": issue_num,
            "error_class": entry.error_class,
            "retry_count": entry.retry_count,
            "reason": reason,
            "escalated_at": timestamp,
        }
        already_present = any(
            e.get("type") == "exhausted_retry" and e.get("issue") == issue_num
            for e in state.needs_human_input
        )
        if not already_present:
            state.needs_human_input.append(human_entry)

        # Optionally add GitHub comment
        if add_comment:
            comment = (
                f"**Blocked Issue: Human Review Required (backlog prune)**\n\n"
                f"This issue has exceeded its automatic retry budget and was identified "
                f"during a backlog triage run.\n\n"
                f"**Error class**: `{entry.error_class}`\n"
                f"**Retry attempts**: {entry.retry_count}\n"
                f"**Reason**: {reason}\n\n"
                f"Please review this issue and either:\n"
                f"- Fix the underlying problem and remove the `loom:blocked` label to re-queue it, or\n"
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

    # Save updated state
    write_json_file(state_file, state.to_dict())

    print(f"Escalated {escalated_count} issue(s) to needs_human_input in daemon state.")
    if add_comment:
        print("GitHub comments added.")
    else:
        print("(Use --comment to also post GitHub comments.)")

    return 0


def main(argv: Sequence[str] | None = None) -> int:
    """Entry point for the loom-backlog command."""
    parser = argparse.ArgumentParser(
        prog="loom-backlog",
        description="Backlog management for the Loom daemon",
    )
    subparsers = parser.add_subparsers(dest="command", help="Subcommand")

    # list subcommand
    subparsers.add_parser(
        "list",
        help="List blocked issues with tiered retry policy info",
    )

    # prune subcommand
    prune_parser = subparsers.add_parser(
        "prune",
        help=(
            "Apply tiered retry policy retroactively; escalate exhausted issues "
            "to the human input queue"
        ),
    )
    prune_parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Preview changes without modifying daemon state",
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
