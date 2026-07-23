"""Shared mkdir-based file lock for the tokens package.

Several token state files (``.bad_tokens``, the rotation cursor) are shared
across concurrent bash and Python writers. We coordinate with a sibling
``*.lock`` directory, created via ``mkdir`` (POSIX atomic). ``flock`` is
intentionally not used because it is unavailable on stock macOS.

This module is import-safe: no I/O occurs at import time.
"""

from __future__ import annotations

import time
from pathlib import Path

# Lock parameters (shared defaults; matched historic bad_tokens values).
_LOCK_TIMEOUT_SECONDS = 5.0
_LOCK_POLL_INTERVAL = 0.1
_STALE_LOCK_THRESHOLD_SECONDS = 30.0


class MkdirLock:
    """Context manager wrapping a directory-as-lock.

    Acquires by creating ``lock_path``. Times out after ``timeout`` seconds.
    Cleans up stale locks (older than ``stale_threshold`` seconds) before
    giving up.

    Always releases the lock on ``__exit__`` (via rmdir). If the lock was
    never acquired (timeout), ``__exit__`` is a no-op.
    """

    def __init__(
        self,
        lock_path: Path,
        *,
        timeout: float = _LOCK_TIMEOUT_SECONDS,
        poll_interval: float = _LOCK_POLL_INTERVAL,
        stale_threshold: float = _STALE_LOCK_THRESHOLD_SECONDS,
    ):
        self._lock_path = lock_path
        self._timeout = timeout
        self._poll_interval = poll_interval
        self._stale_threshold = stale_threshold
        self._acquired = False

    def __enter__(self) -> "MkdirLock":
        deadline = time.monotonic() + self._timeout
        while time.monotonic() < deadline:
            try:
                self._lock_path.mkdir(parents=False, exist_ok=False)
                self._acquired = True
                return self
            except FileExistsError:
                # Stale-lock cleanup
                try:
                    age = time.time() - self._lock_path.stat().st_mtime
                    if age > self._stale_threshold:
                        try:
                            self._lock_path.rmdir()
                        except OSError:
                            pass
                except FileNotFoundError:
                    # Lock vanished between checks; loop and retry mkdir
                    continue
                time.sleep(self._poll_interval)
        raise TimeoutError(
            f"Could not acquire lock at {self._lock_path} "
            f"within {self._timeout}s"
        )

    def __exit__(self, exc_type, exc_val, exc_tb) -> None:
        if self._acquired:
            try:
                self._lock_path.rmdir()
            except OSError:
                # Lock already gone — log-friendly silent
                pass
