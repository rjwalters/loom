"""Loom system status display for Layer 3 observation.

Replaces the former ``loom-status.sh`` (814 LOC) with a Python module that
provides colored terminal formatting.

Live state is read from ``.loom/spawn-loop-state.json`` (Phase 1, #3374)
plus forge queries.

Usage::

    loom-status              # colored terminal output
    loom-status --json       # JSON output (snapshot data)
    loom-status --help       # show help
"""

from __future__ import annotations

import json
import os
import pathlib
import sys
from datetime import datetime, timezone
from typing import Any, Sequence

from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import (
    read_json_file,
    read_spawn_loop_state,
)
from loom_tools.common.time_utils import now_utc, parse_iso_timestamp
from loom_tools.forge_snapshot import collect_pipeline_data
from loom_tools.models.spawn_loop_state import SpawnLoopState, SpawnLoopTask

# ---------------------------------------------------------------------------
# ANSI color support with TTY detection
# ---------------------------------------------------------------------------

_RED = "\033[0;31m"
_GREEN = "\033[0;32m"
_YELLOW = "\033[1;33m"
_BLUE = "\033[0;34m"
_CYAN = "\033[0;36m"
_GRAY = "\033[0;90m"
_BOLD = "\033[1m"
_RESET = "\033[0m"


def _use_color(stream: Any = None) -> bool:
    """Check if color output should be used on *stream* (default stdout)."""
    if stream is None:
        stream = sys.stdout
    try:
        return os.isatty(stream.fileno())
    except (OSError, ValueError, AttributeError):
        return False


class _Colors:
    """Color palette that respects TTY detection."""

    def __init__(self, *, use_color: bool = True) -> None:
        if use_color:
            self.red = _RED
            self.green = _GREEN
            self.yellow = _YELLOW
            self.blue = _BLUE
            self.cyan = _CYAN
            self.gray = _GRAY
            self.bold = _BOLD
            self.reset = _RESET
        else:
            self.red = ""
            self.green = ""
            self.yellow = ""
            self.blue = ""
            self.cyan = ""
            self.gray = ""
            self.bold = ""
            self.reset = ""


# ---------------------------------------------------------------------------
# Time formatting
# ---------------------------------------------------------------------------

def time_ago(timestamp: str | None, *, _now: datetime | None = None) -> str:
    """Format an ISO timestamp as a human-readable relative time.

    Returns ``"never"`` for None/empty/null, ``"unknown"`` on parse error.
    """
    if not timestamp or timestamp == "null":
        return "never"

    try:
        dt = parse_iso_timestamp(timestamp)
    except (ValueError, OSError):
        return "unknown"

    now = _now or datetime.now(timezone.utc)
    diff = int((now - dt).total_seconds())

    if diff < 0:
        return "just now"
    if diff < 60:
        return f"{diff}s ago"
    if diff < 3600:
        return f"{diff // 60}m ago"
    if diff < 86400:
        hours = diff // 3600
        mins = (diff % 3600) // 60
        return f"{hours}h {mins}m ago"
    days = diff // 86400
    hours = (diff % 86400) // 3600
    return f"{days}d {hours}h ago"


def format_uptime(timestamp: str | None, *, _now: datetime | None = None) -> str:
    """Format duration from ISO timestamp to now as uptime string.

    Returns ``"unknown"`` for None/empty/null or parse errors.
    """
    if not timestamp or timestamp == "null":
        return "unknown"

    try:
        dt = parse_iso_timestamp(timestamp)
    except (ValueError, OSError):
        return "unknown"

    now = _now or datetime.now(timezone.utc)
    diff = int((now - dt).total_seconds())

    if diff < 0:
        return "0s"
    if diff < 60:
        return f"{diff}s"
    if diff < 3600:
        return f"{diff // 60}m"
    if diff < 86400:
        hours = diff // 3600
        mins = (diff % 3600) // 60
        return f"{hours}h {mins}m"
    days = diff // 86400
    hours = (diff % 86400) // 3600
    return f"{days}d {hours}h"


def format_seconds(seconds: int) -> str:
    """Format seconds into human-readable duration."""
    if seconds < 0:
        return "unknown"
    if seconds < 60:
        return f"{seconds}s"
    if seconds < 3600:
        mins = seconds // 60
        secs = seconds % 60
        return f"{mins}m {secs}s" if secs else f"{mins}m"
    if seconds < 86400:
        hours = seconds // 3600
        mins = (seconds % 3600) // 60
        return f"{hours}h {mins}m"
    days = seconds // 86400
    hours = (seconds % 86400) // 3600
    return f"{days}d {hours}h"


# ---------------------------------------------------------------------------
# Section rendering
# ---------------------------------------------------------------------------

def render_daemon_status(
    repo_root: pathlib.Path,
    c: _Colors,
    *,
    _now: datetime | None = None,
    spawn_loop_state: SpawnLoopState | None = None,
) -> list[str]:
    """Render orchestrator status section."""
    lines: list[str] = []
    stop_loop = repo_root / ".loom" / "stop-spawn-loop"
    loop_pidfile = repo_root / ".loom" / "spawn-loop.pid"

    if spawn_loop_state is not None and spawn_loop_state.present:
        loop_alive = loop_pidfile.exists()
        if stop_loop.exists():
            status = f"{c.yellow}Stopping{c.reset}"
        elif loop_alive:
            status = f"{c.green}Running{c.reset}"
        else:
            status = f"{c.red}Stopped{c.reset}"

        uptime = (
            format_uptime(spawn_loop_state.started_at, _now=_now)
            if loop_alive and spawn_loop_state.started_at
            else "n/a"
        )

        lines.append(f"  {c.bold}Spawn Loop:{c.reset} {status}")
        lines.append(f"  {c.bold}Uptime:{c.reset} {uptime}")
        lines.append(f"  {c.bold}Tracked Tasks:{c.reset} {len(spawn_loop_state.running)}")
    else:
        lines.append(f"  {c.bold}Spawn Loop:{c.reset} {c.red}Not running{c.reset}")
        lines.append(f"  {c.bold}Uptime:{c.reset} n/a")
        lines.append(f"  {c.bold}Tracked Tasks:{c.reset} 0")

    return lines


def render_spawn_loop_tasks(
    spawn_loop_state: SpawnLoopState,
    c: _Colors,
    *,
    _now: datetime | None = None,
) -> list[str]:
    """Render spawn-loop in-flight task rows (replacement for render_shepherds).

    Each spawn-loop child is a ``claude -p "/loom:sweep N"`` invocation with
    its own OAuth token. We have far less per-task state than the daemon
    brain emitted (no phases, no judge retries, no PR number), so the row
    is intentionally minimal: ``issue``, ``pid``, ``uptime``, ``token``.
    """
    lines = [f"  {c.bold}In-Flight Tasks:{c.reset}"]

    if not spawn_loop_state.running:
        lines.append(f"    {c.gray}No tasks running (spawn loop idle){c.reset}")
        return lines

    lines.append(f"    {c.cyan}{len(spawn_loop_state.running)} task(s) running{c.reset}")
    lines.append("")

    for task in sorted(spawn_loop_state.running, key=lambda t: t.issue):
        uptime = (
            format_uptime(task.started_at, _now=_now)
            if task.started_at
            else "unknown"
        )
        token_part = f" [token: {task.token}]" if task.token and task.token != "unknown" else ""
        heartbeat_part = ""
        if task.last_heartbeat:
            heartbeat_part = f" [heartbeat: {time_ago(task.last_heartbeat, _now=_now)}]"
        lines.append(
            f"    {c.green}sweep-{task.issue}:{c.reset} Issue #{task.issue} "
            f"pid={task.pid} ({uptime}){token_part}{heartbeat_part}"
        )

    return lines


def render_system_state(
    snapshot: dict[str, Any],
    c: _Colors,
) -> list[str]:
    """Render system state section (issue/PR counts)."""
    computed = snapshot.get("computed", {})
    proposals = snapshot.get("proposals", {})
    architect_count = len(proposals.get("architect", []))
    hermit_count = len(proposals.get("hermit", []))

    lines = [f"  {c.bold}System State:{c.reset}"]
    lines.append(f"    Ready issues (loom:issue): {c.bold}{computed.get('total_ready', 0)}{c.reset}")
    lines.append(f"    Building (loom:building): {c.bold}{computed.get('total_building', 0)}{c.reset}")
    lines.append(f"    Curated (awaiting approval): {c.bold}{len(proposals.get('curated', []))}{c.reset}")
    total_proposals = architect_count + hermit_count
    lines.append(f"    Proposals pending: {c.bold}{total_proposals}{c.reset} (arch: {architect_count}, hermit: {hermit_count})")
    lines.append(f"    PRs pending review: {c.bold}{computed.get('prs_awaiting_review', 0)}{c.reset}")
    lines.append(f"    PRs ready to merge: {c.bold}{computed.get('prs_ready_to_merge', 0)}{c.reset}")

    return lines


def render_stuck_detection(
    repo_root: pathlib.Path,
    c: _Colors,
) -> list[str]:
    """Render stuck detection status section."""
    lines = [f"  {c.bold}Stuck Detection:{c.reset}"]

    interventions_dir = repo_root / ".loom" / "interventions"
    stuck_config_file = repo_root / ".loom" / "stuck-config.json"

    # Count active interventions
    intervention_count = 0
    intervention_details: list[tuple[str, str, str]] = []
    if interventions_dir.is_dir():
        for f in interventions_dir.glob("*.json"):
            data = read_json_file(f)
            if isinstance(data, dict):
                intervention_count += 1
                agent_id = data.get("agent_id", "unknown")
                severity = data.get("severity", "unknown")
                itype = data.get("intervention_type", "unknown")
                intervention_details.append((agent_id, itype, severity))

    if intervention_count > 0:
        lines.append(f"    Status: {c.red}{intervention_count} active intervention(s){c.reset}")
        for agent_id, itype, severity in intervention_details:
            lines.append(f"      {c.yellow}{agent_id}{c.reset}: {itype} ({severity})")
    else:
        lines.append(f"    Status: {c.green}All agents healthy{c.reset}")

    # Show configuration
    if stuck_config_file.exists():
        config = read_json_file(stuck_config_file)
        if isinstance(config, dict):
            idle_threshold = config.get("idle_threshold", 600)
            working_threshold = config.get("working_threshold", 1800)
            intervention_mode = config.get("intervention_mode", "escalate")
            lines.append(f"    Config: idle={idle_threshold // 60}m, working={working_threshold // 60}m, mode={intervention_mode}")
    else:
        lines.append(f"    Config: {c.gray}Using defaults (idle=10m, working=30m, mode=escalate){c.reset}")

    return lines


def render_layer3_actions(
    snapshot: dict[str, Any],
    repo_root: pathlib.Path,
    c: _Colors,
) -> list[str]:
    """Render available Layer 3 actions section."""
    lines = [f"  {c.bold}Layer 3 Actions Available:{c.reset}", ""]

    proposals = snapshot.get("proposals", {})
    architect_count = len(proposals.get("architect", []))
    hermit_count = len(proposals.get("hermit", []))
    curated_count = len(proposals.get("curated", []))

    if architect_count > 0 or hermit_count > 0:
        lines.append(f"    {c.yellow}Pending Approvals:{c.reset}")
        if architect_count > 0:
            lines.append(f"      - View architect proposals: {c.cyan}gh issue list --label loom:architect{c.reset}")
            lines.append(f"      - Approve proposal: {c.cyan}gh issue edit <N> --remove-label loom:architect --add-label loom:issue{c.reset}")
        if hermit_count > 0:
            lines.append(f"      - View hermit proposals: {c.cyan}gh issue list --label loom:hermit{c.reset}")
            lines.append(f"      - Approve proposal: {c.cyan}gh issue edit <N> --remove-label loom:hermit --add-label loom:issue{c.reset}")
        lines.append("")

    if curated_count > 0:
        lines.append(f"    {c.yellow}Curated Issues Awaiting Approval:{c.reset}")
        lines.append(f"      - View curated: {c.cyan}gh issue list --label loom:curated{c.reset}")
        lines.append(f"      - Approve: {c.cyan}gh issue edit <N> --add-label loom:issue{c.reset}")
        lines.append(f"      {c.gray}(loom:curated is preserved to indicate curation status){c.reset}")
        lines.append("")

    # Stuck agent actions
    interventions_dir = repo_root / ".loom" / "interventions"
    if interventions_dir.is_dir() and any(interventions_dir.glob("*.json")):
        lines.append(f"    {c.yellow}Stuck Agent Actions:{c.reset}")
        lines.append(f"      - View stuck status: {c.cyan}loom-stuck-detection status{c.reset}")
        lines.append(f"      - Clear intervention: {c.cyan}loom-stuck-detection clear <agent-id>{c.reset}")
        lines.append(f"      - Resume agent: {c.cyan}./.loom/scripts/signal.sh clear <agent-id>{c.reset}")
        lines.append(f"      - View history: {c.cyan}loom-stuck-detection history{c.reset}")
        lines.append("")

    return lines


# ---------------------------------------------------------------------------
# Output modes
# ---------------------------------------------------------------------------

def output_formatted(
    snapshot: dict[str, Any],
    repo_root: pathlib.Path,
    *,
    use_color: bool = True,
    _now: datetime | None = None,
    spawn_loop_state: SpawnLoopState | None = None,
) -> str:
    """Render full formatted status display."""
    c = _Colors(use_color=use_color)
    lines: list[str] = []

    lines.append("")
    lines.append(f"{c.bold}{c.cyan}======================================================================={c.reset}")
    lines.append(f"{c.bold}{c.cyan}  LOOM SYSTEM STATUS (read-only){c.reset}")
    lines.append(f"{c.bold}{c.cyan}======================================================================={c.reset}")
    lines.append("")

    lines.extend(render_daemon_status(repo_root, c, _now=_now, spawn_loop_state=spawn_loop_state))
    lines.append("")
    lines.extend(render_system_state(snapshot, c))
    lines.append("")
    if spawn_loop_state is not None and spawn_loop_state.present:
        lines.extend(render_spawn_loop_tasks(spawn_loop_state, c, _now=_now))
    else:
        lines.append(f"  {c.bold}In-Flight Tasks:{c.reset}")
        lines.append(f"    {c.gray}Spawn loop not active{c.reset}")
    lines.append("")
    lines.extend(render_stuck_detection(repo_root, c))
    lines.append("")
    lines.extend(render_layer3_actions(snapshot, repo_root, c))

    lines.append(f"{c.bold}{c.cyan}======================================================================={c.reset}")
    lines.append("")

    return "\n".join(lines)


def output_json(snapshot: dict[str, Any]) -> str:
    """Render JSON output from snapshot data."""
    return json.dumps(snapshot, indent=2)


def render_agents_table(
    repo_root: pathlib.Path,
    *,
    _now: datetime | None = None,
    spawn_loop_state: SpawnLoopState | None = None,
) -> None:
    """Render a rich table of all active spawn-loop sweep tasks directly to stdout."""
    from rich.console import Console
    from rich.table import Table
    import rich.box

    console = Console()
    table = Table(box=rich.box.SIMPLE_HEAVY, show_header=True, header_style="bold")
    table.add_column("Agent", style="bold")
    table.add_column("Status")
    table.add_column("Issue")
    table.add_column("Phase")
    table.add_column("Runtime")
    table.add_column("Last Heartbeat")

    if spawn_loop_state is not None and spawn_loop_state.present:
        for task in sorted(spawn_loop_state.running, key=lambda t: t.issue):
            status_markup = "[green]running[/green]"
            runtime_str = (
                format_uptime(task.started_at, _now=_now)
                if task.started_at
                else "-"
            )
            heartbeat_str = (
                time_ago(task.last_heartbeat, _now=_now)
                if task.last_heartbeat
                else "-"
            )
            table.add_row(
                f"sweep-{task.issue}",
                status_markup,
                f"#{task.issue}",
                "sweep",
                runtime_str,
                heartbeat_str,
            )
        if not spawn_loop_state.running:
            table.add_row("(spawn-loop)", "[dim]idle[/dim]", "-", "-", "-", "-")
    else:
        table.add_row("(spawn-loop)", "[dim]not running[/dim]", "-", "-", "-", "-")

    console.print(table)


def output_fast(
    repo_root: pathlib.Path,
    *,
    _now: datetime | None = None,
    spawn_loop_state: SpawnLoopState | None = None,
) -> None:
    """Print a fast agent status table (no gh queries)."""
    from rich.console import Console

    console = Console()
    stop_loop = repo_root / ".loom" / "stop-spawn-loop"
    loop_pidfile = repo_root / ".loom" / "spawn-loop.pid"

    loop_alive = loop_pidfile.exists()
    if spawn_loop_state is not None and spawn_loop_state.present:
        if stop_loop.exists():
            loop_status = "[yellow]Stopping[/yellow]"
        elif loop_alive:
            loop_status = "[green]Running[/green]"
        else:
            loop_status = "[red]Stopped[/red]"

        uptime = (
            format_uptime(spawn_loop_state.started_at, _now=_now)
            if loop_alive and spawn_loop_state.started_at
            else "n/a"
        )
        pid_str = "unknown"
        if loop_pidfile.exists():
            try:
                pid_str = loop_pidfile.read_text().strip() or "unknown"
            except OSError:
                pass
    else:
        loop_status = "[red]Not running[/red]"
        uptime = "n/a"
        pid_str = "unknown"

    console.print(f"Spawn Loop: {loop_status}  |  Uptime: {uptime}  |  PID: {pid_str}")
    console.print()
    render_agents_table(repo_root, _now=_now, spawn_loop_state=spawn_loop_state)


# ---------------------------------------------------------------------------
# File idle time helper
# ---------------------------------------------------------------------------

def _get_file_idle_seconds(output_file: str) -> int:
    """Get idle seconds from output file modification time. Returns -1 on error."""
    try:
        p = pathlib.Path(output_file)
        if not p.exists():
            return -1
        mtime = p.stat().st_mtime
        import time
        return int(time.time() - mtime)
    except OSError:
        return -1


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

_HELP_TEXT = """\
loom-status - Loom System Status (Read-Only)

USAGE:
    loom-status              Display full system status
    loom-status --fast       Display agent table (no gh queries, fast)
    loom-status --json       Output status as JSON
    loom-status --help       Show this help message

DESCRIPTION:
    This command provides a read-only observation interface for the Loom
    orchestration system. It displays:

    - Daemon status (running/stopped, uptime)
    - System state (issue counts by label)
    - Shepherd pool status (active/idle, assigned issues, idle time)
    - Support role status (Architect, Hermit, Guide, Champion)
    - Session statistics (completed issues, PRs merged)
    - Pipeline status (blocked items)
    - Recent warnings
    - Stuck detection status
    - Available Layer 3 interventions

LAYER 3 ROLE:
    The human observer (Layer 3) uses this command to:

    - Monitor autonomous development progress
    - Identify issues needing human intervention
    - Approve pending proposals
    - Initiate graceful shutdown when needed

EXAMPLES:
    # View current system status
    loom-status

    # Get status as JSON for scripting
    loom-status --json | jq '.computed'

RELATED COMMANDS:
    /loom status                Equivalent to this script
    /loom:sweep <issue>         Run a single-issue lifecycle
    mcp__loom__dispatch_sweep   Dispatch a sweep against loom-daemon
    touch .loom/stop-daemon     Signal graceful daemon shutdown
"""


def main(argv: Sequence[str] | None = None) -> None:
    """CLI entry point."""
    args = list(argv if argv is not None else sys.argv[1:])

    json_mode = False
    fast_mode = False
    for arg in args:
        if arg in ("--help", "-h"):
            print(_HELP_TEXT, end="")
            sys.exit(0)
        elif arg == "--json":
            json_mode = True
        elif arg == "--fast":
            fast_mode = True
        else:
            print(f"Error: Unknown option '{arg}'", file=sys.stderr)
            print("Run 'loom-status --help' for usage", file=sys.stderr)
            sys.exit(1)

    if fast_mode and json_mode:
        print("Error: --fast and --json are mutually exclusive", file=sys.stderr)
        sys.exit(1)

    repo_root = find_repo_root()

    if fast_mode:
        spawn_loop_state = read_spawn_loop_state(repo_root)
        output_fast(repo_root, spawn_loop_state=spawn_loop_state)
        return

    # Build snapshot from forge queries (replaces build_snapshot() from deleted snapshot.py).
    pipeline = collect_pipeline_data(repo_root)
    now = now_utc()
    timestamp = now.strftime("%Y-%m-%dT%H:%M:%SZ")

    # Build a minimal snapshot dict that the render functions expect.
    ready_to_merge = pipeline.get("ready_to_merge", [])
    merge_conflicted = [
        pr for pr in ready_to_merge
        if any(
            (lbl.get("name") if isinstance(lbl, dict) else None) == "loom:merge-conflict"
            for lbl in pr.get("labels", []) or []
        )
    ]
    snapshot: dict[str, Any] = {
        "timestamp": timestamp,
        "pipeline": {
            "ready_issues": pipeline.get("ready_issues", []),
            "building_issues": pipeline.get("building_issues", []),
            "blocked_issues": pipeline.get("blocked_issues", []),
        },
        "proposals": {
            "architect": pipeline.get("architect_proposals", []),
            "hermit": pipeline.get("hermit_proposals", []),
            "curated": pipeline.get("curated_issues", []),
        },
        "prs": {
            "review_requested": pipeline.get("review_requested", []),
            "changes_requested": pipeline.get("changes_requested", []),
            "ready_to_merge": ready_to_merge,
            "merge_conflicted": merge_conflicted,
            "merge_conflict_count": len(merge_conflicted),
        },
        "computed": {
            "total_ready": len(pipeline.get("ready_issues", [])),
            "total_building": len(pipeline.get("building_issues", [])),
            "total_blocked": len(pipeline.get("blocked_issues", [])),
            "total_uncurated": len(pipeline.get("uncurated_issues", [])),
            "prs_awaiting_review": len(pipeline.get("review_requested", [])),
            "prs_needing_fixes": len(pipeline.get("changes_requested", [])),
            "prs_ready_to_merge": len(ready_to_merge),
            "prs_with_merge_conflicts": len(merge_conflicted),
        },
        "usage": pipeline.get("usage", {"error": "no data"}),
        "ci_status": pipeline.get("ci_status", {"status": "unknown"}),
    }

    if json_mode:
        print(output_json(snapshot))
    else:
        spawn_loop_state = read_spawn_loop_state(repo_root)
        colored = _use_color()
        print(output_formatted(
            snapshot, repo_root,
            use_color=colored,
            spawn_loop_state=spawn_loop_state,
        ))


if __name__ == "__main__":
    main()
