"""Diagnostic health check for Loom orchestration.

This module provides functionality to:
- Validate spawn-loop state file structure and integrity
- Query GitHub for pipeline state (label counts)
- Detect orphaned loom:building issues (not tracked by spawn loop)
- Detect stale loom:building issues (no PR after threshold)

Exit codes:
    0 - Healthy (no warnings or critical issues)
    1 - Warnings detected (degraded but functional)
    2 - Critical issues (state corruption, orphaned work)
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from dataclasses import dataclass, field
from typing import Any

from loom_tools.common.github import gh_parallel_queries, gh_pr_list
from loom_tools.common.paths import LoomPaths
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_spawn_loop_state
from loom_tools.common.time_utils import elapsed_seconds, format_duration


# Default thresholds
STALE_BUILDING_MINUTES = int(os.environ.get("LOOM_STALE_BUILDING_MINUTES", "15"))

# Support role expected intervals (seconds)
GUIDE_INTERVAL = int(os.environ.get("LOOM_GUIDE_INTERVAL", "900"))  # 15 minutes
CHAMPION_INTERVAL = int(os.environ.get("LOOM_CHAMPION_INTERVAL", "600"))  # 10 minutes
DOCTOR_INTERVAL = int(os.environ.get("LOOM_DOCTOR_INTERVAL", "300"))  # 5 minutes
AUDITOR_INTERVAL = int(os.environ.get("LOOM_AUDITOR_INTERVAL", "600"))  # 10 minutes
JUDGE_INTERVAL = int(os.environ.get("LOOM_JUDGE_INTERVAL", "300"))  # 5 minutes

SUPPORT_ROLE_INTERVALS = {
    "guide": (GUIDE_INTERVAL, "15 min"),
    "judge": (JUDGE_INTERVAL, "5 min"),
    "champion": (CHAMPION_INTERVAL, "10 min"),
    "doctor": (DOCTOR_INTERVAL, "5 min"),
    "auditor": (AUDITOR_INTERVAL, "10 min"),
}


@dataclass
class ValidationResult:
    """Result of state file validation."""

    valid: bool = True
    status: str = "ok"  # ok, missing, corrupt, incomplete
    missing_fields: list[str] = field(default_factory=list)
    details: str = ""


@dataclass
class SupportRoleStatus:
    """Status of a support role."""

    name: str
    elapsed: str  # "NEVER_SPAWNED", "UNKNOWN", or duration string
    interval: str  # Expected interval string
    status: str  # "idle", "running", etc.


@dataclass
class StaleIssue:
    """An issue that has been in building state too long without a PR."""

    number: int
    age_minutes: int


@dataclass
class PipelineState:
    """Current state of the pipeline from GitHub."""

    ready: list[dict[str, Any]] = field(default_factory=list)
    building: list[dict[str, Any]] = field(default_factory=list)
    review_requested: list[dict[str, Any]] = field(default_factory=list)
    ready_to_merge: list[dict[str, Any]] = field(default_factory=list)
    blocked: list[dict[str, Any]] = field(default_factory=list)

    @property
    def ready_count(self) -> int:
        return len(self.ready)

    @property
    def building_count(self) -> int:
        return len(self.building)

    @property
    def review_requested_count(self) -> int:
        return len(self.review_requested)

    @property
    def ready_to_merge_count(self) -> int:
        return len(self.ready_to_merge)

    @property
    def blocked_count(self) -> int:
        return len(self.blocked)


@dataclass
class HealthReport:
    """Complete health report for the Loom orchestration system."""

    state_file_path: str = ""
    validation: ValidationResult = field(default_factory=ValidationResult)
    daemon_running: bool = False
    daemon_started_at: str = ""
    pipeline: PipelineState = field(default_factory=PipelineState)
    orphaned_building: list[int] = field(default_factory=list)
    stale_building: list[StaleIssue] = field(default_factory=list)
    support_roles: list[SupportRoleStatus] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)
    criticals: list[str] = field(default_factory=list)
    recommendations: list[str] = field(default_factory=list)

    @property
    def exit_code(self) -> int:
        """Determine exit code based on health status."""
        if self.criticals:
            return 2
        if self.warnings:
            return 1
        return 0

    def add_warning(self, msg: str) -> None:
        """Add a warning message."""
        self.warnings.append(msg)

    def add_critical(self, msg: str) -> None:
        """Add a critical message."""
        self.criticals.append(msg)

    def add_recommendation(self, msg: str) -> None:
        """Add a recommendation message."""
        self.recommendations.append(msg)


def validate_state_file(state_file_path: str) -> ValidationResult:
    """Validate the spawn-loop state file exists and is parseable."""
    import pathlib
    import json as json_module

    path = pathlib.Path(state_file_path)
    result = ValidationResult()

    if not path.exists():
        result.valid = False
        result.status = "missing"
        result.details = f"No state file found at {state_file_path}"
        return result

    try:
        text = path.read_text()
        if not text.strip():
            result.valid = False
            result.status = "corrupt"
            result.details = "State file is empty"
            return result
        data = json_module.loads(text)
        if isinstance(data, list):
            result.valid = False
            result.status = "corrupt"
            result.details = "State file contains a list instead of an object"
            return result
    except json_module.JSONDecodeError:
        result.valid = False
        result.status = "corrupt"
        result.details = "State file contains invalid JSON"
        return result
    except Exception:
        result.valid = False
        result.status = "corrupt"
        result.details = "State file is unreadable"
        return result

    result.valid = True
    result.status = "ok"
    return result


def get_pipeline_state() -> PipelineState:
    """Query GitHub for current pipeline state."""
    queries = [
        (["issue", "list", "--label", "loom:issue", "--state", "open", "--json", "number,title"],),
        (["issue", "list", "--label", "loom:building", "--state", "open", "--json", "number,title,createdAt,updatedAt"],),
        (["pr", "list", "--label", "loom:review-requested", "--state", "open", "--json", "number,title"],),
        (["pr", "list", "--label", "loom:pr", "--state", "open", "--json", "number,title"],),
        (["issue", "list", "--label", "loom:blocked", "--state", "open", "--json", "number,title"],),
    ]

    results = gh_parallel_queries(queries)

    return PipelineState(
        ready=results[0],
        building=results[1],
        review_requested=results[2],
        ready_to_merge=results[3],
        blocked=results[4],
    )


def check_orphaned_building(
    building_issues: list[dict[str, Any]],
    tracked_issues: set[int] | None = None,
) -> list[int]:
    """Find issues labeled loom:building but not tracked by any active task."""
    if not building_issues:
        return []

    if tracked_issues is None:
        tracked_issues = set()

    orphaned = []
    for issue in building_issues:
        issue_num = issue.get("number")
        if issue_num and issue_num not in tracked_issues:
            orphaned.append(issue_num)

    return orphaned


def check_stale_building(
    building_issues: list[dict[str, Any]],
    threshold_minutes: int = STALE_BUILDING_MINUTES,
) -> list[StaleIssue]:
    """Find issues in building state for too long without a PR."""
    if not building_issues:
        return []

    threshold_secs = threshold_minutes * 60
    stale = []

    # Get all open PRs for matching
    try:
        open_prs = gh_pr_list(
            state="open",
            fields=["number", "headRefName", "body"],
        )
    except Exception:
        open_prs = []

    for issue in building_issues:
        issue_num = issue.get("number")
        if not issue_num:
            continue

        updated_at = issue.get("updatedAt") or issue.get("createdAt")
        if not updated_at:
            continue

        try:
            age_secs = elapsed_seconds(updated_at)
        except Exception:
            continue

        if age_secs < threshold_secs:
            continue

        # Check if a PR exists for this issue
        has_pr = False
        for pr in open_prs:
            body = pr.get("body", "") or ""
            head_ref = pr.get("headRefName", "") or ""
            # Check for "Closes #N", "Fixes #N", "Resolves #N" in body
            pattern = rf"(Closes|Fixes|Resolves) #{issue_num}\b"
            if re.search(pattern, body, re.IGNORECASE):
                has_pr = True
                break
            # Check for issue-N in branch name
            if re.search(rf"issue-{issue_num}\b", head_ref):
                has_pr = True
                break

        if not has_pr:
            stale.append(StaleIssue(number=issue_num, age_minutes=age_secs // 60))

    return stale


def check_support_roles() -> tuple[list[SupportRoleStatus], list[str]]:
    """Return empty support role status.

    Support roles now run as GitHub Actions cron jobs (Phase 2a, #3375).
    Daemon-state-driven role tracking was removed in Phase 3.4 (#3401).
    """
    return [], []


def run_health_check() -> HealthReport:
    """Run the complete health check and return a report."""
    report = HealthReport()

    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        report.add_critical("Not in a git repository with .loom directory")
        return report

    paths = LoomPaths(repo_root)
    spawn_loop_state_file = paths.spawn_loop_state_file
    report.state_file_path = str(spawn_loop_state_file)

    # 1. Validate spawn-loop state file
    report.validation = validate_state_file(str(spawn_loop_state_file))

    if not report.validation.valid:
        if report.validation.status == "missing":
            report.add_critical("Spawn-loop state file not found")
            report.add_recommendation("Start the spawn loop with: LOOM_USE_SPAWN_LOOP=1 ./.loom/scripts/spawn-loop.sh start")
        elif report.validation.status in ("corrupt", "incomplete"):
            report.add_critical("Spawn-loop state file is corrupt or incomplete")
            report.add_recommendation("Delete .loom/spawn-loop-state.json and restart the spawn loop")
    else:
        spawn_loop_state = read_spawn_loop_state(repo_root)
        report.daemon_running = spawn_loop_state.present
        report.daemon_started_at = spawn_loop_state.started_at or ""

    # Support roles: now run as GitHub Actions cron jobs (#3375)
    report.support_roles, support_warnings = check_support_roles()

    # 2. Get pipeline state from GitHub
    report.pipeline = get_pipeline_state()

    # 3. Check orphaned building issues (loom:building but not tracked by spawn loop)
    if report.validation.valid:
        spawn_loop_state = read_spawn_loop_state(repo_root)
        tracked: set[int] = {t.issue for t in spawn_loop_state.running if t.issue is not None}
        report.orphaned_building = check_orphaned_building(
            report.pipeline.building,
            tracked,
        )

        if report.orphaned_building:
            orphan_list = ", ".join(f"#{n}" for n in report.orphaned_building)
            report.add_warning(f"Orphaned loom:building issues (labeled but not tracked by spawn loop): {orphan_list}")
            report.add_recommendation("Check orphaned issues with: ./.loom/scripts/stale-building-check.sh --recover")

    # 4. Check stale building issues
    report.stale_building = check_stale_building(report.pipeline.building)

    if report.stale_building:
        for stale in report.stale_building:
            report.add_warning(f"#{stale.number} in loom:building for {stale.age_minutes} min with no PR")
        report.add_recommendation(f"Check stale loom:building issues (>{STALE_BUILDING_MINUTES} min without PR)")

    return report


def format_json_output(report: HealthReport) -> str:
    """Format the health report as JSON."""
    stale_json = [
        {"issue": s.number, "age_minutes": s.age_minutes}
        for s in report.stale_building
    ]

    output = {
        "state_file": {
            "path": report.state_file_path,
            "status": report.validation.status,
        },
        "spawn_loop": {
            "running": report.daemon_running,
            "started_at": report.daemon_started_at,
        },
        "pipeline": {
            "ready": {"count": report.pipeline.ready_count, "issues": report.pipeline.ready},
            "building": {"count": report.pipeline.building_count, "issues": report.pipeline.building},
            "review_requested": {"count": report.pipeline.review_requested_count, "prs": report.pipeline.review_requested},
            "ready_to_merge": {"count": report.pipeline.ready_to_merge_count, "prs": report.pipeline.ready_to_merge},
            "blocked": {"count": report.pipeline.blocked_count, "issues": report.pipeline.blocked},
        },
        "consistency": {
            "orphaned_building": report.orphaned_building,
            "stale_building": stale_json,
        },
        "diagnostics": {
            "warnings": report.warnings,
            "criticals": report.criticals,
            "recommendations": report.recommendations,
            "warning_count": len(report.warnings),
            "critical_count": len(report.criticals),
            "exit_code": report.exit_code,
        },
    }

    return json.dumps(output, indent=2)


def time_ago(timestamp: str) -> str:
    """Format timestamp as 'N min ago' style string."""
    if not timestamp:
        return "never"
    try:
        secs = elapsed_seconds(timestamp)
        return f"{format_duration(secs)} ago"
    except Exception:
        return "unknown"


def format_numbers(items: list[dict[str, Any]]) -> str:
    """Format issue/PR numbers from a list of items."""
    numbers = [item.get("number") for item in items if item.get("number")]
    if not numbers:
        return ""
    return ", ".join(f"#{n}" for n in numbers)


def format_human_output(report: HealthReport) -> str:
    """Format the health report for human-readable output."""
    lines = []

    lines.append("")
    lines.append("LOOM DIAGNOSTIC")
    lines.append("===============")
    lines.append("")

    # State File section
    lines.append(f"State File: {report.state_file_path}")

    if report.validation.status == "ok":
        status_label = "present" if report.daemon_running else "absent"
        lines.append(f"  Status: {status_label}")

        if report.daemon_started_at:
            lines.append(f"  Started: {report.daemon_started_at} ({time_ago(report.daemon_started_at)})")
    elif report.validation.status == "missing":
        lines.append("  CRITICAL: State file not found")
        lines.append("  Start the spawn loop: LOOM_USE_SPAWN_LOOP=1 ./.loom/scripts/spawn-loop.sh start")
    elif report.validation.status in ("corrupt", "incomplete"):
        lines.append("  CRITICAL: State file is invalid")
        lines.append(f"  {report.validation.details}")

    lines.append("")

    # Pipeline Consistency
    lines.append("Pipeline Consistency:")

    ready_nums = format_numbers(report.pipeline.ready)
    building_nums = format_numbers(report.pipeline.building)
    review_nums = format_numbers(report.pipeline.review_requested)
    merge_nums = format_numbers(report.pipeline.ready_to_merge)
    blocked_nums = format_numbers(report.pipeline.blocked)

    line = f"  {'loom:issue (ready):':<27} {report.pipeline.ready_count} issues"
    if ready_nums:
        line += f" ({ready_nums})"
    lines.append(line)

    line = f"  {'loom:building:':<27} {report.pipeline.building_count} issues"
    if building_nums:
        line += f" ({building_nums})"
    lines.append(line)

    line = f"  {'loom:review-requested:':<27} {report.pipeline.review_requested_count} PRs"
    if review_nums:
        line += f" ({review_nums})"
    lines.append(line)

    line = f"  {'loom:pr (ready merge):':<27} {report.pipeline.ready_to_merge_count} PRs"
    if merge_nums:
        line += f" ({merge_nums})"
    lines.append(line)

    line = f"  {'loom:blocked:':<27} {report.pipeline.blocked_count} issues"
    if blocked_nums:
        line += f" ({blocked_nums})"
    lines.append(line)

    lines.append("")

    # Orphaned/stale building
    if not report.orphaned_building:
        lines.append("  Orphaned loom:building: NONE")
    else:
        lines.append(f"  Orphaned loom:building: {len(report.orphaned_building)} issue(s)")
        for num in report.orphaned_building:
            lines.append(f"    #{num} (labeled but not tracked by spawn loop)")

    if not report.stale_building:
        lines.append("  Stale loom:building:    NONE")
    else:
        lines.append(f"  Stale loom:building:    {len(report.stale_building)} issue(s)")
        for stale in report.stale_building:
            lines.append(f"    #{stale.number} ({stale.age_minutes} min, no PR yet)")

    lines.append("")

    # Recommendations
    if report.recommendations:
        lines.append("Recommendations:")
        for i, msg in enumerate(report.recommendations, 1):
            lines.append(f"  {i}. {msg}")
        lines.append("")

    # Summary
    if report.exit_code == 0:
        lines.append("Diagnostic: OK - No issues detected")
    elif report.exit_code == 1:
        lines.append(f"Diagnostic: WARNINGS - {len(report.warnings)} warning(s) detected")
    else:
        lines.append(f"Diagnostic: CRITICAL - {len(report.criticals)} critical issue(s), {len(report.warnings)} warning(s)")

    lines.append("")

    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    """Main entry point for the diagnostic CLI."""
    parser = argparse.ArgumentParser(
        description="Diagnostic health check for Loom orchestration",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Exit codes:
    0   Healthy - no warnings or critical issues
    1   Warnings detected - degraded but functional
    2   Critical issues - state corruption, orphaned work

Environment Variables:
    LOOM_STALE_BUILDING_MINUTES    Minutes before flagging stale building (default: 15)
""",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Output diagnostic report as JSON",
    )

    args = parser.parse_args(argv)

    report = run_health_check()

    if args.json:
        print(format_json_output(report))
    else:
        print(format_human_output(report))

    return report.exit_code


if __name__ == "__main__":
    sys.exit(main())
