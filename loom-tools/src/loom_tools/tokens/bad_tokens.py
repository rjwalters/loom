"""Bad-token tracking with mkdir-based locking.

Tokens that fail with TOKEN_EXPIRED, TOKEN_EXHAUSTED, or otherwise prove
unusable are appended to ``.loom/tokens/.bad_tokens``. Subsequent selection
calls skip these tokens.

The file is shared across concurrent bash and Python writers. We coordinate
with a sibling ``.bad_tokens.lock`` directory, created via ``mkdir`` (POSIX
atomic). ``flock`` is intentionally not used because it is unavailable on
stock macOS.

File format (one entry per line):
    <ISO8601 UTC timestamp> <token_name> <reason words...>

Reads use a word-boundary regex so ``agent-1`` and ``agent-10`` do not
collide.
"""

from __future__ import annotations

import re
import time
from datetime import datetime, timezone
from pathlib import Path

# Lock parameters
_LOCK_TIMEOUT_SECONDS = 5.0
_LOCK_POLL_INTERVAL = 0.1
_STALE_LOCK_THRESHOLD_SECONDS = 30.0


class _MkdirLock:
    """Context manager wrapping a directory-as-lock.

    Acquires by creating ``lock_path``. Times out after _LOCK_TIMEOUT_SECONDS.
    Cleans up stale locks (older than _STALE_LOCK_THRESHOLD_SECONDS) before
    giving up.

    Always releases the lock on ``__exit__`` (via rmdir). If the lock was
    never acquired (timeout), ``__exit__`` is a no-op.
    """

    def __init__(self, lock_path: Path):
        self._lock_path = lock_path
        self._acquired = False

    def __enter__(self) -> "_MkdirLock":
        deadline = time.monotonic() + _LOCK_TIMEOUT_SECONDS
        while time.monotonic() < deadline:
            try:
                self._lock_path.mkdir(parents=False, exist_ok=False)
                self._acquired = True
                return self
            except FileExistsError:
                # Stale-lock cleanup
                try:
                    age = time.time() - self._lock_path.stat().st_mtime
                    if age > _STALE_LOCK_THRESHOLD_SECONDS:
                        try:
                            self._lock_path.rmdir()
                        except OSError:
                            pass
                except FileNotFoundError:
                    # Lock vanished between checks; loop and retry mkdir
                    continue
                time.sleep(_LOCK_POLL_INTERVAL)
        raise TimeoutError(
            f"Could not acquire bad_tokens lock at {self._lock_path} "
            f"within {_LOCK_TIMEOUT_SECONDS}s"
        )

    def __exit__(self, exc_type, exc_val, exc_tb) -> None:
        if self._acquired:
            try:
                self._lock_path.rmdir()
            except OSError:
                # Lock already gone — log-friendly silent
                pass


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
    tokens_dir = workspace_path / ".loom" / "tokens"
    if not tokens_dir.is_dir():
        raise FileNotFoundError(f"Tokens dir does not exist: {tokens_dir}")

    timestamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    safe_reason = reason.replace("\n", " ").replace("\r", " ").strip()
    line = f"{timestamp} {token_name} {safe_reason}\n"

    with _MkdirLock(_lock_path(tokens_dir)):
        with open(_bad_tokens_path(tokens_dir), "a", encoding="utf-8") as fh:
            fh.write(line)


def is_bad(workspace_path: Path | str, token_name: str) -> bool:
    """Return True if ``token_name`` appears in the bad_tokens file.

    Uses a word-boundary regex so ``agent-1`` does not match ``agent-10``.
    Reads are unsynchronized — readers see a consistent file because writers
    only ever append whole lines.

    Args:
        workspace_path: Repo root.
        token_name: Token basename to look up.
    """
    workspace_path = Path(workspace_path)
    bad_file = _bad_tokens_path(workspace_path / ".loom" / "tokens")
    if not bad_file.is_file():
        return False
    try:
        text = bad_file.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return False
    return bool(_name_pattern(token_name).search(text))


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
    tokens_dir = workspace_path / ".loom" / "tokens"
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
