"""Formatting functions for stuck agent detection output.

Extracted from stuck_detection.py to improve modularity and testability.
All formatters consume StuckDetection, StuckDetectionConfig, and related
data models, producing formatted strings for CLI output.
"""

from __future__ import annotations

import json
import pathlib
from typing import TYPE_CHECKING

from loom_tools.common.state import read_json_file, read_stuck_history
from loom_tools.common.time_utils import now_utc
from loom_tools.models.stuck import StuckDetection

if TYPE_CHECKING:
    from loom_tools.stuck_detection import StuckDetectionConfig


def format_check_json(
    results: list[StuckDetection],
    stuck_agents: list[str],
    config: StuckDetectionConfig,
) -> str:
    """Format check-all results as JSON."""
    output = {
        "checked_at": now_utc().strftime("%Y-%m-%dT%H:%M:%SZ"),
        "total_checked": len(results),
        "stuck_count": len(stuck_agents),
        "stuck_agents": stuck_agents,
        "results": [r.to_dict() for r in results],
        "config": {
            "idle_threshold": config.idle_threshold,
            "working_threshold": config.working_threshold,
            "intervention_mode": config.intervention_mode,
        },
    }
    return json.dumps(output, indent=2)


def format_check_human(
    results: list[StuckDetection],
    stuck_agents: list[str],
    config: StuckDetectionConfig,
) -> str:
    """Format check-all results for human display."""
    lines = [
        "",
        "=======================================================================",
        "  STUCK AGENT DETECTION",
        "=======================================================================",
        "",
        "  Configuration:",
        f"    Idle threshold: {config.idle_threshold}s",
        f"    Working threshold: {config.working_threshold}s",
        f"    Intervention mode: {config.intervention_mode}",
        "",
        "  Results:",
        f"    Total checked: {len(results)}",
        f"    Stuck agents: {len(stuck_agents)}",
        "",
    ]

    for result in results:
        if result.stuck:
            severity_marker = {
                "warning": "WARNING",
                "elevated": "ELEVATED",
                "critical": "CRITICAL",
            }.get(result.severity, result.severity.upper())

            lines.append(
                f"    STUCK {result.agent_id} (issue #{result.issue})"
            )
            lines.append(f"      Severity: {severity_marker}")
            lines.append(f"      Indicators: {', '.join(result.indicators)}")
            lines.append("")
        else:
            if result.status == "idle":
                lines.append(f"    {result.agent_id}: idle")
            elif result.status == "unknown":
                lines.append(f"    {result.agent_id}: unknown")
            else:
                lines.append(
                    f"    {result.agent_id}: working normally (issue #{result.issue})"
                )

    lines.extend(
        [
            "=======================================================================",
            "",
        ]
    )

    return "\n".join(lines)


def format_agent_json(detection: StuckDetection) -> str:
    """Format single agent detection as JSON."""
    output = detection.to_dict()
    # Add missing_milestones field for compatibility
    missing = []
    for indicator in detection.indicators:
        if indicator.startswith("missing_milestone:"):
            missing = indicator.split(":")[1].split(",")
            break
    output["missing_milestones"] = missing
    return json.dumps(output, indent=2)


def format_status_human(
    repo_root: pathlib.Path, config: StuckDetectionConfig
) -> str:
    """Format status summary for human display."""
    loom_dir = repo_root / ".loom"
    interventions_dir = loom_dir / "interventions"
    history_path = loom_dir / "stuck-history.json"

    lines = [
        "",
        "=======================================================================",
        "  STUCK DETECTION STATUS",
        "=======================================================================",
        "",
        "  Configuration:",
        f"    Idle threshold: {config.idle_threshold}s ({config.idle_threshold // 60}m)",
        f"    Working threshold: {config.working_threshold}s ({config.working_threshold // 60}m)",
        f"    Loop threshold: {config.loop_threshold}x",
        f"    Error spike threshold: {config.error_spike_threshold}",
        f"    Intervention mode: {config.intervention_mode}",
        "",
        "  Active Interventions:",
    ]

    # List active interventions
    intervention_count = 0
    if interventions_dir.exists():
        for intervention_file in sorted(interventions_dir.glob("*.json")):
            try:
                data = read_json_file(intervention_file)
                if isinstance(data, dict):
                    agent_id = data.get("agent_id", "unknown")
                    severity = data.get("severity", "unknown")
                    intervention_type = data.get("intervention_type", "unknown")
                    triggered_at = data.get("triggered_at", "unknown")
                    lines.append(
                        f"    {agent_id}: {intervention_type} ({severity}) - {triggered_at}"
                    )
                    intervention_count += 1
            except Exception:
                pass

    if intervention_count == 0:
        lines.append("    No active interventions")

    lines.append("")
    lines.append("  Recent Detections:")

    # Show recent history
    if history_path.exists():
        try:
            history = read_stuck_history(repo_root)
            recent = history.entries[-5:] if history.entries else []
            for entry in recent:
                agent_id = entry.detection.agent_id
                severity = entry.detection.severity
                lines.append(f"    {entry.detected_at}: {agent_id} - {severity}")
            if not recent:
                lines.append("    No recent detections")
        except Exception:
            lines.append("    No recent detections")
    else:
        lines.append("    No history available")

    lines.extend(
        [
            "",
            "=======================================================================",
            "",
        ]
    )

    return "\n".join(lines)


def format_history_human(
    repo_root: pathlib.Path, agent_id: str | None = None
) -> str:
    """Format history for human display."""
    history = read_stuck_history(repo_root)

    lines = ["", "Stuck Detection History", ""]

    if not history.entries:
        lines.append("  No stuck detection history available")
        return "\n".join(lines)

    if agent_id:
        lines.append(f"Agent: {agent_id}")
        lines.append("")
        entries = [e for e in history.entries if e.detection.agent_id == agent_id]
    else:
        entries = history.entries[-20:]

    for entry in entries:
        indicators = ", ".join(entry.detection.indicators)
        lines.append(
            f"  {entry.detected_at}: {entry.detection.agent_id} - {entry.detection.severity} - {indicators}"
        )

    return "\n".join(lines)


def format_intervention_summary(
    detection: StuckDetection, timestamp: str, loom_dir: pathlib.Path
) -> str:
    """Format human-readable intervention summary.

    Args:
        detection: The stuck detection result.
        timestamp: When the intervention was triggered.
        loom_dir: Path to the .loom directory (used in file path output).
    """
    lines = [
        "STUCK AGENT INTERVENTION",
        "========================",
        "",
        f"Agent:       {detection.agent_id}",
        f"Issue:       #{detection.issue}" if detection.issue else "Issue:       none",
        f"Severity:    {detection.severity}",
        f"Type:        {detection.suggested_intervention}",
        f"Detected:    {timestamp}",
        "",
        f"Indicators:  {', '.join(detection.indicators)}",
        "",
        "Suggested Actions:",
    ]

    intervention_type = detection.suggested_intervention
    issue = detection.issue or "ISSUE"

    if intervention_type == "alert":
        lines.extend(
            [
                f'  - Review agent output: cat $(jq -r \'.shepherds["{detection.agent_id}"].output_file\' .loom/daemon-state.json)',
                f"  - Check issue status: gh issue view {issue}",
            ]
        )
    elif intervention_type == "suggest":
        lines.extend(
            [
                "  - Consider switching roles (Builder -> Doctor)",
                "  - Check if issue dependencies are blocking",
                "  - Review issue for missing requirements",
            ]
        )
    elif intervention_type == "pause":
        lines.extend(
            [
                "  - Agent has been paused automatically",
                "  - Review loop patterns in output",
                f"  - Restart with: signal.sh clear {detection.agent_id}",
            ]
        )
    elif intervention_type == "clarify":
        lines.extend(
            [
                "  - Request clarification from issue author",
                "  - Add loom:blocked label with reason",
                f"  - Command: gh issue edit {issue} --add-label loom:blocked",
            ]
        )
    elif intervention_type == "escalate":
        lines.extend(
            [
                "  - ESCALATION: All interventions triggered",
                "  - Human attention required immediately",
            ]
        )

    lines.extend(
        [
            "",
            f"Intervention file: {loom_dir}/interventions/{detection.agent_id}-*.json",
        ]
    )

    return "\n".join(lines)
