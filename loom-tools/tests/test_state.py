"""Tests for loom_tools.common.state."""

from __future__ import annotations

import json
import pathlib

from loom_tools.common.state import read_json_file, write_json_file


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
