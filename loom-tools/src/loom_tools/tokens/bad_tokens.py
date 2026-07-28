"""Bad-token tracking with mkdir-based locking.

Tokens that fail with TOKEN_EXPIRED, TOKEN_EXHAUSTED, or otherwise prove
unusable are appended to ``.loom/tokens/.bad_tokens``. Subsequent selection
calls skip these tokens.

The file is shared across concurrent bash and Python writers. We coordinate
with a sibling ``.bad_tokens.lock`` directory, created via ``mkdir`` (POSIX
atomic) — see ``loom_tools.tokens._locking.MkdirLock``. ``flock`` is
intentionally not used because it is unavailable on stock macOS.

File format (one entry per line):
    <ISO8601 UTC timestamp> <token_name> <reason words...>

Reads use a word-boundary regex so ``agent-1`` and ``agent-10`` do not
collide.
"""

from __future__ import annotations

import os
import re
from datetime import datetime, timezone
from pathlib import Path

from loom_tools.tokens._locking import MkdirLock as _MkdirLock
from loom_tools.tokens.paths import resolve_tokens_dir

# Default cooldown (seconds) after which a non-auth (exhaustion) bad-token
# entry stops blocking selection (#4122 / #4212). Weekly/session/5h-limit
# exhaustion is transient — the account recovers on its own within a rate-limit
# window — so those entries expire while auth-reason entries (a broken
# credential) stay permanent. Mirrors the Rust reference
# (``tokens_pool::bad_tokens::DEFAULT_EXHAUSTION_COOLDOWN_SECS``) so the two
# implementations stay byte-for-byte in lockstep (the live spawn path is this
# Python one — issue #4212).
DEFAULT_EXHAUSTION_COOLDOWN_SECS = 6 * 3600

# Env override (whole seconds, must parse to ``> 0``) for
# ``DEFAULT_EXHAUSTION_COOLDOWN_SECS``. Same name as the Rust side.
EXHAUSTION_COOLDOWN_ENV = "LOOM_TOKEN_EXHAUSTION_COOLDOWN_SECS"

# Reasons treated as "auth" (a broken credential) rather than transient
# exhaustion. Auth entries block selection permanently and are the default
# scope of ``loom-tokens unblock``; exhaustion entries expire on their own
# (cooldown / cleanup). Canonical definition lives here so ``is_bad`` (this
# module) and ``cli.py``'s ``unblock`` share ONE regex — a drift between the
# two is exactly the lockstep bug #4212 guards against. Mirrors the Rust
# ``tokens_pool::bad_tokens::auth_reason_regex``.
AUTH_REASON_RE = re.compile(
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


def exhaustion_cooldown_secs() -> int:
    """Resolve the exhaustion-entry cooldown in seconds.

    Precedence: ``EXHAUSTION_COOLDOWN_ENV`` (whole seconds, ``> 0``) else
    ``DEFAULT_EXHAUSTION_COOLDOWN_SECS``. Mirrors the Rust
    ``exhaustion_cooldown_secs`` resolver.
    """
    raw = os.environ.get(EXHAUSTION_COOLDOWN_ENV)
    if raw is not None:
        try:
            n = int(raw.strip())
        except (ValueError, AttributeError):
            n = 0
        if n > 0:
            return n
    return DEFAULT_EXHAUSTION_COOLDOWN_SECS


def is_auth_reason(reason: str) -> bool:
    """Return True when ``reason`` names an auth failure (permanent block).

    A broken credential (401 / OAuth / expired / blocked) never self-heals, so
    such entries block until an explicit ``unblock``. Anything else is treated
    as transient exhaustion, subject to the cooldown in ``is_bad``.
    """
    return bool(AUTH_REASON_RE.search(reason))


def _tokens_dir(workspace_path: Path | str) -> Path:
    """Resolve the effective pool dir (per-repo, else shared) — issue #3938.

    Routing every bad-token read/write through the shared resolver keeps the
    ``.bad_tokens`` state file beside the ``*.token`` files that selection
    actually picks, so it never forks between the per-repo and shared pools.
    """
    return resolve_tokens_dir(workspace_path)


def _bad_tokens_path(tokens_dir: Path) -> Path:
    return tokens_dir / ".bad_tokens"


def _lock_path(tokens_dir: Path) -> Path:
    return tokens_dir / ".bad_tokens.lock"


def _name_pattern(token_name: str) -> re.Pattern[str]:
    """Word-boundary regex: matches the token name as a discrete token.

    The bad_tokens file format is whitespace-separated, so the field
    boundary is whitespace (or start/end of line).
    """
    return re.compile(
        r"(^|\s)" + re.escape(token_name) + r"(\s|$)",
        re.MULTILINE,
    )


def mark_bad(workspace_path: Path | str, token_name: str, reason: str) -> None:
    """Append a bad-token entry atomically.

    Args:
        workspace_path: Repo root containing ``.loom/tokens/``.
        token_name: Token name (basename of the .token file, no extension).
        reason: Free-form reason string. Newlines are replaced with spaces.

    Raises:
        TimeoutError: If the lock cannot be acquired in time.
        FileNotFoundError: If ``.loom/tokens/`` does not exist.
    """
    workspace_path = Path(workspace_path)
    tokens_dir = _tokens_dir(workspace_path)
    if not tokens_dir.is_dir():
        raise FileNotFoundError(f"Tokens dir does not exist: {tokens_dir}")

    timestamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    safe_reason = reason.replace("\n", " ").replace("\r", " ").strip()
    line = f"{timestamp} {token_name} {safe_reason}\n"

    with _MkdirLock(_lock_path(tokens_dir)):
        with open(_bad_tokens_path(tokens_dir), "a", encoding="utf-8") as fh:
            fh.write(line)


def is_bad(workspace_path: Path | str, token_name: str) -> bool:
    """Return True if ``token_name`` is currently bad-marked.

    Uses a word-boundary regex so ``agent-1`` does not match ``agent-10``.
    Reads are unsynchronized — readers see a consistent file because writers
    only ever append whole lines.

    Reason-aware expiry (#4122 / #4212): auth-reason entries (a broken
    credential — matched by ``AUTH_REASON_RE``) block permanently. Non-auth
    ("exhaustion") entries block only until they age past
    ``exhaustion_cooldown_secs``: weekly/session/5h-limit exhaustion is
    transient, so a stale exhaustion line no longer keeps an otherwise-healthy
    account out of rotation even before ``cleanup_bad_tokens`` prunes it from
    disk — the account re-enters the pool with no operator action. A line whose
    timestamp cannot be parsed is treated as permanent (fail-closed — we never
    silently un-block a token on a malformed entry). This mirrors the Rust
    ``tokens_pool::bad_tokens::is_bad`` byte-for-byte; the live spawn path is
    this Python one, so without it a recovered account stays blocked until a
    manual ``unblock`` (the 2026-07-28 incident, #4212).

    Args:
        workspace_path: Repo root.
        token_name: Token basename to look up.
    """
    workspace_path = Path(workspace_path)
    bad_file = _bad_tokens_path(_tokens_dir(workspace_path))
    if not bad_file.is_file():
        return False
    try:
        text = bad_file.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return False

    pattern = _name_pattern(token_name)
    cooldown = exhaustion_cooldown_secs()
    now = datetime.now(timezone.utc).timestamp()
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or not pattern.search(stripped):
            continue
        # Parse `<ts> <name> <reason...>` from this matching line.
        parts = stripped.split(" ", 2)
        ts_str = parts[0] if parts else ""
        reason = parts[2] if len(parts) >= 3 else ""
        # Auth entries never expire.
        if is_auth_reason(reason):
            return True
        # Non-auth (exhaustion) entries expire after the cooldown. A
        # malformed/missing timestamp fails closed (permanent). An expired
        # entry does NOT short-circuit — a later line may still block.
        try:
            ts_epoch = datetime.strptime(ts_str, "%Y-%m-%dT%H:%M:%SZ").replace(
                tzinfo=timezone.utc,
            ).timestamp()
        except ValueError:
            return True
        if now - ts_epoch < cooldown:
            return True
    return False


def cleanup_bad_tokens(
    workspace_path: Path | str,
    max_age_seconds: int = 6 * 3600,
) -> int:
    """Drop bad_tokens entries older than ``max_age_seconds``.

    Args:
        workspace_path: Repo root.
        max_age_seconds: Cutoff age in seconds (default 6 hours).

    Returns:
        Number of entries retained after pruning.
    """
    workspace_path = Path(workspace_path)
    tokens_dir = _tokens_dir(workspace_path)
    bad_file = _bad_tokens_path(tokens_dir)
    if not bad_file.is_file():
        return 0

    cutoff_dt = datetime.now(timezone.utc).timestamp() - max_age_seconds
    kept: list[str] = []

    with _MkdirLock(_lock_path(tokens_dir)):
        try:
            lines = bad_file.read_text(encoding="utf-8").splitlines()
        except OSError:
            return 0
        for line in lines:
            stripped = line.strip()
            if not stripped:
                continue
            ts_str = stripped.split(" ", 1)[0]
            try:
                # Accept the canonical UTC format we write.
                ts_dt = datetime.strptime(ts_str, "%Y-%m-%dT%H:%M:%SZ").replace(
                    tzinfo=timezone.utc,
                )
                ts_epoch = ts_dt.timestamp()
            except ValueError:
                # Malformed line — keep it so we don't silently lose data.
                kept.append(line)
                continue
            if ts_epoch >= cutoff_dt:
                kept.append(line)

        # Atomic replacement: write to temp file then rename
        tmp = bad_file.with_suffix(bad_file.suffix + ".tmp")
        if kept:
            tmp.write_text("\n".join(kept) + "\n", encoding="utf-8")
        else:
            tmp.write_text("", encoding="utf-8")
        tmp.replace(bad_file)

    return len(kept)
