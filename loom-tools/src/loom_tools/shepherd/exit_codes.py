"""Granular exit codes for shepherd orchestration.

Exit codes convey what was accomplished during orchestration, enabling the daemon
to make smarter retry/escalate decisions and preserving partial progress.

See: https://github.com/rjwalters/loom/issues/2045
"""

from __future__ import annotations

from enum import IntEnum


class ShepherdExitCode(IntEnum):
    """Exit codes for shepherd orchestration.

    These codes convey what was accomplished during orchestration:

    | Exit Code | Meaning                       | Daemon Action                      |
    |-----------|-------------------------------|-----------------------------------|
    | 0         | Full success (merged/approved)| Mark complete                     |
    | 1         | No PR created (builder failed)| Retry or escalate                 |
    | 2         | PR created, tests failed      | Send to Doctor or flag for review |
    | 3         | Shutdown signal received      | Clean exit, requeue               |
    | 4         | Stuck/blocked, needs help     | Alert human                       |
    | 5         | Skipped (already complete)    | No action                         |
    | 6         | No changes needed             | Close issue, mark complete        |

    Using IntEnum allows these to be used directly as exit codes:
        return ShepherdExitCode.SUCCESS
        sys.exit(ShepherdExitCode.PR_TESTS_FAILED)
    """

    # Full success - orchestration completed, PR merged or approved
    SUCCESS = 0

    # Builder failed to produce a PR - no recoverable artifact exists
    # Daemon should retry the issue or escalate to human
    BUILDER_FAILED = 1

    # PR was created but tests failed after exhausting Doctor retries
    # Valuable work exists that can be recovered manually
    PR_TESTS_FAILED = 2

    # Graceful shutdown signal received (stop file or abort label)
    # Issue should be requeued for later processing
    SHUTDOWN = 3

    # Stuck/blocked state requiring human intervention
    # This covers: judge exhausted, doctor exhausted, baseline blocked, etc.
    NEEDS_INTERVENTION = 4

    # Issue was already complete (closed, merged, etc.) - nothing to do
    SKIPPED = 5

    # Builder analyzed issue and determined no changes are needed
    # The reported problem doesn't exist or is already resolved on main
    NO_CHANGES_NEEDED = 6


# Convenience mapping for code interpretation
EXIT_CODE_DESCRIPTIONS = {
    ShepherdExitCode.SUCCESS: "Full success - PR merged or approved",
    ShepherdExitCode.BUILDER_FAILED: "Builder failed - no PR created",
    ShepherdExitCode.PR_TESTS_FAILED: "PR created but tests failed",
    ShepherdExitCode.SHUTDOWN: "Shutdown signal received",
    ShepherdExitCode.NEEDS_INTERVENTION: "Stuck/blocked - needs human intervention",
    ShepherdExitCode.SKIPPED: "Skipped - issue already complete",
    ShepherdExitCode.NO_CHANGES_NEEDED: "No changes needed - problem already resolved",
}


def describe_exit_code(code: int) -> str:
    """Get human-readable description of an exit code.

    Args:
        code: Exit code value

    Returns:
        Description string, or "Unknown exit code" for unrecognized values
    """
    try:
        exit_code = ShepherdExitCode(code)
        return EXIT_CODE_DESCRIPTIONS.get(exit_code, f"Exit code {code}")
    except ValueError:
        return f"Unknown exit code: {code}"
