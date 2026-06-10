"""Agent performance metrics for self-aware agents.

Enables agents to query their own effectiveness, costs, and velocity.
Reads exclusively from the activity database (``~/.loom/activity.db``).

If the activity database is missing, the CLI emits a clear error and
exits non-zero.

Commands:
    summary         Overall metrics summary (default)
    effectiveness   Agent effectiveness by role
    costs           Cost breakdown by issue
    velocity        Development velocity trends

Exit codes:
    0 - Success
    1 - Error (including activity database not available)
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import sqlite3
import sys
from dataclasses import dataclass
from typing import Any

from loom_tools.common.logging import log_error
from loom_tools.common.repo import find_repo_root

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
    """Effectiveness metrics for a single role (optionally per model).

    ``model`` (#3482, Phase 3a) is populated only when grouping by model
    (``--by-model``); NULL/absent model values in the DB render as
    ``"default"``. When empty, the key is omitted from ``to_dict`` so the
    pre-#3482 JSON shape is byte-identical for existing consumers.
    """

    role: str = ""
    total_prompts: int = 0
    successful_prompts: int = 0
    success_rate: float = 0.0
    avg_cost: float = 0.0
    avg_duration_sec: float = 0.0
    model: str = ""

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "role": self.role,
            "total_prompts": self.total_prompts,
            "successful_prompts": self.successful_prompts,
            "success_rate": self.success_rate,
            "avg_cost": self.avg_cost,
            "avg_duration_sec": self.avg_duration_sec,
        }
        if self.model:
            d["model"] = self.model
        return d


@dataclass
class CostRow:
    """Cost breakdown for a single issue (optionally per model).

    ``model`` semantics match :class:`EffectivenessRow` (#3482).
    """

    issue_number: int = 0
    prompt_count: int = 0
    total_cost: float = 0.0
    total_tokens: int = 0
    model: str = ""

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "issue_number": self.issue_number,
            "prompt_count": self.prompt_count,
            "total_cost": self.total_cost,
            "total_tokens": self.total_tokens,
        }
        if self.model:
            d["model"] = self.model
        return d


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


def _db_has_model_column(db_path: pathlib.Path) -> bool:
    """Return True when ``resource_usage.model`` exists in *db_path*.

    Older activity databases predate the model column (and possibly the
    ``resource_usage`` table itself); per-model grouping must degrade
    gracefully rather than erroring on them (#3482).
    """
    conn = sqlite3.connect(str(db_path))
    try:
        rows = conn.execute("PRAGMA table_info(resource_usage)").fetchall()
        return any(row[1] == "model" for row in rows)
    except sqlite3.Error:
        return False
    finally:
        conn.close()


def _model_expr(db_path: pathlib.Path) -> str:
    """SQL expression for the model dimension, NULL/absent-tolerant.

    NULL and empty-string models render as ``'default'`` (rows recorded
    before per-model attribution, or spawns that inherited the session/CLI
    default). On schemas without the column, a literal ``'default'`` keeps
    the query valid so old databases never break metrics (#3482).
    """
    if _db_has_model_column(db_path):
        return "COALESCE(NULLIF(r.model, ''), 'default')"
    return "'default'"


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


def get_effectiveness(
    db_path: pathlib.Path,
    role: str,
    period: str,
    by_model: bool = False,
) -> list[EffectivenessRow]:
    """Get effectiveness metrics grouped by role (and model when *by_model*).

    With ``by_model=True`` (#3482, Phase 3a) each (role, model) pair gets
    its own row, using the existing ``resource_usage.model`` column;
    NULL/absent model values group under ``'default'``.
    """
    role_filter = _get_role_filter(role)
    period_filter = _get_period_filter(period)
    model_select = ""
    model_group = ""
    if by_model:
        model_select = f"{_model_expr(db_path)} as model,"
        model_group = f", {_model_expr(db_path)}"

    rows = _query_db(
        db_path,
        f"""
        SELECT
            COALESCE(i.agent_role, 'unknown') as role,
            {model_select}
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
        GROUP BY COALESCE(i.agent_role, 'unknown'){model_group}
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
            model=(r.get("model") or "default") if by_model else "",
        )
        for r in rows
    ]


def get_costs(
    db_path: pathlib.Path,
    issue_number: int | None,
    by_model: bool = False,
) -> list[CostRow]:
    """Get cost breakdown by issue (and model when *by_model*).

    With ``by_model=True`` (#3482, Phase 3a) each (issue, model) pair gets
    its own row; NULL/absent model values group under ``'default'``.
    """
    issue_filter = f"WHERE pg.issue_number = {issue_number}" if issue_number else ""
    model_select = ""
    model_group = ""
    if by_model:
        model_select = f"{_model_expr(db_path)} as model,"
        model_group = f", {_model_expr(db_path)}"

    rows = _query_db(
        db_path,
        f"""
        SELECT
            pg.issue_number,
            {model_select}
            COUNT(DISTINCT i.id) as prompt_count,
            ROUND(COALESCE(SUM(r.cost_usd), 0), 4) as total_cost,
            COALESCE(SUM(r.tokens_input + r.tokens_output), 0) as total_tokens
        FROM prompt_github pg
        JOIN agent_inputs i ON pg.input_id = i.id
        LEFT JOIN resource_usage r ON i.id = r.input_id
        {issue_filter}
        GROUP BY pg.issue_number{model_group}
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
            model=(r.get("model") or "default") if by_model else "",
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


def format_effectiveness_text(rows: list[EffectivenessRow], period: str) -> str:
    """Format effectiveness table for human display.

    A Model column is included when any row carries a model dimension
    (``--by-model``, #3482).
    """
    with_model = any(r.model for r in rows)
    if with_model:
        header = (
            f"{'Role':<12} {'Model':<22} {'Prompts':>10} {'Success':>10} "
            f"{'Rate':>10} {'Avg Cost':>10} {'Avg Time':>10}"
        )
        width = 90
    else:
        header = (
            f"{'Role':<12} {'Prompts':>10} {'Success':>10} "
            f"{'Rate':>10} {'Avg Cost':>10} {'Avg Time':>10}"
        )
        width = 68
    lines = [
        "",
        _c(_BLUE, "Agent Effectiveness by Role") + f" ({period})",
        _c(_GRAY, "\u2500" * width),
        header,
        _c(_GRAY, "\u2500" * width),
    ]
    for r in rows:
        rate_clr = _rate_color(r.success_rate)
        rate_str = _c(rate_clr, f"{r.success_rate:.1f}%")
        model_col = f"{(r.model or 'default'):<22} " if with_model else ""
        lines.append(
            f"{r.role:<12} {model_col}{r.total_prompts:>10} {r.successful_prompts:>10} "
            f"{rate_str:>10} {'$' + f'{r.avg_cost:.4f}':>10} {f'{r.avg_duration_sec:.1f}s':>10}"
        )
    lines.append("")
    return "\n".join(lines)


def format_costs_text(rows: list[CostRow]) -> str:
    """Format cost table for human display.

    A Model column is included when any row carries a model dimension
    (``--by-model``, #3482).
    """
    with_model = any(r.model for r in rows)
    if with_model:
        header = f"{'Issue':<8} {'Model':<22} {'Prompts':>10} {'Cost':>12} {'Tokens':>12}"
        width = 90
    else:
        header = f"{'Issue':<8} {'Prompts':>10} {'Cost':>12} {'Tokens':>12}"
        width = 68
    lines = [
        "",
        _c(_BLUE, "Cost Breakdown by Issue"),
        _c(_GRAY, "\u2500" * width),
        header,
        _c(_GRAY, "\u2500" * width),
    ]
    for r in rows:
        model_col = f"{(r.model or 'default'):<22} " if with_model else ""
        lines.append(
            f"{'#' + str(r.issue_number):<8} {model_col}{r.prompt_count:>10} "
            f"{'$' + f'{r.total_cost:.4f}':>12} {r.total_tokens:>12}"
        )
    lines.append("")
    return "\n".join(lines)


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


def format_db_unavailable_text(db_path: pathlib.Path) -> str:
    """Format error message when the activity database is missing."""
    return _c(
        _RED,
        f"Error: activity database not found at {db_path}. "
        "Set LOOM_ACTIVITY_DB or enable agent activity tracking.",
    )


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
  --by-model            Add a per-model dimension (effectiveness/costs commands);
                        NULL/absent model values render as 'default' (#3482)

Exit codes:
  0 - Success
  1 - Error

Examples:
  loom-agent-metrics                           # Summary for this week
  loom-agent-metrics --role builder            # Builder metrics
  loom-agent-metrics effectiveness             # Effectiveness by role
  loom-agent-metrics effectiveness --by-model  # Effectiveness by role x model
  loom-agent-metrics costs --issue 123         # Cost for issue #123
  loom-agent-metrics costs --by-model          # Cost per issue x model
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
    parser.add_argument(
        "--by-model",
        dest="by_model",
        action="store_true",
        help=(
            "Add a per-model dimension to effectiveness/costs output; "
            "NULL/absent model values render as 'default' (#3482)"
        ),
    )

    args = parser.parse_args(argv)

    # Validate role against allow-list to prevent SQL injection
    if args.role and args.role not in _VALID_ROLES:
        log_error(f"Invalid role: {args.role}")
        return 1

    try:
        # Anchor to a git repo so the CLI behaves consistently with the
        # other loom-tools entry points; the activity database itself lives
        # under ``~/.loom`` (overridable via LOOM_ACTIVITY_DB), not in the
        # repo, but failing fast outside a repo matches operator expectations.
        find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return 1

    db_path = _get_activity_db_path()
    if not db_path.is_file():
        # The activity DB is the single source of truth for metrics; surface a
        # clear error so operators know what to configure rather than silently
        # returning empty/stale state.
        if args.output_format == "json":
            print(
                json.dumps(
                    {
                        "error": "Activity database not available",
                        "db_path": str(db_path),
                    },
                    indent=2,
                )
            )
        else:
            print(format_db_unavailable_text(db_path))
        return 1

    try:
        if args.command == "summary":
            return _cmd_summary(db_path, args)
        elif args.command == "effectiveness":
            return _cmd_effectiveness(db_path, args)
        elif args.command == "costs":
            return _cmd_costs(db_path, args)
        elif args.command == "velocity":
            return _cmd_velocity(db_path, args)
    except Exception as e:
        log_error(f"Failed to get metrics: {e}")
        return 1

    return 0


def _cmd_summary(db_path: pathlib.Path, args: argparse.Namespace) -> int:
    metrics = get_summary(db_path, args.role, args.period)
    if args.output_format == "json":
        print(json.dumps(metrics.to_dict(), indent=2))
    else:
        print(format_summary_text(metrics, args.period))
    return 0


def _cmd_effectiveness(db_path: pathlib.Path, args: argparse.Namespace) -> int:
    rows = get_effectiveness(db_path, args.role, args.period, by_model=args.by_model)
    if args.output_format == "json":
        print(json.dumps([r.to_dict() for r in rows], indent=2))
    else:
        print(format_effectiveness_text(rows, args.period))
    return 0


def _cmd_costs(db_path: pathlib.Path, args: argparse.Namespace) -> int:
    rows = get_costs(db_path, args.issue, by_model=args.by_model)
    if args.output_format == "json":
        print(json.dumps([r.to_dict() for r in rows], indent=2))
    else:
        print(format_costs_text(rows))
    return 0


def _cmd_velocity(db_path: pathlib.Path, args: argparse.Namespace) -> int:
    rows = get_velocity(db_path)
    if args.output_format == "json":
        print(json.dumps([r.to_dict() for r in rows], indent=2))
    else:
        print(format_velocity_text(rows))
    return 0


if __name__ == "__main__":
    sys.exit(main())
