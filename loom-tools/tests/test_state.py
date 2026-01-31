"""Tests for loom_tools.common.state."""

from __future__ import annotations

import json
import pathlib
import subprocess
from unittest.mock import Mock

from loom_tools.common.state import (
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
