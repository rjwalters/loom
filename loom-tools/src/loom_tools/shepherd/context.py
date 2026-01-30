"""Shared context for shepherd orchestration."""

from __future__ import annotations

import json
import logging
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from loom_tools.common.repo import find_repo_root
from loom_tools.shepherd.config import ShepherdConfig
from loom_tools.shepherd.errors import (
    IssueBlockedError,
    IssueClosedError,
    IssueNotFoundError,
)
from loom_tools.shepherd.labels import LabelCache


@dataclass
class ShepherdContext:
    """Shared state across shepherd phases.

    Created at the start of orchestration and passed to each phase.
    Holds configuration, caches, and runtime state.
    """

    config: ShepherdConfig
    repo_root: Path = field(default_factory=find_repo_root)

    # Runtime state
    issue_title: str = ""
    pr_number: int | None = None
    worktree_path: Path | None = None
    completed_phases: list[str] = field(default_factory=list)

    # Caches
    label_cache: LabelCache = field(init=False)

    def __post_init__(self) -> None:
        self.label_cache = LabelCache(self.repo_root)
        # Set worktree path based on issue number
        self.worktree_path = (
            self.repo_root / ".loom" / "worktrees" / f"issue-{self.config.issue}"
        )
        self._cleanup_stale_progress_for_issue()

    def _cleanup_stale_progress_for_issue(self) -> None:
        """Remove stale progress files for this issue.

        When a new shepherd starts for an issue that already has a progress
        file (from a previous crashed/orphaned run), remove it to prevent
        stale data from interfering with the new run.
        """
        logger = logging.getLogger(__name__)
        progress_dir = self.repo_root / ".loom" / "progress"
        if not progress_dir.is_dir():
            return

        issue = self.config.issue
        for progress_file in progress_dir.glob("shepherd-*.json"):
            try:
                data = json.loads(progress_file.read_text())
            except (json.JSONDecodeError, OSError):
                continue

            if data.get("issue") != issue:
                continue

            # Don't remove our own progress file (matched by task_id)
            if data.get("task_id") == self.config.task_id:
                continue

            logger.info(
                "Removing stale progress file %s for issue #%s (task_id: %s)",
                progress_file.name,
                issue,
                data.get("task_id", "unknown"),
            )
            try:
                progress_file.unlink()
            except OSError:
                pass

    @property
    def scripts_dir(self) -> Path:
        """Path to .loom/scripts directory."""
        return self.repo_root / ".loom" / "scripts"

    @property
    def progress_dir(self) -> Path:
        """Path to .loom/progress directory."""
        return self.repo_root / ".loom" / "progress"

    def run_script(
        self,
        script_name: str,
        args: list[str],
        *,
        check: bool = True,
        capture: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        """Run a script from .loom/scripts.

        Args:
            script_name: Name of script (e.g., "worktree.sh")
            args: Arguments to pass to script
            check: Raise on non-zero exit code
            capture: Capture stdout/stderr

        Returns:
            CompletedProcess result
        """
        script_path = self.scripts_dir / script_name
        cmd = [str(script_path), *args]
        return subprocess.run(
            cmd,
            cwd=self.repo_root,
            text=True,
            capture_output=capture,
            check=check,
        )

    def validate_issue(self) -> dict[str, Any]:
        """Validate issue exists and is in valid state.

        Fetches issue metadata and pre-populates label cache.

        Returns:
            Issue metadata dict

        Raises:
            IssueNotFoundError: Issue doesn't exist
            IssueClosedError: Issue is already closed
            IssueBlockedError: Issue has loom:blocked label (unless force mode)
        """
        issue = self.config.issue

        # Fetch metadata
        cmd = ["gh", "issue", "view", str(issue), "--json", "url,state,title,labels"]
        result = subprocess.run(
            cmd,
            cwd=self.repo_root,
            text=True,
            capture_output=True,
            check=False,
        )

        if result.returncode != 0 or not result.stdout.strip():
            raise IssueNotFoundError(issue)

        try:
            meta = json.loads(result.stdout)
        except json.JSONDecodeError:
            raise IssueNotFoundError(issue)

        # Verify it's an issue, not a PR
        url = meta.get("url", "")
        if "/pull/" in url:
            raise IssueNotFoundError(issue)  # It's a PR

        # Check state
        state = meta.get("state", "").upper()
        if state != "OPEN":
            raise IssueClosedError(issue, state)

        # Check for stale remote branch
        self._check_stale_branch(issue)

        # Pre-populate label cache
        labels = {label["name"] for label in meta.get("labels", [])}
        self.label_cache.set_issue_labels(issue, labels)

        # Check for blocked label
        if "loom:blocked" in labels:
            if not self.config.is_force_mode:
                raise IssueBlockedError(issue)
            # In force mode, log warning but continue

        # Store title
        self.issue_title = meta.get("title", f"Issue #{issue}")

        return meta

    def _check_stale_branch(self, issue: int) -> None:
        """Check for existing remote branch and log a warning if found.

        A stale remote branch ``feature/issue-N`` may indicate a previous
        attempt that left artifacts behind.  We warn but always proceed
        so orchestration is not blocked.
        """
        logger = logging.getLogger(__name__)
        branch_name = f"feature/issue-{issue}"
        try:
            result = subprocess.run(
                ["git", "ls-remote", "--heads", "origin", branch_name],
                cwd=self.repo_root,
                text=True,
                capture_output=True,
                check=False,
            )
            if result.returncode == 0 and result.stdout.strip():
                logger.warning(
                    "Stale branch %s exists on remote. "
                    "Previous attempt may have left artifacts.",
                    branch_name,
                )
        except OSError:
            # git not available or other OS error — skip the check
            pass

    def has_issue_label(self, label: str) -> bool:
        """Check if the issue has a specific label."""
        return self.label_cache.has_issue_label(self.config.issue, label)

    def has_pr_label(self, label: str) -> bool:
        """Check if the PR has a specific label.

        Requires pr_number to be set.
        """
        if self.pr_number is None:
            return False
        return self.label_cache.has_pr_label(self.pr_number, label)

    def check_shutdown(self) -> bool:
        """Check for shutdown signals.

        Returns True if shutdown requested.
        """
        # Check for global shutdown signal
        stop_file = self.repo_root / ".loom" / "stop-shepherds"
        if stop_file.exists():
            return True

        # Check for issue-specific abort
        return self.has_issue_label("loom:abort")

    def report_milestone(
        self,
        event: str,
        *,
        quiet: bool = True,
        **kwargs: Any,
    ) -> bool:
        """Report a progress milestone.

        Args:
            event: Milestone event name
            quiet: Suppress output on success
            **kwargs: Event-specific arguments

        Returns:
            True if milestone was reported successfully
        """
        script = self.scripts_dir / "report-milestone.sh"
        if not script.is_file():
            return False

        args = [event, "--task-id", self.config.task_id]

        if quiet:
            args.append("--quiet")

        # Add event-specific arguments
        for key, value in kwargs.items():
            arg_name = f"--{key.replace('_', '-')}"
            if isinstance(value, bool):
                if value:
                    args.append(arg_name)
            else:
                args.extend([arg_name, str(value)])

        try:
            self.run_script("report-milestone.sh", args, check=False)
            return True
        except Exception:
            return False
