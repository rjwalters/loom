"""Shared context for shepherd orchestration."""

from __future__ import annotations

import logging
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from loom_tools.common.paths import LoomPaths, NamingConventions
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_json_file, safe_parse_json
from loom_tools.shepherd.config import ShepherdConfig
from loom_tools.shepherd.errors import (
    IssueBlockedError,
    IssueClosedError,
    IssueNotFoundError,
)
from loom_tools.shepherd.labels import LabelCache, remove_issue_label


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

    # Progress tracking state
    _progress_initialized: bool = field(default=False, init=False, repr=False)

    def __post_init__(self) -> None:
        self.label_cache = LabelCache(self.repo_root)
        # Set worktree path based on issue number
        self._paths = LoomPaths(self.repo_root)
        self.worktree_path = self._paths.worktree_path(self.config.issue)
        self._cleanup_stale_progress_for_issue()

    def _cleanup_stale_progress_for_issue(self) -> None:
        """Remove stale progress files for this issue.

        When a new shepherd starts for an issue that already has a progress
        file (from a previous crashed/orphaned run), remove it to prevent
        stale data from interfering with the new run.
        """
        logger = logging.getLogger(__name__)
        if not self._paths.progress_dir.is_dir():
            return

        issue = self.config.issue
        for progress_file in self._paths.progress_dir.glob("shepherd-*.json"):
            data = read_json_file(progress_file)
            if not isinstance(data, dict):
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
        return self._paths.scripts_dir

    @property
    def progress_dir(self) -> Path:
        """Path to .loom/progress directory."""
        return self._paths.progress_dir

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

        Raises:
            FileNotFoundError: If the script does not exist (e.g. the
                working tree is on a branch that predates Loom installation).
        """
        script_path = self.scripts_dir / script_name
        if not script_path.is_file():
            raise FileNotFoundError(
                f"Script not found: {script_path} — "
                "the branch may predate Loom installation"
            )
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

        meta = safe_parse_json(result.stdout)
        if not isinstance(meta, dict):
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
            # In force mode (--merge), remove loom:blocked and continue
            logger = logging.getLogger(__name__)
            logger.warning(
                "Issue #%d has loom:blocked label - removing due to merge mode override",
                issue,
            )
            remove_issue_label(issue, "loom:blocked", self.repo_root)
            labels.discard("loom:blocked")
            self.label_cache.set_issue_labels(issue, labels)

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
        branch_name = NamingConventions.branch_name(issue)
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
        if self._paths.stop_shepherds_file.exists():
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
        from loom_tools.milestones import report_milestone as _report

        logger = logging.getLogger(__name__)

        # If progress file was never initialized, skip silently to avoid
        # repeated "No progress file found" errors on every milestone call.
        # The initial warning is logged when the "started" event fails.
        if event != "started" and not self._progress_initialized:
            return False

        try:
            result = _report(
                self.repo_root, self.config.task_id, event, quiet=quiet, **kwargs
            )

            # Track whether the progress file was successfully initialized
            if event == "started":
                if result:
                    self._progress_initialized = True
                else:
                    # Log a warning that will help diagnose why subsequent
                    # milestones are not being recorded
                    logger.warning(
                        "Failed to initialize progress file for task %s "
                        "(started milestone returned False). "
                        "Subsequent milestones will be skipped.",
                        self.config.task_id,
                    )

            return result
        except Exception as exc:
            # Log the exception for the "started" event since it's critical
            if event == "started":
                logger.warning(
                    "Failed to initialize progress file for task %s: %s. "
                    "Subsequent milestones will be skipped.",
                    self.config.task_id,
                    exc,
                )
            return False
