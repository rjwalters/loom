"""Token selection algorithm — 3-tier priority.

Selection order:
    1. Ranking file (.ranking, <10 min old): pick randomly among the top-N
       non-exhausted/non-blocked accounts (default N=3, configurable via
       ``LOOM_TOKEN_SPREAD_TOP_N`` / ``tokens.spreadTopN``; N=1 = greedy
       first-eligible), skipping bad tokens. Spreading across the top-N
       avoids concurrent spawners colliding on .ranking[0] (issue #3736).
    2. Allowlist file (.allowlist): random pick from allowed accounts.
    3. Random pick from all .token files.

In all tiers, tokens marked bad (via bad_tokens.is_bad) are skipped.

Stale-ranking fail-safe (issue #3894): when ``.ranking`` exists but is older
than the freshness window, tier-1 declines — but rather than discarding the
ranking entirely and degrading to *fully-random* selection into accounts a
recent probe already flagged ``exhausted``/``blocked`` (which wedges sweeps at
startup), the stale ranking's exhausted/blocked entries are carried forward as
an **advisory exclusion set** applied to the allowlist and random tiers. If the
exclusions would empty the pool (e.g. a stale "everything exhausted" ranking),
selection retries ignoring them so a live pool can never hard-fail on stale
advice.

This module is import-safe: no I/O occurs at import time. Both the daemon
(via ``import``) and the bash wrapper (via ``python3 -m``) call
``select_token`` from concurrent processes.

Worktree handling: when invoked from a worktree, callers should pass the
canonical repo root (i.e. ``git rev-parse --show-toplevel`` of the main
checkout, not the worktree). The bash wrapper uses
``git rev-parse --git-common-dir`` to derive this.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable

from loom_tools.common.config import env_int
from loom_tools.tokens.bad_tokens import is_bad

# Ranking file is considered fresh for this many seconds.
_RANKING_FRESH_SECONDS = 600  # 10 min

# Exit code when no token is available (matches sysexits.h EX_CONFIG)
EX_CONFIG = 78

# Default number of top-ranked eligible accounts to spread spawns across.
# See issue #3736: picking only .ranking[0] deterministically collides
# concurrent spawners (e.g. sibling daemons on a shared claude-monitor pool)
# onto the single least-utilized account, tripping its session limit.
_DEFAULT_SPREAD_TOP_N = 3


class TokenSelectionError(Exception):
    """Base class for token selection failures."""


class EmptyTokenPoolError(TokenSelectionError):
    """No tokens available — bootstrap has not been run, or all are bad."""


@dataclass(frozen=True)
class SelectedToken:
    """A token chosen by ``select_token``."""

    name: str  # basename without .token extension
    file: Path  # absolute path to .token file
    key: str  # token contents (whitespace-stripped)
    mode: str  # "ranked" | "allowlist" | "random"


def _read_token_file(token_path: Path) -> str:
    """Read a .token file, stripping all whitespace defensively."""
    raw = token_path.read_text(encoding="utf-8", errors="strict")
    return "".join(raw.split())


def _file_age_seconds(path: Path) -> float | None:
    """Return file age in seconds, or None if file missing."""
    try:
        return time.time() - path.stat().st_mtime
    except FileNotFoundError:
        return None


def _list_token_files(tokens_dir: Path) -> list[Path]:
    """Return all .token files sorted by name (deterministic ordering)."""
    return sorted(tokens_dir.glob("*.token"))


def _strip_comment(line: str) -> str:
    """Drop ``#`` comments and trim whitespace."""
    line = line.split("#", 1)[0]
    return line.strip()


def _read_ranking(ranking_file: Path) -> Iterable[tuple[str, str]]:
    """Yield (name, status) pairs from the ranking file.

    Format: ``name|status`` per line. Lines starting with ``#`` are skipped.
    Malformed lines are skipped. ``status`` defaults to empty string.
    """
    try:
        for raw in ranking_file.read_text(encoding="utf-8").splitlines():
            stripped = _strip_comment(raw)
            if not stripped:
                continue
            parts = stripped.split("|", 1)
            name = parts[0].strip()
            status = parts[1].strip() if len(parts) > 1 else ""
            if name:
                yield name, status
    except OSError:
        return


def _read_allowlist(allowlist_file: Path) -> list[str]:
    """Return list of token names in the allowlist (no .token extension)."""
    out: list[str] = []
    try:
        for raw in allowlist_file.read_text(encoding="utf-8").splitlines():
            stripped = _strip_comment(raw)
            if stripped:
                out.append(stripped)
    except OSError:
        pass
    return out


def _resolve_spread_top_n(workspace_path: Path) -> int:
    """Resolve the top-N spread window for the ranked strategy.

    Precedence (highest first), mirroring the nested-key + env-override
    precedent in ``common/paths.py`` and ``common/gitea.py``:

        1. ``LOOM_TOKEN_SPREAD_TOP_N`` env var.
        2. ``.loom/config.json`` -> ``tokens.spreadTopN`` (soft-fail read).
        3. Default (``_DEFAULT_SPREAD_TOP_N`` = 3).

    Values are clamped to ``>= 1``. ``N == 1`` restores the historical greedy
    first-eligible behavior exactly (back-compat escape hatch).
    """
    # 1. Env var override (highest precedence).
    if os.environ.get("LOOM_TOKEN_SPREAD_TOP_N") is not None:
        return max(1, env_int("LOOM_TOKEN_SPREAD_TOP_N", default=_DEFAULT_SPREAD_TOP_N))

    # 2. Config key — .loom/config.json -> tokens.spreadTopN (soft-fail read).
    config_n = _read_config_spread_top_n(workspace_path)
    if config_n is not None:
        return max(1, config_n)

    # 3. Default.
    return _DEFAULT_SPREAD_TOP_N


def _read_config_spread_top_n(workspace_path: Path) -> int | None:
    """Read ``.loom/config.json`` -> ``tokens.spreadTopN``, soft-failing to ``None``.

    Missing file, parse error, missing key, or a non-int value all resolve to
    ``None`` (never a hard error), mirroring the soft-fail config reads in
    ``common/paths.py``.
    """
    # Imported lazily to keep this module import-safe (no I/O / heavy deps at
    # import time — see module docstring).
    from loom_tools.common.state import read_json_file

    config_path = workspace_path / ".loom" / "config.json"
    data = read_json_file(config_path, default={})
    if not isinstance(data, dict):
        return None
    tokens_cfg = data.get("tokens")
    if not isinstance(tokens_cfg, dict):
        return None
    value = tokens_cfg.get("spreadTopN")
    # Reject bool (a subclass of int) and non-int values.
    if isinstance(value, bool) or not isinstance(value, int):
        return None
    return value


def _try_ranking(
    tokens_dir: Path,
    ranking_file: Path,
    workspace_path: Path,
    rng: random.Random,
) -> SelectedToken | None:
    """Strategy 1: read .ranking, pick randomly among the top-N eligible entries.

    Historically this returned the *first* non-exhausted/non-blocked entry.
    Because ``.ranking`` is ordered most-available-first, every concurrent
    spawner reading a fresh ranking picked the identical account, serializing
    load onto one account and tripping its session limit (issue #3736).

    We now collect up to the top-N eligible ranked entries (skipping
    exhausted/blocked/bad/missing/empty tokens, preserving ranking order) and
    ``rng.choice`` among them, spreading concurrent spawners across the most
    available accounts while still preferring healthy ones. N is resolved via
    ``_resolve_spread_top_n``; ``N == 1`` restores the old greedy behavior.
    """
    age = _file_age_seconds(ranking_file)
    if age is None or age >= _RANKING_FRESH_SECONDS:
        return None

    top_n = _resolve_spread_top_n(workspace_path)
    eligible: list[SelectedToken] = []
    for name, status in _read_ranking(ranking_file):
        if status in ("exhausted", "blocked"):
            continue
        token_file = tokens_dir / f"{name}.token"
        if not token_file.is_file():
            continue
        if is_bad(workspace_path, name):
            continue
        try:
            key = _read_token_file(token_file)
        except OSError:
            continue
        if not key:
            continue
        eligible.append(
            SelectedToken(name=name, file=token_file, key=key, mode="ranked"),
        )
        if len(eligible) >= top_n:
            break

    if not eligible:
        return None
    return rng.choice(eligible)


def _stale_ranking_exclusions(ranking_file: Path) -> set[str]:
    """Advisory exclusion set sourced from a *stale* ``.ranking`` (issue #3894).

    When ``.ranking`` is older than the freshness window, tier-1 (``_try_ranking``)
    declines and selection would otherwise degrade to fully-random — repeatedly
    handing out accounts a recent probe already flagged ``exhausted``/``blocked``,
    wedging sweeps at startup. Rather than discard the stale ranking, treat its
    exhausted/blocked entries as an advisory exclusion set for the lower tiers.

    Returns an empty set when the ranking is fresh (tier-1 owns that case) or
    missing/unreadable — so callers get exclusions *only* in the stale-but-present
    window, and the pre-#3894 behavior is preserved everywhere else.
    """
    age = _file_age_seconds(ranking_file)
    if age is None or age < _RANKING_FRESH_SECONDS:
        return set()
    return {
        name
        for name, status in _read_ranking(ranking_file)
        if status in ("exhausted", "blocked")
    }


def _try_allowlist(
    tokens_dir: Path,
    allowlist_file: Path,
    workspace_path: Path,
    rng: random.Random,
    exclude: frozenset[str] | set[str] = frozenset(),
) -> SelectedToken | None:
    """Strategy 2: random pick from allowlist.

    ``exclude`` is an advisory set of account names to skip (stale-ranking
    exhausted/blocked entries, issue #3894).
    """
    if not allowlist_file.is_file():
        return None
    names = _read_allowlist(allowlist_file)
    eligible: list[Path] = []
    for name in names:
        if name in exclude:
            continue
        token_file = tokens_dir / f"{name}.token"
        if token_file.is_file() and not is_bad(workspace_path, name):
            eligible.append(token_file)
    if not eligible:
        return None
    rng.shuffle(eligible)
    for token_file in eligible:
        try:
            key = _read_token_file(token_file)
        except OSError:
            continue
        if not key:
            continue
        return SelectedToken(
            name=token_file.stem,
            file=token_file,
            key=key,
            mode="allowlist",
        )
    return None


def _try_random(
    tokens_dir: Path,
    workspace_path: Path,
    rng: random.Random,
    exclude: frozenset[str] | set[str] = frozenset(),
) -> SelectedToken | None:
    """Strategy 3: random pick from all tokens.

    ``exclude`` is an advisory set of account names to skip (stale-ranking
    exhausted/blocked entries, issue #3894).
    """
    candidates = [
        p
        for p in _list_token_files(tokens_dir)
        if not is_bad(workspace_path, p.stem) and p.stem not in exclude
    ]
    if not candidates:
        return None
    rng.shuffle(candidates)
    for token_file in candidates:
        try:
            key = _read_token_file(token_file)
        except OSError:
            continue
        if not key:
            continue
        return SelectedToken(
            name=token_file.stem,
            file=token_file,
            key=key,
            mode="random",
        )
    return None


def select_token(
    workspace_path: Path | str,
    *,
    rng: random.Random | None = None,
) -> SelectedToken:
    """Select an OAuth token using the 3-tier algorithm.

    Args:
        workspace_path: Repo root containing ``.loom/tokens/``. When called
            from a worktree, pass the canonical (main checkout) root, not
            the worktree path.
        rng: Optional random.Random instance for deterministic testing.
            Defaults to a module-level Random seeded from os.urandom.

    Returns:
        SelectedToken with name, absolute file path, key, and selection mode.

    Raises:
        EmptyTokenPoolError: When ``.loom/tokens/`` is missing, contains no
            ``.token`` files, or every token is marked bad.
            The bash wrapper hard-fails (exit 78) and prompts the user to
            run ``loom-tokens bootstrap`` — never silently falls back.
    """
    workspace_path = Path(workspace_path)
    tokens_dir = workspace_path / ".loom" / "tokens"

    if not tokens_dir.is_dir():
        raise EmptyTokenPoolError(
            f"Token directory does not exist: {tokens_dir}. "
            f"Run `loom-tokens bootstrap` to populate it.",
        )

    all_tokens = _list_token_files(tokens_dir)
    if not all_tokens:
        raise EmptyTokenPoolError(
            f"No .token files in {tokens_dir}. Run `loom-tokens bootstrap`.",
        )

    if rng is None:
        rng = random.Random(os.urandom(16))

    ranking_file = tokens_dir / ".ranking"
    allowlist_file = tokens_dir / ".allowlist"

    selected = _try_ranking(tokens_dir, ranking_file, workspace_path, rng)
    if selected is not None:
        return selected

    # Tier-1 declined: .ranking is absent or stale. If a stale ranking exists,
    # carry its exhausted/blocked entries forward as an advisory exclusion set
    # so the lower tiers don't degrade to random selection into known-bad
    # accounts (issue #3894). A fresh/missing ranking yields no exclusions.
    exclude = _stale_ranking_exclusions(ranking_file)

    selected = _try_allowlist(
        tokens_dir, allowlist_file, workspace_path, rng, exclude=exclude,
    )
    if selected is not None:
        return selected

    selected = _try_random(tokens_dir, workspace_path, rng, exclude=exclude)
    if selected is not None:
        return selected

    # Fail-safe: the advisory exclusions emptied the pool (e.g. a stale
    # "everything exhausted" ranking). Retry ignoring them so a live pool can
    # never hard-fail on stale advice — better to spawn into a possibly-tired
    # account than to refuse all work.
    if exclude:
        selected = _try_allowlist(tokens_dir, allowlist_file, workspace_path, rng)
        if selected is not None:
            return selected
        selected = _try_random(tokens_dir, workspace_path, rng)
        if selected is not None:
            return selected

    raise EmptyTokenPoolError(
        f"All {len(all_tokens)} tokens in {tokens_dir} are marked bad or empty. "
        f"Inspect .bad_tokens or run `loom-tokens bootstrap --force`.",
    )


def _main(argv: list[str] | None = None) -> int:
    """CLI entry: emit the selected token as JSON or shell-export lines.

    The bash wrapper invokes us via ``python3 -m loom_tools.tokens.select``
    and parses the JSON form. ``--export`` is provided for convenience
    debugging.
    """
    parser = argparse.ArgumentParser(
        prog="python -m loom_tools.tokens.select",
        description="Select a Claude Code OAuth token from .loom/tokens/.",
    )
    parser.add_argument(
        "--workspace",
        required=True,
        help="Repo root containing .loom/tokens/.",
    )
    fmt = parser.add_mutually_exclusive_group()
    fmt.add_argument("--json", action="store_true", help="Emit JSON (default).")
    fmt.add_argument(
        "--export",
        action="store_true",
        help="Emit shell `export CLAUDE_CODE_OAUTH_TOKEN=...` lines.",
    )
    parser.add_argument(
        "--no-key",
        action="store_true",
        help="Omit the secret key from output (for safe inspection).",
    )
    args = parser.parse_args(argv)

    try:
        sel = select_token(args.workspace)
    except EmptyTokenPoolError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return EX_CONFIG

    if args.export:
        if args.no_key:
            print(f"# selected={sel.name} mode={sel.mode} file={sel.file}")
        else:
            print(f"export CLAUDE_CODE_OAUTH_TOKEN={sel.key!r}")
            print(f"# selected={sel.name} mode={sel.mode} file={sel.file}")
        return 0

    payload = {
        "name": sel.name,
        "file": str(sel.file),
        "mode": sel.mode,
    }
    if not args.no_key:
        payload["key"] = sel.key
    print(json.dumps(payload))
    return 0


if __name__ == "__main__":
    sys.exit(_main())
