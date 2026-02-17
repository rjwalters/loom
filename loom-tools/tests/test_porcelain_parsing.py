"""Tests for parse_porcelain_path in loom_tools.common.git."""

from __future__ import annotations

import pytest

from loom_tools.common.git import parse_porcelain_path


class TestParsePorcelainPath:
    """Unit tests for extracting file paths from git status --porcelain lines."""

    def test_modified_file(self) -> None:
        assert parse_porcelain_path(" M path/to/file") == "path/to/file"

    def test_added_file(self) -> None:
        assert parse_porcelain_path("A  path/to/file") == "path/to/file"

    def test_untracked_file(self) -> None:
        assert parse_porcelain_path("?? path/to/file") == "path/to/file"

    def test_quoted_path_with_spaces(self) -> None:
        assert parse_porcelain_path(' M "path with spaces/file"') == "path with spaces/file"

    def test_rename_format(self) -> None:
        # Callers split on " -> " further if needed
        assert parse_porcelain_path("R  old -> new") == "old -> new"

    def test_short_line(self) -> None:
        # "M " is not valid porcelain (2 chars < 3 min), falls through to strip
        assert parse_porcelain_path("M ") == "M"

    def test_extra_whitespace_after_status(self) -> None:
        """The off-by-one scenario from the original bug report."""
        assert parse_porcelain_path(" M  path/to/file") == "path/to/file"

    def test_deleted_file(self) -> None:
        assert parse_porcelain_path(" D path/to/deleted") == "path/to/deleted"

    def test_staged_and_modified(self) -> None:
        assert parse_porcelain_path("MM path/to/file") == "path/to/file"

    def test_empty_string(self) -> None:
        assert parse_porcelain_path("") == ""

    def test_status_only_no_path(self) -> None:
        # 3-char line "M  " → line[2:].lstrip() → ""
        assert parse_porcelain_path("M  ") == ""

    def test_deeply_nested_path(self) -> None:
        assert (
            parse_porcelain_path("?? a/b/c/d/e/file.txt") == "a/b/c/d/e/file.txt"
        )

    def test_dotfile(self) -> None:
        assert parse_porcelain_path("?? .loom/daemon-state.json") == ".loom/daemon-state.json"

    def test_quoted_path_with_special_chars(self) -> None:
        assert (
            parse_porcelain_path(' M "path/with\\"quotes/file"')
            == 'path/with\\"quotes/file'
        )
