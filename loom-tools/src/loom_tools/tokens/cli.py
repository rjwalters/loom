"""CLI entry point for ``loom-tokens``.

Currently exposes a single subcommand, ``bootstrap``, which materializes
the ``.loom/tokens/`` pool from ``.env``. Additional subcommands (status,
allowlist, ranking sync, etc.) can be added later without breaking the
top-level surface — the dispatch table mirrors
``loom_tools.shepherd.cli``.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from loom_tools.common.logging import log_error
from loom_tools.common.repo import find_repo_root
from loom_tools.tokens.bootstrap import bootstrap_tokens


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="loom-tokens",
        description="Manage the .loom/tokens/ OAuth pool for Claude Code rotation.",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    bp = sub.add_parser(
        "bootstrap",
        help="Materialize .loom/tokens/ from ACCOUNT_*_N triples in .env.",
    )
    bp.add_argument(
        "--env",
        type=Path,
        default=None,
        help="Path to .env file (default: <repo-root>/.env).",
    )
    bp.add_argument(
        "--force",
        action="store_true",
        help="Overwrite existing token files even if their fingerprint matches.",
    )
    bp.add_argument(
        "--dry-run",
        action="store_true",
        help="Report what would change without writing any files.",
    )
    bp.add_argument(
        "--json",
        dest="emit_json",
        action="store_true",
        help="Emit a JSON summary on stdout (in addition to log lines).",
    )

    return parser


def main(argv: list[str] | None = None) -> int:
    """CLI entry point for ``loom-tokens``."""
    parser = _build_parser()
    args = parser.parse_args(argv)

    if args.command == "bootstrap":
        return _cmd_bootstrap(args)

    # argparse `required=True` should make this unreachable.
    parser.print_help(sys.stderr)
    return 1


def _cmd_bootstrap(args: argparse.Namespace) -> int:
    try:
        repo_root = find_repo_root()
    except FileNotFoundError as exc:
        log_error(str(exc))
        return 1

    try:
        result = bootstrap_tokens(
            repo_root,
            env_path=args.env,
            force=args.force,
            dry_run=args.dry_run,
        )
    except FileNotFoundError as exc:
        log_error(str(exc))
        return 1
    except ValueError as exc:
        log_error(str(exc))
        return 1

    if args.emit_json:
        print(json.dumps(result.to_dict(), indent=2))

    # Treat unresolved drift (without --force) as a non-zero exit so CI
    # can detect divergence.
    if result.drifted and not args.force:
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
