"""Loom worktree helper - safely create and manage git worktrees.

Handles worktree creation with:
    - Automatic navigation from nested worktrees
    - Branch reuse for abandoned work
    - Stale worktree detection and cleanup
    - Submodule initialization with shared objects
    - Post-worktree hook execution
    - Return-to directory tracking

Exit codes:
    0 - Success
    1 - Error
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import subprocess
import sys
from dataclasses import dataclass
from typing import Any

from loom_tools.common.logging import log_error, log_info, log_success, log_warning


@dataclass
class WorktreeResult:
    """Result of worktree operation."""

    success: bool
    worktree_path: str | None = None
    branch_name: str | None = None
    issue_number: int | None = None
    return_to: str | None = None
    error: str | None = None

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"success": self.success}
        if self.worktree_path:
            d["worktreePath"] = self.worktree_path
        if self.branch_name:
            d["branchName"] = self.branch_name
        if self.issue_number is not None:
            d["issueNumber"] = self.issue_number
        if self.return_to:
            d["returnTo"] = self.return_to
        if self.error:
            d["error"] = self.error
        return d


def _run_git(
    args: list[str],
    cwd: pathlib.Path | str | None = None,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    """Run a git command."""
    cmd = ["git"] + args
    return subprocess.run(
        cmd,
        cwd=cwd,
        capture_output=True,
        text=True,
        check=check,
    )


def _is_in_worktree() -> bool:
    """Check if we're currently in a worktree (not the main working directory)."""
    try:
        git_dir = _run_git(["rev-parse", "--git-common-dir"], check=False).stdout.strip()
        work_dir = _run_git(["rev-parse", "--show-toplevel"], check=False).stdout.strip()

        if not git_dir or not work_dir:
            return False

        # In main working directory, git_dir would be "work_dir/.git"
        expected_git = f"{work_dir}/.git"
        return git_dir != expected_git
    except Exception:
        return False


def _get_worktree_info() -> dict[str, str] | None:
    """Get information about current worktree."""
    if not _is_in_worktree():
        return None

    try:
        worktree_path = _run_git(["rev-parse", "--show-toplevel"]).stdout.strip()
        branch = _run_git(["rev-parse", "--abbrev-ref", "HEAD"]).stdout.strip()
        return {"path": worktree_path, "branch": branch}
    except Exception:
        return None


def _get_main_workspace() -> pathlib.Path | None:
    """Get the main workspace path (parent of .git directory)."""
    try:
        git_common_dir = _run_git(["rev-parse", "--git-common-dir"]).stdout.strip()
        if not git_common_dir:
            return None

        # The main workspace is the parent of .git
        git_path = pathlib.Path(git_common_dir).resolve()
        if git_path.name == ".git":
            return git_path.parent
        else:
            # For worktrees, git_common_dir points to .git/worktrees/...
            # Walk up to find .git
            p = git_path
            while p.name != ".git" and p != p.parent:
                p = p.parent
            if p.name == ".git":
                return p.parent
    except Exception:
        pass
    return None


def _fetch_latest_main(json_output: bool = False) -> bool:
    """Fetch latest changes from origin/main.

    Uses fetch-only approach to avoid conflicts with worktrees that have
    main checked out. Never touches the working tree or local branches.

    Returns:
        True if successful, False otherwise.
    """
    if not json_output:
        log_info("Fetching latest changes from origin/main...")

    try:
        result = _run_git(["fetch", "origin", "main"], check=False)
        if result.returncode == 0:
            if not json_output:
                log_success("Fetched latest origin/main")
            return True
        else:
            if not json_output:
                log_warning("Could not fetch origin/main (continuing with local state)")
            return False
    except Exception:
        if not json_output:
            log_warning("Could not fetch origin/main (continuing with local state)")
        return False


def _reset_stale_worktree_in_place(worktree_path: pathlib.Path, json_output: bool = False) -> bool:
    """Check if a worktree is stale and reset it in place.

    Instead of removing stale worktrees (which can corrupt the shell's CWD),
    resets them to origin/main via fetch + reset --hard.

    Returns:
        True if worktree was stale and was reset (caller should exit 0),
        False if worktree has real work and should be preserved.
    """
    try:
        # Check commits ahead of main
        result = _run_git(
            ["rev-list", "--count", "origin/main..HEAD"],
            cwd=worktree_path,
            check=False,
        )
        commits_ahead = int(result.stdout.strip()) if result.returncode == 0 else 0

        # Check commits behind main
        result = _run_git(
            ["rev-list", "--count", "HEAD..origin/main"],
            cwd=worktree_path,
            check=False,
        )
        commits_behind = int(result.stdout.strip()) if result.returncode == 0 else 0

        # Check for uncommitted changes
        result = _run_git(["status", "--porcelain"], cwd=worktree_path, check=False)
        uncommitted = result.stdout.strip() if result.returncode == 0 else ""

        # Has real work - not stale
        if commits_ahead > 0 or uncommitted:
            return False

        # Stale: no commits ahead, no uncommitted changes
        if not json_output:
            log_warning(
                f"Stale worktree detected (0 commits ahead, "
                f"{commits_behind} behind main, no uncommitted changes)"
            )
            log_info("Resetting worktree in place to origin/main...")

        # Fetch and reset in place
        fetch_result = _run_git(["fetch", "origin", "main"], cwd=worktree_path, check=False)
        reset_result = _run_git(["reset", "--hard", "origin/main"], cwd=worktree_path, check=False)

        if fetch_result.returncode == 0 and reset_result.returncode == 0:
            if not json_output:
                log_success("Stale worktree reset to origin/main")
        else:
            if not json_output:
                log_warning("Could not reset stale worktree (continuing to use as-is)")

        return True
    except Exception:
        return False


def _init_submodules(
    worktree_path: pathlib.Path,
    main_git_dir: str,
    json_output: bool = False,
) -> None:
    """Initialize submodules with reference to main workspace for object sharing."""
    try:
        # Check for uninitialized submodules
        result = _run_git(["submodule", "status"], cwd=worktree_path, check=False)
        if result.returncode != 0:
            return

        uninit_submodules = []
        for line in result.stdout.strip().split("\n"):
            if line.startswith("-"):
                # Uninitialized submodule (starts with -)
                parts = line.split()
                if len(parts) >= 2:
                    uninit_submodules.append(parts[1])

        if not uninit_submodules:
            return

        if not json_output:
            log_info(f"Initializing {len(uninit_submodules)} submodule(s) with shared objects...")

        failed = False
        for submod_path in uninit_submodules:
            ref_path = pathlib.Path(main_git_dir) / "modules" / submod_path

            if ref_path.is_dir():
                # Use reference to share objects with main workspace (fast, no network)
                result = _run_git(
                    ["submodule", "update", "--init", "--reference", str(ref_path), "--", submod_path],
                    cwd=worktree_path,
                    check=False,
                )
            else:
                # No reference available, initialize normally
                result = _run_git(
                    ["submodule", "update", "--init", "--", submod_path],
                    cwd=worktree_path,
                    check=False,
                )

            if result.returncode != 0:
                failed = True

        if failed:
            if not json_output:
                log_warning("Some submodules failed to initialize (worktree still created)")
                log_info("You may need to run: git submodule update --init --recursive")
        else:
            if not json_output:
                log_success("Submodules initialized with shared objects")

    except Exception:
        pass


def _run_post_worktree_hook(
    worktree_path: pathlib.Path,
    branch_name: str,
    issue_number: int,
    json_output: bool = False,
) -> None:
    """Run project-specific post-worktree hook if it exists."""
    main_workspace = _get_main_workspace()
    if not main_workspace:
        return

    hook_path = main_workspace / ".loom" / "hooks" / "post-worktree.sh"
    if not hook_path.exists() or not hook_path.is_file():
        return

    # Check if executable
    import stat

    if not hook_path.stat().st_mode & stat.S_IXUSR:
        return

    if not json_output:
        log_info("Running project-specific post-worktree hook...")

    try:
        result = subprocess.run(
            [str(hook_path), str(worktree_path), branch_name, str(issue_number)],
            cwd=worktree_path,
            capture_output=True,
            text=True,
        )

        if result.returncode == 0:
            if not json_output:
                log_success("Post-worktree hook completed")
        else:
            if not json_output:
                log_warning("Post-worktree hook failed (worktree still created)")
    except Exception:
        if not json_output:
            log_warning("Post-worktree hook failed (worktree still created)")


def check_worktree() -> int:
    """Check if currently in a worktree and print info."""
    info = _get_worktree_info()
    if info:
        print("Current worktree:")
        print(f"  Path: {info['path']}")
        print(f"  Branch: {info['branch']}")
        return 0
    else:
        print("Not currently in a worktree (you're in the main working directory)")
        return 1


def _handle_feature_branch_in_main_worktree(
    error_output: str,
    branch_name: str,
    issue_number: int,
    json_output: bool = False,
) -> tuple[bool, bool]:
    """Detect and recover when a feature branch is checked out in the main worktree.

    This condition arises when a previous builder manually checked out
    ``feature/issue-N`` in the main workspace and left it there.  Git
    refuses to create a new worktree for that branch with the error:
    ``fatal: 'feature/issue-N' is already used by worktree at '<path>'``

    Recovery strategy:
    1. Detect the "already used by worktree at" pattern in stderr.
    2. Confirm the conflicting worktree is the main workspace.
    3. If main workspace is clean: auto-switch to ``main`` and return
       ``(handled=True, retry=True)`` so the caller can retry.
    4. If main workspace has uncommitted changes: emit an actionable
       error and return ``(handled=True, retry=False)``.

    Returns:
        A (handled, retry) tuple:
        - ``(False, False)`` — not this error, caller should propagate normally.
        - ``(True, False)``  — this error, handled with message, no retry.
        - ``(True, True)``   — this error, auto-recovered, caller should retry.
    """
    if "is already used by worktree at" not in error_output:
        return False, False

    # Extract the conflicting worktree path from the error message.
    # Example: "fatal: 'feature/issue-2853' is already used by worktree at '/path'"
    match = re.search(r"is already used by worktree at '([^']+)'", error_output)
    if not match:
        # Could not parse path — emit actionable guidance and fail.
        if not json_output:
            log_error(
                f"Cannot create worktree: branch '{branch_name}' is already "
                "checked out in another worktree."
            )
            print()
            print("  The branch is in use elsewhere. To free it, find the worktree with:")
            print("    git worktree list")
            print("  Then switch that worktree to main:")
            print("    cd <worktree-path> && git checkout main")
        return True, False

    conflict_path = pathlib.Path(match.group(1)).resolve()

    # Determine the main workspace path for comparison.
    main_workspace = _get_main_workspace()
    if main_workspace is None:
        # Cannot determine main workspace — fall back to generic failure.
        return False, False

    abs_main = main_workspace.resolve()

    if conflict_path != abs_main:
        # Conflicting worktree is not the main workspace — it's another issue
        # worktree.  Emit actionable guidance without auto-recovery.
        if not json_output:
            log_error(f"Cannot create worktree for branch '{branch_name}':")
            print(f"  Branch is already checked out at: {conflict_path}")
            print()
            print("  To fix:")
            print(f"    cd {conflict_path} && git checkout main")
        return True, False

    # The conflict is in the main workspace.  Check for uncommitted changes.
    status_result = _run_git(["status", "--porcelain"], cwd=abs_main, check=False)
    uncommitted = status_result.stdout.strip() if status_result.returncode == 0 else ""

    if uncommitted:
        # Main workspace has uncommitted changes — cannot auto-recover safely.
        if not json_output:
            log_error(
                f"Cannot create worktree for issue #{issue_number}: "
                f"branch '{branch_name}'"
            )
            print(
                f"  is already checked out at '{abs_main}' (main worktree)."
            )
            print()
            print("  The main worktree has uncommitted changes — cannot auto-switch.")
            print("  To fix manually:")
            print(f"    cd {abs_main}")
            print("    git stash  # or commit your changes")
            print("    git checkout main")
            print(f"  Then rerun: ./.loom/scripts/worktree.sh {issue_number}")
        return True, False

    # Main workspace is clean — auto-switch to main.
    if not json_output:
        log_warning(f"Branch '{branch_name}' is checked out in the main worktree.")
        log_info("Main worktree is clean — auto-switching to main branch...")

    checkout_result = _run_git(["checkout", "main"], cwd=abs_main, check=False)
    if checkout_result.returncode == 0:
        if not json_output:
            log_success("Main worktree switched to main branch")
        return True, True  # Auto-recovered: retry worktree creation
    else:
        if not json_output:
            log_error("Failed to switch main worktree to main branch.")
            print("  To fix manually:")
            print(f"    cd {abs_main} && git checkout main")
            print(f"  Then rerun: ./.loom/scripts/worktree.sh {issue_number}")
        return True, False


def create_worktree(
    issue_number: int,
    custom_branch: str | None = None,
    return_to_dir: str | None = None,
    json_output: bool = False,
) -> WorktreeResult:
    """Create a worktree for an issue.

    Args:
        issue_number: The issue number.
        custom_branch: Optional custom branch name suffix.
        return_to_dir: Optional directory to store for return navigation.
        json_output: If True, suppress human-readable output.

    Returns:
        WorktreeResult with success status and details.
    """
    # Handle being in a worktree
    if _is_in_worktree():
        if not json_output:
            log_warning("Currently in a worktree, auto-navigating to main workspace...")
            print()
            info = _get_worktree_info()
            if info:
                print("Current worktree:")
                print(f"  Path: {info['path']}")
                print(f"  Branch: {info['branch']}")
                print()

        main_workspace = _get_main_workspace()
        if not main_workspace:
            return WorktreeResult(success=False, error="Failed to find main workspace")

        if not json_output:
            log_info(f"Found main workspace: {main_workspace}")

        import os

        os.chdir(main_workspace)

        if not json_output:
            log_success("Switched to main workspace")
            print()

    # Fetch latest changes from origin/main
    # Uses fetch-only to avoid conflicts with worktrees that have main checked out
    _fetch_latest_main(json_output)

    # Determine branch name
    if custom_branch:
        branch_name = f"feature/{custom_branch}"
    else:
        branch_name = f"feature/issue-{issue_number}"

    # Worktree path
    worktree_path = pathlib.Path(".loom/worktrees") / f"issue-{issue_number}"

    # Check if worktree already exists
    if worktree_path.exists():
        if not json_output:
            log_warning(f"Worktree already exists at: {worktree_path}")

        # Check if it's registered with git
        result = _run_git(["worktree", "list"], check=False)
        if str(worktree_path) in result.stdout or worktree_path.resolve().as_posix() in result.stdout:
            # Check if stale and reset in place (never remove)
            was_stale = _reset_stale_worktree_in_place(worktree_path.resolve(), json_output)

            if not was_stale:
                # Has real work - show info
                try:
                    commits_ahead = _run_git(
                        ["rev-list", "--count", "origin/main..HEAD"],
                        cwd=worktree_path,
                        check=False,
                    ).stdout.strip()
                    uncommitted = _run_git(
                        ["status", "--porcelain"],
                        cwd=worktree_path,
                        check=False,
                    ).stdout.strip()

                    if not json_output:
                        log_info("Worktree is registered with git")
                        if int(commits_ahead) > 0:
                            log_info(f"Worktree has {commits_ahead} commit(s) ahead of main - preserving existing work")
                        elif uncommitted:
                            log_info("Worktree has uncommitted changes - preserving existing work")
                        print()
                        log_info(f"To use this worktree: cd {worktree_path}")
                except Exception:
                    if not json_output:
                        log_info("Worktree is registered with git")
                        log_info(f"To use this worktree: cd {worktree_path}")
            else:
                # Stale worktree was reset in place
                if not json_output:
                    print()
                    log_info(f"To use this worktree: cd {worktree_path}")

            return WorktreeResult(
                success=True,
                worktree_path=str(worktree_path.resolve()),
                branch_name=branch_name,
                issue_number=issue_number,
            )
        else:
            return WorktreeResult(
                success=False,
                error="Directory exists but is not a registered worktree. "
                f"Remove it: rm -rf {worktree_path}",
            )

    # Check if branch already exists
    result = _run_git(["show-ref", "--verify", "--quiet", f"refs/heads/{branch_name}"], check=False)
    branch_exists = result.returncode == 0

    if branch_exists:
        if not json_output:
            log_warning(f"Branch '{branch_name}' already exists - reusing it")
            log_info("To create a new branch instead, use a custom branch name:")
            print(f"  loom-worktree {issue_number} <custom-branch-name>")
            print()
        create_args = [str(worktree_path), branch_name]
    else:
        if not json_output:
            log_info("Creating new branch from main")
        create_args = [str(worktree_path), "-b", branch_name, "origin/main"]

    # Create the worktree
    if not json_output:
        log_info("Creating worktree...")
        print(f"  Path: {worktree_path}")
        print(f"  Branch: {branch_name}")
        print()

    result = _run_git(["worktree", "add"] + create_args, check=False)
    if result.returncode != 0:
        error_output = (result.stderr or result.stdout or "").strip()
        handled, should_retry = _handle_feature_branch_in_main_worktree(
            error_output, branch_name, issue_number, json_output
        )

        if should_retry:
            # Auto-recovered (main worktree switched to main): retry once.
            if not json_output:
                log_info("Retrying worktree creation...")
            result = _run_git(["worktree", "add"] + create_args, check=False)
            if result.returncode != 0:
                error_detail = (result.stderr or result.stdout or "").strip()
                return WorktreeResult(
                    success=False,
                    error=f"Failed to create worktree after auto-recovery: {error_detail}",
                )
        elif handled:
            # Error was handled with a message above — return clean failure.
            return WorktreeResult(success=False, error="Failed to create worktree")
        else:
            # Unrecognised error — propagate git's message.
            return WorktreeResult(
                success=False,
                error=f"Failed to create worktree: {error_output}" if error_output
                else "Failed to create worktree",
            )

    # Get absolute path
    abs_worktree_path = worktree_path.resolve()

    # Store return-to directory if provided
    abs_return_to = None
    if return_to_dir:
        try:
            abs_return_to = pathlib.Path(return_to_dir).resolve()
            (abs_worktree_path / ".loom-return-to").write_text(str(abs_return_to))
            if not json_output:
                log_info(f"Stored return directory: {abs_return_to}")
        except Exception:
            pass

    # Initialize submodules
    try:
        main_git_dir = _run_git(["rev-parse", "--git-common-dir"]).stdout.strip()
        _init_submodules(abs_worktree_path, main_git_dir, json_output)
    except Exception:
        pass

    # Run post-worktree hook
    _run_post_worktree_hook(abs_worktree_path, branch_name, issue_number, json_output)

    # Success output
    if not json_output:
        log_success("Worktree created successfully!")
        print()
        log_info("Next steps:")
        print(f"  cd {worktree_path}")
        print("  # Do your work...")
        print("  git add -A")
        print('  git commit -m "Your message"')
        print(f"  git push -u origin {branch_name}")
        print("  gh pr create")

    return WorktreeResult(
        success=True,
        worktree_path=str(abs_worktree_path),
        branch_name=branch_name,
        issue_number=issue_number,
        return_to=str(abs_return_to) if abs_return_to else None,
    )


def main(argv: list[str] | None = None) -> int:
    """Main entry point for the worktree CLI."""
    parser = argparse.ArgumentParser(
        description="Loom worktree helper - safely create and manage git worktrees",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Usage:
  loom-worktree <issue-number>                    Create worktree for issue
  loom-worktree <issue-number> <branch>           Create worktree with custom branch
  loom-worktree --check                           Check if in a worktree
  loom-worktree --json <issue-number>             Machine-readable JSON output
  loom-worktree --return-to <dir> <issue-number>  Store return directory

Examples:
  loom-worktree 42
    Creates: .loom/worktrees/issue-42
    Branch: feature/issue-42

  loom-worktree 42 fix-bug
    Creates: .loom/worktrees/issue-42
    Branch: feature/fix-bug

Safety Features:
  - Detects if already in a worktree
  - Uses sandbox-safe path (.loom/worktrees/)
  - Pulls latest origin/main before creating worktree
  - Automatically creates branch from main
  - Prevents nested worktrees
  - Non-interactive (safe for AI agents)
  - Reuses existing branches automatically
  - Runs project-specific hooks after creation
  - Stashes/restores local changes during pull
""",
    )

    parser.add_argument(
        "--check",
        action="store_true",
        help="Check if currently in a worktree",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Output machine-readable JSON",
    )
    parser.add_argument(
        "--return-to",
        metavar="DIR",
        help="Store return directory for later navigation",
    )
    parser.add_argument(
        "issue_number",
        nargs="?",
        help="Issue number to create worktree for",
    )
    parser.add_argument(
        "custom_branch",
        nargs="?",
        help="Custom branch name suffix (optional)",
    )

    args = parser.parse_args(argv)

    if args.check:
        return check_worktree()

    if not args.issue_number:
        parser.print_help()
        return 0

    # Validate issue number
    try:
        issue_number = int(args.issue_number)
    except ValueError:
        if args.json:
            print(json.dumps({"success": False, "error": f"Invalid issue number: {args.issue_number}"}))
        else:
            log_error(f"Issue number must be numeric (got: '{args.issue_number}')")
        return 1

    # Validate return-to directory if provided
    if args.return_to:
        return_to_path = pathlib.Path(args.return_to)
        if not return_to_path.is_dir():
            if args.json:
                print(json.dumps({"error": "Return directory does not exist", "returnTo": args.return_to}))
            else:
                log_error(f"Return directory does not exist: {args.return_to}")
            return 1

    result = create_worktree(
        issue_number=issue_number,
        custom_branch=args.custom_branch,
        return_to_dir=args.return_to,
        json_output=args.json,
    )

    if args.json:
        print(json.dumps(result.to_dict()))

    return 0 if result.success else 1


if __name__ == "__main__":
    sys.exit(main())
