"""Tests for daemon exit codes."""

import pytest

from loom_tools.daemon_v2.exit_codes import DaemonExitCode


class TestDaemonExitCode:
    """Tests for DaemonExitCode enum."""

    def test_success_is_zero(self):
        """Test SUCCESS exit code is 0."""
        assert DaemonExitCode.SUCCESS == 0

    def test_startup_failed_is_one(self):
        """Test STARTUP_FAILED exit code is 1."""
        assert DaemonExitCode.STARTUP_FAILED == 1

    def test_session_conflict_is_two(self):
        """Test SESSION_CONFLICT exit code is 2."""
        assert DaemonExitCode.SESSION_CONFLICT == 2

    def test_signal_shutdown_is_three(self):
        """Test SIGNAL_SHUTDOWN exit code is 3."""
        assert DaemonExitCode.SIGNAL_SHUTDOWN == 3

    def test_error_is_four(self):
        """Test ERROR exit code is 4."""
        assert DaemonExitCode.ERROR == 4

    def test_can_use_as_int(self):
        """Test exit codes can be used as integers."""
        # Should work with sys.exit()
        code = DaemonExitCode.SUCCESS
        assert isinstance(int(code), int)
        assert int(code) == 0
