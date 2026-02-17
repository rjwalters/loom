"""Shared context for shepherd orchestration."""

from __future__ import annotations

import logging
import os
import subprocess
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from loom_tools.common.github import gh_issue_view, gh_list
from loom_tools.common.paths import LoomPaths, NamingConventions
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_json_file
from loom_tools.common.time_utils import elapsed_seconds
from loom_tools.shepherd.config import ShepherdConfig
from loom_tools.shepherd.errors import (
    IssueBlockedError,
    IssueClosedError,
    IssueIsEpicError,
    IssueNotFoundError,
)
from loom_tools.shepherd.labels import LabelCache, remove_issue_label

# Heartbeats fresher than this (in seconds) indicate a live shepherd.
# agent-wait-bg.sh sends heartbeats every 60s, so 300s gives ~5 missed
# heartbeats before we consider the shepherd dead.
_HEARTBEAT_FRESH_THRESHOLD = int(
    os.environ.get("LOOM_HEARTBEAT_STALE_THRESHOLD", "300")
)


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
    warnings: list[str] = field(default_factory=list)

    # Caches
    label_cache: LabelCache = field(init=False)

    # Low-output cause classification (set by run_phase_with_retry)
    last_low_output_cause: str | None = field(default=None, init=False)

    # Preflight baseline status (set by orchestrator after preflight phase).
    # "healthy" means baseline tests pass and builder can skip re-running them.
    preflight_baseline_status: str | None = field(default=None, init=False)

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

        Files with a fresh heartbeat (< ``_HEARTBEAT_FRESH_THRESHOLD``
        seconds old) are left intact because they likely belong to a
        shepherd that is still actively running.
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

            # Don't remove files with fresh heartbeats — another shepherd
            # is likely still running.
            last_hb = data.get("last_heartbeat", "")
            if last_hb:
                try:
                    age = elapsed_seconds(last_hb)
                    if age < _HEARTBEAT_FRESH_THRESHOLD:
                        logger.info(
                            "Skipping progress file %s for issue #%s — "
                            "heartbeat is fresh (%ds old, threshold %ds)",
                            progress_file.name,
                            issue,
                            age,
                            _HEARTBEAT_FRESH_THRESHOLD,
                        )
                        continue
                except (ValueError, OverflowError):
                    # Unparseable timestamp — treat as stale
                    pass

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

        If the script is not found on the current branch (e.g. after
        ``gh pr checkout`` switches to a PR branch that predates the
        script), it is extracted from the ``main`` branch via
        ``git show`` and executed from a temporary file.

        Args:
            script_name: Name of script (e.g., "worktree.sh")
            args: Arguments to pass to script
            check: Raise on non-zero exit code
            capture: Capture stdout/stderr

        Returns:
            CompletedProcess result

        Raises:
            FileNotFoundError: If the script does not exist on the
                current branch *and* cannot be extracted from main.
        """
        script_path = self.scripts_dir / script_name
        if script_path.is_file():
            cmd = [str(script_path), *args]
            return subprocess.run(
                cmd,
                cwd=self.repo_root,
                text=True,
                capture_output=capture,
                check=check,
                stdin=subprocess.DEVNULL,
            )

        # Fallback: extract from main branch (handles PR branch checkouts)
        logger = logging.getLogger(__name__)
        git_path = f".loom/scripts/{script_name}"
        logger.warning(
            "Script not found at %s — attempting to extract from main branch",
            script_path,
        )
        return self._run_script_from_main(
            git_path, script_name, args, check=check, capture=capture
        )

    def _run_script_from_main(
        self,
        git_path: str,
        script_name: str,
        args: list[str],
        *,
        check: bool = True,
        capture: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        """Extract a script from the main branch and execute it.

        Uses ``git show main:<path>`` to read the script content, writes
        it to a temporary file, and runs it.

        Raises:
            FileNotFoundError: If the script cannot be extracted from main.
        """
        result = subprocess.run(
            ["git", "show", f"main:{git_path}"],
            cwd=self.repo_root,
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            raise FileNotFoundError(
                f"Script not found: .loom/scripts/{script_name} — "
                "not on current branch and could not extract from main"
            )

        # Write to a temp file and execute
        fd, tmp_path = tempfile.mkstemp(suffix=f"-{script_name}", prefix="loom-")
        try:
            os.write(fd, result.stdout.encode())
            os.close(fd)
            os.chmod(tmp_path, 0o755)

            logger = logging.getLogger(__name__)
            logger.info(
                "Running %s extracted from main branch (temp: %s)",
                script_name,
                tmp_path,
            )
            cmd = [tmp_path, *args]
            return subprocess.run(
                cmd,
                cwd=self.repo_root,
                text=True,
                capture_output=capture,
                check=check,
                stdin=subprocess.DEVNULL,
            )
        finally:
            try:
                os.unlink(tmp_path)
            except OSError:
                pass

    def validate_issue(self) -> dict[str, Any]:
        """Validate issue exists and is in valid state.

        Fetches issue metadata and pre-populates label cache.
        Uses the dual-mode GitHub API layer (GraphQL with REST fallback).

        Returns:
            Issue metadata dict

        Raises:
            IssueNotFoundError: Issue doesn't exist
            IssueClosedError: Issue is already closed
            IssueBlockedError: Issue has loom:blocked label (unless force mode)
        """
        issue = self.config.issue

        # Fetch metadata via dual-mode API layer
        meta = gh_issue_view(
            issue,
            fields=["url", "state", "title", "labels"],
            cwd=self.repo_root,
        )

        if meta is None:
            raise IssueNotFoundError(issue)

        # Verify it's an issue, not a PR
        url = meta.get("url", "")
        if "/pull/" in url:
            raise IssueNotFoundError(issue)  # It's a PR

        # Check state
        state = meta.get("state", "").upper()
        if state != "OPEN":
            raise IssueClosedError(issue, state)

        # Check for already-merged PR (issue is OPEN but work is done)
        self._check_merged_pr(issue)

        # Check for stale remote branch
        self._check_stale_branch(issue)

        # Pre-populate label cache
        labels = {label["name"] for label in meta.get("labels", [])}
        self.label_cache.set_issue_labels(issue, labels)

        # Check for epic labels — epics cannot be implemented directly.
        # loom:epic-phase issues are individual work items and are allowed through.
        if "loom:epic-phase" not in labels:
            if "loom:epic" in labels or "epic" in labels:
                raise IssueIsEpicError(issue)

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

    def _check_merged_pr(self, issue: int) -> None:
        """Check if a PR for this issue has already been merged.

        When an issue is still OPEN but its PR has already been merged
        (e.g., due to label re-application or missed auto-close), the
        shepherd should exit early rather than running the builder phase
        unnecessarily.

        Raises:
            IssueClosedError: If a merged PR is found for this issue's branch.
        """
        branch_name = NamingConventions.branch_name(issue)
        try:
            merged_prs = gh_list(
                "pr",
                head=branch_name,
                state="merged",
                fields=["number"],
                limit=1,
            )
            if merged_prs:
                pr_num = merged_prs[0].get("number", "?")
                raise IssueClosedError(
                    issue, f"RESOLVED by merged PR #{pr_num}"
                )
        except IssueClosedError:
            raise
        except Exception:
            # Non-critical check — if gh fails, proceed with orchestration
            logging.getLogger(__name__).debug(
                "Could not check for merged PRs for issue #%d", issue
            )

    def _check_stale_branch(self, issue: int) -> None:
        """Check for existing remote branch and clean up or warn.

        A stale remote branch ``feature/issue-N`` may indicate a previous
        attempt that left artifacts behind.

        In force/merge mode (``--merge``), stale branches are automatically
        cleaned up: any open PRs on the branch are closed and the remote
        branch is deleted so the builder can start fresh (issue #2415).

        In default mode we warn but always proceed so orchestration is
        not blocked.  Branches that back an open PR are not considered
        stale unless in force mode.
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
                # Check if this branch has an open PR
                open_prs: list[dict[str, Any]] = []
                try:
                    open_prs = gh_list(
                        "pr",
                        head=branch_name,
                        state="open",
                        fields=["number"],
                        limit=1,
                    )
                    if open_prs and not self.config.is_force_mode:
                        logger.debug(
                            "Branch %s has open PR #%s — not stale",
                            branch_name,
                            open_prs[0].get("number", "?"),
                        )
                        return
                except Exception:
                    # If the PR check fails, fall through to the warning
                    logger.debug(
                        "Could not check open PRs for branch %s", branch_name
                    )

                if self.config.is_force_mode:
                    self._cleanup_stale_remote_branch(
                        branch_name, open_prs, logger
                    )
                else:
                    msg = (
                        f"Stale branch {branch_name} exists on remote. "
                        "Previous attempt may have left artifacts."
                    )
                    logger.warning(msg)
                    self.warnings.append(msg)
        except OSError:
            # git not available or other OS error — skip the check
            pass

    def _cleanup_stale_remote_branch(
        self,
        branch_name: str,
        open_prs: list[dict[str, Any]],
        logger: logging.Logger,
    ) -> None:
        """Clean up a stale remote branch and its associated PRs.

        Called in force/merge mode to ensure the builder starts fresh.

        1. Closes any open PRs on the branch
        2. Deletes the remote branch
        3. Removes the local branch and worktree if present
        """
        issue = self.config.issue

        # Close stale open PRs
        for pr in open_prs:
            pr_num = pr.get("number")
            if pr_num is not None:
                try:
                    subprocess.run(
                        [
                            "gh", "pr", "close", str(pr_num),
                            "--comment",
                            f"Closing stale PR from previous attempt. "
                            f"Shepherd re-running issue #{issue} in merge mode.",
                        ],
                        cwd=self.repo_root,
                        capture_output=True,
                        check=False,
                    )
                    logger.info(
                        "Closed stale PR #%s for branch %s", pr_num, branch_name
                    )
                except OSError:
                    logger.debug("Failed to close PR #%s", pr_num)

        # Delete remote branch
        del_result = subprocess.run(
            ["git", "push", "origin", "--delete", branch_name],
            cwd=self.repo_root,
            capture_output=True,
            text=True,
            check=False,
        )
        if del_result.returncode == 0:
            logger.info("Deleted stale remote branch %s", branch_name)
        else:
            logger.warning(
                "Failed to delete remote branch %s: %s",
                branch_name,
                del_result.stderr.strip(),
            )

        # Remove local branch if it exists
        subprocess.run(
            ["git", "branch", "-D", branch_name],
            cwd=self.repo_root,
            capture_output=True,
            check=False,
        )

        # Remove local worktree if it exists
        worktree_path = LoomPaths(self.repo_root).worktree_path(issue)
        if worktree_path.is_dir():
            subprocess.run(
                ["git", "worktree", "remove", str(worktree_path), "--force"],
                cwd=self.repo_root,
                capture_output=True,
                check=False,
            )
            logger.info("Removed stale worktree %s", worktree_path)

        logger.info(
            "Cleaned up stale artifacts for issue #%d (merge mode)", issue
        )

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
            elif not result:
                # Progress file may have been deleted mid-run by a concurrent
                # shepherd cleaning up stale files for the same issue.
                # Re-engage the silent suppression guard so subsequent
                # milestone calls don't produce repeated error messages.
                self._progress_initialized = False
                logger.debug(
                    "Progress file for task %s appears to have been removed. "
                    "Disabling further milestone reporting.",
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
