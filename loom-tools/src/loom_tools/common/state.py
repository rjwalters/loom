"""JSON state file I/O with atomic writes.

Provides centralized JSON parsing utilities with consistent error handling.
All parsing functions gracefully handle failures by returning defaults.

Phase 3.2 note: read_daemon_state(), read_progress_files(), and
find_progress_for_issue() were removed in PR #3399 along with the Python
daemon brain (daemon_v2/) and the state files they read
(.loom/daemon-state.json, .loom/progress/).  Callers that still need a
DaemonState placeholder for backwards-compat render paths import
DaemonState directly and instantiate a default: ``DaemonState()``.
"""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import tempfile
from typing import Any, TypeVar

from loom_tools.common.paths import LoomPaths
from loom_tools.models.baseline_health import BaselineHealth
from loom_tools.models.health import AlertsFile, HealthMetrics
from loom_tools.models.spawn_loop_state import SpawnLoopState
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


def read_daemon_state(repo_root: pathlib.Path) -> "DaemonState":
    """Stub: always returns an empty DaemonState.

    The daemon-state.json producer (daemon_v2/) was deleted in Phase 3.2
    (#3399). This stub is kept so that Phase 3.1.x CLI ports (status.py,
    completions.py) that have a daemon-state fallback path continue to
    import without error.  Phase 3.4 (#3401) removes this stub along with
    all remaining daemon-state read paths.
    """
    from loom_tools.models.daemon_state import DaemonState  # local import avoids circular

    return DaemonState()


def read_progress_files(repo_root: pathlib.Path) -> list:
    """Stub: always returns empty list.

    Progress files (.loom/progress/shepherd-*.json) are retired in Phase 3.2
    (#3399). This stub is kept so that callers (test_failure_analysis.py,
    validate_phase.py) continue to import without error.
    Phase 3.3 (#3400) or Phase 3.4 (#3401) will remove the callers.
    """
    return []


def find_progress_for_issue(repo_root: pathlib.Path, issue: int) -> None:
    """Stub: always returns None.

    Progress files (.loom/progress/shepherd-*.json) are retired in Phase 3.2
    (#3399). This stub keeps validate_phase.py importable until Phase 3.4.
    """
    return None


def read_spawn_loop_state(repo_root: pathlib.Path) -> SpawnLoopState:
    """Load ``.loom/spawn-loop-state.json`` into a :class:`SpawnLoopState`.

    Returns a :class:`SpawnLoopState` with ``present=False`` when the file
    is missing — callers (e.g. ``loom-status``) use this to fall back to
    ``.loom/daemon-state.json`` for back-compat (Phase 3 port, #3390).

    Phase 3.4 (#3401) trims the daemon-state fallback once all 3.1.x ports
    have landed.
    """
    paths = LoomPaths(repo_root)
    if not paths.spawn_loop_state_file.exists():
        return SpawnLoopState.absent()
    data = read_json_file(paths.spawn_loop_state_file)
    if not isinstance(data, dict):
        # File exists but malformed — still mark as present so the caller
        # knows the spawn loop is at least configured; just return empty.
        return SpawnLoopState(present=True)
    return SpawnLoopState.from_dict(data)


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


def read_baseline_health(repo_root: pathlib.Path) -> BaselineHealth:
    """Load ``.loom/baseline-health.json`` into a :class:`BaselineHealth`.

    Returns a default ``BaselineHealth`` (status="unknown") if the file
    is missing, empty, or contains invalid JSON.
    """
    paths = LoomPaths(repo_root)
    data = read_json_file(paths.baseline_health_file)
    if isinstance(data, list):
        return BaselineHealth()
    return BaselineHealth.from_dict(data)


def write_baseline_health(repo_root: pathlib.Path, health: BaselineHealth) -> None:
    """Write a :class:`BaselineHealth` to ``.loom/baseline-health.json``."""
    paths = LoomPaths(repo_root)
    write_json_file(paths.baseline_health_file, health.to_dict())
