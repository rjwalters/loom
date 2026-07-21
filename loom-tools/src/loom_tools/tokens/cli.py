"""CLI entry point for ``loom-tokens``.

Subcommands:

* ``bootstrap`` (#3234) — materialize ``.loom/tokens/`` from
  ``ACCOUNT_*_N`` triples in ``.env``.
* ``check`` (#3237) — probe each bootstrapped account for rate-limit
  headers and (optionally) write ``.loom/tokens/.ranking`` for the
  spawn-time selector (#3235).
* ``pin`` / ``unpin`` (#3238) — operator-managed allowlist controlling
  which accounts the spawn-time selector is allowed to pick.
* ``unblock`` (#3238) — clear ``auth``-reason entries from
  ``.bad_tokens`` so the named accounts become eligible again.

Additional subcommands (status, ranking sync, etc.) can be added later
without breaking the top-level surface — the dispatch table mirrors
``loom_tools.shepherd.cli``.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import re
import sys
from pathlib import Path

from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.repo import find_repo_root
from loom_tools.tokens import allowlist as allowlist_mod
from loom_tools.tokens import failure_counts
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
        help=(
            "Materialize .loom/tokens/ from ACCOUNT_*_N triples, merging the "
            "home master (~/.loom/accounts.env) with the repo-local source. "
            "ACCOUNT_TOKEN_FILE_N is optional: when omitted it is auto-derived "
            "from ACCOUNT_EMAIL_N (e.g. robb@2amlogic.com -> robb-2amlogic.token)."
        ),
    )
    bp.add_argument(
        "--env",
        type=Path,
        default=None,
        help=(
            "Path to the repo-local account source (default: "
            "<repo>/.loom/accounts.env if present, else <repo>/.env)."
        ),
    )
    bp.add_argument(
        "--home-env",
        type=Path,
        default=None,
        help=(
            "Path to the home-dir master account source (default: "
            "$LOOM_ACCOUNTS_ENV or ~/.loom/accounts.env)."
        ),
    )
    bp.add_argument(
        "--no-home",
        action="store_true",
        help="Ignore the home master; bootstrap from the repo-local source only.",
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
        "--source",
        choices=["auto", "monitor", "probe"],
        default=None,
        help=(
            "Where to source the ranking (#3697): 'auto' (default) uses "
            "claude-monitor's ~/.claude-monitor/ranking.json when present and "
            "fresh, else probes; 'monitor' uses claude-monitor only (no "
            "probe); 'probe' always live-probes (pre-#3697 behavior). "
            "Overrides $LOOM_RANKING_SOURCE."
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

    # ---- pin (allowlist mutation) -------------------------------------
    pp = sub.add_parser(
        "pin",
        help=(
            "Manage the .loom/tokens/.allowlist file. The spawn-time "
            "selector will only pick accounts present in the allowlist."
        ),
    )
    pp_sub = pp.add_subparsers(dest="pin_action")
    pp_sub.required = False

    pp_set = pp_sub.add_parser(
        "set",
        help="Replace the allowlist with exactly the given accounts.",
    )
    pp_set.add_argument("names", nargs="+", help="Account names (exact match).")

    pp_add = pp_sub.add_parser(
        "add",
        help="Add account(s) to the existing allowlist.",
    )
    pp_add.add_argument("names", nargs="+", help="Account names (exact match).")

    pp_remove = pp_sub.add_parser(
        "remove",
        help="Remove account(s) from the allowlist.",
    )
    pp_remove.add_argument("names", nargs="+", help="Account names (exact match).")

    pp_status = pp_sub.add_parser(
        "status",
        help="Show the current allowlist and all available accounts.",
    )
    pp_status.add_argument(
        "--json",
        dest="emit_json",
        action="store_true",
        help="Emit JSON instead of a human table.",
    )

    # Bare positional accounts (no subcommand) == implicit `set` for legacy
    # `pin <names...>` form (matches lean-genius and the locked CLI).
    pp.add_argument(
        "legacy_names",
        nargs="*",
        help=argparse.SUPPRESS,
    )

    # ---- unpin (clear the allowlist) ----------------------------------
    up = sub.add_parser(
        "unpin",
        help="Clear the allowlist (all accounts eligible).",
    )
    up.add_argument(
        "--json",
        dest="emit_json",
        action="store_true",
        help="Emit a JSON status instead of a human message.",
    )

    # ---- unblock (clear bad-token auth entries) -----------------------
    ub = sub.add_parser(
        "unblock",
        help=(
            "Remove auth-reason entries for the given accounts from "
            ".bad_tokens (e.g. after re-authenticating)."
        ),
    )
    ub.add_argument(
        "names",
        nargs="+",
        help="Account names to unblock (exact match).",
    )
    ub.add_argument(
        "--all-reasons",
        action="store_true",
        help=(
            "Also drop non-auth entries (TTL-style, exhausted/expired). "
            "Default is auth-reason only — TTL entries clear themselves."
        ),
    )
    ub.add_argument(
        "--json",
        dest="emit_json",
        action="store_true",
        help="Emit JSON status.",
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
    if args.command == "pin":
        return _cmd_pin(args)
    if args.command == "unpin":
        return _cmd_unpin(args)
    if args.command == "unblock":
        return _cmd_unblock(args)

    # argparse `required=True` should make this unreachable.
    parser.print_help(sys.stderr)
    return 1


def _cmd_bootstrap(args: argparse.Namespace) -> int:
    try:
        repo_root = find_repo_root()
    except FileNotFoundError as exc:
        log_error(str(exc))
        return 1

    # `--no-home` disables the master (pass None); otherwise pass an explicit
    # --home-env path, or fall through to the default resolution sentinel.
    home_kwargs: dict[str, object] = {}
    if args.no_home:
        home_kwargs["home_env_path"] = None
    elif args.home_env is not None:
        home_kwargs["home_env_path"] = args.home_env

    try:
        result = bootstrap_tokens(
            repo_root,
            env_path=args.env,
            force=args.force,
            dry_run=args.dry_run,
            **home_kwargs,
        )
    except FileNotFoundError as exc:
        log_error(str(exc))
        return 1
    except ValueError as exc:
        log_error(str(exc))
        return 1

    if args.emit_json:
        print(json.dumps(result.to_dict(), indent=2))
    else:
        _print_effective_accounts(result)

    # Treat unresolved drift (without --force) as a non-zero exit so CI
    # can detect divergence.
    if result.drifted and not args.force:
        return 2
    return 0


# Human-readable label for each provenance tag from the merge (#3695).
_SOURCE_LABEL = {
    "home": "home",
    "repo": "repo",
    "repo-override": "repo (overrides home)",
}


def _print_effective_accounts(result: "object") -> None:
    """Print the effective merged account set and where each came from.

    Satisfies the #3695 acceptance criterion that ``bootstrap`` (and
    ``--dry-run``) reports the effective set with provenance. Secrets are never
    shown — only email, token filename, and source.
    """
    effective = getattr(result, "effective", []) or []
    home_env = getattr(result, "home_env", None)
    repo_env = getattr(result, "repo_env", None)

    print("Account sources:")
    print(f"  home: {home_env if home_env else '(none)'}")
    print(f"  repo: {repo_env if repo_env else '(none)'}")

    if not effective:
        print("Effective accounts: (none)")
        return

    print(f"Effective accounts ({len(effective)}):")
    width = max(len(a.get("name", "")) for a in effective)
    for a in effective:
        label = _SOURCE_LABEL.get(a.get("source", ""), a.get("source", ""))
        name = a.get("name", "")
        email = a.get("email", "")
        print(f"  {name:<{width}}  {email}  [{label}]")


# Environment override for the ranking source (#3697). The --source flag
# takes precedence; this env is the fallback; 'auto' is the built-in default.
_RANKING_SOURCE_VAR = "LOOM_RANKING_SOURCE"
_VALID_RANKING_SOURCES = ("auto", "monitor", "probe")


def _resolve_ranking_source(flag_value: str | None) -> str:
    """Resolve the ranking source: flag > $LOOM_RANKING_SOURCE > 'auto'.

    An invalid env value is ignored (with a warning) and treated as unset so
    a typo never silently disables ranking.
    """
    if flag_value is not None:
        return flag_value
    env = os.environ.get(_RANKING_SOURCE_VAR)
    if env is not None:
        candidate = env.strip().lower()
        if candidate in _VALID_RANKING_SOURCES:
            return candidate
        if candidate:
            log_warning(
                f"Ignoring invalid {_RANKING_SOURCE_VAR}={env!r}; "
                f"expected one of {', '.join(_VALID_RANKING_SOURCES)}."
            )
    return "auto"


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

    source = _resolve_ranking_source(args.source)

    report = run_check(
        tokens_dir,
        source=source,
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


def _resolve_workspace() -> Path | None:
    """Resolve the workspace root, honoring ``LOOM_WORKSPACE`` for tests."""
    env = os.environ.get("LOOM_WORKSPACE")
    if env:
        return Path(env)
    try:
        return find_repo_root()
    except FileNotFoundError as exc:
        log_error(str(exc))
        return None


def _cmd_pin(args: argparse.Namespace) -> int:
    """Handle ``loom-tokens pin [<action>] <names...>``."""
    workspace = _resolve_workspace()
    if workspace is None:
        return 1

    # Determine action. The bare form `loom-tokens pin <name...>` (no
    # subcommand) is accepted as a `set` for compatibility with the
    # lean-genius CLI and the locked muscle memory of operators.
    action = args.pin_action
    legacy = list(getattr(args, "legacy_names", []) or [])

    if action == "status" or (action is None and not legacy):
        return _pin_status(workspace, emit_json=getattr(args, "emit_json", False))

    if action is None and legacy:
        # Legacy form: `loom-tokens pin agent-1 agent-2`
        action = "set"
        names = legacy
    else:
        names = list(getattr(args, "names", []) or [])

    if not names:
        log_error(f"`pin {action}` requires at least one account name.")
        return 1

    try:
        if action == "set":
            written = allowlist_mod.write_allowlist(workspace, names)
            failure_counts.reset_all(workspace)
            if written:
                log_success(
                    f"Allowlist set to {len(written)} account(s): "
                    f"{', '.join(written)}",
                )
            else:
                log_warning(
                    "Allowlist cleared (no names resolved). "
                    "All accounts are eligible.",
                )
            return 0

        if action == "add":
            added, skipped = allowlist_mod.add_to_allowlist(workspace, names)
            failure_counts.reset_all(workspace)
            if added:
                log_success(f"Added {len(added)} account(s): {', '.join(added)}")
            if skipped:
                log_info(f"Already present: {', '.join(skipped)}")
            return 0

        if action == "remove":
            removed, skipped = allowlist_mod.remove_from_allowlist(workspace, names)
            failure_counts.reset_all(workspace)
            if removed:
                log_success(f"Removed {len(removed)} account(s): {', '.join(removed)}")
            if skipped:
                log_warning(f"Not in allowlist: {', '.join(skipped)}")
            return 0

    except allowlist_mod.UnknownAccountError as exc:
        log_error(str(exc))
        return 2
    except FileNotFoundError as exc:
        log_error(str(exc))
        return 1

    log_error(f"Unknown pin action: {action}")
    return 1


def _pin_status(workspace: Path, *, emit_json: bool) -> int:
    """Print the current allowlist and account roster."""
    try:
        all_accounts = allowlist_mod.list_accounts(workspace)
        active = allowlist_mod.read_allowlist(workspace)
    except OSError as exc:
        log_error(str(exc))
        return 1

    if emit_json:
        payload = {
            "allowlist_active": bool(active),
            "allowlist": active,
            "accounts": all_accounts,
        }
        print(json.dumps(payload, indent=2))
        return 0

    if active:
        print(f"Allowlist active ({len(active)} account(s)):")
        for name in active:
            print(f"  * {name}")
    else:
        print("No allowlist active — all accounts are eligible.")

    if all_accounts:
        print()
        print(f"Available accounts ({len(all_accounts)}):")
        active_set = set(active)
        for name in all_accounts:
            mark = "*" if name in active_set else " "
            print(f"  {mark} {name}")
    else:
        print()
        log_warning(
            "No .token files found. Run `loom-tokens bootstrap` first.",
        )
    return 0


def _cmd_unpin(args: argparse.Namespace) -> int:
    """Handle ``loom-tokens unpin``."""
    workspace = _resolve_workspace()
    if workspace is None:
        return 1
    try:
        had_file = allowlist_mod.clear_allowlist(workspace)
    except OSError as exc:
        log_error(str(exc))
        return 1
    failure_counts.reset_all(workspace)

    if getattr(args, "emit_json", False):
        print(json.dumps({"cleared": had_file}, indent=2))
        return 0

    if had_file:
        log_success("Allowlist cleared. All accounts are eligible.")
    else:
        log_info("No allowlist was active.")
    return 0


# Reasons we treat as "auth" for the unblock command. Match
# case-insensitively against the free-form reason field of bad_tokens
# entries. We do NOT match TOKEN_EXHAUSTED — those expire on their own.
_AUTH_REASON_RE = re.compile(
    r"\b("
    r"401|"
    r"oauth|"
    r"auth(entication)?|"
    r"unauthorized|"
    r"token[_\s]?expired|"
    r"expired|"
    r"blocked"
    r")\b",
    re.IGNORECASE,
)


def _cmd_unblock(args: argparse.Namespace) -> int:
    """Handle ``loom-tokens unblock <names...> [--all-reasons]``."""
    workspace = _resolve_workspace()
    if workspace is None:
        return 1

    # Validate names against bootstrapped accounts (EXACT match).
    available = allowlist_mod.list_accounts(workspace)
    available_set = set(available)
    names: list[str] = []
    for raw in args.names:
        name = raw.strip()
        if not name:
            continue
        if name not in available_set:
            log_error(
                f"Unknown account '{name}'. "
                f"Available: {', '.join(available) if available else '(none)'}",
            )
            return 2
        names.append(name)

    if not names:
        log_error("`unblock` requires at least one account name.")
        return 1

    bad_file = workspace / ".loom" / "tokens" / ".bad_tokens"
    lock_path = workspace / ".loom" / "tokens" / ".bad_tokens.lock"

    # Reuse the bad_tokens lock to coordinate with concurrent appenders.
    from loom_tools.tokens.bad_tokens import _MkdirLock

    if not bad_file.is_file():
        if getattr(args, "emit_json", False):
            print(json.dumps({"removed": 0, "kept": 0}, indent=2))
        else:
            log_info("No .bad_tokens file. Nothing to unblock.")
        return 0

    target_set = set(names)
    removed = 0
    kept_lines: list[str] = []
    with _MkdirLock(lock_path):
        try:
            lines = bad_file.read_text(encoding="utf-8").splitlines()
        except OSError as exc:
            log_error(f"Failed to read .bad_tokens: {exc}")
            return 1
        for line in lines:
            stripped = line.strip()
            if not stripped:
                continue
            parts = stripped.split(maxsplit=2)
            if len(parts) < 2:
                # Malformed — keep so we don't lose data.
                kept_lines.append(line)
                continue
            entry_name = parts[1]
            reason = parts[2] if len(parts) >= 3 else ""
            if entry_name in target_set:
                if args.all_reasons or _AUTH_REASON_RE.search(reason):
                    removed += 1
                    continue
            kept_lines.append(line)

        # Atomic rewrite via temp file (matches cleanup_bad_tokens).
        tmp = bad_file.with_suffix(bad_file.suffix + ".tmp")
        if kept_lines:
            tmp.write_text(
                "\n".join(kept_lines) + "\n",
                encoding="utf-8",
            )
        else:
            tmp.write_text("", encoding="utf-8")
        os.replace(tmp, bad_file)

    # Reset failure counters for the unblocked accounts so the auto-unpin
    # heuristic doesn't fire immediately on the next failure.
    for name in names:
        failure_counts.record_success(workspace, name)

    if getattr(args, "emit_json", False):
        print(
            json.dumps(
                {"removed": removed, "kept": len(kept_lines)},
                indent=2,
            ),
        )
    else:
        if removed:
            log_success(
                f"Removed {removed} bad-token entr{'y' if removed == 1 else 'ies'}"
                f" for: {', '.join(names)}",
            )
        else:
            log_info(
                f"No matching entries removed (use --all-reasons to drop "
                f"non-auth entries too).",
            )
    return 0


if __name__ == "__main__":
    sys.exit(main())
