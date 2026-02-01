"""Tests for loom_tools.common.state."""

from __future__ import annotations

import json
import pathlib
import subprocess
from unittest.mock import Mock

from loom_tools.common.state import (
    find_progress_for_issue,
    parse_command_output,
    read_json_file,
    safe_parse_json,
    write_json_file,
)


# ---------------------------------------------------------------------------
# Tests for safe_parse_json
# ---------------------------------------------------------------------------


def test_safe_parse_json_valid_dict() -> None:
    result = safe_parse_json('{"key": "value"}')
    assert result == {"key": "value"}


def test_safe_parse_json_valid_list() -> None:
    result = safe_parse_json('[1, 2, 3]')
    assert result == [1, 2, 3]


def test_safe_parse_json_empty_string() -> None:
    result = safe_parse_json("")
    assert result == {}


def test_safe_parse_json_whitespace_only() -> None:
    result = safe_parse_json("   \n  \t  ")
    assert result == {}


def test_safe_parse_json_invalid_json() -> None:
    result = safe_parse_json("{invalid json")
    assert result == {}


def test_safe_parse_json_custom_default_dict() -> None:
    result = safe_parse_json("invalid", default={"fallback": True})
    assert result == {"fallback": True}


def test_safe_parse_json_custom_default_list() -> None:
    result = safe_parse_json("", default=[])
    assert result == []


def test_safe_parse_json_none_text() -> None:
    # Handles None-like empty values
    result = safe_parse_json("")
    assert result == {}


# ---------------------------------------------------------------------------
# Tests for parse_command_output
# ---------------------------------------------------------------------------


def test_parse_command_output_success() -> None:
    result = Mock(spec=subprocess.CompletedProcess)
    result.returncode = 0
    result.stdout = '{"key": "value"}'
    parsed = parse_command_output(result)
    assert parsed == {"key": "value"}


def test_parse_command_output_success_list() -> None:
    result = Mock(spec=subprocess.CompletedProcess)
    result.returncode = 0
    result.stdout = '[{"number": 1}, {"number": 2}]'
    parsed = parse_command_output(result)
    assert parsed == [{"number": 1}, {"number": 2}]


def test_parse_command_output_nonzero_exit() -> None:
    result = Mock(spec=subprocess.CompletedProcess)
    result.returncode = 1
    result.stdout = '{"key": "value"}'
    parsed = parse_command_output(result)
    assert parsed == {}


def test_parse_command_output_empty_stdout() -> None:
    result = Mock(spec=subprocess.CompletedProcess)
    result.returncode = 0
    result.stdout = ""
    parsed = parse_command_output(result)
    assert parsed == {}


def test_parse_command_output_invalid_json() -> None:
    result = Mock(spec=subprocess.CompletedProcess)
    result.returncode = 0
    result.stdout = "not valid json"
    parsed = parse_command_output(result)
    assert parsed == {}


def test_parse_command_output_custom_default() -> None:
    result = Mock(spec=subprocess.CompletedProcess)
    result.returncode = 1
    result.stdout = ""
    parsed = parse_command_output(result, default=[])
    assert parsed == []


def test_parse_command_output_whitespace_stdout() -> None:
    result = Mock(spec=subprocess.CompletedProcess)
    result.returncode = 0
    result.stdout = "   \n   "
    parsed = parse_command_output(result)
    assert parsed == {}


# ---------------------------------------------------------------------------
# Tests for read_json_file
# ---------------------------------------------------------------------------


def test_read_json_file_missing(tmp_path: pathlib.Path) -> None:
    result = read_json_file(tmp_path / "does-not-exist.json")
    assert result == {}


def test_read_json_file_empty(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "empty.json"
    p.write_text("")
    assert read_json_file(p) == {}


def test_read_json_file_whitespace(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "ws.json"
    p.write_text("   \n  ")
    assert read_json_file(p) == {}


def test_read_json_file_corrupt(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "bad.json"
    p.write_text("{invalid json")
    assert read_json_file(p) == {}


def test_read_json_file_valid_dict(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "data.json"
    p.write_text('{"key": "value"}')
    assert read_json_file(p) == {"key": "value"}


def test_read_json_file_valid_list(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "list.json"
    p.write_text('[1, 2, 3]')
    assert read_json_file(p) == [1, 2, 3]


def test_write_json_file_creates_parents(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "a" / "b" / "out.json"
    write_json_file(p, {"hello": "world"})
    assert p.exists()
    data = json.loads(p.read_text())
    assert data == {"hello": "world"}


def test_write_json_file_overwrites(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "out.json"
    write_json_file(p, {"v": 1})
    write_json_file(p, {"v": 2})
    data = json.loads(p.read_text())
    assert data == {"v": 2}


def test_write_json_file_trailing_newline(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "out.json"
    write_json_file(p, {"x": 1})
    assert p.read_text().endswith("\n")


def test_write_then_read_roundtrip(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "rt.json"
    original = {"key": [1, 2], "nested": {"a": True}}
    write_json_file(p, original)
    result = read_json_file(p)
    assert result == original


def test_read_json_file_custom_default_dict(tmp_path: pathlib.Path) -> None:
    result = read_json_file(tmp_path / "missing.json", default={"fallback": True})
    assert result == {"fallback": True}


def test_read_json_file_custom_default_list(tmp_path: pathlib.Path) -> None:
    result = read_json_file(tmp_path / "missing.json", default=[])
    assert result == []


def test_read_json_file_corrupt_with_custom_default(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "bad.json"
    p.write_text("{invalid json")
    result = read_json_file(p, default={"error": "fallback"})
    assert result == {"error": "fallback"}


# ---------------------------------------------------------------------------
# Tests for find_progress_for_issue
# ---------------------------------------------------------------------------


def test_find_progress_for_issue_no_progress_dir(tmp_path: pathlib.Path) -> None:
    """Returns None when progress directory doesn't exist."""
    result = find_progress_for_issue(tmp_path, 42)
    assert result is None


def test_find_progress_for_issue_no_matching_files(tmp_path: pathlib.Path) -> None:
    """Returns None when no progress files match the issue."""
    progress_dir = tmp_path / ".loom" / "progress"
    progress_dir.mkdir(parents=True)
    # Create a progress file for a different issue
    (progress_dir / "shepherd-abc123.json").write_text(
        json.dumps({
            "task_id": "abc123",
            "issue": 100,
            "started_at": "2026-01-01T10:00:00Z",
            "current_phase": "builder",
        })
    )
    result = find_progress_for_issue(tmp_path, 42)
    assert result is None


def test_find_progress_for_issue_finds_matching_file(tmp_path: pathlib.Path) -> None:
    """Returns progress when a matching file exists."""
    progress_dir = tmp_path / ".loom" / "progress"
    progress_dir.mkdir(parents=True)
    (progress_dir / "shepherd-def456.json").write_text(
        json.dumps({
            "task_id": "def456",
            "issue": 42,
            "started_at": "2026-01-01T10:00:00Z",
            "current_phase": "builder",
            "status": "working",
        })
    )
    result = find_progress_for_issue(tmp_path, 42)
    assert result is not None
    assert result.issue == 42
    assert result.task_id == "def456"
    assert result.current_phase == "builder"


def test_find_progress_for_issue_returns_most_recent(tmp_path: pathlib.Path) -> None:
    """Returns the most recent progress file when multiple exist for same issue."""
    progress_dir = tmp_path / ".loom" / "progress"
    progress_dir.mkdir(parents=True)
    # Older attempt
    (progress_dir / "shepherd-old.json").write_text(
        json.dumps({
            "task_id": "old",
            "issue": 42,
            "started_at": "2026-01-01T09:00:00Z",
            "current_phase": "curator",
        })
    )
    # Newer attempt
    (progress_dir / "shepherd-new.json").write_text(
        json.dumps({
            "task_id": "new",
            "issue": 42,
            "started_at": "2026-01-01T10:00:00Z",
            "current_phase": "builder",
        })
    )
    result = find_progress_for_issue(tmp_path, 42)
    assert result is not None
    assert result.task_id == "new"  # Most recent by started_at


def test_find_progress_for_issue_handles_corrupt_file(tmp_path: pathlib.Path) -> None:
    """Skips corrupt JSON files without crashing."""
    progress_dir = tmp_path / ".loom" / "progress"
    progress_dir.mkdir(parents=True)
    (progress_dir / "shepherd-corrupt.json").write_text("{invalid json")
    (progress_dir / "shepherd-valid.json").write_text(
        json.dumps({
            "task_id": "valid",
            "issue": 42,
            "started_at": "2026-01-01T10:00:00Z",
            "current_phase": "builder",
        })
    )
    result = find_progress_for_issue(tmp_path, 42)
    assert result is not None
    assert result.task_id == "valid"
