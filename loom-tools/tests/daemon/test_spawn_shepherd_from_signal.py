"""Tests for _spawn_shepherd_from_signal in loom_tools.daemon_v2.loop."""

from __future__ import annotations

import pathlib
from unittest import mock

import pytest

from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.daemon_v2.loop import _spawn_shepherd_from_signal
from loom_tools.models.daemon_state import DaemonState, ShepherdEntry


def _make_ctx(tmp_path: pathlib.Path) -> DaemonContext:
    """Create a minimal DaemonContext with one idle shepherd slot."""
    # Create the shepherd script so the existence check passes
    scripts_dir = tmp_path / ".loom" / "scripts"
    scripts_dir.mkdir(parents=True)
    shepherd_script = scripts_dir / "loom-shepherd.sh"
    shepherd_script.write_text("#!/bin/sh\n")
    shepherd_script.chmod(0o755)

    state = DaemonState()
    state.shepherds["shepherd-1"] = ShepherdEntry(status="idle")

    ctx = DaemonContext(
        config=DaemonConfig(),
        repo_root=tmp_path,
        state=state,
    )
    return ctx


class TestSpawnShepherdFromSignalTaskId:
    """Verify that --task-id is passed to loom-shepherd.sh."""

    def test_task_id_passed_to_subprocess(self, tmp_path: pathlib.Path) -> None:
        """The subprocess args must include --task-id <id> matching daemon-state."""
        ctx = _make_ctx(tmp_path)

        with mock.patch(
            "loom_tools.daemon_v2.loop.gh_issue_view",
            return_value={"state": "OPEN"},
        ), mock.patch("subprocess.Popen") as mock_popen:
            mock_proc = mock.MagicMock()
            mock_proc.pid = 12345
            mock_popen.return_value = mock_proc

            _spawn_shepherd_from_signal(ctx, issue=42, mode="default", flags=[])

        assert mock_popen.called, "subprocess.Popen should have been called"
        call_args = mock_popen.call_args[0][0]  # First positional arg: the command list
        assert "--task-id" in call_args, f"--task-id not in args: {call_args}"
        task_id_index = call_args.index("--task-id")
        task_id_value = call_args[task_id_index + 1]

        # The task_id stored in daemon state must match the one passed to the script
        entry = ctx.state.shepherds["shepherd-1"]
        assert entry.task_id == task_id_value, (
            f"daemon-state task_id={entry.task_id!r} does not match "
            f"subprocess --task-id={task_id_value!r}"
        )

    def test_task_id_passed_in_force_mode(self, tmp_path: pathlib.Path) -> None:
        """--task-id must appear alongside --merge when mode=force."""
        ctx = _make_ctx(tmp_path)

        with mock.patch(
            "loom_tools.daemon_v2.loop.gh_issue_view",
            return_value={"state": "OPEN"},
        ), mock.patch("subprocess.Popen") as mock_popen:
            mock_proc = mock.MagicMock()
            mock_proc.pid = 12345
            mock_popen.return_value = mock_proc

            _spawn_shepherd_from_signal(ctx, issue=42, mode="force", flags=[])

        call_args = mock_popen.call_args[0][0]
        assert "--merge" in call_args, f"--merge not in args: {call_args}"
        assert "--task-id" in call_args, f"--task-id not in args: {call_args}"
        task_id_index = call_args.index("--task-id")
        task_id_value = call_args[task_id_index + 1]

        entry = ctx.state.shepherds["shepherd-1"]
        assert entry.task_id == task_id_value

    def test_issue_number_is_first_arg(self, tmp_path: pathlib.Path) -> None:
        """Issue number must be the first argument after the script path."""
        ctx = _make_ctx(tmp_path)

        with mock.patch(
            "loom_tools.daemon_v2.loop.gh_issue_view",
            return_value={"state": "OPEN"},
        ), mock.patch("subprocess.Popen") as mock_popen:
            mock_proc = mock.MagicMock()
            mock_proc.pid = 12345
            mock_popen.return_value = mock_proc

            _spawn_shepherd_from_signal(ctx, issue=99, mode="default", flags=[])

        call_args = mock_popen.call_args[0][0]
        # call_args[0] is the script path, call_args[1] is the issue number
        assert call_args[1] == "99", f"Expected issue '99' at index 1, got {call_args}"
