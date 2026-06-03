"""Tests for ``loom_tools.cleanup`` (the post-daemon-brain log archival CLI).

Coverage focus matches the surviving slice from #3396:

* Config loading (env-driven).
* ``run_archive_logs`` -- delegation to ``archive-logs.sh`` with correct
  flag passthrough.
* ``handle_logs`` -- happy path and ``LOOM_ARCHIVE_LOGS=0`` skip.
* CLI parsing and exit codes via ``main()``.

Daemon event-driven cleanup (``shepherd-complete``, ``daemon-startup``,
``daemon-shutdown``, ``periodic``, ``prune-sessions``) is intentionally
**not** tested here -- those code paths no longer exist on the
``loom-cleanup`` surface.  The leftover implementations in
``daemon_cleanup.py`` are exercised by ``test_daemon_cleanup.py`` and
retire with the daemon brain in Phase 3.2.
"""

from __future__ import annotations

import os
import pathlib
import subprocess
from unittest import mock

import pytest

from loom_tools import cleanup as cleanup_mod
from loom_tools.cleanup import (
    CleanupConfig,
    _find_archive_logs_script,
    handle_logs,
    load_config,
    main,
    run_archive_logs,
)


# ---------------------------------------------------------------------------
# load_config
# ---------------------------------------------------------------------------


class TestLoadConfig:
    def test_defaults(self, monkeypatch: pytest.MonkeyPatch) -> None:
        for k in (
            "LOOM_CLEANUP_ENABLED",
            "LOOM_ARCHIVE_LOGS",
            "LOOM_RETENTION_DAYS",
        ):
            monkeypatch.delenv(k, raising=False)
        cfg = load_config()
        assert cfg.cleanup_enabled is True
        assert cfg.archive_logs is True
        assert cfg.retention_days == 7

    def test_overrides(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("LOOM_CLEANUP_ENABLED", "false")
        monkeypatch.setenv("LOOM_ARCHIVE_LOGS", "0")
        monkeypatch.setenv("LOOM_RETENTION_DAYS", "14")
        cfg = load_config()
        assert cfg.cleanup_enabled is False
        assert cfg.archive_logs is False
        assert cfg.retention_days == 14


# ---------------------------------------------------------------------------
# _find_archive_logs_script
# ---------------------------------------------------------------------------


class TestFindArchiveLogsScript:
    def test_prefers_scripts_dir(self, tmp_path: pathlib.Path) -> None:
        scripts = tmp_path / "scripts"
        scripts.mkdir()
        target = scripts / "archive-logs.sh"
        target.write_text("#!/bin/sh\nexit 0\n")
        target.chmod(0o755)

        # Also create a dot-loom variant; scripts/ should win.
        loom_scripts = tmp_path / ".loom" / "scripts"
        loom_scripts.mkdir(parents=True)
        alt = loom_scripts / "archive-logs.sh"
        alt.write_text("#!/bin/sh\nexit 0\n")
        alt.chmod(0o755)

        assert _find_archive_logs_script(tmp_path) == target

    def test_falls_back_to_dot_loom(self, tmp_path: pathlib.Path) -> None:
        loom_scripts = tmp_path / ".loom" / "scripts"
        loom_scripts.mkdir(parents=True)
        target = loom_scripts / "archive-logs.sh"
        target.write_text("#!/bin/sh\nexit 0\n")
        target.chmod(0o755)
        assert _find_archive_logs_script(tmp_path) == target

    def test_returns_none_when_missing(self, tmp_path: pathlib.Path) -> None:
        assert _find_archive_logs_script(tmp_path) is None

    def test_skips_non_executable(self, tmp_path: pathlib.Path) -> None:
        scripts = tmp_path / "scripts"
        scripts.mkdir()
        target = scripts / "archive-logs.sh"
        target.write_text("#!/bin/sh\nexit 0\n")
        target.chmod(0o644)  # not executable
        assert _find_archive_logs_script(tmp_path) is None


# ---------------------------------------------------------------------------
# run_archive_logs
# ---------------------------------------------------------------------------


class TestRunArchiveLogs:
    def _make_script(self, tmp_path: pathlib.Path) -> pathlib.Path:
        scripts = tmp_path / "scripts"
        scripts.mkdir()
        target = scripts / "archive-logs.sh"
        target.write_text("#!/bin/sh\nexit 0\n")
        target.chmod(0o755)
        return target

    def test_missing_script_returns_nonzero(self, tmp_path: pathlib.Path) -> None:
        assert run_archive_logs(tmp_path) == 1

    def test_passes_dry_run_flag(self, tmp_path: pathlib.Path) -> None:
        self._make_script(tmp_path)
        with mock.patch("loom_tools.cleanup.subprocess.run") as m:
            m.return_value = subprocess.CompletedProcess(
                args=[], returncode=0, stdout="", stderr=""
            )
            assert run_archive_logs(tmp_path, dry_run=True) == 0
        called_cmd = m.call_args.args[0]
        assert "--dry-run" in called_cmd

    def test_passes_prune_only_flag(self, tmp_path: pathlib.Path) -> None:
        self._make_script(tmp_path)
        with mock.patch("loom_tools.cleanup.subprocess.run") as m:
            m.return_value = subprocess.CompletedProcess(
                args=[], returncode=0, stdout="", stderr=""
            )
            run_archive_logs(tmp_path, prune_only=True)
        called_cmd = m.call_args.args[0]
        assert "--prune-only" in called_cmd

    def test_passes_retention_days(self, tmp_path: pathlib.Path) -> None:
        self._make_script(tmp_path)
        with mock.patch("loom_tools.cleanup.subprocess.run") as m:
            m.return_value = subprocess.CompletedProcess(
                args=[], returncode=0, stdout="", stderr=""
            )
            run_archive_logs(tmp_path, retention_days=14)
        called_cmd = m.call_args.args[0]
        assert "--retention-days" in called_cmd
        idx = called_cmd.index("--retention-days")
        assert called_cmd[idx + 1] == "14"

    def test_timeout_returns_nonzero(self, tmp_path: pathlib.Path) -> None:
        self._make_script(tmp_path)
        with mock.patch(
            "loom_tools.cleanup.subprocess.run",
            side_effect=subprocess.TimeoutExpired(cmd="x", timeout=60),
        ):
            assert run_archive_logs(tmp_path) == 1

    def test_subprocess_failure_returns_exit_code(
        self, tmp_path: pathlib.Path
    ) -> None:
        self._make_script(tmp_path)
        with mock.patch("loom_tools.cleanup.subprocess.run") as m:
            m.return_value = subprocess.CompletedProcess(
                args=[], returncode=2, stdout="", stderr="boom"
            )
            assert run_archive_logs(tmp_path) == 2


# ---------------------------------------------------------------------------
# handle_logs
# ---------------------------------------------------------------------------


class TestHandleLogs:
    def test_skips_when_archive_disabled_and_not_prune(
        self, tmp_path: pathlib.Path
    ) -> None:
        cfg = CleanupConfig(
            cleanup_enabled=True, archive_logs=False, retention_days=7
        )
        with mock.patch("loom_tools.cleanup.run_archive_logs") as m:
            rc = handle_logs(tmp_path, cfg)
        assert rc == 0
        m.assert_not_called()

    def test_runs_archival_when_enabled(self, tmp_path: pathlib.Path) -> None:
        cfg = CleanupConfig(
            cleanup_enabled=True, archive_logs=True, retention_days=7
        )
        with mock.patch("loom_tools.cleanup.run_archive_logs", return_value=0) as m:
            rc = handle_logs(tmp_path, cfg)
        assert rc == 0
        m.assert_called_once()
        call_kwargs = m.call_args.kwargs
        assert call_kwargs["retention_days"] == 7

    def test_prune_only_overrides_disabled_archive(
        self, tmp_path: pathlib.Path
    ) -> None:
        """With --prune-only, archival-disabled config should still prune."""
        cfg = CleanupConfig(
            cleanup_enabled=True, archive_logs=False, retention_days=7
        )
        with mock.patch("loom_tools.cleanup.run_archive_logs", return_value=0) as m:
            rc = handle_logs(tmp_path, cfg, prune_only=True)
        assert rc == 0
        m.assert_called_once()

    def test_retention_days_override(self, tmp_path: pathlib.Path) -> None:
        cfg = CleanupConfig(
            cleanup_enabled=True, archive_logs=True, retention_days=7
        )
        with mock.patch("loom_tools.cleanup.run_archive_logs", return_value=0) as m:
            handle_logs(tmp_path, cfg, retention_days=30)
        assert m.call_args.kwargs["retention_days"] == 30


# ---------------------------------------------------------------------------
# main (CLI)
# ---------------------------------------------------------------------------


class TestMain:
    def test_disabled_cleanup_returns_zero(
        self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv("LOOM_CLEANUP_ENABLED", "false")
        monkeypatch.setattr(
            "loom_tools.cleanup.find_repo_root", lambda: tmp_path
        )
        with mock.patch("loom_tools.cleanup.run_archive_logs") as m:
            assert main(["logs"]) == 0
        m.assert_not_called()

    def test_logs_invokes_handle_logs(
        self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        for k in (
            "LOOM_CLEANUP_ENABLED",
            "LOOM_ARCHIVE_LOGS",
            "LOOM_RETENTION_DAYS",
        ):
            monkeypatch.delenv(k, raising=False)
        monkeypatch.setattr(
            "loom_tools.cleanup.find_repo_root", lambda: tmp_path
        )
        with mock.patch("loom_tools.cleanup.run_archive_logs", return_value=0) as m:
            assert main(["logs"]) == 0
        m.assert_called_once()

    def test_missing_repo_root_returns_one(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        def _raise() -> pathlib.Path:
            raise FileNotFoundError("no repo")

        monkeypatch.delenv("LOOM_CLEANUP_ENABLED", raising=False)
        monkeypatch.setattr("loom_tools.cleanup.find_repo_root", _raise)
        assert main(["logs"]) == 1

    def test_help_succeeds(self, capsys: pytest.CaptureFixture[str]) -> None:
        with pytest.raises(SystemExit) as excinfo:
            main(["--help"])
        assert excinfo.value.code == 0
        captured = capsys.readouterr()
        assert "loom-cleanup" in captured.out

    def test_no_subcommand_fails(self, capsys: pytest.CaptureFixture[str]) -> None:
        """Argparse should reject invocation without a subcommand."""
        with pytest.raises(SystemExit):
            main([])

    def test_logs_dry_run_passthrough(
        self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        for k in (
            "LOOM_CLEANUP_ENABLED",
            "LOOM_ARCHIVE_LOGS",
            "LOOM_RETENTION_DAYS",
        ):
            monkeypatch.delenv(k, raising=False)
        monkeypatch.setattr(
            "loom_tools.cleanup.find_repo_root", lambda: tmp_path
        )
        with mock.patch("loom_tools.cleanup.run_archive_logs", return_value=0) as m:
            assert main(["logs", "--dry-run", "--retention-days", "14"]) == 0
        kwargs = m.call_args.kwargs
        assert kwargs["dry_run"] is True
        assert kwargs["retention_days"] == 14


# ---------------------------------------------------------------------------
# Removed-event regression guards (issue #3396)
# ---------------------------------------------------------------------------


class TestRemovedEventsAreNotExposed:
    """Guard against accidental reintroduction of daemon-event CLI surface."""

    @pytest.mark.parametrize(
        "removed_event",
        [
            "shepherd-complete",
            "daemon-startup",
            "daemon-shutdown",
            "periodic",
            "prune-sessions",
        ],
    )
    def test_removed_event_is_rejected(
        self,
        tmp_path: pathlib.Path,
        monkeypatch: pytest.MonkeyPatch,
        removed_event: str,
    ) -> None:
        monkeypatch.delenv("LOOM_CLEANUP_ENABLED", raising=False)
        monkeypatch.setattr(
            "loom_tools.cleanup.find_repo_root", lambda: tmp_path
        )
        # argparse should reject any removed event name as an unknown subcommand.
        with pytest.raises(SystemExit):
            main([removed_event])

    def test_module_does_not_export_event_handlers(self) -> None:
        """The trimmed module must not re-expose the daemon-event handlers."""
        for symbol in (
            "handle_shepherd_complete",
            "handle_daemon_startup",
            "handle_daemon_shutdown",
            "handle_periodic",
            "handle_prune_sessions",
        ):
            assert not hasattr(cleanup_mod, symbol), (
                f"loom_tools.cleanup should not expose {symbol}; "
                "session-rotation events were removed in #3396"
            )
