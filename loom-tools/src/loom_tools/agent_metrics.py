"""Agent performance metrics for self-aware agents.

Enables agents to query their own effectiveness, costs, and velocity.
Reads from the activity database (~/.loom/activity.db) when available,
falling back to daemon-state.json and GitHub API.

Commands:
    summary         Overall metrics summary (default)
    effectiveness   Agent effectiveness by role
    costs           Cost breakdown by issue
    velocity        Development velocity trends

Exit codes:
    0 - Success
    1 - Error
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import sqlite3
import sys
from dataclasses import dataclass, field
from typing import Any

from loom_tools.common.github import gh_run
from loom_tools.common.logging import log_error, log_warning
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_daemon_state

# ANSI color codes for direct stdout formatting.
# Logging helpers write to stderr; metrics output goes to stdout.
_RED = "\033[0;31m"
_GREEN = "\033[0;32m"
_YELLOW = "\033[1;33m"
_BLUE = "\033[0;34m"
_GRAY = "\033[0;90m"
_RESET = "\033[0m"

_VALID_PERIODS = ("today", "week", "month", "all")
_VALID_ROLES = (
    "builder",
    "judge",
    "curator",
    "architect",
    "hermit",
    "doctor",
    "guide",
    "champion",
    "shepherd",
)


def _use_color() -> bool:
    """Check if stdout supports color output."""
    import io

    try:
        return os.isatty(sys.stdout.fileno())
    except (OSError, ValueError, io.UnsupportedOperation):
        return False


def _c(code: str, text: str) -> str:
    """Wrap *text* in ANSI *code* if color is enabled."""
    if _use_color():
        return f"{code}{text}{_RESET}"
    return text


# ---------------------------------------------------------------------------
# Data models
# ---------------------------------------------------------------------------


@dataclass
class SummaryMetrics:
    """Overall summary metrics."""

    total_prompts: int = 0
    total_tokens: int = 0
    total_cost: float = 0.0
    issues_count: int = 0
    prs_count: int = 0
    success_rate: float = 0.0

    def to_dict(self) -> dict[str, Any]:
        return {
            "total_prompts": self.total_prompts,
            "total_tokens": self.total_tokens,
            "total_cost": self.total_cost,
            "issues_count": self.issues_count,
            "prs_count": self.prs_count,
            "success_rate": self.success_rate,
        }


@dataclass
class EffectivenessRow:
    """Effectiveness metrics for a single role."""

    role: str = ""
    total_prompts: int = 0
    successful_prompts: int = 0
    success_rate: float = 0.0
    avg_cost: float = 0.0
    avg_duration_sec: float = 0.0

    def to_dict(self) -> dict[str, Any]:
        return {
            "role": self.role,
            "total_prompts": self.total_prompts,
            "successful_prompts": self.successful_prompts,
            "success_rate": self.success_rate,
            "avg_cost": self.avg_cost,
            "avg_duration_sec": self.avg_duration_sec,
        }


@dataclass
class CostRow:
    """Cost breakdown for a single issue."""

    issue_number: int = 0
    prompt_count: int = 0
    total_cost: float = 0.0
    total_tokens: int = 0

    def to_dict(self) -> dict[str, Any]:
        return {
            "issue_number": self.issue_number,
            "prompt_count": self.prompt_count,
            "total_cost": self.total_cost,
            "total_tokens": self.total_tokens,
        }


@dataclass
class VelocityRow:
    """Velocity metrics for a single week."""

    week: str = ""
    prompts: int = 0
    issues: int = 0
    prs_merged: int = 0
    cost: float = 0.0

    def to_dict(self) -> dict[str, Any]:
        return {
            "week": self.week,
            "prompts": self.prompts,
            "issues": self.issues,
            "prs_merged": self.prs_merged,
            "cost": self.cost,
        }


@dataclass
class FallbackSummary:
    """Limited metrics when the activity DB is unavailable."""

    completed_issues: int = 0
    total_prs_merged: int = 0
    open_issues: int = 0
    open_prs: int = 0
    note: str = "Limited data - activity database not available"

    def to_dict(self) -> dict[str, Any]:
        return {
            "completed_issues": self.completed_issues,
            "total_prs_merged": self.total_prs_merged,
            "open_issues": self.open_issues,
            "open_prs": self.open_prs,
            "note": self.note,
        }


@dataclass
class FallbackVelocity:
    """Limited velocity when the activity DB is unavailable."""

    completed_issues: int = 0
    prs_merged: int = 0
    session_started: str = ""
    note: str = "Velocity from daemon state (limited data)"

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "completed_issues": self.completed_issues,
            "prs_merged": self.prs_merged,
            "note": self.note,
        }
        if self.session_started:
            d["session_started"] = self.session_started
        return d


# ---------------------------------------------------------------------------
# SQL helpers
# ---------------------------------------------------------------------------


def _get_period_filter(period: str) -> str:
    """Return a SQL WHERE clause fragment for the given *period*."""
    if period == "today":
        return "AND timestamp >= datetime('now', 'start of day')"
    if period == "week":
        return "AND timestamp >= datetime('now', '-7 days')"
    if period == "month":
        return "AND timestamp >= datetime('now', '-30 days')"
    return ""


def _get_role_filter(role: str) -> str:
    """Return a SQL WHERE clause fragment for the given *role*."""
    if role:
        # Use parameterised queries where possible, but for dynamic SQL
        # composition we validate against the allow-list.
        return f"AND agent_role = '{role}'"
    return ""


def _get_activity_db_path() -> pathlib.Path:
    """Return the path to the activity database."""
    return pathlib.Path(os.environ.get("LOOM_ACTIVITY_DB", "~/.loom/activity.db")).expanduser()


def _query_db(db_path: pathlib.Path, sql: str) -> list[dict[str, Any]]:
    """Execute *sql* against *db_path* and return rows as dicts."""
    conn = sqlite3.connect(str(db_path))
    conn.row_factory = sqlite3.Row
    try:
        rows = conn.execute(sql).fetchall()
        return [dict(row) for row in rows]
    finally:
        conn.close()


# ---------------------------------------------------------------------------
# Command implementations
# ---------------------------------------------------------------------------


def get_summary(
    db_path: pathlib.Path,
    role: str,
    period: str,
) -> SummaryMetrics:
    """Get summary metrics from the activity database."""
    role_filter = _get_role_filter(role)
    period_filter = _get_period_filter(period)

    rows = _query_db(
        db_path,
        f"""
        SELECT
            COUNT(*) as total_prompts,
            COALESCE(SUM(r.tokens_input + r.tokens_output), 0) as total_tokens,
            ROUND(COALESCE(SUM(r.cost_usd), 0), 4) as total_cost,
            COUNT(DISTINCT pg.issue_number) as issues_count,
            COUNT(DISTINCT pg.pr_number) as prs_count
        FROM agent_inputs i
        LEFT JOIN resource_usage r ON i.id = r.input_id
        LEFT JOIN prompt_github pg ON i.id = pg.input_id
        WHERE 1=1 {role_filter} {period_filter}
        """,
    )

    sr_rows = _query_db(
        db_path,
        f"""
        SELECT
            ROUND(
                100.0 * SUM(
                    CASE WHEN q.tests_passed > 0
                         AND (q.tests_failed IS NULL OR q.tests_failed = 0)
                    THEN 1 ELSE 0 END
                ) / NULLIF(COUNT(*), 0),
                1
            ) as success_rate
        FROM agent_inputs i
        LEFT JOIN quality_metrics q ON i.id = q.input_id
        WHERE 1=1 {role_filter} {period_filter}
        """,
    )

    row = rows[0] if rows else {}
    sr = sr_rows[0].get("success_rate", 0) if sr_rows else 0

    return SummaryMetrics(
        total_prompts=row.get("total_prompts", 0) or 0,
        total_tokens=row.get("total_tokens", 0) or 0,
        total_cost=row.get("total_cost", 0.0) or 0.0,
        issues_count=row.get("issues_count", 0) or 0,
        prs_count=row.get("prs_count", 0) or 0,
        success_rate=sr or 0.0,
    )


def get_summary_fallback(repo_root: pathlib.Path) -> FallbackSummary:
    """Get limited summary metrics from daemon state and GitHub."""
    state = read_daemon_state(repo_root)

    completed = len(state.completed_issues) if state.completed_issues else 0
    prs_merged = state.total_prs_merged or 0

    open_issues = 0
    open_prs = 0
    try:
        result = gh_run(
            ["issue", "list", "--state", "open", "--json", "number", "--jq", "length"],
            check=False,
        )
        if result.returncode == 0 and result.stdout.strip():
            open_issues = int(result.stdout.strip())
    except Exception:
        pass

    try:
        result = gh_run(
            ["pr", "list", "--state", "open", "--json", "number", "--jq", "length"],
            check=False,
        )
        if result.returncode == 0 and result.stdout.strip():
            open_prs = int(result.stdout.strip())
    except Exception:
        pass

    return FallbackSummary(
        completed_issues=completed,
        total_prs_merged=prs_merged,
        open_issues=open_issues,
        open_prs=open_prs,
    )


def get_effectiveness(
    db_path: pathlib.Path,
    role: str,
    period: str,
) -> list[EffectivenessRow]:
    """Get effectiveness metrics grouped by role."""
    role_filter = _get_role_filter(role)
    period_filter = _get_period_filter(period)

    rows = _query_db(
        db_path,
        f"""
        SELECT
            COALESCE(i.agent_role, 'unknown') as role,
            COUNT(*) as total_prompts,
            SUM(
                CASE WHEN q.tests_passed > 0
                     AND (q.tests_failed IS NULL OR q.tests_failed = 0)
                THEN 1 ELSE 0 END
            ) as successful_prompts,
            ROUND(
                100.0 * SUM(
                    CASE WHEN q.tests_passed > 0
                         AND (q.tests_failed IS NULL OR q.tests_failed = 0)
                    THEN 1 ELSE 0 END
                ) / NULLIF(COUNT(*), 0),
                1
            ) as success_rate,
            ROUND(COALESCE(AVG(r.cost_usd), 0), 4) as avg_cost,
            ROUND(COALESCE(AVG(r.duration_ms / 1000.0), 0), 1) as avg_duration_sec
        FROM agent_inputs i
        LEFT JOIN quality_metrics q ON i.id = q.input_id
        LEFT JOIN resource_usage r ON i.id = r.input_id
        WHERE 1=1 {role_filter} {period_filter}
        GROUP BY COALESCE(i.agent_role, 'unknown')
        ORDER BY success_rate DESC
        """,
    )

    return [
        EffectivenessRow(
            role=r.get("role", "unknown"),
            total_prompts=r.get("total_prompts", 0) or 0,
            successful_prompts=r.get("successful_prompts", 0) or 0,
            success_rate=r.get("success_rate", 0.0) or 0.0,
            avg_cost=r.get("avg_cost", 0.0) or 0.0,
            avg_duration_sec=r.get("avg_duration_sec", 0.0) or 0.0,
        )
        for r in rows
    ]


def get_costs(
    db_path: pathlib.Path,
    issue_number: int | None,
) -> list[CostRow]:
    """Get cost breakdown by issue."""
    issue_filter = f"WHERE pg.issue_number = {issue_number}" if issue_number else ""

    rows = _query_db(
        db_path,
        f"""
        SELECT
            pg.issue_number,
            COUNT(DISTINCT i.id) as prompt_count,
            ROUND(COALESCE(SUM(r.cost_usd), 0), 4) as total_cost,
            COALESCE(SUM(r.tokens_input + r.tokens_output), 0) as total_tokens
        FROM prompt_github pg
        JOIN agent_inputs i ON pg.input_id = i.id
        LEFT JOIN resource_usage r ON i.id = r.input_id
        {issue_filter}
        GROUP BY pg.issue_number
        ORDER BY total_cost DESC
        LIMIT 20
        """,
    )

    return [
        CostRow(
            issue_number=r.get("issue_number", 0) or 0,
            prompt_count=r.get("prompt_count", 0) or 0,
            total_cost=r.get("total_cost", 0.0) or 0.0,
            total_tokens=r.get("total_tokens", 0) or 0,
        )
        for r in rows
        if r.get("issue_number") is not None
    ]


def get_velocity(db_path: pathlib.Path) -> list[VelocityRow]:
    """Get weekly velocity metrics for the last 8 weeks."""
    rows = _query_db(
        db_path,
        """
        SELECT
            strftime('%Y-W%W', timestamp) as week,
            COUNT(*) as prompts,
            COUNT(DISTINCT pg.issue_number) as issues,
            COUNT(DISTINCT CASE WHEN pg.event_type = 'pr_merged'
                  THEN pg.pr_number END) as prs_merged,
            ROUND(SUM(r.cost_usd), 2) as cost
        FROM agent_inputs i
        LEFT JOIN prompt_github pg ON i.id = pg.input_id
        LEFT JOIN resource_usage r ON i.id = r.input_id
        WHERE timestamp >= datetime('now', '-56 days')
        GROUP BY week
        ORDER BY week DESC
        LIMIT 8
        """,
    )

    return [
        VelocityRow(
            week=r.get("week", ""),
            prompts=r.get("prompts", 0) or 0,
            issues=r.get("issues", 0) or 0,
            prs_merged=r.get("prs_merged", 0) or 0,
            cost=r.get("cost", 0.0) or 0.0,
        )
        for r in rows
    ]


def get_velocity_fallback(repo_root: pathlib.Path) -> FallbackVelocity:
    """Get limited velocity from daemon state."""
    state = read_daemon_state(repo_root)

    return FallbackVelocity(
        completed_issues=len(state.completed_issues) if state.completed_issues else 0,
        prs_merged=state.total_prs_merged or 0,
        session_started=state.started_at or "",
    )


# ---------------------------------------------------------------------------
# Text formatters
# ---------------------------------------------------------------------------


def _rate_color(rate: float) -> str:
    """Return ANSI color code for a success rate value."""
    if rate >= 90:
        return _GREEN
    if rate >= 70:
        return _YELLOW
    return _RED


def format_summary_text(m: SummaryMetrics, period: str) -> str:
    """Format summary metrics for human display."""
    rate_clr = _rate_color(m.success_rate)
    tokens_k = m.total_tokens // 1000 if m.total_tokens else 0
    lines = [
        "",
        _c(_BLUE, "Agent Performance Summary") + f" ({period})",
        _c(_GRAY, "\u2500" * 40),
        f"  Total Prompts:   {m.total_prompts}",
        f"  Total Tokens:    {tokens_k}K",
        f"  Total Cost:      ${m.total_cost:.4f}",
        f"  Issues Worked:   {m.issues_count}",
        f"  PRs Created:     {m.prs_count}",
        f"  Success Rate:    {_c(rate_clr, f'{m.success_rate:.1f}%')}",
        "",
    ]
    return "\n".join(lines)


def format_summary_fallback_text(m: FallbackSummary) -> str:
    """Format fallback summary for human display."""
    lines = [
        "",
        _c(_BLUE, "Agent Performance Summary") + " (daemon state)",
        _c(_GRAY, "\u2500" * 40),
        f"  Completed Issues: {m.completed_issues}",
        f"  PRs Merged:       {m.total_prs_merged}",
        f"  Open Issues:      {m.open_issues}",
        f"  Open PRs:         {m.open_prs}",
        "",
        _c(_YELLOW, "Note: Activity database not available. Enable tracking for detailed metrics."),
        "",
    ]
    return "\n".join(lines)


def format_effectiveness_text(rows: list[EffectivenessRow], period: str) -> str:
    """Format effectiveness table for human display."""
    lines = [
        "",
        _c(_BLUE, "Agent Effectiveness by Role") + f" ({period})",
        _c(_GRAY, "\u2500" * 68),
        f"{'Role':<12} {'Prompts':>10} {'Success':>10} {'Rate':>10} {'Avg Cost':>10} {'Avg Time':>10}",
        _c(_GRAY, "\u2500" * 68),
    ]
    for r in rows:
        rate_clr = _rate_color(r.success_rate)
        rate_str = _c(rate_clr, f"{r.success_rate:.1f}%")
        lines.append(
            f"{r.role:<12} {r.total_prompts:>10} {r.successful_prompts:>10} "
            f"{rate_str:>10} {'$' + f'{r.avg_cost:.4f}':>10} {f'{r.avg_duration_sec:.1f}s':>10}"
        )
    lines.append("")
    return "\n".join(lines)


def format_effectiveness_unavailable_text() -> str:
    """Format message when effectiveness data is unavailable."""
    return _c(_YELLOW, "Activity database not available. Enable tracking for effectiveness metrics.")


def format_costs_text(rows: list[CostRow]) -> str:
    """Format cost table for human display."""
    lines = [
        "",
        _c(_BLUE, "Cost Breakdown by Issue"),
        _c(_GRAY, "\u2500" * 68),
        f"{'Issue':<8} {'Prompts':>10} {'Cost':>12} {'Tokens':>12}",
        _c(_GRAY, "\u2500" * 68),
    ]
    for r in rows:
        lines.append(
            f"{'#' + str(r.issue_number):<8} {r.prompt_count:>10} "
            f"{'$' + f'{r.total_cost:.4f}':>12} {r.total_tokens:>12}"
        )
    lines.append("")
    return "\n".join(lines)


def format_costs_unavailable_text() -> str:
    """Format message when cost data is unavailable."""
    return _c(_YELLOW, "Activity database not available. Enable tracking for cost metrics.")


def format_velocity_text(rows: list[VelocityRow]) -> str:
    """Format velocity table for human display."""
    lines = [
        "",
        _c(_BLUE, "Development Velocity (Last 8 Weeks)"),
        _c(_GRAY, "\u2500" * 68),
        f"{'Week':<10} {'Prompts':>10} {'Issues':>10} {'PRs':>10} {'Cost':>10}",
        _c(_GRAY, "\u2500" * 68),
    ]
    for r in rows:
        lines.append(
            f"{r.week:<10} {r.prompts:>10} {r.issues:>10} "
            f"{r.prs_merged:>10} {'$' + f'{r.cost:.2f}':>10}"
        )
    lines.append("")
    return "\n".join(lines)


def format_velocity_fallback_text(m: FallbackVelocity) -> str:
    """Format fallback velocity for human display."""
    lines = [
        "",
        _c(_BLUE, "Development Velocity") + " (daemon state)",
        _c(_GRAY, "\u2500" * 40),
        f"  Issues Completed: {m.completed_issues}",
        f"  PRs Merged:       {m.prs_merged}",
    ]
    if m.session_started:
        lines.append(f"  Session Started:  {m.session_started}")
    lines.append("")
    return "\n".join(lines)


def format_velocity_unavailable_text() -> str:
    """Format message when no velocity data is available."""
    return _c(_YELLOW, "No velocity data available.")


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    """Main entry point for the agent-metrics CLI."""
    parser = argparse.ArgumentParser(
        description="Agent performance metrics for self-aware agents",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Commands:
  summary         Show overall metrics summary (default)
  effectiveness   Show agent effectiveness by role
  costs           Show cost breakdown by issue
  velocity        Show development velocity trends

Options:
  --role ROLE           Filter by agent role
  --period PERIOD       Time period: today, week, month, all (default: week)
  --format FORMAT       Output format: text, json (default: text)
  --issue NUMBER        Filter by issue number (costs command)

Exit codes:
  0 - Success
  1 - Error

Examples:
  loom-agent-metrics                           # Summary for this week
  loom-agent-metrics --role builder            # Builder metrics
  loom-agent-metrics effectiveness             # Effectiveness by role
  loom-agent-metrics costs --issue 123         # Cost for issue #123
  loom-agent-metrics velocity --format json    # Velocity as JSON
""",
    )

    parser.add_argument(
        "command",
        nargs="?",
        default="summary",
        choices=["summary", "effectiveness", "costs", "velocity"],
        help="Metrics command to run (default: summary)",
    )
    parser.add_argument(
        "--role",
        default="",
        choices=[""] + list(_VALID_ROLES),
        help="Filter by agent role",
    )
    parser.add_argument(
        "--period",
        default="week",
        choices=list(_VALID_PERIODS),
        help="Time period (default: week)",
    )
    parser.add_argument(
        "--format",
        dest="output_format",
        default="text",
        choices=["text", "json"],
        help="Output format (default: text)",
    )
    parser.add_argument(
        "--issue",
        type=int,
        default=None,
        help="Filter by issue number (costs command)",
    )

    args = parser.parse_args(argv)

    # Validate role against allow-list to prevent SQL injection
    if args.role and args.role not in _VALID_ROLES:
        log_error(f"Invalid role: {args.role}")
        return 1

    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return 1

    db_path = _get_activity_db_path()
    db_available = db_path.is_file()

    try:
        if args.command == "summary":
            return _cmd_summary(db_available, db_path, repo_root, args)
        elif args.command == "effectiveness":
            return _cmd_effectiveness(db_available, db_path, args)
        elif args.command == "costs":
            return _cmd_costs(db_available, db_path, args)
        elif args.command == "velocity":
            return _cmd_velocity(db_available, db_path, repo_root, args)
    except Exception as e:
        log_error(f"Failed to get metrics: {e}")
        return 1

    return 0


def _cmd_summary(
    db_available: bool,
    db_path: pathlib.Path,
    repo_root: pathlib.Path,
    args: argparse.Namespace,
) -> int:
    if db_available:
        metrics = get_summary(db_path, args.role, args.period)
        if args.output_format == "json":
            print(json.dumps(metrics.to_dict(), indent=2))
        else:
            print(format_summary_text(metrics, args.period))
    else:
        fb = get_summary_fallback(repo_root)
        if args.output_format == "json":
            print(json.dumps(fb.to_dict(), indent=2))
        else:
            print(format_summary_fallback_text(fb))
    return 0


def _cmd_effectiveness(
    db_available: bool,
    db_path: pathlib.Path,
    args: argparse.Namespace,
) -> int:
    if not db_available:
        if args.output_format == "json":
            print(json.dumps({"error": "Activity database not available", "roles": []}))
        else:
            print(format_effectiveness_unavailable_text())
        return 0

    rows = get_effectiveness(db_path, args.role, args.period)
    if args.output_format == "json":
        print(json.dumps([r.to_dict() for r in rows], indent=2))
    else:
        print(format_effectiveness_text(rows, args.period))
    return 0


def _cmd_costs(
    db_available: bool,
    db_path: pathlib.Path,
    args: argparse.Namespace,
) -> int:
    if not db_available:
        if args.output_format == "json":
            print(json.dumps({"error": "Activity database not available", "costs": []}))
        else:
            print(format_costs_unavailable_text())
        return 0

    rows = get_costs(db_path, args.issue)
    if args.output_format == "json":
        print(json.dumps([r.to_dict() for r in rows], indent=2))
    else:
        print(format_costs_text(rows))
    return 0


def _cmd_velocity(
    db_available: bool,
    db_path: pathlib.Path,
    repo_root: pathlib.Path,
    args: argparse.Namespace,
) -> int:
    if db_available:
        rows = get_velocity(db_path)
        if args.output_format == "json":
            print(json.dumps([r.to_dict() for r in rows], indent=2))
        else:
            print(format_velocity_text(rows))
    else:
        fb = get_velocity_fallback(repo_root)
        if fb.completed_issues or fb.prs_merged:
            if args.output_format == "json":
                print(json.dumps(fb.to_dict(), indent=2))
            else:
                print(format_velocity_fallback_text(fb))
        else:
            if args.output_format == "json":
                print(json.dumps({"error": "No velocity data available"}))
            else:
                print(format_velocity_unavailable_text())
    return 0


if __name__ == "__main__":
    sys.exit(main())
