"""Tests for degraded session detection (issue #2631).

Tests the detection of builder sessions running under rate limits that enter
a non-productive Crystallizing loop.  Detection requires BOTH:
1. A rate limit warning ("You've used X% of your weekly limit")
2. Excessive "Crystallizing..." repetitions (>= threshold)
"""

from __future__ import annotations

import subprocess
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.shepherd.config import ShepherdConfig
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.phases.base import (
    DEGRADED_CRYSTALLIZING_THRESHOLD,
    DEGRADED_SCAN_TAIL_LINES,
    _is_degraded_session,
    _scan_log_for_degradation,
)
from loom_tools.shepherd.phases.builder import BuilderPhase, PhaseStatus


# ---------------------------------------------------------------------------
# Fixtures & helpers
# ---------------------------------------------------------------------------


@pytest.fixture
def tmp_log(tmp_path: Path) -> Path:
    """Return a path for a temporary log file."""
    return tmp_path / "loom-builder-issue-42.log"


def _write_log(log_path: Path, content: str) -> None:
    """Write log content with the CLI start sentinel."""
    log_path.write_text(f"# CLAUDE_CLI_START\n{content}")


def _make_degraded_log(
    *,
    rate_limit_pct: int = 87,
    crystallizing_count: int = DEGRADED_CRYSTALLIZING_THRESHOLD,
    extra_lines: str = "",
) -> str:
    """Build a realistic degraded session log body."""
    lines = [
        f"You've used {rate_limit_pct}% of your weekly limit · resets Feb 20 at 11am",
    ]
    if extra_lines:
        lines.append(extra_lines)
    lines.extend(["Crystallizing…"] * crystallizing_count)
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# _is_degraded_session tests
# ---------------------------------------------------------------------------


class TestIsDegradedSession:
    """Test _is_degraded_session() post-session detection."""

    def test_degraded_with_both_signals(self, tmp_log: Path) -> None:
        """Rate limit + Crystallizing above threshold → degraded."""
        _write_log(tmp_log, _make_degraded_log())
        assert _is_degraded_session(tmp_log) is True

    def test_not_degraded_without_rate_limit(self, tmp_log: Path) -> None:
        """Crystallizing alone without rate limit warning → not degraded."""
        content = "\n".join(
            ["Crystallizing…"] * (DEGRADED_CRYSTALLIZING_THRESHOLD + 5)
        )
        _write_log(tmp_log, content)
        assert _is_degraded_session(tmp_log) is False

    def test_not_degraded_with_few_crystallizing(self, tmp_log: Path) -> None:
        """Rate limit warning + few Crystallizing → not degraded."""
        content = _make_degraded_log(
            crystallizing_count=DEGRADED_CRYSTALLIZING_THRESHOLD - 1
        )
        _write_log(tmp_log, content)
        assert _is_degraded_session(tmp_log) is False

    def test_not_degraded_on_normal_session(self, tmp_log: Path) -> None:
        """Normal productive session → not degraded."""
        _write_log(
            tmp_log,
            "Read tool output...\nEdit tool applied...\nTests passed\n",
        )
        assert _is_degraded_session(tmp_log) is False

    def test_not_degraded_on_missing_file(self, tmp_log: Path) -> None:
        """Missing log file → not degraded."""
        assert _is_degraded_session(tmp_log) is False

    def test_not_degraded_on_empty_file(self, tmp_log: Path) -> None:
        """Empty log file → not degraded."""
        tmp_log.write_text("")
        assert _is_degraded_session(tmp_log) is False

    def test_not_degraded_without_sentinel(self, tmp_log: Path) -> None:
        """Log without CLI start sentinel → not degraded (CLI never started)."""
        tmp_log.write_text(
            _make_degraded_log(crystallizing_count=20)
        )
        assert _is_degraded_session(tmp_log) is False

    def test_exact_threshold_is_degraded(self, tmp_log: Path) -> None:
        """Exactly at Crystallizing threshold → degraded."""
        content = _make_degraded_log(
            crystallizing_count=DEGRADED_CRYSTALLIZING_THRESHOLD
        )
        _write_log(tmp_log, content)
        assert _is_degraded_session(tmp_log) is True

    def test_rate_limit_with_different_percentages(self, tmp_log: Path) -> None:
        """Different rate limit percentages should all trigger detection."""
        for pct in (50, 75, 87, 99):
            _write_log(tmp_log, _make_degraded_log(rate_limit_pct=pct))
            assert _is_degraded_session(tmp_log) is True, f"Failed for {pct}%"


# ---------------------------------------------------------------------------
# _scan_log_for_degradation tests (in-flight polling variant)
# ---------------------------------------------------------------------------


class TestScanLogForDegradation:
    """Test _scan_log_for_degradation() for in-flight log scanning."""

    def test_detects_degradation(self, tmp_log: Path) -> None:
        """Detects degradation from log tail during polling."""
        _write_log(tmp_log, _make_degraded_log(crystallizing_count=10))
        assert _scan_log_for_degradation(tmp_log) is True

    def test_no_degradation_in_normal_log(self, tmp_log: Path) -> None:
        """Normal log does not trigger degradation."""
        _write_log(tmp_log, "Working on feature...\nEdit applied.\nTests pass.\n")
        assert _scan_log_for_degradation(tmp_log) is False

    def test_no_degradation_without_rate_limit(self, tmp_log: Path) -> None:
        """Crystallizing without rate limit in tail → not degraded."""
        content = "\n".join(["Crystallizing…"] * 20)
        _write_log(tmp_log, content)
        assert _scan_log_for_degradation(tmp_log) is False

    def test_handles_missing_file(self, tmp_log: Path) -> None:
        """Missing file returns False without error."""
        assert _scan_log_for_degradation(tmp_log) is False


# ---------------------------------------------------------------------------
# Builder phase exit code 11 handling
# ---------------------------------------------------------------------------


class TestBuilderDegradedSessionHandling:
    """Test BuilderPhase handling of exit code 11 (degraded session)."""

    @pytest.fixture
    def mock_context(self) -> MagicMock:
        ctx = MagicMock(spec=ShepherdContext)
        ctx.config = ShepherdConfig(issue=42)
        ctx.repo_root = Path("/fake/repo")
        ctx.scripts_dir = Path("/fake/repo/.loom/scripts")
        ctx.worktree_path = MagicMock()
        ctx.worktree_path.is_dir.return_value = True
        ctx.worktree_path.name = "issue-42"
        ctx.worktree_path.__str__ = lambda self: "/fake/repo/.loom/worktrees/issue-42"
        ctx.pr_number = None
        ctx.label_cache = MagicMock()
        ctx.check_shutdown.return_value = False
        return ctx

    def test_exit_code_11_returns_failed_with_degraded_flag(
        self, mock_context: MagicMock
    ) -> None:
        """Exit code 11 should produce a FAILED result with degraded_session=True."""
        builder = BuilderPhase()

        with patch(
            "loom_tools.shepherd.phases.builder.run_phase_with_retry",
            return_value=11,
        ), patch(
            "loom_tools.shepherd.phases.builder.transition_issue_labels",
        ), patch(
            "loom_tools.shepherd.phases.builder.get_pr_for_issue",
            return_value=None,
        ), patch(
            "loom_tools.shepherd.phases.builder.validate_issue_quality_with_gates",
            return_value=None,
        ), patch.object(
            builder, "_is_rate_limited", return_value=False
        ), patch.object(
            builder, "_cleanup_stale_worktree",
        ), patch.object(
            builder, "_snapshot_main_dirty", return_value=set()
        ), patch.object(
            builder, "_run_quality_validation", return_value=None
        ), patch.object(
            builder, "_run_reproducibility_check", return_value=None
        ), patch.object(
            builder, "_get_log_path",
            return_value=Path("/fake/logs/builder-42.log"),
        ):
            result = builder.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert result.data.get("degraded_session") is True
        assert result.data.get("exit_code") == 11
        assert "rate limit" in result.message.lower()
        assert "crystallizing" in result.message.lower()
