"""JSON state file I/O with atomic writes.

Provides centralized JSON parsing utilities with consistent error handling.
All parsing functions gracefully handle failures by returning defaults.
"""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import tempfile
from typing import Any, TypeVar

from loom_tools.common.paths import LoomPaths
from loom_tools.models.daemon_state import DaemonState
from loom_tools.models.health import AlertsFile, HealthMetrics
from loom_tools.models.progress import ShepherdProgress
from loom_tools.models.stuck import StuckHistory


# Type variable for generic default handling
T = TypeVar("T", dict[str, Any], list[Any])


def safe_parse_json(
    text: str,
    default: T | None = None,
) -> dict[str, Any] | list[Any]:
    """Parse JSON text, returning default on failure.

    Handles empty strings, whitespace-only strings, and invalid JSON
    gracefully by returning the provided default (or ``{}`` if not
    specified).

    Args:
        text: JSON string to parse.
        default: Value to return on parse failure. Defaults to ``{}``.

    Returns:
        Parsed JSON as dict or list, or the default on failure.

    Examples:
        >>> safe_parse_json('{"key": "value"}')
        {'key': 'value'}
        >>> safe_parse_json('invalid json')
        {}
        >>> safe_parse_json('', default=[])
        []
    """
    if default is None:
        default = {}  # type: ignore[assignment]
    if not text or not text.strip():
        return default
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return default


def parse_command_output(
    result: subprocess.CompletedProcess[str],
    default: T | None = None,
) -> dict[str, Any] | list[Any]:
    """Parse JSON from subprocess stdout.

    Handles non-zero exit codes, empty stdout, and invalid JSON gracefully
    by returning the provided default (or ``{}`` if not specified).

    Args:
        result: CompletedProcess from subprocess.run().
        default: Value to return on parse failure. Defaults to ``{}``.

    Returns:
        Parsed JSON as dict or list, or the default on failure.

    Examples:
        >>> result = subprocess.run(['gh', 'issue', 'list', '--json', 'number'],
        ...                         capture_output=True, text=True)
        >>> parse_command_output(result)
        [{'number': 1}, {'number': 2}]
        >>> parse_command_output(result, default=[])  # On failure
        []
    """
    if default is None:
        default = {}  # type: ignore[assignment]
    if result.returncode != 0:
        return default
    return safe_parse_json(result.stdout, default)


def read_json_file(
    path: pathlib.Path,
    default: T | None = None,
) -> dict[str, Any] | list[Any]:
    """Read and parse a JSON file.

    Returns the provided default (or ``{}``) if the file is missing, empty,
    or contains invalid JSON.

    Args:
        path: Path to the JSON file.
        default: Value to return on read/parse failure. Defaults to ``{}``.

    Returns:
        Parsed JSON as dict or list, or the default on failure.
    """
    if default is None:
        default = {}  # type: ignore[assignment]
    try:
        text = path.read_text()
        return safe_parse_json(text, default)
    except (FileNotFoundError, OSError):
        return default


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
    paths = LoomPaths(repo_root)
    data = read_json_file(paths.daemon_state_file)
    if isinstance(data, list):
        return DaemonState()
    return DaemonState.from_dict(data)


def read_progress_files(repo_root: pathlib.Path) -> list[ShepherdProgress]:
    """Load all ``.loom/progress/shepherd-*.json`` files."""
    paths = LoomPaths(repo_root)
    if not paths.progress_dir.is_dir():
        return []
    results: list[ShepherdProgress] = []
    for p in sorted(paths.progress_dir.glob("shepherd-*.json")):
        data = read_json_file(p)
        if isinstance(data, dict):
            results.append(ShepherdProgress.from_dict(data))
    return results


def read_health_metrics(repo_root: pathlib.Path) -> HealthMetrics:
    """Load ``.loom/health-metrics.json`` into a :class:`HealthMetrics`."""
    paths = LoomPaths(repo_root)
    data = read_json_file(paths.health_metrics_file)
    if isinstance(data, list):
        return HealthMetrics()
    return HealthMetrics.from_dict(data)


def read_alerts(repo_root: pathlib.Path) -> AlertsFile:
    """Load ``.loom/alerts.json`` into an :class:`AlertsFile`."""
    paths = LoomPaths(repo_root)
    data = read_json_file(paths.alerts_file)
    if isinstance(data, list):
        return AlertsFile()
    return AlertsFile.from_dict(data)


def read_stuck_history(repo_root: pathlib.Path) -> StuckHistory:
    """Load ``.loom/stuck-history.json`` into a :class:`StuckHistory`."""
    paths = LoomPaths(repo_root)
    data = read_json_file(paths.stuck_history_file)
    if isinstance(data, list):
        return StuckHistory()
    return StuckHistory.from_dict(data)
