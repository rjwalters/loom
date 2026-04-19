"""CLI entry point for forge-agnostic auto-merge.

Enables auto-merge for a PR using the appropriate forge backend:
- GitHub: delegates to ``gh pr merge --auto`` (server-side queue)
- Gitea: polls CI status and merges when checks pass

Usage::

    loom-auto-merge <pr-number> [--method squash] [--poll-interval 30] [--timeout 600]

Environment variables::

    LOOM_AUTO_MERGE_POLL_INTERVAL  - Poll interval in seconds (default: 30)
    LOOM_AUTO_MERGE_TIMEOUT        - Timeout in seconds (default: 600)
"""

from __future__ import annotations

import argparse
import logging
import os
import sys

logger = logging.getLogger(__name__)


def main(argv: list[str] | None = None) -> int:
    """CLI entry point for ``loom-auto-merge``."""
    parser = argparse.ArgumentParser(
        prog="loom-auto-merge",
        description="Enable auto-merge for a PR (forge-agnostic)",
    )
    parser.add_argument(
        "pr_number",
        type=int,
        help="Pull request number",
    )
    parser.add_argument(
        "--method",
        default="squash",
        choices=["squash", "merge", "rebase"],
        help="Merge method (default: squash)",
    )
    parser.add_argument(
        "--poll-interval",
        type=int,
        default=int(os.environ.get("LOOM_AUTO_MERGE_POLL_INTERVAL", "30")),
        help="Seconds between CI status polls (default: 30, env: LOOM_AUTO_MERGE_POLL_INTERVAL)",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=int(os.environ.get("LOOM_AUTO_MERGE_TIMEOUT", "600")),
        help="Max seconds to wait for CI (default: 600, env: LOOM_AUTO_MERGE_TIMEOUT)",
    )
    parser.add_argument(
        "--verbose", "-v",
        action="store_true",
        help="Enable verbose logging",
    )

    args = parser.parse_args(argv)

    # Configure logging
    level = logging.DEBUG if args.verbose else logging.INFO
    logging.basicConfig(
        level=level,
        format="%(levelname)s: %(message)s",
        stream=sys.stderr,
    )

    from loom_tools.common.forge import get_forge

    forge = get_forge(cached=False)

    logger.info(
        "Auto-merge PR #%d via %s (method=%s, poll=%ds, timeout=%ds)",
        args.pr_number, forge.forge_type, args.method,
        args.poll_interval, args.timeout,
    )

    success = forge.auto_merge_pull_request(
        args.pr_number,
        method=args.method,
        poll_interval=args.poll_interval,
        timeout=args.timeout,
    )

    if success:
        print(f"Auto-merge {'enabled' if forge.forge_type == 'github' else 'completed'} for PR #{args.pr_number}")
        return 0
    else:
        print(f"Failed to auto-merge PR #{args.pr_number}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
