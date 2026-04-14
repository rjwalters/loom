"""Tests for issue dependency parsing and filtering."""

from __future__ import annotations

import pathlib
from unittest import mock

import pytest

from loom_tools.daemon_v2.actions.dependencies import (
    filter_issues_by_dependencies,
    parse_dependencies,
)


class TestParseDependencies:
    """Tests for parse_dependencies()."""

    def test_no_body(self):
        assert parse_dependencies(None) == set()
        assert parse_dependencies("") == set()

    def test_no_dependencies_section(self):
        body = "## Summary\nThis is a feature request.\n## Details\nMore info."
        assert parse_dependencies(body) == set()

    def test_single_dependency(self):
        body = "## Summary\nSome text.\n## Dependencies\n- #10\n## Details\nMore."
        assert parse_dependencies(body) == {10}

    def test_multiple_dependencies_separate_lines(self):
        body = (
            "## Summary\nSome text.\n"
            "## Dependencies\n"
            "- #10\n"
            "- #13\n"
            "- #14\n"
            "## Details\nMore."
        )
        assert parse_dependencies(body) == {10, 13, 14}

    def test_multiple_dependencies_same_line(self):
        body = "## Dependencies\n- #10, #13, #14\n"
        assert parse_dependencies(body) == {10, 13, 14}

    def test_dependencies_with_descriptions(self):
        body = (
            "## Dependencies\n"
            "- #10 (bootstrap/scaffold)\n"
            "- #13 document management must be done first\n"
        )
        assert parse_dependencies(body) == {10, 13}

    def test_prose_format(self):
        body = "## Dependencies\nDepends on #10 and #13.\n"
        assert parse_dependencies(body) == {10, 13}

    def test_dependencies_at_end_of_body(self):
        body = "## Summary\nText.\n## Dependencies\n- #42\n- #99"
        assert parse_dependencies(body) == {42, 99}

    def test_case_insensitive_heading(self):
        body = "## dependencies\n- #5\n"
        assert parse_dependencies(body) == {5}

    def test_extra_whitespace_in_heading(self):
        body = "##  Dependencies  \n- #7\n"
        assert parse_dependencies(body) == {7}

    def test_does_not_pick_up_refs_outside_section(self):
        body = (
            "## Summary\nRelated to #99.\n"
            "## Dependencies\n- #10\n"
            "## Notes\nSee also #50."
        )
        assert parse_dependencies(body) == {10}


class TestFilterIssuesByDependencies:
    """Tests for filter_issues_by_dependencies()."""

    def _make_issue(self, num: int, body: str = "") -> dict:
        return {"number": num, "title": f"Issue #{num}", "body": body}

    def test_no_dependencies(self):
        issues = [self._make_issue(1), self._make_issue(2)]
        result = filter_issues_by_dependencies(issues, {1, 2, 3})
        assert len(result) == 2

    def test_all_deps_closed(self):
        """Dependencies not in open set => issue is schedulable."""
        issues = [
            self._make_issue(5, "## Dependencies\n- #1\n- #2\n"),
        ]
        # open issues are 5, 6, 7 -- deps #1, #2 are closed
        result = filter_issues_by_dependencies(issues, {5, 6, 7})
        assert len(result) == 1
        assert result[0]["number"] == 5

    def test_unmet_dependency_filtered(self):
        """Issue with open dependency should be filtered out."""
        issues = [
            self._make_issue(5, "## Dependencies\n- #3\n"),
        ]
        # #3 is still open
        result = filter_issues_by_dependencies(issues, {3, 5})
        assert len(result) == 0

    def test_partial_deps_met(self):
        """Issue with one met and one unmet dep should be filtered."""
        issues = [
            self._make_issue(5, "## Dependencies\n- #1\n- #3\n"),
        ]
        # #1 is closed, #3 is open
        result = filter_issues_by_dependencies(issues, {3, 5})
        assert len(result) == 0

    def test_mixed_issues(self):
        """Mix of issues with and without dependencies."""
        issues = [
            self._make_issue(1),  # no deps
            self._make_issue(2, "## Dependencies\n- #50\n"),  # dep on closed #50
            self._make_issue(3, "## Dependencies\n- #10\n"),  # dep on open #10
            self._make_issue(4, "## Dependencies\n- #10\n- #20\n"),  # deps on open #10
        ]
        # #50 is NOT in open set (closed), #10 IS open
        open_issues = {1, 2, 3, 4, 10}
        result = filter_issues_by_dependencies(issues, open_issues)
        assert [r["number"] for r in result] == [1, 2]

    def test_preserves_order(self):
        issues = [
            self._make_issue(10),
            self._make_issue(5),
            self._make_issue(1),
        ]
        result = filter_issues_by_dependencies(issues, {1, 5, 10})
        assert [r["number"] for r in result] == [10, 5, 1]

    def test_self_reference_not_treated_as_dependency(self):
        """An issue referencing itself in deps shouldn't block itself.

        This tests the real behavior: the issue IS in the open set, so a
        self-reference would block it.  Authors should avoid self-references.
        But practically, parse_dependencies only extracts from the deps
        section, and self-references there are a user error.
        """
        issues = [
            self._make_issue(5, "## Dependencies\n- #5\n"),
        ]
        # #5 is itself open, so it IS in the open set => blocked
        result = filter_issues_by_dependencies(issues, {5})
        assert len(result) == 0

    def test_empty_issues_list(self):
        result = filter_issues_by_dependencies([], {1, 2, 3})
        assert result == []

    def test_empty_open_set(self):
        """No open issues => all deps are met."""
        issues = [
            self._make_issue(1, "## Dependencies\n- #99\n"),
        ]
        result = filter_issues_by_dependencies(issues, set())
        assert len(result) == 1


class TestCollectOpenIssueNumbers:
    """Tests for _collect_open_issue_numbers()."""

    def test_collects_from_all_pipeline_keys(self):
        from loom_tools.daemon_v2.actions.shepherds import _collect_open_issue_numbers
        from loom_tools.daemon_v2.config import DaemonConfig
        from loom_tools.daemon_v2.context import DaemonContext

        ctx = DaemonContext(config=DaemonConfig(), repo_root=pathlib.Path("/tmp"))
        ctx.snapshot = {
            "pipeline": {
                "ready_issues": [{"number": 1}, {"number": 2}],
                "building_issues": [{"number": 3}],
                "blocked_issues": [{"number": 4}],
            },
        }

        result = _collect_open_issue_numbers(ctx)
        assert result == {1, 2, 3, 4}

    def test_empty_snapshot(self):
        from loom_tools.daemon_v2.actions.shepherds import _collect_open_issue_numbers
        from loom_tools.daemon_v2.config import DaemonConfig
        from loom_tools.daemon_v2.context import DaemonContext

        ctx = DaemonContext(config=DaemonConfig(), repo_root=pathlib.Path("/tmp"))
        ctx.snapshot = None

        result = _collect_open_issue_numbers(ctx)
        assert result == set()


class TestSpawnShepherdsWithDependencies:
    """Integration tests for spawn_shepherds with dependency filtering."""

    def test_skips_issues_with_unmet_deps(self):
        """spawn_shepherds should not schedule issues with open dependencies."""
        from loom_tools.daemon_v2.actions.shepherds import spawn_shepherds
        from loom_tools.daemon_v2.config import DaemonConfig
        from loom_tools.daemon_v2.context import DaemonContext
        from loom_tools.models.daemon_state import DaemonState, ShepherdEntry

        ctx = DaemonContext(config=DaemonConfig(), repo_root=pathlib.Path("/tmp"))
        ctx.state = DaemonState()
        ctx.state.shepherds["shepherd-1"] = ShepherdEntry(status="idle")

        ctx.snapshot = {
            "computed": {
                "available_shepherd_slots": 3,
            },
            "pipeline": {
                "ready_issues": [
                    {"number": 10, "title": "Bootstrap", "body": ""},
                    {"number": 13, "title": "Doc mgmt", "body": "## Dependencies\n- #10\n"},
                    {"number": 14, "title": "Editor", "body": "## Dependencies\n- #13\n"},
                ],
                "building_issues": [],
                "blocked_issues": [],
            },
        }

        # All three are in ready state (open). #13 depends on #10, #14 depends on #13.
        # Only #10 (no deps) should be schedulable.
        with (
            mock.patch(
                "loom_tools.daemon_v2.actions.shepherds._spawn_single_shepherd",
                return_value=True,
            ) as mock_spawn,
        ):
            result = spawn_shepherds(ctx)

        assert result == 1
        mock_spawn.assert_called_once()
        # The issue number passed should be 10 (the one with no deps)
        assert mock_spawn.call_args[0][2] == 10
