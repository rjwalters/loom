"""Tests for agent_monitor module."""

from __future__ import annotations

import json
import pathlib
from unittest import mock

import pytest

from loom_tools.agent_monitor import (
    CONTRACT_INITIAL_DELAY,
    AgentMonitor,
    ProgressTracker,
    capture_pane,
    get_adaptive_contract_interval,
    kill_session,
    send_keys,
    session_exists,
)
from loom_tools.models.agent_wait import (
    CompletionReason,
    MonitorConfig,
    SignalType,
    StuckAction,
    StuckConfig,
    WaitStatus,
)


class TestProgressTracker:
    def test_initial_state(self) -> None:
        tracker = ProgressTracker(name="test-agent")
        assert tracker.name == "test-agent"
        assert tracker.last_hash == ""
        assert tracker.last_progress_time > 0

    def test_get_idle_time(self) -> None:
        tracker = ProgressTracker(name="test-agent")
        # Initial idle time should be very small (just created)
        assert tracker.get_idle_time() < 2

    def test_check_progress_no_content(self) -> None:
        tracker = ProgressTracker(name="test-agent")
        with mock.patch("loom_tools.agent_monitor.capture_pane", return_value=""):
            result = tracker.check_progress("loom-test-agent")
        assert result is False

    def test_check_progress_content_changed(self) -> None:
        tracker = ProgressTracker(name="test-agent")
        tracker.last_hash = "old_hash"

        with mock.patch(
            "loom_tools.agent_monitor.capture_pane", return_value="new content"
        ):
            result = tracker.check_progress("loom-test-agent")
        assert result is True
        assert tracker.last_hash != "old_hash"

    def test_check_progress_content_unchanged(self) -> None:
        tracker = ProgressTracker(name="test-agent")
        content = "unchanged content"
        import hashlib

        expected_hash = hashlib.md5(content.encode()).hexdigest()
        tracker.last_hash = expected_hash

        with mock.patch("loom_tools.agent_monitor.capture_pane", return_value=content):
            result = tracker.check_progress("loom-test-agent")
        assert result is False


class TestTmuxHelpers:
    def test_capture_pane_subprocess_error(self) -> None:
        with mock.patch("subprocess.run", side_effect=Exception("tmux not found")):
            result = capture_pane("test-session")
        assert result == ""

    def test_capture_pane_success(self) -> None:
        mock_result = mock.Mock()
        mock_result.returncode = 0
        mock_result.stdout = "pane content"
        with mock.patch("subprocess.run", return_value=mock_result):
            result = capture_pane("test-session")
        assert result == "pane content"

    def test_send_keys_success(self) -> None:
        with mock.patch("subprocess.run") as mock_run:
            result = send_keys("test-session", "hello")
        assert result is True
        mock_run.assert_called_once()

    def test_send_keys_failure(self) -> None:
        with mock.patch("subprocess.run", side_effect=Exception("failed")):
            result = send_keys("test-session", "hello")
        assert result is False

    def test_kill_session_success(self) -> None:
        with mock.patch("subprocess.run") as mock_run:
            result = kill_session("test-session")
        assert result is True
        mock_run.assert_called_once()

    def test_session_exists_true(self) -> None:
        mock_result = mock.Mock()
        mock_result.returncode = 0
        with mock.patch("subprocess.run", return_value=mock_result):
            result = session_exists("test-session")
        assert result is True

    def test_session_exists_false(self) -> None:
        mock_result = mock.Mock()
        mock_result.returncode = 1
        with mock.patch("subprocess.run", return_value=mock_result):
            result = session_exists("test-session")
        assert result is False


class TestAgentMonitor:
    @pytest.fixture
    def temp_repo(self, tmp_path: pathlib.Path) -> pathlib.Path:
        """Create a temporary repo structure."""
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "logs").mkdir()
        (loom_dir / "scripts").mkdir()
        (loom_dir / "progress").mkdir()

        # Create .git directory to make it look like a repo
        (tmp_path / ".git").mkdir()

        return tmp_path

    @pytest.fixture
    def basic_config(self) -> MonitorConfig:
        return MonitorConfig(
            name="builder-issue-42",
            timeout=10,
            poll_interval=1,
            issue=42,
            stuck_config=StuckConfig(
                warning_threshold=5,
                critical_threshold=10,
            ),
        )

    def test_elapsed(
        self, basic_config: MonitorConfig, temp_repo: pathlib.Path
    ) -> None:
        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(basic_config)
            assert monitor.elapsed >= 0

    def test_session_name(
        self, basic_config: MonitorConfig, temp_repo: pathlib.Path
    ) -> None:
        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(basic_config)
            assert monitor.session_name == "loom-builder-issue-42"

    def test_extract_phase_from_config(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="test", phase="curator")
        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            assert monitor._extract_phase() == "curator"

    def test_extract_phase_from_name(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="builder-issue-42")
        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            assert monitor._extract_phase() == "builder"

    def test_extract_role_command_builder(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="builder-issue-42")
        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            assert monitor._extract_role_command() == "/builder 42"

    def test_extract_role_command_other(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="judge-123")
        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            assert monitor._extract_role_command() == ""

    def test_check_errored_status_no_task_id(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="test")
        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            assert monitor._check_errored_status() is False

    def test_check_errored_status_file_exists_errored(
        self, temp_repo: pathlib.Path
    ) -> None:
        config = MonitorConfig(name="test", task_id="abc123")
        progress_file = temp_repo / ".loom" / "progress" / "shepherd-abc123.json"
        progress_file.write_text(json.dumps({"status": "errored"}))

        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            assert monitor._check_errored_status() is True

    def test_check_errored_status_file_exists_working(
        self, temp_repo: pathlib.Path
    ) -> None:
        config = MonitorConfig(name="test", task_id="abc123")
        progress_file = temp_repo / ".loom" / "progress" / "shepherd-abc123.json"
        progress_file.write_text(json.dumps({"status": "working"}))

        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            assert monitor._check_errored_status() is False


class TestCompletionPatterns:
    @pytest.fixture
    def temp_repo(self, tmp_path: pathlib.Path) -> pathlib.Path:
        """Create a temporary repo structure with log file."""
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "logs").mkdir()
        (loom_dir / "scripts").mkdir()
        (tmp_path / ".git").mkdir()
        return tmp_path

    def test_explicit_exit_pattern(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="builder-issue-42")
        log_file = temp_repo / ".loom" / "logs" / "loom-builder-issue-42.log"
        log_file.write_text("some output\n❯ /exit\nmore output")

        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            reason = monitor._check_completion_patterns()

        assert reason == CompletionReason.EXPLICIT_EXIT

    def test_builder_pr_created_pattern(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="builder-issue-42", phase="builder")
        log_file = temp_repo / ".loom" / "logs" / "loom-builder-issue-42.log"
        log_file.write_text("PR created: https://github.com/owner/repo/pull/123\n")

        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            reason = monitor._check_completion_patterns()

        assert reason == CompletionReason.BUILDER_PR_CREATED

    def test_judge_review_complete_pattern(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="judge-pr-100", phase="judge")
        log_file = temp_repo / ".loom" / "logs" / "loom-judge-pr-100.log"
        log_file.write_text('gh pr edit 100 --add-label "loom:pr"\n')

        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            reason = monitor._check_completion_patterns()

        assert reason == CompletionReason.JUDGE_REVIEW_COMPLETE

    def test_curator_complete_pattern(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="curator-issue-50", phase="curator")
        log_file = temp_repo / ".loom" / "logs" / "loom-curator-issue-50.log"
        log_file.write_text('gh issue edit 50 --add-label "loom:curated"\n')

        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            reason = monitor._check_completion_patterns()

        assert reason == CompletionReason.CURATOR_CURATION_COMPLETE

    def test_no_completion_pattern(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="builder-issue-42", phase="builder")
        log_file = temp_repo / ".loom" / "logs" / "loom-builder-issue-42.log"
        log_file.write_text("Still working on implementation...\n")

        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            reason = monitor._check_completion_patterns()

        assert reason is None

    def test_no_log_file(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="builder-issue-42")

        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            reason = monitor._check_completion_patterns()

        assert reason is None


class TestStuckDetection:
    @pytest.fixture
    def temp_repo(self, tmp_path: pathlib.Path) -> pathlib.Path:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "logs").mkdir()
        (loom_dir / "scripts").mkdir()
        (loom_dir / "signals").mkdir()
        (loom_dir / "diagnostics").mkdir()
        (tmp_path / ".git").mkdir()
        return tmp_path

    def test_check_stuck_status_not_stuck(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(
            name="test-agent",
            stuck_config=StuckConfig(warning_threshold=300, critical_threshold=600),
        )
        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            # Progress tracker just initialized, so idle time is ~0
            result = monitor._check_stuck_status()

        assert result is None

    def test_handle_stuck_warn_action(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(
            name="test-agent",
            stuck_config=StuckConfig(action=StuckAction.WARN),
        )
        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            result = monitor._handle_stuck("CRITICAL", 600)

        # WARN action should not return a result (continue waiting)
        assert result is None

    def test_handle_stuck_pause_action(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(
            name="test-agent",
            stuck_config=StuckConfig(action=StuckAction.PAUSE),
        )
        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            result = monitor._handle_stuck("CRITICAL", 600)

        assert result is not None
        assert result.status == WaitStatus.STUCK
        assert result.stuck_action == "paused"

        # Check signal file was created
        signal_file = temp_repo / ".loom" / "signals" / "pause-test-agent"
        assert signal_file.exists()

    def test_handle_stuck_restart_action(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(
            name="test-agent",
            stuck_config=StuckConfig(action=StuckAction.RESTART),
        )
        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            with mock.patch("loom_tools.agent_monitor.kill_session") as mock_kill:
                with mock.patch(
                    "loom_tools.agent_monitor.capture_pane", return_value="test content"
                ):
                    monitor = AgentMonitor(config)
                    result = monitor._handle_stuck("CRITICAL", 600)

        assert result is not None
        assert result.status == WaitStatus.STUCK
        assert result.stuck_action == "restart"
        mock_kill.assert_called_once()


class TestSignalChecking:
    @pytest.fixture
    def temp_repo(self, tmp_path: pathlib.Path) -> pathlib.Path:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "logs").mkdir()
        (loom_dir / "scripts").mkdir()
        (tmp_path / ".git").mkdir()
        return tmp_path

    @pytest.mark.asyncio
    async def test_check_signals_none(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="test-agent")
        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            signal = await monitor._check_signals()

        assert signal is None

    @pytest.mark.asyncio
    async def test_check_signals_shutdown(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="test-agent")
        stop_file = temp_repo / ".loom" / "stop-shepherds"
        stop_file.touch()

        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            monitor = AgentMonitor(config)
            signal = await monitor._check_signals()

        assert signal == SignalType.SHUTDOWN

    @pytest.mark.asyncio
    async def test_check_signals_abort(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="test-agent", issue=42)

        mock_result = mock.Mock()
        mock_result.stdout = "loom:abort\nloom:building\n"

        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            with mock.patch("subprocess.run", return_value=mock_result):
                monitor = AgentMonitor(config)
                signal = await monitor._check_signals()

        assert signal == SignalType.ABORT


class TestPromptResolution:
    @pytest.fixture
    def temp_repo(self, tmp_path: pathlib.Path) -> pathlib.Path:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "logs").mkdir()
        (tmp_path / ".git").mkdir()
        return tmp_path

    def test_check_and_resolve_prompts_no_prompt(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="test-agent")
        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            with mock.patch(
                "loom_tools.agent_monitor.capture_pane", return_value="normal output"
            ):
                monitor = AgentMonitor(config)
                result = monitor._check_and_resolve_prompts()

        assert result is False

    def test_check_and_resolve_prompts_plan_mode(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="test-agent")
        pane_content = """
Would you like to proceed?
1. Yes, clear context and bypass permissions
2. Yes, and bypass permissions
        """
        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            with mock.patch(
                "loom_tools.agent_monitor.capture_pane", return_value=pane_content
            ):
                with mock.patch("loom_tools.agent_monitor.send_keys") as mock_send:
                    monitor = AgentMonitor(config)
                    result = monitor._check_and_resolve_prompts()

        assert result is True
        # Should send "1" and then Enter
        assert mock_send.call_count == 2


class TestStuckAtPrompt:
    @pytest.fixture
    def temp_repo(self, tmp_path: pathlib.Path) -> pathlib.Path:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "logs").mkdir()
        (tmp_path / ".git").mkdir()
        return tmp_path

    def test_is_stuck_at_prompt_not_stuck(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="builder-issue-42")
        pane_content = "Working on implementation...\nesc to interrupt"

        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            with mock.patch(
                "loom_tools.agent_monitor.capture_pane", return_value=pane_content
            ):
                monitor = AgentMonitor(config)
                result = monitor._is_stuck_at_prompt()

        assert result is False

    def test_is_stuck_at_prompt_command_processing(
        self, temp_repo: pathlib.Path
    ) -> None:
        config = MonitorConfig(name="builder-issue-42")
        pane_content = "❯ /builder 42\nesc to interrupt"

        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            with mock.patch(
                "loom_tools.agent_monitor.capture_pane", return_value=pane_content
            ):
                monitor = AgentMonitor(config)
                result = monitor._is_stuck_at_prompt()

        # Not stuck because processing indicators are present
        assert result is False

    def test_is_stuck_at_prompt_stuck(self, temp_repo: pathlib.Path) -> None:
        config = MonitorConfig(name="builder-issue-42")
        pane_content = "❯ /builder 42\nWaiting..."

        with mock.patch(
            "loom_tools.agent_monitor.find_repo_root", return_value=temp_repo
        ):
            with mock.patch(
                "loom_tools.agent_monitor.capture_pane", return_value=pane_content
            ):
                monitor = AgentMonitor(config)
                result = monitor._is_stuck_at_prompt()

        # Stuck: command visible but no processing indicators
        assert result is True


class TestAdaptiveContractIntervals:
    """Tests verifying adaptive contract check intervals match bash behavior.

    These tests validate that agent_monitor.py's adaptive interval logic matches
    the behavior of agent-wait-bg.sh (see issue #1678, #1691).

    Bash schedule:
      0-180s:   No contract checks (return 0)
      180-270s: 90s interval
      270-330s: 60s interval
      330-360s: 30s interval
      360s+:    10s interval
    """

    def test_initial_delay_period_no_checks(self) -> None:
        """During initial 180s delay, no contract checks should occur."""
        # 0-179s should all return 0 (skip check)
        assert get_adaptive_contract_interval(0) == 0
        assert get_adaptive_contract_interval(60) == 0
        assert get_adaptive_contract_interval(120) == 0
        assert get_adaptive_contract_interval(179) == 0

    def test_early_work_phase_90s_interval(self) -> None:
        """From 180-269s, use 90s interval."""
        assert get_adaptive_contract_interval(180) == 90
        assert get_adaptive_contract_interval(200) == 90
        assert get_adaptive_contract_interval(269) == 90

    def test_mid_work_phase_60s_interval(self) -> None:
        """From 270-329s, use 60s interval."""
        assert get_adaptive_contract_interval(270) == 60
        assert get_adaptive_contract_interval(300) == 60
        assert get_adaptive_contract_interval(329) == 60

    def test_nearing_completion_30s_interval(self) -> None:
        """From 330-359s, use 30s interval."""
        assert get_adaptive_contract_interval(330) == 30
        assert get_adaptive_contract_interval(345) == 30
        assert get_adaptive_contract_interval(359) == 30

    def test_final_detection_10s_interval(self) -> None:
        """From 360s onward, use 10s interval for rapid detection."""
        assert get_adaptive_contract_interval(360) == 10
        assert get_adaptive_contract_interval(600) == 10
        assert get_adaptive_contract_interval(3600) == 10

    def test_override_returns_fixed_value(self) -> None:
        """When override is set, always return that value."""
        assert get_adaptive_contract_interval(0, override=45) == 45
        assert get_adaptive_contract_interval(180, override=45) == 45
        assert get_adaptive_contract_interval(360, override=45) == 45

    def test_override_zero_uses_adaptive(self) -> None:
        """Override of 0 should use adaptive schedule."""
        assert get_adaptive_contract_interval(180, override=0) == 90
        assert get_adaptive_contract_interval(360, override=0) == 10

    def test_initial_delay_constant(self) -> None:
        """Verify CONTRACT_INITIAL_DELAY matches bash script."""
        assert CONTRACT_INITIAL_DELAY == 180


class TestBehaviorParity:
    """Tests verifying Python implementation matches bash behavior.

    These tests document and verify parity between agent_monitor.py and
    agent-wait-bg.sh for critical behaviors.
    """

    def test_exit_codes_match_bash(self) -> None:
        """Verify exit code mapping matches bash script."""
        # From agent-wait-bg.sh:
        #   0 - Agent completed
        #   1 - Timeout reached
        #   2 - Session not found
        #   3 - Shutdown signal detected
        #   4 - Agent stuck and intervention triggered

        # Verify these are the actual exit codes from the module
        from loom_tools.agent_monitor import main
        import inspect

        source = inspect.getsource(main)
        assert "WaitStatus.COMPLETED: 0" in source
        assert "WaitStatus.TIMEOUT: 1" in source
        assert "WaitStatus.SESSION_NOT_FOUND: 2" in source
        assert "WaitStatus.SIGNAL: 3" in source
        assert "WaitStatus.STUCK: 4" in source
        assert "WaitStatus.ERRORED: 4" in source

    def test_default_timeout_matches_bash(self) -> None:
        """Default timeout should be 3600s (1 hour)."""
        from loom_tools.models.agent_wait import MonitorConfig

        config = MonitorConfig(name="test")
        assert config.timeout == 3600

    def test_default_poll_interval_matches_bash(self) -> None:
        """Default poll interval should be 5s."""
        from loom_tools.models.agent_wait import MonitorConfig

        config = MonitorConfig(name="test")
        assert config.poll_interval == 5

    def test_default_idle_timeout_matches_bash(self) -> None:
        """Default idle timeout should be 60s."""
        from loom_tools.models.agent_wait import MonitorConfig

        config = MonitorConfig(name="test")
        assert config.idle_timeout == 60

    def test_default_heartbeat_interval_matches_bash(self) -> None:
        """Default heartbeat interval should be 60s."""
        from loom_tools.models.agent_wait import MonitorConfig

        config = MonitorConfig(name="test")
        assert config.heartbeat_interval == 60

    def test_default_stuck_thresholds_match_bash(self) -> None:
        """Default stuck detection thresholds should match bash."""
        from loom_tools.models.agent_wait import StuckConfig

        config = StuckConfig()
        # Bash defaults: STUCK_WARNING_THRESHOLD=300, STUCK_CRITICAL_THRESHOLD=600
        assert config.warning_threshold == 300
        assert config.critical_threshold == 600

    def test_default_prompt_stuck_threshold_matches_bash(self) -> None:
        """Prompt stuck threshold should be 30s."""
        from loom_tools.models.agent_wait import StuckConfig

        config = StuckConfig()
        assert config.prompt_stuck_threshold == 30

    def test_processing_indicator_matches_bash(self) -> None:
        """Processing indicator pattern should match bash."""
        from loom_tools.agent_monitor import PROCESSING_INDICATORS

        # Bash: PROCESSING_INDICATORS='esc to interrupt'
        assert PROCESSING_INDICATORS == "esc to interrupt"

    def test_tmux_socket_matches_bash(self) -> None:
        """tmux socket name should be 'loom'."""
        from loom_tools.agent_monitor import TMUX_SOCKET

        # Bash: TMUX_SOCKET="loom"
        assert TMUX_SOCKET == "loom"

    def test_session_prefix_matches_bash(self) -> None:
        """Session prefix should be 'loom-'."""
        from loom_tools.agent_monitor import SESSION_PREFIX

        # Bash: SESSION_PREFIX="loom-"
        assert SESSION_PREFIX == "loom-"

    def test_progress_dir_matches_bash(self) -> None:
        """Progress tracking directory should match bash."""
        from loom_tools.agent_monitor import PROGRESS_DIR

        # Bash: PROGRESS_DIR="/tmp/loom-agent-progress"
        assert str(PROGRESS_DIR) == "/tmp/loom-agent-progress"
