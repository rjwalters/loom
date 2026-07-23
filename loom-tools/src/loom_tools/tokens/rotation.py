"""Rotation cursor for one-per-account distribution of concurrent dispatches.

Issue #3909: the ranked selection tier used to pick the single best-ranked
(or a random top-N) account, so a burst of N concurrent dispatches stacked all
N onto one account — exhausting its 5h limit while others sat idle. This module
provides a persistent, concurrency-safe *rotation cursor* so that consecutive
selections (whether sequential or concurrent) round-robin across the available
accounts one-per-account, in rotating order, until they wrap.

Mechanism: a monotonic integer counter is stored in ``.loom/tokens/.rotation_cursor``.
Each ``next_rotation_index`` call, under a mkdir-based lock (POSIX atomic;
``flock`` is unavailable on stock macOS — see ``_locking.MkdirLock``):

    1. reads the current counter (initializing it to a *random* offset the
       first time, so sibling daemons in different repos that share an account
       pool but not this cursor file start de-correlated rather than all
       colliding on index 0),
    2. computes ``counter % modulus`` as the index to return,
    3. writes ``counter + 1`` back.

Because concurrent callers each acquire the lock and take the next distinct
counter value, N concurrent selections over M available accounts receive N
consecutive counter values → ``min(N, M)`` distinct indices, wrapping past M.
Using a monotonic counter modulo the *current* modulus means the round-robin
adapts automatically as accounts drop in/out of the available set.

This module is import-safe: no I/O occurs at import time.
"""

from __future__ import annotations

import random
from pathlib import Path

from loom_tools.tokens._locking import MkdirLock

# Keep the initial random offset well within a range that never overflows a
# text int and stays comfortably above any realistic account count so the
# first modulo is uniformly distributed across accounts.
_INIT_OFFSET_BOUND = 1 << 30


def _cursor_path(tokens_dir: Path) -> Path:
    return tokens_dir / ".rotation_cursor"


def _lock_path(tokens_dir: Path) -> Path:
    return tokens_dir / ".rotation_cursor.lock"


def _read_counter(cursor_file: Path) -> int | None:
    """Read the stored monotonic counter, or ``None`` if missing/malformed."""
    try:
        raw = cursor_file.read_text(encoding="utf-8").strip()
    except OSError:
        return None
    if not raw:
        return None
    try:
        return int(raw)
    except ValueError:
        return None


def next_rotation_index(
    tokens_dir: Path,
    modulus: int,
    rng: random.Random,
) -> int:
    """Return the next round-robin index in ``[0, modulus)`` and advance the cursor.

    Args:
        tokens_dir: The ``.loom/tokens/`` directory holding the cursor file.
        modulus: Number of available accounts to rotate across (must be >= 1).
        rng: Random source used only to seed the cursor's initial offset the
            first time (for cross-repo de-correlation). Subsequent calls are
            deterministic round-robin regardless of ``rng``.

    The read-compute-write cycle is guarded by a mkdir-based lock so concurrent
    callers receive distinct consecutive counter values (one-per-account).

    On any lock-timeout or I/O failure this degrades gracefully to a random
    index rather than raising — selection must never hard-fail on cursor
    bookkeeping while accounts remain available (preserves the #3907 floor).
    """
    if modulus <= 1:
        # A single (or empty) window has nothing to rotate across.
        return 0

    cursor_file = _cursor_path(tokens_dir)
    try:
        with MkdirLock(_lock_path(tokens_dir)):
            counter = _read_counter(cursor_file)
            if counter is None:
                # First use (or a corrupt cursor): start at a random offset so
                # sibling spawners that don't share this file don't all begin
                # at index 0. Subsequent calls advance deterministically.
                counter = rng.randrange(_INIT_OFFSET_BOUND)
            index = counter % modulus
            try:
                cursor_file.write_text(str(counter + 1), encoding="utf-8")
            except OSError:
                # Best-effort persistence; still return a valid index.
                pass
            return index
    except (TimeoutError, OSError):
        # Never wedge selection on cursor bookkeeping — spread randomly.
        return rng.randrange(modulus)
