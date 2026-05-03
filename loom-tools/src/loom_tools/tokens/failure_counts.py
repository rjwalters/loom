"""Per-account consecutive-failure counter for token rotation.

Tracks consecutive ``TOKEN_EXHAUSTED`` failures per account so the
spawn wrapper can auto-unpin a stuck pin. The counter is stored at
``.loom/tokens/.failure_counts`` as JSON::

    {"<account_name>": {"count": <int>, "last_failure": "<ISO8601 UTC>"}}

The counter is reset on:

* a successful spawn for that account (``record_success``),
* any operator allowlist mutation (``pin``, ``unpin``, ``add``, ``remove``).

Reaching the threshold (default 5) is the signal that the operator's
pin must be auto-released by the wrapper. The threshold check is
``>= 5`` — a 6th failure does not bump the count past the trigger; it
still triggers (idempotent at-or-above).

Concurrency:
    Writes are guarded by a sibling ``mkdir``-lock and use the
    "atomic rewrite" pattern (``mktemp`` + ``os.replace``). Concurrent
    bash and Python writers (e.g. spawn-claude.sh and the operator
    CLI) are coordinated via the same lock dir.

Public API:
    record_failure(workspace, name, *, threshold=5) -> int
    record_success(workspace, name) -> None
    reset_all(workspace) -> None
    get_count(workspace, name) -> int
    threshold_reached(workspace, name, *, threshold=5) -> bool
    DEFAULT_THRESHOLD
"""

from __future__ import annotations

import json
import os
import tempfile
from datetime import datetime, timezone
from pathlib import Path

# Reuse the lock primitive from bad_tokens (mkdir-only, macOS-safe).
from loom_tools.tokens.bad_tokens import _MkdirLock

DEFAULT_THRESHOLD = 5


def _tokens_dir(workspace: Path | str) -> Path:
    return Path(workspace) / ".loom" / "tokens"


def _state_path(workspace: Path | str) -> Path:
    return _tokens_dir(workspace) / ".failure_counts"


def _lock_path(workspace: Path | str) -> Path:
    return _tokens_dir(workspace) / ".failure_counts.lock"


def _ensure_dir(workspace: Path | str) -> Path:
    d = _tokens_dir(workspace)
    d.mkdir(parents=True, exist_ok=True)
    return d


def _read_state(path: Path) -> dict[str, dict[str, object]]:
    """Read the JSON state file, returning ``{}`` on any failure.

    A malformed or unreadable file is treated as an empty state — better
    to lose history than to refuse to record new failures.
    """
    try:
        raw = path.read_text(encoding="utf-8")
    except (FileNotFoundError, OSError):
        return {}
    if not raw.strip():
        return {}
    try:
        loaded = json.loads(raw)
    except json.JSONDecodeError:
        return {}
    if not isinstance(loaded, dict):
        return {}
    # Filter out malformed entries defensively
    cleaned: dict[str, dict[str, object]] = {}
    for k, v in loaded.items():
        if not isinstance(k, str) or not isinstance(v, dict):
            continue
        count = v.get("count", 0)
        if not isinstance(count, int) or count < 0:
            continue
        last = v.get("last_failure", "")
        if not isinstance(last, str):
            last = ""
        cleaned[k] = {"count": count, "last_failure": last}
    return cleaned


def _write_state_atomic(path: Path, state: dict[str, dict[str, object]]) -> None:
    """Write state via mktemp + os.replace (atomic on POSIX)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_str = tempfile.mkstemp(
        prefix=path.name + ".",
        suffix=".tmp",
        dir=str(path.parent),
    )
    tmp = Path(tmp_str)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            json.dump(state, fh, indent=2, sort_keys=True)
            fh.write("\n")
        os.replace(tmp, path)
    except Exception:
        try:
            tmp.unlink(missing_ok=True)
        except OSError:
            pass
        raise


def _now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def record_failure(
    workspace: Path | str,
    name: str,
    *,
    threshold: int = DEFAULT_THRESHOLD,
) -> int:
    """Increment the consecutive-failure counter for ``name``.

    Args:
        workspace: Repo root containing ``.loom/tokens/``.
        name: Account name (token file stem).
        threshold: Counter is capped at ``threshold`` to avoid unbounded
            growth (we only ever check ``>=``, so capping is harmless).

    Returns:
        The new count (post-increment), capped at ``threshold``.
    """
    _ensure_dir(workspace)
    path = _state_path(workspace)
    with _MkdirLock(_lock_path(workspace)):
        state = _read_state(path)
        entry = state.get(name) or {"count": 0, "last_failure": ""}
        new_count = min(int(entry.get("count", 0)) + 1, threshold)
        state[name] = {"count": new_count, "last_failure": _now_iso()}
        _write_state_atomic(path, state)
        return new_count


def record_success(workspace: Path | str, name: str) -> None:
    """Reset the counter for ``name`` (drops the key entirely).

    Args:
        workspace: Repo root containing ``.loom/tokens/``.
        name: Account name (token file stem).

    No-op if the state file is missing or the key is absent.
    """
    path = _state_path(workspace)
    if not path.is_file():
        return
    with _MkdirLock(_lock_path(workspace)):
        state = _read_state(path)
        if name not in state:
            return
        del state[name]
        if state:
            _write_state_atomic(path, state)
        else:
            try:
                path.unlink()
            except FileNotFoundError:
                pass


def reset_all(workspace: Path | str) -> None:
    """Drop all per-account counters.

    Called from the allowlist CLI after any mutation (``pin``,
    ``unpin``, ``add``, ``remove``) so a fresh allowlist starts with
    a clean slate. No-op if the state file is missing.
    """
    path = _state_path(workspace)
    if not path.is_file():
        return
    with _MkdirLock(_lock_path(workspace)):
        try:
            path.unlink()
        except FileNotFoundError:
            pass


def get_count(workspace: Path | str, name: str) -> int:
    """Return the current consecutive-failure count for ``name`` (0 if absent)."""
    path = _state_path(workspace)
    if not path.is_file():
        return 0
    state = _read_state(path)
    entry = state.get(name)
    if not entry:
        return 0
    count = entry.get("count", 0)
    return int(count) if isinstance(count, int) else 0


def threshold_reached(
    workspace: Path | str,
    name: str,
    *,
    threshold: int = DEFAULT_THRESHOLD,
) -> bool:
    """Return True iff the counter for ``name`` is ``>= threshold``."""
    return get_count(workspace, name) >= threshold


def all_counts(workspace: Path | str) -> dict[str, dict[str, object]]:
    """Return the entire state dict (for status / debugging).

    The returned dict is a snapshot — mutating it does not affect the
    persisted state.
    """
    path = _state_path(workspace)
    if not path.is_file():
        return {}
    return _read_state(path)
