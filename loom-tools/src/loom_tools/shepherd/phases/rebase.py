"""Rebase phase implementation."""

from __future__ import annotations

import json
import subprocess

from loom_tools.common.git import attempt_rebase, force_push_branch, is_branch_behind
from loom_tools.common.logging import log_info
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.labels import add_pr_label, remove_pr_label
from loom_tools.shepherd.phases.base import BasePhase, PhaseResult


def _is_pr_merged(pr_number: int, repo_root: str | None) -> bool:
    """Check if a PR is already merged via ``gh pr view``."""
    result = subprocess.run(
        ["gh", "pr", "view", str(pr_number), "--json", "state", "--jq", ".state"],
        cwd=repo_root,
        capture_output=True,
        text=True,
        check=False,
    )
    return result.stdout.strip() == "MERGED"


def _is_pr_mergeable(pr_number: int, repo_root: str | None) -> bool:
    """Check if GitHub considers a PR mergeable despite local rebase failure.

    Queries ``gh pr view --json mergeable,mergeStateStatus`` and returns True
    only when GitHub reports the PR as ``MERGEABLE`` with merge state ``CLEAN``.

    This handles cases where ``git rebase`` fails locally (stale tracking refs,
    race conditions, algorithm differences) but GitHub's merge evaluation
    considers the PR conflict-free.  See issue #2601.
    """
    result = subprocess.run(
        [
            "gh", "pr", "view", str(pr_number),
            "--json", "mergeable,mergeStateStatus",
        ],
        cwd=repo_root,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        return False

    try:
        data = json.loads(result.stdout)
    except (json.JSONDecodeError, ValueError):
        return False

    return (
        data.get("mergeable") == "MERGEABLE"
        and data.get("mergeStateStatus") == "CLEAN"
    )


class RebasePhase(BasePhase):
    """Phase 5.5: Rebase — Update feature branch onto main before merge."""

    phase_name = "rebase"

    def should_skip(self, ctx: ShepherdContext) -> tuple[bool, str]:
        """Rebase phase never skips via --from."""
        return False, ""

    def run(self, ctx: ShepherdContext) -> PhaseResult:
        """Run rebase phase.

        1. Check for shutdown signal
        2. Verify we have a worktree to operate in
        3. Fetch and check if branch is behind origin/main
        4. If not behind, skip (already up to date)
        5. If behind, attempt rebase
        6. On success: force-push, remove loom:merge-conflict label
        7. On failure: apply loom:merge-conflict label, add diagnostic comment
        """
        if ctx.check_shutdown():
            return self.shutdown("shutdown signal detected")

        if ctx.worktree_path is None or not ctx.worktree_path.is_dir():
            if ctx.pr_number and _is_pr_mergeable(ctx.pr_number, str(ctx.repo_root)):
                log_info(
                    f"No worktree available but PR #{ctx.pr_number} is CLEAN on GitHub"
                    f" — skipping rebase"
                )
                return self.success(
                    "no worktree available but PR is mergeable on GitHub",
                    {"reason": "github_mergeable_fallback"},
                )
            return self.failed(
                "no worktree path available for rebase",
                {"reason": "no_worktree"},
            )

        cwd = ctx.worktree_path

        # Check if branch is behind origin/main
        if not is_branch_behind("origin/main", cwd=cwd):
            return self.skipped("branch is already up to date with origin/main")

        # Attempt rebase
        ctx.report_milestone("heartbeat", action="rebasing onto origin/main")
        success, detail = attempt_rebase("origin/main", cwd=cwd)

        if success:
            # Force-push the rebased branch
            if not force_push_branch(cwd=cwd):
                # Check if the PR was already merged (e.g., by Champion)
                # before declaring failure — force-push to a merged branch
                # is expected to fail and is not an error.
                if ctx.pr_number is not None and _is_pr_merged(
                    ctx.pr_number, ctx.repo_root
                ):
                    return self.success(
                        f"force-push failed but PR #{ctx.pr_number} is already merged"
                    )
                return self.failed(
                    "rebase succeeded but force-push failed",
                    {"reason": "push_failed"},
                )

            # Remove merge-conflict label if present
            if ctx.pr_number is not None:
                remove_pr_label(ctx.pr_number, "loom:merge-conflict", ctx.repo_root)
                ctx.label_cache.invalidate_pr(ctx.pr_number)

            return self.success("rebased onto origin/main and force-pushed")

        # Rebase failed locally — check if GitHub still considers the PR
        # mergeable before giving up.  Local rebase can disagree with GitHub
        # due to stale tracking refs, race conditions, or algorithm
        # differences.  Since the merge phase uses GitHub's API, we can
        # safely skip the local rebase when GitHub says CLEAN.  See #2601.
        if ctx.pr_number is not None and _is_pr_mergeable(
            ctx.pr_number, ctx.repo_root
        ):
            log_info(
                f"Local rebase failed but GitHub reports PR #{ctx.pr_number} "
                f"as MERGEABLE/CLEAN — skipping local rebase"
            )
            return self.success(
                f"local rebase failed but PR #{ctx.pr_number} is mergeable on GitHub",
                {"reason": "github_mergeable_fallback", "local_detail": detail},
            )

        # Both local rebase and GitHub agree: conflicts exist
        if ctx.pr_number is not None:
            add_pr_label(ctx.pr_number, "loom:merge-conflict", ctx.repo_root)
            ctx.label_cache.invalidate_pr(ctx.pr_number)

            # Add diagnostic comment
            body = (
                f"**Shepherd rebase failed** for issue #{ctx.config.issue}.\n\n"
                f"The feature branch has conflicts with `main` that cannot be "
                f"automatically resolved.\n\n"
                f"```\n{detail}\n```\n\n"
                f"A human or Doctor agent needs to resolve these conflicts."
            )
            subprocess.run(
                ["gh", "pr", "comment", str(ctx.pr_number), "--body", body],
                cwd=ctx.repo_root,
                capture_output=True,
                check=False,
            )

        return self.failed(
            f"rebase onto origin/main failed: {detail}",
            {"reason": "merge_conflict", "detail": detail},
        )

    def validate(self, ctx: ShepherdContext) -> bool:
        """Validate rebase phase contract.

        After a successful rebase the branch should not be behind origin/main.
        """
        if ctx.worktree_path is None or not ctx.worktree_path.is_dir():
            return False
        return not is_branch_behind("origin/main", cwd=ctx.worktree_path)
