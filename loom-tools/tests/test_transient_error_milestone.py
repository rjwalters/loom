"""Tests for transient_error milestone event in milestones.py."""

from __future__ import annotations

import json
import pathlib

import pytest

from loom_tools.milestones import (
    VALID_EVENTS,
    _build_milestone_data,
    report_milestone,
)


@pytest.fixture
def repo_root(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a mock repo with progress directory."""
    (tmp_path / ".git").mkdir()
    (tmp_path / ".loom").mkdir()
    (tmp_path / ".loom" / "progress").mkdir()
    return tmp_path


class TestTransientErrorEvent:
    """Tests for the transient_error milestone event type."""

    def test_transient_error_in_valid_events(self) -> None:
        assert "transient_error" in VALID_EVENTS

    def test_build_milestone_data(self) -> None:
        data = _build_milestone_data(
            "transient_error",
            error="500 Internal Server Error",
            pattern="500",
        )
        assert data["error"] == "500 Internal Server Error"
        assert data["pattern"] == "500"

    def test_build_milestone_data_without_pattern(self) -> None:
        data = _build_milestone_data(
            "transient_error",
            error="Rate limit exceeded",
        )
        assert data["error"] == "Rate limit exceeded"
        assert data["pattern"] == ""

    def test_report_transient_error_requires_started_first(
        self, repo_root: pathlib.Path
    ) -> None:
        # Reporting transient_error without a started event should fail
        ok = report_milestone(
            repo_root,
            "abc1234",
            "transient_error",
            quiet=True,
            error="500 Internal Server Error",
        )
        assert ok is False

    def test_report_transient_error_sets_errored_status(
        self, repo_root: pathlib.Path
    ) -> None:
        # First create the progress file with started
        ok = report_milestone(
            repo_root,
            "abc1234",
            "started",
            quiet=True,
            issue=42,
            mode="default",
        )
        assert ok is True

        # Report transient error
        ok = report_milestone(
            repo_root,
            "abc1234",
            "transient_error",
            quiet=True,
            error="500 Internal Server Error",
            pattern="500",
        )
        assert ok is True

        # Read progress file and verify status
        progress_file = repo_root / ".loom" / "progress" / "shepherd-abc1234.json"
        data = json.loads(progress_file.read_text())
        assert data["status"] == "errored"

    def test_report_transient_error_adds_milestone(
        self, repo_root: pathlib.Path
    ) -> None:
        report_milestone(
            repo_root, "abc1234", "started", quiet=True, issue=42, mode="default"
        )
        report_milestone(
            repo_root,
            "abc1234",
            "transient_error",
            quiet=True,
            error="Rate limit exceeded",
            pattern="rate_limit",
        )

        progress_file = repo_root / ".loom" / "progress" / "shepherd-abc1234.json"
        data = json.loads(progress_file.read_text())

        milestones = data["milestones"]
        transient_ms = [m for m in milestones if m["event"] == "transient_error"]
        assert len(transient_ms) == 1
        assert transient_ms[0]["data"]["error"] == "Rate limit exceeded"
        assert transient_ms[0]["data"]["pattern"] == "rate_limit"

    def test_transient_error_requires_error_kwarg(
        self, repo_root: pathlib.Path
    ) -> None:
        report_milestone(
            repo_root, "abc1234", "started", quiet=True, issue=42, mode="default"
        )

        # Missing --error should fail
        ok = report_milestone(
            repo_root,
            "abc1234",
            "transient_error",
            quiet=True,
        )
        assert ok is False
