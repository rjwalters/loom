"""Centralized path constants and naming conventions for .loom directory structure.

This module provides a single source of truth for:
- Directory paths within .loom/
- File paths for state files (daemon-state.json, health-metrics.json, etc.)
- Naming conventions for branches and worktrees
"""

from __future__ import annotations

from pathlib import Path


class LoomPaths:
    """Centralized path constants for .loom directory structure.

    Usage:
        paths = LoomPaths(repo_root)
        print(paths.daemon_state_file)  # repo_root/.loom/daemon-state.json
        print(paths.worktree_path(42))  # repo_root/.loom/worktrees/issue-42
    """

    # Directory names (relative to .loom/)
    LOOM_DIR = ".loom"
    SCRIPTS_DIR = "scripts"
    PROGRESS_DIR = "progress"
    WORKTREES_DIR = "worktrees"
    LOGS_DIR = "logs"
    DOCS_DIR = "docs"
    ROLES_DIR = "roles"
    DIAGNOSTICS_DIR = "diagnostics"
    METRICS_DIR = "metrics"

    # State file names
    DAEMON_STATE_FILE = "daemon-state.json"
    HEALTH_METRICS_FILE = "health-metrics.json"
    ALERTS_FILE = "alerts.json"
    STUCK_HISTORY_FILE = "stuck-history.json"
    CONFIG_FILE = "config.json"
    STOP_DAEMON_FILE = "stop-daemon"
    STOP_SHEPHERDS_FILE = "stop-shepherds"
    RECOVERY_EVENTS_FILE = "recovery-events.json"
    BASELINE_HEALTH_FILE = "baseline-health.json"

    def __init__(self, repo_root: Path) -> None:
        """Initialize with repository root path.

        Args:
            repo_root: The root path of the repository.
        """
        self.repo_root = repo_root

    @property
    def loom_dir(self) -> Path:
        """Path to .loom directory."""
        return self.repo_root / self.LOOM_DIR

    @property
    def scripts_dir(self) -> Path:
        """Path to .loom/scripts directory."""
        return self.loom_dir / self.SCRIPTS_DIR

    @property
    def progress_dir(self) -> Path:
        """Path to .loom/progress directory."""
        return self.loom_dir / self.PROGRESS_DIR

    @property
    def worktrees_dir(self) -> Path:
        """Path to .loom/worktrees directory."""
        return self.loom_dir / self.WORKTREES_DIR

    @property
    def logs_dir(self) -> Path:
        """Path to .loom/logs directory."""
        return self.loom_dir / self.LOGS_DIR

    @property
    def docs_dir(self) -> Path:
        """Path to .loom/docs directory."""
        return self.loom_dir / self.DOCS_DIR

    @property
    def roles_dir(self) -> Path:
        """Path to .loom/roles directory."""
        return self.loom_dir / self.ROLES_DIR

    @property
    def diagnostics_dir(self) -> Path:
        """Path to .loom/diagnostics directory."""
        return self.loom_dir / self.DIAGNOSTICS_DIR

    @property
    def metrics_dir(self) -> Path:
        """Path to .loom/metrics directory."""
        return self.loom_dir / self.METRICS_DIR

    @property
    def daemon_state_file(self) -> Path:
        """Path to .loom/daemon-state.json."""
        return self.loom_dir / self.DAEMON_STATE_FILE

    @property
    def health_metrics_file(self) -> Path:
        """Path to .loom/health-metrics.json."""
        return self.loom_dir / self.HEALTH_METRICS_FILE

    @property
    def alerts_file(self) -> Path:
        """Path to .loom/alerts.json."""
        return self.loom_dir / self.ALERTS_FILE

    @property
    def stuck_history_file(self) -> Path:
        """Path to .loom/stuck-history.json."""
        return self.loom_dir / self.STUCK_HISTORY_FILE

    @property
    def config_file(self) -> Path:
        """Path to .loom/config.json."""
        return self.loom_dir / self.CONFIG_FILE

    @property
    def stop_daemon_file(self) -> Path:
        """Path to .loom/stop-daemon (shutdown signal)."""
        return self.loom_dir / self.STOP_DAEMON_FILE

    @property
    def stop_shepherds_file(self) -> Path:
        """Path to .loom/stop-shepherds (shepherd shutdown signal)."""
        return self.loom_dir / self.STOP_SHEPHERDS_FILE

    @property
    def baseline_health_file(self) -> Path:
        """Path to .loom/baseline-health.json."""
        return self.loom_dir / self.BASELINE_HEALTH_FILE

    @property
    def recovery_events_file(self) -> Path:
        """Path to .loom/metrics/recovery-events.json."""
        return self.metrics_dir / self.RECOVERY_EVENTS_FILE

    def worktree_path(self, issue: int) -> Path:
        """Path to worktree for a specific issue.

        Args:
            issue: The issue number.

        Returns:
            Path to .loom/worktrees/issue-{N}
        """
        return self.worktrees_dir / NamingConventions.worktree_name(issue)

    def progress_file(self, task_id: str) -> Path:
        """Path to progress file for a specific shepherd task.

        Args:
            task_id: The 7-character hex task ID.

        Returns:
            Path to .loom/progress/shepherd-{task_id}.json
        """
        return self.progress_dir / f"shepherd-{task_id}.json"

    def builder_log_file(self, issue: int) -> Path:
        """Path to builder log file for a specific issue.

        Args:
            issue: The issue number.

        Returns:
            Path to .loom/logs/loom-builder-issue-{N}.log
        """
        return self.logs_dir / f"loom-builder-issue-{issue}.log"

    def worker_log_file(self, role: str, issue: int) -> Path:
        """Path to worker log file for a specific role and issue.

        Args:
            role: The worker role (e.g., "judge", "builder").
            issue: The issue number.

        Returns:
            Path to .loom/logs/loom-{role}-issue-{N}.log
        """
        return self.logs_dir / f"loom-{role}-issue-{issue}.log"


class NamingConventions:
    """Naming conventions for branches, worktrees, and other identifiers.

    All methods are static - no instance needed.
    """

    BRANCH_PREFIX = "feature/issue-"
    WORKTREE_PREFIX = "issue-"

    @staticmethod
    def branch_name(issue: int) -> str:
        """Generate branch name for an issue.

        Args:
            issue: The issue number.

        Returns:
            Branch name in format "feature/issue-{N}"
        """
        return f"{NamingConventions.BRANCH_PREFIX}{issue}"

    @staticmethod
    def worktree_name(issue: int) -> str:
        """Generate worktree directory name for an issue.

        Args:
            issue: The issue number.

        Returns:
            Worktree directory name in format "issue-{N}"
        """
        return f"{NamingConventions.WORKTREE_PREFIX}{issue}"

    @staticmethod
    def issue_from_branch(branch: str) -> int | None:
        """Extract issue number from branch name.

        Args:
            branch: Branch name (e.g., "feature/issue-42")

        Returns:
            Issue number if branch matches pattern, None otherwise.
        """
        if branch.startswith(NamingConventions.BRANCH_PREFIX):
            try:
                return int(branch[len(NamingConventions.BRANCH_PREFIX) :])
            except ValueError:
                pass
        return None

    @staticmethod
    def issue_from_worktree(worktree_name: str) -> int | None:
        """Extract issue number from worktree directory name.

        Args:
            worktree_name: Worktree directory name (e.g., "issue-42")

        Returns:
            Issue number if name matches pattern, None otherwise.
        """
        if worktree_name.startswith(NamingConventions.WORKTREE_PREFIX):
            try:
                return int(worktree_name[len(NamingConventions.WORKTREE_PREFIX) :])
            except ValueError:
                pass
        return None
