"""Loom system status display for Layer 3 observation.

Replaces the former ``loom-status.sh`` (814 LOC) with a Python module that
reuses ``snapshot.build_snapshot()`` for data collection and provides
colored terminal formatting.

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
from loom_tools.common.state import read_daemon_state, read_json_file
from loom_tools.common.time_utils import parse_iso_timestamp
from loom_tools.models.daemon_state import DaemonState
from loom_tools.snapshot import build_snapshot

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
    daemon_state: DaemonState,
    repo_root: pathlib.Path,
    c: _Colors,
    *,
    _now: datetime | None = None,
) -> list[str]:
    """Render daemon status section."""
    lines: list[str] = []
    stop_file = repo_root / ".loom" / "stop-daemon"

    if stop_file.exists():
        status = f"{c.yellow}Stopping{c.reset}"
    elif daemon_state.running:
        status = f"{c.green}Running{c.reset}"
    else:
        status = f"{c.red}Stopped{c.reset}"

    uptime = format_uptime(daemon_state.started_at, _now=_now) if daemon_state.running else "n/a"
    last_poll = time_ago(daemon_state.last_poll, _now=_now) if daemon_state.running else "n/a"

    lines.append(f"  {c.bold}Daemon:{c.reset} {status}")
    lines.append(f"  {c.bold}Uptime:{c.reset} {uptime}")
    lines.append(f"  {c.bold}Last Poll:{c.reset} {last_poll}")

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


def render_shepherds(
    daemon_state: DaemonState,
    c: _Colors,
    *,
    _now: datetime | None = None,
) -> list[str]:
    """Render shepherd pool status section."""
    lines = [f"  {c.bold}Shepherds:{c.reset}"]

    if not daemon_state.shepherds:
        lines.append(f"    {c.gray}No daemon state available{c.reset}")
        return lines

    active = sum(1 for e in daemon_state.shepherds.values() if e.issue is not None)
    total = len(daemon_state.shepherds)
    lines.append(f"    {c.cyan}{active}/{total} active{c.reset}")
    lines.append("")

    for sid in sorted(daemon_state.shepherds):
        entry = daemon_state.shepherds[sid]

        if entry.issue is not None:
            duration = format_uptime(entry.started, _now=_now)
            details = ""
            if entry.last_phase:
                details += f" [phase: {entry.last_phase}]"
            if entry.pr_number:
                details += f" [PR #{entry.pr_number}]"

            # Compute idle from output file
            idle_str = ""
            if entry.output_file:
                idle_secs = _get_file_idle_seconds(entry.output_file)
                if idle_secs >= 0:
                    idle_str = f", idle {format_seconds(idle_secs)}"

            lines.append(f"    {c.green}{sid}:{c.reset} Issue #{entry.issue} ({duration}{idle_str}){details}")
        else:
            # Idle shepherd
            idle_info = ""
            if entry.idle_since:
                idle_duration = format_uptime(entry.idle_since, _now=_now)
                idle_info = f"({idle_duration})"

            reason_display = ""
            if entry.idle_reason:
                reason_map = {
                    "no_ready_issues": " - no ready issues",
                    "at_capacity": " - at capacity",
                    "completed_issue": " - awaiting next",
                    "rate_limited": " - rate limited",
                    "shutdown_signal": " - shutdown",
                    "needs_human_input": " - waiting for human input",
                }
                reason_display = reason_map.get(entry.idle_reason, f" - {entry.idle_reason}")

            status_color = c.gray
            if entry.status == "errored":
                status_color = c.red
            elif entry.status == "paused":
                status_color = c.yellow

            status_display = entry.status or "idle"
            lines.append(f"    {status_color}{sid}:{c.reset} {status_display} {idle_info}{reason_display}")

    return lines


def render_support_roles(
    daemon_state: DaemonState,
    c: _Colors,
    *,
    _now: datetime | None = None,
) -> list[str]:
    """Render support roles status section."""
    lines = [f"  {c.bold}Support Roles:{c.reset}"]

    if not daemon_state.support_roles:
        lines.append(f"    {c.gray}No daemon state available{c.reset}")
        return lines

    for role in ("architect", "hermit", "guide", "champion"):
        entry = daemon_state.support_roles.get(role)
        role_display = role[0].upper() + role[1:]

        extra_info = ""
        if entry:
            raw_data = entry.to_dict()
            if role == "architect":
                proposals = raw_data.get("proposals_created", 0)
                if proposals:
                    extra_info = f" (proposals: {proposals})"
            elif role == "champion":
                merged = raw_data.get("prs_merged_this_session", 0)
                if merged:
                    extra_info = f" (merged: {merged})"

        if entry and (entry.status == "running" or entry.task_id):
            lines.append(f"    {c.green}{role_display}:{c.reset} running{extra_info}")
        elif entry and entry.status == "errored":
            lines.append(f"    {c.red}{role_display}:{c.reset} errored{extra_info}")
        else:
            last_ago = time_ago(entry.last_completed if entry else None, _now=_now)
            result_info = ""
            if entry:
                raw = entry.to_dict()
                last_result = raw.get("last_result")
                if last_result:
                    result_info = f" [{last_result}]"
            lines.append(f"    {c.gray}{role_display}:{c.reset} idle (last: {last_ago}){result_info}{extra_info}")

    return lines


def render_session_stats(
    daemon_state: DaemonState,
    c: _Colors,
) -> list[str]:
    """Render session statistics section."""
    lines = [f"  {c.bold}Session Statistics:{c.reset}"]

    if not daemon_state.started_at and daemon_state.iteration == 0:
        lines.append(f"    {c.gray}No session data available{c.reset}")
        return lines

    lines.append(f"    Iteration: {c.bold}{daemon_state.iteration}{c.reset}")
    lines.append(f"    Issues completed: {c.bold}{len(daemon_state.completed_issues)}{c.reset}")
    lines.append(f"    PRs merged: {c.bold}{daemon_state.total_prs_merged}{c.reset}")

    return lines


def render_pipeline_status(
    daemon_state: DaemonState,
    c: _Colors,
    *,
    _now: datetime | None = None,
) -> list[str]:
    """Render pipeline status section."""
    lines = [f"  {c.bold}Pipeline Status:{c.reset}"]

    ps = daemon_state.pipeline_state
    blocked = ps.blocked or []
    blocked_count = len(blocked)

    if blocked_count > 0:
        lines.append(f"    {c.red}Blocked Items: {blocked_count}{c.reset}")
        for item in blocked:
            item_type = item.get("type", "?")
            number = item.get("number", "?")
            reason = item.get("reason", "unknown")
            lines.append(f"    {c.yellow}  {item_type} #{number}: {reason}{c.reset}")
    else:
        lines.append(f"    {c.green}No blocked items{c.reset}")

    # Show last sync if pipeline data exists
    if ps.last_updated:
        lines.append(f"    {c.gray}Last sync: {time_ago(ps.last_updated, _now=_now)}{c.reset}")

    return lines


def render_warnings(
    daemon_state: DaemonState,
    c: _Colors,
) -> list[str]:
    """Render recent warnings section."""
    lines = [f"  {c.bold}Recent Warnings:{c.reset}"]

    warnings = daemon_state.warnings or []
    if not warnings:
        lines.append(f"    {c.green}No warnings{c.reset}")
        return lines

    unack = [w for w in warnings if not w.acknowledged]
    if not unack:
        lines.append(f"    {c.green}All warnings acknowledged{c.reset} ({len(warnings)} total)")
        return lines

    # Show last 5 unacknowledged
    recent = unack[-5:]
    lines.append(f"    {c.yellow}{len(unack)} unacknowledged warning(s){c.reset}")

    for w in recent:
        # Extract time portion
        time_str = w.time
        if "T" in time_str:
            time_str = time_str.split("T")[1].rstrip("Z")

        line = f"      [{w.severity}] {w.message} ({time_str})"
        if w.severity == "error":
            lines.append(f"    {c.red}{line}{c.reset}")
        elif w.severity == "warning":
            lines.append(f"    {c.yellow}{line}{c.reset}")
        else:
            lines.append(f"    {c.gray}{line}{c.reset}")

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
    daemon_state: DaemonState,
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

    stop_file = repo_root / ".loom" / "stop-daemon"
    lines.append(f"    {c.yellow}Daemon Control:{c.reset}")
    if stop_file.exists():
        lines.append(f"      - Cancel shutdown: {c.cyan}rm .loom/stop-daemon{c.reset}")
    else:
        lines.append(f"      - Stop daemon: {c.cyan}touch .loom/stop-daemon{c.reset}")
    lines.append(f"      - View daemon state: {c.cyan}cat .loom/daemon-state.json | jq{c.reset}")
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
    daemon_state: DaemonState,
    repo_root: pathlib.Path,
    *,
    use_color: bool = True,
    _now: datetime | None = None,
) -> str:
    """Render full formatted status display."""
    c = _Colors(use_color=use_color)
    lines: list[str] = []

    lines.append("")
    lines.append(f"{c.bold}{c.cyan}======================================================================={c.reset}")
    lines.append(f"{c.bold}{c.cyan}  LOOM SYSTEM STATUS (read-only){c.reset}")
    lines.append(f"{c.bold}{c.cyan}======================================================================={c.reset}")
    lines.append("")

    lines.extend(render_daemon_status(daemon_state, repo_root, c, _now=_now))
    lines.append("")
    lines.extend(render_system_state(snapshot, c))
    lines.append("")
    lines.extend(render_shepherds(daemon_state, c, _now=_now))
    lines.append("")
    lines.extend(render_support_roles(daemon_state, c, _now=_now))
    lines.append("")
    lines.extend(render_session_stats(daemon_state, c))
    lines.append("")
    lines.extend(render_pipeline_status(daemon_state, c, _now=_now))
    lines.append("")
    lines.extend(render_warnings(daemon_state, c))
    lines.append("")
    lines.extend(render_stuck_detection(repo_root, c))
    lines.append("")
    lines.extend(render_layer3_actions(snapshot, daemon_state, repo_root, c))

    lines.append(f"{c.bold}{c.cyan}======================================================================={c.reset}")
    lines.append("")

    return "\n".join(lines)


def output_json(snapshot: dict[str, Any]) -> str:
    """Render JSON output from snapshot data."""
    return json.dumps(snapshot, indent=2)


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

FILES:
    .loom/daemon-state.json     Daemon state file
    .loom/stop-daemon           Shutdown signal file

RELATED COMMANDS:
    /loom                       Run the daemon (Layer 2)
    /loom status                Equivalent to this script
    touch .loom/stop-daemon     Signal graceful shutdown
"""


def main(argv: Sequence[str] | None = None) -> None:
    """CLI entry point."""
    args = list(argv if argv is not None else sys.argv[1:])

    json_mode = False
    for arg in args:
        if arg in ("--help", "-h"):
            print(_HELP_TEXT, end="")
            sys.exit(0)
        elif arg == "--json":
            json_mode = True
        else:
            print(f"Error: Unknown option '{arg}'", file=sys.stderr)
            print("Run 'loom-status --help' for usage", file=sys.stderr)
            sys.exit(1)

    # Build snapshot (includes parallel gh queries)
    snapshot = build_snapshot()

    if json_mode:
        print(output_json(snapshot))
    else:
        repo_root = find_repo_root()
        daemon_state = read_daemon_state(repo_root)
        colored = _use_color()
        print(output_formatted(snapshot, daemon_state, repo_root, use_color=colored))


if __name__ == "__main__":
    main()
