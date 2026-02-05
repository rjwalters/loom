"""Analyze shepherd test failure patterns from progress files.

Parses ``.loom/progress/shepherd-*.json`` files to categorize blocked runs
by root cause, track Doctor effectiveness, and generate metrics for
reducing the test failure block rate.

Commands:
    summary         Overall test failure summary (default)
    categorize      Categorize each blocked run by root cause
    doctor          Doctor effectiveness analysis
    trends          Failure trends over time

Exit codes:
    0 - Success
    1 - Error
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import sys
from dataclasses import asdict, dataclass, field
from typing import Any

from loom_tools.common.logging import log_error, log_warning
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_progress_files
from loom_tools.models.progress import ShepherdProgress

# ANSI color codes
_RED = "\033[0;31m"
_GREEN = "\033[0;32m"
_YELLOW = "\033[1;33m"
_BLUE = "\033[0;34m"
_GRAY = "\033[0;90m"
_RESET = "\033[0m"


def _use_color() -> bool:
    import io

    try:
        return os.isatty(sys.stdout.fileno())
    except (OSError, ValueError, io.UnsupportedOperation):
        return False


def _c(code: str, text: str) -> str:
    if _use_color():
        return f"{code}{text}{_RESET}"
    return text


# ---------------------------------------------------------------------------
# Failure categories
# ---------------------------------------------------------------------------

CATEGORY_PRE_EXISTING = "pre_existing"
CATEGORY_BUILDER_BUG = "builder_bug"
CATEGORY_ENVIRONMENT = "environment"
CATEGORY_UNKNOWN = "unknown"

CATEGORY_LABELS = {
    CATEGORY_PRE_EXISTING: "Pre-existing failures on main",
    CATEGORY_BUILDER_BUG: "Builder-introduced bugs",
    CATEGORY_ENVIRONMENT: "Environment/infrastructure issues",
    CATEGORY_UNKNOWN: "Unknown/unclassified",
}


# ---------------------------------------------------------------------------
# Data models
# ---------------------------------------------------------------------------


@dataclass
class FailureCategorization:
    """Categorization result for a single blocked shepherd run."""

    task_id: str = ""
    issue: int = 0
    category: str = CATEGORY_UNKNOWN
    doctor_outcome: str = ""  # "skipped_unrelated", "attempted", "not_run"
    test_command: str = ""
    exit_code: int | None = None
    test_duration_seconds: int | None = None
    builder_duration_seconds: int | None = None
    has_pre_existing_signal: bool = False
    has_doctor_skip: bool = False
    has_doctor_attempted: bool = False
    details: str = ""


@dataclass
class DoctorEffectiveness:
    """Doctor effectiveness metrics."""

    total_invocations: int = 0
    skipped_unrelated: int = 0
    attempted: int = 0
    # Among attempted:
    succeeded: int = 0  # Run completed, not blocked after
    failed: int = 0  # Run completed, still blocked after


@dataclass
class AnalysisSummary:
    """Overall test failure analysis summary."""

    total_runs: int = 0
    completed_runs: int = 0
    blocked_runs: int = 0
    block_rate_percent: float = 0.0
    categories: dict[str, int] = field(default_factory=dict)
    doctor: DoctorEffectiveness = field(default_factory=DoctorEffectiveness)
    categorized_failures: list[FailureCategorization] = field(
        default_factory=list
    )


# ---------------------------------------------------------------------------
# Analysis functions
# ---------------------------------------------------------------------------


def _extract_blocked_details(progress: ShepherdProgress) -> dict[str, Any]:
    """Extract test failure details from milestone events."""
    details: dict[str, Any] = {
        "test_command": "",
        "exit_code": None,
        "test_duration_str": "",
        "has_pre_existing_signal": False,
        "doctor_outcome": "not_run",
        "builder_duration_seconds": None,
    }

    seen_blocked = False
    verify_count_after_blocked = 0

    for m in progress.milestones:
        # Extract blocked event details
        if m.event == "blocked" and m.data.get("reason") == "test_failure":
            seen_blocked = True
            detail_str = m.data.get("details", "")
            # Parse "test verification failed (pnpm check:ci:lite, exit code 101)"
            if "exit code" in detail_str:
                parts = detail_str.split("exit code")
                if len(parts) == 2:
                    code_str = parts[1].strip().rstrip(")")
                    try:
                        details["exit_code"] = int(code_str)
                    except ValueError:
                        pass
            # Extract test command
            if "(" in detail_str and "," in detail_str:
                cmd = detail_str.split("(")[1].split(",")[0].strip()
                details["test_command"] = cmd

        # Check for pre-existing failure heartbeat signals
        if m.event == "heartbeat":
            action = m.data.get("action", "")
            if "pre-existing failures" in action:
                details["has_pre_existing_signal"] = True
            # Extract test duration from "test verification failed (57s)"
            if "test verification failed" in action:
                duration_part = action.split("(")[-1].rstrip(")")
                details["test_duration_str"] = duration_part
            # Track post-blocked test verification (Doctor retries)
            if seen_blocked and "verifying tests:" in action:
                verify_count_after_blocked += 1

        # Check for doctor_testfix phase completion
        if (
            m.event == "phase_completed"
            and m.data.get("phase") == "doctor_testfix"
        ):
            status = m.data.get("status", "")
            if status == "skipped_unrelated":
                details["doctor_outcome"] = "skipped_unrelated"
            elif status == "success":
                details["doctor_outcome"] = "success"
            else:
                details["doctor_outcome"] = status or "completed"

        # Check for doctor heartbeats (indicates doctor was attempted)
        if m.event == "heartbeat" and "doctor running" in m.data.get(
            "action", ""
        ):
            if details["doctor_outcome"] == "not_run":
                details["doctor_outcome"] = "attempted"

        # Extract builder duration
        if (
            m.event == "phase_completed"
            and m.data.get("phase") == "builder"
        ):
            details["builder_duration_seconds"] = m.data.get(
                "duration_seconds"
            )

    # If there's a second test verification after blocked, Doctor ran
    # even if no explicit doctor phase events were recorded.
    if verify_count_after_blocked > 0 and details["doctor_outcome"] == "not_run":
        details["doctor_outcome"] = "attempted"

    return details


def categorize_failure(progress: ShepherdProgress) -> FailureCategorization:
    """Categorize a single blocked shepherd run by root cause."""
    details = _extract_blocked_details(progress)

    cat = FailureCategorization(
        task_id=progress.task_id,
        issue=progress.issue,
        test_command=details["test_command"],
        exit_code=details["exit_code"],
        builder_duration_seconds=details["builder_duration_seconds"],
        has_pre_existing_signal=details["has_pre_existing_signal"],
        has_doctor_skip=details["doctor_outcome"] == "skipped_unrelated",
        has_doctor_attempted=details["doctor_outcome"]
        in ("attempted", "success"),
        doctor_outcome=details["doctor_outcome"],
    )

    # Parse test duration
    dur_str = details.get("test_duration_str", "")
    if dur_str and dur_str.endswith("s"):
        try:
            cat.test_duration_seconds = int(dur_str[:-1])
        except ValueError:
            pass

    # Categorization logic
    if details["has_pre_existing_signal"]:
        cat.category = CATEGORY_PRE_EXISTING
        cat.details = "Heartbeat signaled pre-existing failures on main"
    elif details["doctor_outcome"] == "skipped_unrelated":
        # Doctor determined failures are unrelated to builder's changes.
        # This strongly suggests pre-existing or environmental issues.
        cat.category = CATEGORY_PRE_EXISTING
        cat.details = "Doctor skipped as unrelated (pre-existing or env)"
    elif cat.test_duration_seconds is not None and cat.test_duration_seconds <= 2:
        # Very fast test failures (1-2s) suggest environment issues
        # (missing deps, syntax errors in config, etc.)
        cat.category = CATEGORY_ENVIRONMENT
        cat.details = f"Instant failure ({cat.test_duration_seconds}s) suggests environment issue"
    elif details["doctor_outcome"] in ("attempted", "success"):
        # Doctor was invoked and ran - this means builder introduced a bug
        # that Doctor tried (possibly unsuccessfully) to fix.
        cat.category = CATEGORY_BUILDER_BUG
        cat.details = "Doctor attempted fix (builder-introduced failure)"
    else:
        cat.category = CATEGORY_UNKNOWN
        cat.details = f"Could not determine cause (doctor={details['doctor_outcome']})"

    return cat


def analyze_progress_files(
    progress_files: list[ShepherdProgress],
) -> AnalysisSummary:
    """Analyze all progress files and produce a summary."""
    summary = AnalysisSummary()
    summary.total_runs = len(progress_files)

    for pf in progress_files:
        if pf.status == "completed":
            summary.completed_runs += 1
        elif pf.status == "blocked":
            # Check if it's a test failure block
            is_test_failure = any(
                m.event == "blocked" and m.data.get("reason") == "test_failure"
                for m in pf.milestones
            )
            if is_test_failure:
                summary.blocked_runs += 1
                cat = categorize_failure(pf)
                summary.categorized_failures.append(cat)

                # Update category counts
                summary.categories[cat.category] = (
                    summary.categories.get(cat.category, 0) + 1
                )

                # Update doctor metrics
                if cat.has_doctor_skip:
                    summary.doctor.total_invocations += 1
                    summary.doctor.skipped_unrelated += 1
                elif cat.has_doctor_attempted:
                    summary.doctor.total_invocations += 1
                    summary.doctor.attempted += 1
                    # If still blocked after doctor attempted, it failed
                    summary.doctor.failed += 1

    if summary.total_runs > 0:
        summary.block_rate_percent = round(
            (summary.blocked_runs / summary.total_runs) * 100, 1
        )

    return summary


# ---------------------------------------------------------------------------
# Output formatting
# ---------------------------------------------------------------------------


def format_summary_text(summary: AnalysisSummary) -> str:
    """Format analysis summary as human-readable text."""
    lines: list[str] = []

    lines.append(_c(_BLUE, "=== Shepherd Test Failure Analysis ==="))
    lines.append("")

    # Overall stats
    lines.append(_c(_BLUE, "Overall Statistics:"))
    lines.append(f"  Total shepherd runs:  {summary.total_runs}")
    lines.append(f"  Completed:            {summary.completed_runs}")

    block_color = _RED if summary.block_rate_percent >= 20 else _YELLOW
    lines.append(
        f"  Blocked (test fail):  {_c(block_color, str(summary.blocked_runs))} "
        f"({_c(block_color, f'{summary.block_rate_percent}%')})"
    )
    lines.append("")

    # Category breakdown
    if summary.categories:
        lines.append(_c(_BLUE, "Failure Categories:"))
        for cat_key, count in sorted(
            summary.categories.items(), key=lambda x: -x[1]
        ):
            label = CATEGORY_LABELS.get(cat_key, cat_key)
            pct = round((count / summary.blocked_runs) * 100, 1) if summary.blocked_runs else 0
            lines.append(f"  {label}: {count} ({pct}%)")
        lines.append("")

    # Doctor effectiveness
    doc = summary.doctor
    if doc.total_invocations > 0:
        lines.append(_c(_BLUE, "Doctor Effectiveness:"))
        lines.append(f"  Total invocations:  {doc.total_invocations}")
        lines.append(f"  Skipped unrelated:  {doc.skipped_unrelated}")
        lines.append(f"  Attempted fixes:    {doc.attempted}")
        if doc.attempted > 0:
            lines.append(f"    Succeeded:        {doc.succeeded}")
            lines.append(f"    Failed:           {doc.failed}")
        skip_rate = round(
            (doc.skipped_unrelated / doc.total_invocations) * 100, 1
        )
        lines.append(f"  Skip rate:          {skip_rate}%")
        lines.append("")

    return "\n".join(lines)


def format_categorize_text(
    failures: list[FailureCategorization],
) -> str:
    """Format individual failure categorizations as text."""
    lines: list[str] = []
    lines.append(_c(_BLUE, "=== Blocked Shepherd Runs ==="))
    lines.append("")

    for f in failures:
        cat_color = {
            CATEGORY_PRE_EXISTING: _YELLOW,
            CATEGORY_BUILDER_BUG: _RED,
            CATEGORY_ENVIRONMENT: _GRAY,
            CATEGORY_UNKNOWN: _GRAY,
        }.get(f.category, _GRAY)

        lines.append(
            f"  #{f.issue} ({f.task_id}): "
            f"{_c(cat_color, CATEGORY_LABELS.get(f.category, f.category))}"
        )
        lines.append(f"    Test: {f.test_command or 'unknown'}")
        if f.exit_code is not None:
            lines.append(f"    Exit code: {f.exit_code}")
        if f.test_duration_seconds is not None:
            lines.append(f"    Test duration: {f.test_duration_seconds}s")
        lines.append(f"    Doctor: {f.doctor_outcome}")
        lines.append(f"    Details: {f.details}")
        lines.append("")

    return "\n".join(lines)


def format_doctor_text(summary: AnalysisSummary) -> str:
    """Format Doctor effectiveness analysis as text."""
    lines: list[str] = []
    lines.append(_c(_BLUE, "=== Doctor Effectiveness Analysis ==="))
    lines.append("")

    doc = summary.doctor
    if doc.total_invocations == 0:
        lines.append("  No Doctor invocations found in progress data.")
        return "\n".join(lines)

    lines.append(f"  Total invocations:    {doc.total_invocations}")
    lines.append(
        f"  Skipped (unrelated):  {doc.skipped_unrelated} "
        f"({round(doc.skipped_unrelated / doc.total_invocations * 100, 1)}%)"
    )
    lines.append(
        f"  Attempted fixes:      {doc.attempted} "
        f"({round(doc.attempted / doc.total_invocations * 100, 1)}%)"
    )

    if doc.attempted > 0:
        lines.append("")
        lines.append("  Fix attempts breakdown:")
        lines.append(
            f"    Succeeded: {doc.succeeded} "
            f"({round(doc.succeeded / doc.attempted * 100, 1)}%)"
        )
        lines.append(
            f"    Failed:    {doc.failed} "
            f"({round(doc.failed / doc.attempted * 100, 1)}%)"
        )

    lines.append("")
    lines.append(_c(_BLUE, "  Patterns in Doctor Skips:"))

    # Group skip reasons from failures
    skip_failures = [
        f
        for f in summary.categorized_failures
        if f.has_doctor_skip
    ]
    if skip_failures:
        exit_codes: dict[int | None, int] = {}
        for f in skip_failures:
            exit_codes[f.exit_code] = exit_codes.get(f.exit_code, 0) + 1
        for code, count in sorted(exit_codes.items(), key=lambda x: -x[1]):
            lines.append(f"    Exit code {code}: {count} occurrences")
    else:
        lines.append("    No skip patterns found")

    lines.append("")
    return "\n".join(lines)


def format_json(summary: AnalysisSummary) -> str:
    """Format analysis as JSON."""
    data: dict[str, Any] = {
        "total_runs": summary.total_runs,
        "completed_runs": summary.completed_runs,
        "blocked_runs": summary.blocked_runs,
        "block_rate_percent": summary.block_rate_percent,
        "categories": summary.categories,
        "doctor": {
            "total_invocations": summary.doctor.total_invocations,
            "skipped_unrelated": summary.doctor.skipped_unrelated,
            "attempted": summary.doctor.attempted,
            "succeeded": summary.doctor.succeeded,
            "failed": summary.doctor.failed,
        },
        "failures": [asdict(f) for f in summary.categorized_failures],
    }
    return json.dumps(data, indent=2)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Analyze shepherd test failure patterns"
    )
    parser.add_argument(
        "command",
        nargs="?",
        default="summary",
        choices=["summary", "categorize", "doctor", "trends"],
        help="Analysis command (default: summary)",
    )
    parser.add_argument(
        "--format",
        choices=["text", "json"],
        default="text",
        help="Output format (default: text)",
    )
    parser.add_argument(
        "--repo-root",
        type=pathlib.Path,
        default=None,
        help="Repository root (auto-detected if not provided)",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)

    repo_root = args.repo_root
    if repo_root is None:
        repo_root = find_repo_root()
        if repo_root is None:
            log_error("Could not find repository root")
            return 1

    progress = read_progress_files(repo_root)
    if not progress:
        log_warning("No shepherd progress files found")
        return 0

    summary = analyze_progress_files(progress)

    if args.format == "json":
        print(format_json(summary))
    elif args.command == "summary":
        print(format_summary_text(summary))
    elif args.command == "categorize":
        print(format_categorize_text(summary.categorized_failures))
    elif args.command == "doctor":
        print(format_doctor_text(summary))
    elif args.command == "trends":
        # Trends just shows summary for now - could be enhanced with time bucketing
        print(format_summary_text(summary))

    return 0


if __name__ == "__main__":
    sys.exit(main())
