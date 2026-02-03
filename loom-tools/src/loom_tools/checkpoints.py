"""Builder checkpoint management for structured progress tracking.

Checkpoints allow the shepherd to detect partial progress when the builder
fails, enabling smarter recovery decisions instead of always retrying from
scratch.

Checkpoint file location: ``.loom/worktrees/issue-N/.loom-checkpoint``

Checkpoint Stages:
    planning     - Builder is reading issue, planning approach
    implementing - Writing code, making changes
    tested       - Tests ran (pass or fail)
    committed    - Changes committed locally
    pushed       - Branch pushed to remote
    pr_created   - PR exists with proper labels
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
from dataclasses import dataclass, field
from typing import Any

from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_json_file, write_json_file
from loom_tools.common.time_utils import now_utc

# Valid checkpoint stages in order of progression
CHECKPOINT_STAGES = (
    "planning",
    "implementing",
    "tested",
    "committed",
    "pushed",
    "pr_created",
)

# Recovery paths based on checkpoint stage
RECOVERY_PATHS = {
    "planning": "retry_from_scratch",  # No useful work done
    "implementing": "check_changes",  # May have useful changes
    "tested": "route_to_commit",  # Tests ran, route based on result
    "committed": "push_and_pr",  # Just needs push and PR
    "pushed": "create_pr",  # Just needs PR creation
    "pr_created": "verify_labels",  # Just needs label check
}

CHECKPOINT_FILENAME = ".loom-checkpoint"


@dataclass
class CheckpointDetails:
    """Optional details about the checkpoint stage."""

    files_changed: int = 0
    test_command: str = ""
    test_result: str = ""  # "pass", "fail", or ""
    test_output_summary: str = ""
    commit_sha: str = ""
    pr_number: int | None = None
    extra: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        """Convert to dictionary for JSON serialization."""
        d: dict[str, Any] = {}
        if self.files_changed:
            d["files_changed"] = self.files_changed
        if self.test_command:
            d["test_command"] = self.test_command
        if self.test_result:
            d["test_result"] = self.test_result
        if self.test_output_summary:
            d["test_output_summary"] = self.test_output_summary
        if self.commit_sha:
            d["commit_sha"] = self.commit_sha
        if self.pr_number is not None:
            d["pr_number"] = self.pr_number
        if self.extra:
            d.update(self.extra)
        return d

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> CheckpointDetails:
        """Create from dictionary."""
        known_keys = {
            "files_changed",
            "test_command",
            "test_result",
            "test_output_summary",
            "commit_sha",
            "pr_number",
        }
        extra = {k: v for k, v in data.items() if k not in known_keys}
        return cls(
            files_changed=data.get("files_changed", 0),
            test_command=data.get("test_command", ""),
            test_result=data.get("test_result", ""),
            test_output_summary=data.get("test_output_summary", ""),
            commit_sha=data.get("commit_sha", ""),
            pr_number=data.get("pr_number"),
            extra=extra,
        )


@dataclass
class Checkpoint:
    """A builder checkpoint representing progress at a specific stage."""

    stage: str
    timestamp: str
    issue: int = 0
    details: CheckpointDetails = field(default_factory=CheckpointDetails)

    def to_dict(self) -> dict[str, Any]:
        """Convert to dictionary for JSON serialization."""
        d: dict[str, Any] = {
            "stage": self.stage,
            "timestamp": self.timestamp,
        }
        if self.issue:
            d["issue"] = self.issue
        details_dict = self.details.to_dict()
        if details_dict:
            d["details"] = details_dict
        return d

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> Checkpoint:
        """Create from dictionary."""
        details_data = data.get("details", {})
        return cls(
            stage=data.get("stage", ""),
            timestamp=data.get("timestamp", ""),
            issue=data.get("issue", 0),
            details=CheckpointDetails.from_dict(details_data),
        )

    @property
    def recovery_path(self) -> str:
        """Get the recommended recovery path for this checkpoint stage."""
        return RECOVERY_PATHS.get(self.stage, "retry_from_scratch")

    @property
    def stage_index(self) -> int:
        """Get the index of this stage in the progression (0-based)."""
        try:
            return CHECKPOINT_STAGES.index(self.stage)
        except ValueError:
            return -1

    def is_after(self, other_stage: str) -> bool:
        """Check if this checkpoint is after the given stage."""
        try:
            other_index = CHECKPOINT_STAGES.index(other_stage)
            return self.stage_index > other_index
        except ValueError:
            return False


def get_checkpoint_path(worktree_path: pathlib.Path) -> pathlib.Path:
    """Get the checkpoint file path for a worktree."""
    return worktree_path / CHECKPOINT_FILENAME


def read_checkpoint(worktree_path: pathlib.Path) -> Checkpoint | None:
    """Read checkpoint from a worktree.

    Returns None if no checkpoint exists or if the file is invalid.
    """
    checkpoint_path = get_checkpoint_path(worktree_path)
    if not checkpoint_path.is_file():
        return None

    try:
        data = read_json_file(checkpoint_path)
        if not isinstance(data, dict):
            return None
        # Require at least a stage field to be a valid checkpoint
        if "stage" not in data or not data["stage"]:
            return None
        return Checkpoint.from_dict(data)
    except Exception:
        return None


def write_checkpoint(
    worktree_path: pathlib.Path,
    stage: str,
    issue: int = 0,
    *,
    quiet: bool = False,
    **kwargs: Any,
) -> bool:
    """Write a checkpoint to a worktree.

    Parameters
    ----------
    worktree_path:
        Path to the worktree directory.
    stage:
        Checkpoint stage (one of CHECKPOINT_STAGES).
    issue:
        Issue number being worked on.
    quiet:
        Suppress informational output.
    **kwargs:
        Additional details (files_changed, test_command, test_result, etc.)

    Returns
    -------
    bool
        True on success, False on error.
    """
    if stage not in CHECKPOINT_STAGES:
        if not quiet:
            log_error(f"Invalid checkpoint stage '{stage}'")
            log_error(f"Valid stages: {', '.join(CHECKPOINT_STAGES)}")
        return False

    if not worktree_path.is_dir():
        if not quiet:
            log_error(f"Worktree directory does not exist: {worktree_path}")
        return False

    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
    details = CheckpointDetails(
        files_changed=kwargs.get("files_changed", 0),
        test_command=kwargs.get("test_command", ""),
        test_result=kwargs.get("test_result", ""),
        test_output_summary=kwargs.get("test_output_summary", ""),
        commit_sha=kwargs.get("commit_sha", ""),
        pr_number=kwargs.get("pr_number"),
    )

    checkpoint = Checkpoint(
        stage=stage,
        timestamp=timestamp,
        issue=issue,
        details=details,
    )

    checkpoint_path = get_checkpoint_path(worktree_path)
    try:
        write_json_file(checkpoint_path, checkpoint.to_dict())
        if not quiet:
            log_success(f"Checkpoint saved: stage={stage}")
        return True
    except Exception as exc:
        if not quiet:
            log_error(f"Failed to write checkpoint: {exc}")
        return False


def clear_checkpoint(worktree_path: pathlib.Path, *, quiet: bool = False) -> bool:
    """Remove checkpoint file from a worktree.

    Returns True if the file was removed or didn't exist.
    """
    checkpoint_path = get_checkpoint_path(worktree_path)
    if not checkpoint_path.is_file():
        return True

    try:
        checkpoint_path.unlink()
        if not quiet:
            log_info("Checkpoint cleared")
        return True
    except Exception as exc:
        if not quiet:
            log_error(f"Failed to clear checkpoint: {exc}")
        return False


def get_recovery_recommendation(checkpoint: Checkpoint | None) -> dict[str, Any]:
    """Get recovery recommendation based on checkpoint state.

    Returns a dictionary with:
        - recovery_path: The recommended recovery action
        - skip_stages: List of stages that can be skipped
        - details: Additional context for the recovery
    """
    if checkpoint is None:
        return {
            "recovery_path": "retry_from_scratch",
            "skip_stages": [],
            "details": "No checkpoint found",
        }

    recovery_path = checkpoint.recovery_path
    stage_index = checkpoint.stage_index

    # Calculate which stages can be skipped
    skip_stages = list(CHECKPOINT_STAGES[: stage_index + 1]) if stage_index >= 0 else []

    # Build details message
    details_parts = [f"Checkpoint at stage '{checkpoint.stage}'"]
    if checkpoint.details.test_result:
        details_parts.append(f"test_result={checkpoint.details.test_result}")
    if checkpoint.details.files_changed:
        details_parts.append(f"files_changed={checkpoint.details.files_changed}")
    if checkpoint.details.pr_number:
        details_parts.append(f"pr_number={checkpoint.details.pr_number}")

    return {
        "recovery_path": recovery_path,
        "skip_stages": skip_stages,
        "details": ", ".join(details_parts),
        "checkpoint": checkpoint.to_dict(),
    }


# ── CLI ─────────────────────────────────────────────────────────


def main(argv: list[str] | None = None) -> int:
    """CLI entry point (registered as ``loom-checkpoint``)."""
    parser = argparse.ArgumentParser(
        description="Manage builder checkpoints",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Commands:
  write    Write a checkpoint to a worktree
  read     Read checkpoint from a worktree
  clear    Clear checkpoint from a worktree
  stages   List valid checkpoint stages

Examples:
  loom-checkpoint write --worktree .loom/worktrees/issue-42 --stage implementing --issue 42
  loom-checkpoint write --worktree . --stage tested --test-result pass --test-command "pnpm check:ci"
  loom-checkpoint read --worktree .loom/worktrees/issue-42
  loom-checkpoint clear --worktree .loom/worktrees/issue-42
  loom-checkpoint stages
""",
    )

    parser.add_argument(
        "command",
        nargs="?",
        choices=["write", "read", "clear", "stages"],
        help="Command to run",
    )
    parser.add_argument(
        "--worktree",
        "-w",
        help="Path to worktree directory (default: current directory)",
    )
    parser.add_argument(
        "--stage",
        "-s",
        choices=CHECKPOINT_STAGES,
        help="Checkpoint stage (for 'write')",
    )
    parser.add_argument("--issue", "-i", type=int, help="Issue number (for 'write')")
    parser.add_argument(
        "--files-changed", type=int, help="Number of files changed (for 'write')"
    )
    parser.add_argument("--test-command", help="Test command that was run (for 'write')")
    parser.add_argument(
        "--test-result",
        choices=["pass", "fail"],
        help="Test result (for 'write')",
    )
    parser.add_argument(
        "--test-output-summary", help="Brief test output summary (for 'write')"
    )
    parser.add_argument("--commit-sha", help="Commit SHA (for 'write')")
    parser.add_argument("--pr-number", type=int, help="PR number (for 'write')")
    parser.add_argument(
        "--json", "-j", action="store_true", help="Output in JSON format"
    )
    parser.add_argument(
        "--quiet", "-q", action="store_true", help="Suppress output on success"
    )

    args = parser.parse_args(argv)

    if not args.command:
        parser.print_help()
        return 0

    # Handle 'stages' command (no worktree needed)
    if args.command == "stages":
        if args.json:
            print(json.dumps({"stages": list(CHECKPOINT_STAGES), "recovery_paths": RECOVERY_PATHS}))
        else:
            print("Valid checkpoint stages (in order of progression):")
            for stage in CHECKPOINT_STAGES:
                recovery = RECOVERY_PATHS[stage]
                print(f"  {stage:15} -> recovery: {recovery}")
        return 0

    # Determine worktree path
    if args.worktree:
        worktree_path = pathlib.Path(args.worktree).resolve()
    else:
        # Try to find worktree from current directory
        cwd = pathlib.Path.cwd()
        if ".loom/worktrees/" in str(cwd) or cwd.name.startswith("issue-"):
            worktree_path = cwd
        else:
            # Use current directory
            worktree_path = cwd

    if args.command == "write":
        if not args.stage:
            log_error("--stage is required for 'write'")
            return 1

        kwargs: dict[str, Any] = {}
        if args.files_changed is not None:
            kwargs["files_changed"] = args.files_changed
        if args.test_command:
            kwargs["test_command"] = args.test_command
        if args.test_result:
            kwargs["test_result"] = args.test_result
        if args.test_output_summary:
            kwargs["test_output_summary"] = args.test_output_summary
        if args.commit_sha:
            kwargs["commit_sha"] = args.commit_sha
        if args.pr_number is not None:
            kwargs["pr_number"] = args.pr_number

        ok = write_checkpoint(
            worktree_path,
            args.stage,
            issue=args.issue or 0,
            quiet=args.quiet,
            **kwargs,
        )
        return 0 if ok else 1

    elif args.command == "read":
        checkpoint = read_checkpoint(worktree_path)
        if checkpoint is None:
            if args.json:
                print(json.dumps({"checkpoint": None, "exists": False}))
            else:
                log_warning(f"No checkpoint found in {worktree_path}")
            return 0

        recommendation = get_recovery_recommendation(checkpoint)
        if args.json:
            print(
                json.dumps(
                    {
                        "checkpoint": checkpoint.to_dict(),
                        "exists": True,
                        "recommendation": recommendation,
                    }
                )
            )
        else:
            print(f"Checkpoint: stage={checkpoint.stage}, timestamp={checkpoint.timestamp}")
            if checkpoint.issue:
                print(f"  Issue: #{checkpoint.issue}")
            if checkpoint.details.test_result:
                print(f"  Test result: {checkpoint.details.test_result}")
            if checkpoint.details.files_changed:
                print(f"  Files changed: {checkpoint.details.files_changed}")
            if checkpoint.details.pr_number:
                print(f"  PR number: #{checkpoint.details.pr_number}")
            print(f"  Recovery path: {recommendation['recovery_path']}")
        return 0

    elif args.command == "clear":
        ok = clear_checkpoint(worktree_path, quiet=args.quiet)
        return 0 if ok else 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
