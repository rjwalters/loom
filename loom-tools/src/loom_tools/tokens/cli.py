"""CLI entry point for ``loom-tokens``.

Subcommands:

* ``bootstrap`` (#3234) — materialize ``.loom/tokens/`` from
  ``ACCOUNT_*_N`` triples in ``.env``.
* ``check`` (#3237) — probe each bootstrapped account for rate-limit
  headers and (optionally) write ``.loom/tokens/.ranking`` for the
  spawn-time selector (#3235).

Additional subcommands (status, allowlist, ranking sync, etc.) can be
added later without breaking the top-level surface — the dispatch
table mirrors ``loom_tools.shepherd.cli``.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import sys
from pathlib import Path

from loom_tools.common.logging import log_error
from loom_tools.common.repo import find_repo_root
from loom_tools.tokens.bootstrap import bootstrap_tokens
from loom_tools.tokens.check import (
    DEFAULT_PROBE_PROMPT,
    format_table,
    run_check,
)


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

    cp = sub.add_parser(
        "check",
        help=(
            "Probe each bootstrapped account for rate-limit headers and "
            "rank by available quota."
        ),
    )
    cp.add_argument(
        "--ranking",
        action="store_true",
        help=(
            "Write .loom/tokens/.ranking atomically (consumed by the spawn "
            "wrapper, #3235)."
        ),
    )
    cp.add_argument(
        "--probe-prompt",
        type=str,
        default=DEFAULT_PROBE_PROMPT,
        help=(
            f"Override the probe prompt (default {DEFAULT_PROBE_PROMPT!r}). "
            "The probe always uses max_tokens=1 regardless of prompt."
        ),
    )
    cp.add_argument(
        "--json",
        dest="emit_json",
        action="store_true",
        help="Emit the full report as JSON to stdout (instead of a human table).",
    )
    cp.add_argument(
        "--no-stagger",
        action="store_true",
        help="Skip the 0.5-1.5s jitter between probes (mostly for tests).",
    )

    return parser


def main(argv: list[str] | None = None) -> int:
    """CLI entry point for ``loom-tokens``."""
    parser = _build_parser()
    args = parser.parse_args(argv)

    if args.command == "bootstrap":
        return _cmd_bootstrap(args)
    if args.command == "check":
        return _cmd_check(args)

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


def _cmd_check(args: argparse.Namespace) -> int:
    """Handle ``loom-tokens check``."""
    # Configure logging on stderr so --json stdout output is clean.
    if not logging.getLogger("loom_tools.tokens.check").handlers:
        logging.basicConfig(
            level=logging.INFO,
            format="%(levelname)s %(message)s",
            stream=sys.stderr,
        )

    # Resolve tokens dir. Prefer LOOM_TOKENS_DIR for tests, else
    # <repo-root>/.loom/tokens (matching where bootstrap writes).
    env_dir = os.environ.get("LOOM_TOKENS_DIR")
    if env_dir:
        tokens_dir = Path(env_dir)
    else:
        try:
            repo_root = find_repo_root()
        except FileNotFoundError as exc:
            log_error(str(exc))
            return 1
        tokens_dir = repo_root / ".loom" / "tokens"

    report = run_check(
        tokens_dir,
        write_ranking=args.ranking,
        probe_prompt=args.probe_prompt,
        stagger=not args.no_stagger,
    )

    if args.emit_json:
        json.dump(report.to_dict(), sys.stdout, indent=2)
        sys.stdout.write("\n")
    else:
        print(format_table(report))

    # Exit 0 unless every probe failed (then 1 — selector has nothing usable)
    if report.accounts and all(
        a.status in ("error", "skipped") for a in report.accounts
    ):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
