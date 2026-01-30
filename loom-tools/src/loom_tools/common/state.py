"""JSON state file I/O with atomic writes."""

from __future__ import annotations

import json
import os
import pathlib
import tempfile
from typing import Any

from loom_tools.models.daemon_state import DaemonState
from loom_tools.models.health import AlertsFile, HealthMetrics
from loom_tools.models.progress import ShepherdProgress
from loom_tools.models.stuck import StuckHistory


def read_json_file(path: pathlib.Path) -> dict[str, Any] | list[Any]:
    """Read and parse a JSON file.

    Returns an empty ``{}`` if the file is missing, empty, or contains
    invalid JSON.
    """
    try:
        text = path.read_text()
        if not text.strip():
            return {}
        return json.loads(text)
    except (FileNotFoundError, json.JSONDecodeError):
        return {}


def write_json_file(
    path: pathlib.Path,
    data: dict[str, Any] | list[Any],
) -> None:
    """Write *data* to *path* atomically via a temp file and ``os.replace``."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=path.parent, suffix=".tmp")
    try:
        with os.fdopen(fd, "w") as f:
            json.dump(data, f, indent=2)
            f.write("\n")
        os.replace(tmp, path)
    except BaseException:
        # Clean up the temp file on any failure.
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def read_daemon_state(repo_root: pathlib.Path) -> DaemonState:
    """Load ``.loom/daemon-state.json`` into a :class:`DaemonState`."""
    data = read_json_file(repo_root / ".loom" / "daemon-state.json")
    if isinstance(data, list):
        return DaemonState()
    return DaemonState.from_dict(data)


def read_progress_files(repo_root: pathlib.Path) -> list[ShepherdProgress]:
    """Load all ``.loom/progress/shepherd-*.json`` files."""
    progress_dir = repo_root / ".loom" / "progress"
    if not progress_dir.is_dir():
        return []
    results: list[ShepherdProgress] = []
    for p in sorted(progress_dir.glob("shepherd-*.json")):
        data = read_json_file(p)
        if isinstance(data, dict):
            results.append(ShepherdProgress.from_dict(data))
    return results


def read_health_metrics(repo_root: pathlib.Path) -> HealthMetrics:
    """Load ``.loom/health-metrics.json`` into a :class:`HealthMetrics`."""
    data = read_json_file(repo_root / ".loom" / "health-metrics.json")
    if isinstance(data, list):
        return HealthMetrics()
    return HealthMetrics.from_dict(data)


def read_alerts(repo_root: pathlib.Path) -> AlertsFile:
    """Load ``.loom/alerts.json`` into an :class:`AlertsFile`."""
    data = read_json_file(repo_root / ".loom" / "alerts.json")
    if isinstance(data, list):
        return AlertsFile()
    return AlertsFile.from_dict(data)


def read_stuck_history(repo_root: pathlib.Path) -> StuckHistory:
    """Load ``.loom/stuck-history.json`` into a :class:`StuckHistory`."""
    data = read_json_file(repo_root / ".loom" / "stuck-history.json")
    if isinstance(data, list):
        return StuckHistory()
    return StuckHistory.from_dict(data)
