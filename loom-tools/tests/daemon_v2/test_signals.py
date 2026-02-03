"""Tests for daemon signal handling."""

import os
import pathlib
import tempfile

import pytest

from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.daemon_v2.signals import (
    check_stop_signal,
    check_session_conflict,
    check_existing_pid,
    write_pid_file,
    clear_stop_signal,
    cleanup_on_exit,
)


@pytest.fixture
def temp_repo():
    """Create a temporary repo directory."""
    with tempfile.TemporaryDirectory() as tmpdir:
        repo_root = pathlib.Path(tmpdir)
        loom_dir = repo_root / ".loom"
        loom_dir.mkdir(parents=True)
        yield repo_root


@pytest.fixture
def ctx(temp_repo):
    """Create a daemon context with temp repo."""
    config = DaemonConfig()
    return DaemonContext(config=config, repo_root=temp_repo)


class TestCheckStopSignal:
    """Tests for check_stop_signal."""

    def test_no_signal(self, ctx):
        """Test when stop signal does not exist."""
        assert check_stop_signal(ctx) is False

    def test_signal_exists(self, ctx):
        """Test when stop signal exists."""
        ctx.stop_signal.touch()
        assert check_stop_signal(ctx) is True


class TestCheckSessionConflict:
    """Tests for check_session_conflict."""

    def test_no_state_file(self, ctx):
        """Test when state file does not exist."""
        assert check_session_conflict(ctx) is False

    def test_state_file_same_session(self, ctx):
        """Test when state file has same session ID."""
        import json

        ctx.state_file.write_text(json.dumps({
            "daemon_session_id": ctx.session_id,
        }))
        assert check_session_conflict(ctx) is False

    def test_state_file_different_session(self, ctx):
        """Test when state file has different session ID."""
        import json

        ctx.state_file.write_text(json.dumps({
            "daemon_session_id": "other-session-id",
        }))
        assert check_session_conflict(ctx) is True

    def test_state_file_no_session_id(self, ctx):
        """Test when state file has no session ID."""
        import json

        ctx.state_file.write_text(json.dumps({
            "running": True,
        }))
        assert check_session_conflict(ctx) is False


class TestCheckExistingPid:
    """Tests for check_existing_pid."""

    def test_no_pid_file(self, ctx):
        """Test when PID file does not exist."""
        is_running, pid = check_existing_pid(ctx)
        assert is_running is False
        assert pid is None

    def test_stale_pid_file(self, ctx):
        """Test when PID file has dead process."""
        # Use a PID that's unlikely to exist
        ctx.pid_file.write_text("999999999")
        is_running, pid = check_existing_pid(ctx)
        assert is_running is False
        assert pid is None
        # PID file should be cleaned up
        assert not ctx.pid_file.exists()

    def test_invalid_pid_file(self, ctx):
        """Test when PID file has invalid content."""
        ctx.pid_file.write_text("not-a-number")
        is_running, pid = check_existing_pid(ctx)
        assert is_running is False
        assert pid is None
        # PID file should be cleaned up
        assert not ctx.pid_file.exists()

    def test_running_pid(self, ctx):
        """Test when PID file has current process."""
        # Use current process PID (guaranteed to be running)
        ctx.pid_file.write_text(str(os.getpid()))
        is_running, pid = check_existing_pid(ctx)
        assert is_running is True
        assert pid == os.getpid()


class TestWritePidFile:
    """Tests for write_pid_file."""

    def test_writes_current_pid(self, ctx):
        """Test that current PID is written."""
        write_pid_file(ctx)
        assert ctx.pid_file.exists()
        assert ctx.pid_file.read_text().strip() == str(os.getpid())


class TestClearStopSignal:
    """Tests for clear_stop_signal."""

    def test_clears_existing_signal(self, ctx):
        """Test clearing existing stop signal."""
        ctx.stop_signal.touch()
        assert ctx.stop_signal.exists()
        clear_stop_signal(ctx)
        assert not ctx.stop_signal.exists()

    def test_no_error_if_not_exists(self, ctx):
        """Test no error when signal doesn't exist."""
        clear_stop_signal(ctx)  # Should not raise


class TestCleanupOnExit:
    """Tests for cleanup_on_exit."""

    def test_cleans_up_files(self, ctx):
        """Test cleanup removes stop signal and PID file."""
        ctx.stop_signal.touch()
        ctx.pid_file.write_text(str(os.getpid()))

        cleanup_on_exit(ctx)

        assert not ctx.stop_signal.exists()
        assert not ctx.pid_file.exists()

    def test_no_error_if_files_missing(self, ctx):
        """Test no error when files don't exist."""
        cleanup_on_exit(ctx)  # Should not raise
