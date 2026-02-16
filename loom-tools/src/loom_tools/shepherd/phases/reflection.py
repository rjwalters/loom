"""Reflection phase implementation.

Post-run analysis that reviews shepherd performance and optionally
files upstream issues on rjwalters/loom when actionable improvements
are identified.
"""

from __future__ import annotations

import json
import subprocess
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

from loom_tools.common.logging import log_info, log_warning
from loom_tools.shepherd.phases.base import BasePhase, PhaseResult

if TYPE_CHECKING:
    from loom_tools.shepherd.context import ShepherdContext

# Thresholds for flagging anomalies
SLOW_PHASE_THRESHOLD_SECONDS = 300  # 5 minutes
HIGH_RETRY_THRESHOLD = 2

# Upstream repo for filing issues
UPSTREAM_REPO = "rjwalters/loom"

# Title prefix for searchability
TITLE_PREFIX = "[shepherd-reflection]"

# Label applied to reflection-filed issues (goes through normal triage pipeline)
REFLECTION_ISSUE_LABEL = "loom:triage"


@dataclass
class Finding:
    """A single actionable finding from run analysis."""

    category: str  # e.g., "slow_phase", "excessive_retries", "builder_failure"
    title: str  # Short title for the finding
    details: str  # Detailed description with context
    severity: str = "enhancement"  # "enhancement" or "bug"


@dataclass
class RunSummary:
    """Summary of a shepherd run for reflection analysis."""

    issue: int = 0
    issue_title: str = ""
    mode: str = "default"
    task_id: str = ""
    duration: int = 0
    exit_code: int = 0
    phase_durations: dict[str, int] = field(default_factory=dict)
    completed_phases: list[str] = field(default_factory=list)
    judge_retries: int = 0
    doctor_attempts: int = 0
    test_fix_attempts: int = 0
    warnings: list[str] = field(default_factory=list)


class ReflectionPhase(BasePhase):
    """Phase 7: Post-run reflection and upstream issue filing.

    Analyzes the shepherd run for anomalies and actionable improvements.
    Files issues on the upstream Loom repository when warranted.

    This phase is best-effort: failures do not affect the shepherd exit code.
    """

    phase_name = "reflection"

    def __init__(self, run_summary: RunSummary | None = None) -> None:
        self.run_summary = run_summary or RunSummary()

    def should_skip(self, ctx: ShepherdContext) -> tuple[bool, str]:
        """Skip if --no-reflect is set."""
        if getattr(ctx.config, "no_reflect", False):
            return True, "reflection disabled via --no-reflect"
        return False, ""

    def run(self, ctx: ShepherdContext) -> PhaseResult:
        """Analyze run and file upstream issues if warranted."""
        findings = self._analyze_run(self.run_summary)

        if not findings:
            log_info("Reflection: no actionable findings")
            return self.success("no findings", data={"findings_count": 0})

        log_info(f"Reflection: {len(findings)} finding(s) detected")

        filed_count = 0
        for finding in findings:
            log_info(f"  - [{finding.severity}] {finding.title}")
            if self._should_file_issue(finding, ctx):
                if self._file_upstream_issue(finding, self.run_summary, ctx):
                    filed_count += 1

        return self.success(
            f"{len(findings)} findings, {filed_count} issues filed",
            data={
                "findings_count": len(findings),
                "filed_count": filed_count,
            },
        )

    def validate(self, ctx: ShepherdContext) -> bool:
        """Reflection phase always validates (best-effort)."""
        return True

    def _analyze_run(self, summary: RunSummary) -> list[Finding]:
        """Analyze run data and return actionable findings."""
        findings: list[Finding] = []

        # Check for slow phases
        for phase_name, duration in summary.phase_durations.items():
            if duration > SLOW_PHASE_THRESHOLD_SECONDS:
                findings.append(
                    Finding(
                        category="slow_phase",
                        title=f"Slow {phase_name} phase ({duration}s)",
                        details=(
                            f"The {phase_name} phase took {duration}s "
                            f"(threshold: {SLOW_PHASE_THRESHOLD_SECONDS}s). "
                            f"Issue #{summary.issue}: {summary.issue_title}. "
                            f"Mode: {summary.mode}."
                        ),
                        severity="enhancement",
                    )
                )

        # Check for excessive retries
        if summary.judge_retries >= HIGH_RETRY_THRESHOLD:
            findings.append(
                Finding(
                    category="excessive_retries",
                    title=f"Judge required {summary.judge_retries} retries",
                    details=(
                        f"The Judge phase needed {summary.judge_retries} retries "
                        f"for issue #{summary.issue}. This may indicate problems "
                        f"with review prompt clarity or PR complexity."
                    ),
                    severity="enhancement",
                )
            )

        if summary.doctor_attempts >= HIGH_RETRY_THRESHOLD:
            findings.append(
                Finding(
                    category="excessive_retries",
                    title=f"Doctor required {summary.doctor_attempts} attempts",
                    details=(
                        f"The Doctor phase ran {summary.doctor_attempts} times "
                        f"for issue #{summary.issue}. This may indicate "
                        f"insufficient feedback specificity from the Judge."
                    ),
                    severity="enhancement",
                )
            )

        if summary.test_fix_attempts >= HIGH_RETRY_THRESHOLD:
            findings.append(
                Finding(
                    category="excessive_retries",
                    title=f"Test-fix loop ran {summary.test_fix_attempts} times",
                    details=(
                        f"The builder test-fix loop required "
                        f"{summary.test_fix_attempts} iterations for "
                        f"issue #{summary.issue}."
                    ),
                    severity="enhancement",
                )
            )

        # Check for builder failure (non-zero exit)
        if summary.exit_code == 1:  # BUILDER_FAILED
            findings.append(
                Finding(
                    category="builder_failure",
                    title="Builder failed to create PR",
                    details=(
                        f"Builder phase failed for issue #{summary.issue}: "
                        f"{summary.issue_title}. "
                        f"Completed phases: {', '.join(summary.completed_phases)}."
                    ),
                    severity="bug",
                )
            )

        # Check for stale artifacts in warnings
        stale_warnings = [w for w in summary.warnings if "stale" in w.lower()]
        if stale_warnings:
            findings.append(
                Finding(
                    category="stale_artifacts",
                    title="Stale artifacts detected at startup",
                    details=(
                        f"Stale artifacts were found during issue #{summary.issue} "
                        f"orchestration: {'; '.join(stale_warnings)}. "
                        f"Consider auto-cleanup before builder phase."
                    ),
                    severity="enhancement",
                )
            )

        # Check for missing baseline
        baseline_warnings = [
            w for w in summary.warnings if "baseline" in w.lower()
        ]
        if baseline_warnings:
            findings.append(
                Finding(
                    category="missing_baseline",
                    title="Baseline health cache missing",
                    details=(
                        f"No baseline health cache was available during "
                        f"issue #{summary.issue} orchestration. "
                        f"This reduces confidence in test results."
                    ),
                    severity="enhancement",
                )
            )

        return findings

    def _should_file_issue(
        self, finding: Finding, ctx: ShepherdContext
    ) -> bool:
        """Check if an issue should be filed (no duplicates)."""
        # Search for existing open issues with similar title
        search_query = f"{TITLE_PREFIX} {finding.category}"
        try:
            result = subprocess.run(
                [
                    "gh",
                    "issue",
                    "list",
                    "--repo",
                    UPSTREAM_REPO,
                    "--search",
                    search_query,
                    "--state",
                    "open",
                    "--json",
                    "number,title",
                    "--limit",
                    "5",
                ],
                cwd=ctx.repo_root,
                capture_output=True,
                text=True,
                check=False,
            )
            if result.returncode == 0 and result.stdout.strip():
                existing = json.loads(result.stdout)
                if existing:
                    log_info(
                        f"  Skipping (existing issue #{existing[0]['number']})"
                    )
                    return False
        except (json.JSONDecodeError, OSError):
            # If we can't check, err on the side of not filing
            return False

        return True

    def _file_upstream_issue(
        self,
        finding: Finding,
        summary: RunSummary,
        ctx: ShepherdContext,
    ) -> bool:
        """File an issue on the upstream Loom repository."""
        title = f"{TITLE_PREFIX} {finding.title}"
        body = (
            f"## Automated Shepherd Reflection\n\n"
            f"{finding.details}\n\n"
            f"## Run Context\n\n"
            f"- **Issue**: #{summary.issue}\n"
            f"- **Mode**: {summary.mode}\n"
            f"- **Task ID**: {summary.task_id}\n"
            f"- **Duration**: {summary.duration}s\n"
            f"- **Exit code**: {summary.exit_code}\n"
            f"- **Phase timings**: {json.dumps(summary.phase_durations)}\n"
        )

        if summary.warnings:
            body += f"- **Warnings**: {'; '.join(summary.warnings)}\n"

        body += (
            f"\n---\n"
            f"*Filed automatically by shepherd reflection phase.*"
        )

        try:
            result = subprocess.run(
                [
                    "gh",
                    "issue",
                    "create",
                    "--repo",
                    UPSTREAM_REPO,
                    "--title",
                    title,
                    "--label",
                    REFLECTION_ISSUE_LABEL,
                    "--body",
                    body,
                ],
                cwd=ctx.repo_root,
                capture_output=True,
                text=True,
                check=False,
            )
            if result.returncode == 0:
                issue_url = result.stdout.strip()
                log_info(f"  Filed: {issue_url}")
                return True
            else:
                log_warning(
                    f"  Failed to file issue: {result.stderr.strip()}"
                )
                return False
        except OSError as exc:
            log_warning(f"  Failed to file issue: {exc}")
            return False
