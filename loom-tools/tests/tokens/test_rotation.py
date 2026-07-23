"""Tests for loom_tools.tokens.rotation (issue #3909)."""

from __future__ import annotations

import random
import threading
from pathlib import Path

from loom_tools.tokens.rotation import (
    _INIT_OFFSET_BOUND,
    _cursor_path,
    next_rotation_index,
)


def _make_tokens_dir(tmp_path: Path) -> Path:
    d = tmp_path / ".loom" / "tokens"
    d.mkdir(parents=True)
    return d


def test_modulus_one_returns_zero_and_writes_nothing(tmp_path):
    d = _make_tokens_dir(tmp_path)
    for _ in range(5):
        assert next_rotation_index(d, 1, random.Random(0)) == 0
    # No cursor file is created when there is nothing to rotate across.
    assert not _cursor_path(d).exists()


def test_modulus_zero_returns_zero(tmp_path):
    d = _make_tokens_dir(tmp_path)
    assert next_rotation_index(d, 0, random.Random(0)) == 0


def test_first_index_seeded_from_rng(tmp_path):
    d = _make_tokens_dir(tmp_path)
    modulus = 4
    # The first call initializes the cursor from rng.randrange(_INIT_OFFSET_BOUND).
    expected_offset = random.Random(123).randrange(_INIT_OFFSET_BOUND)
    idx = next_rotation_index(d, modulus, random.Random(123))
    assert idx == expected_offset % modulus


def test_consecutive_calls_round_robin(tmp_path):
    d = _make_tokens_dir(tmp_path)
    modulus = 4
    first = next_rotation_index(d, modulus, random.Random(7))
    # Subsequent calls advance by exactly one each, regardless of rng.
    seq = [first]
    for _ in range(7):
        seq.append(next_rotation_index(d, modulus, random.Random(999)))
    expected = [(first + i) % modulus for i in range(len(seq))]
    assert seq == expected
    # One full cycle (modulus calls) covers every index exactly once.
    assert set(seq[:modulus]) == set(range(modulus))


def test_first_offset_varies_across_pools(tmp_path):
    """Fresh cursors seeded from different rngs de-correlate the first pick."""
    firsts = set()
    for seed in range(20):
        d = _make_tokens_dir(tmp_path / f"pool-{seed}")
        firsts.add(next_rotation_index(d, 5, random.Random(seed)))
    # Not always the same starting index (cross-repo de-correlation).
    assert len(firsts) > 1


def test_concurrent_calls_get_distinct_indices(tmp_path):
    """Threads sharing one cursor receive distinct consecutive indices.

    With modulus >= thread count, every concurrent caller lands on a distinct
    account (one-per-account) — the core #3909 property under real concurrency.
    """
    d = _make_tokens_dir(tmp_path)
    n_threads = 8
    modulus = n_threads  # exactly the thread count => all distinct expected
    results: list[int] = []
    lock = threading.Lock()
    barrier = threading.Barrier(n_threads)

    def worker() -> None:
        barrier.wait()
        idx = next_rotation_index(d, modulus, random.Random())
        with lock:
            results.append(idx)

    threads = [threading.Thread(target=worker) for _ in range(n_threads)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    # All indices distinct and covering the full window: no two threads
    # collided on the same account.
    assert sorted(results) == list(range(modulus))


def test_corrupt_cursor_reinitializes(tmp_path):
    d = _make_tokens_dir(tmp_path)
    _cursor_path(d).write_text("not-an-int", encoding="utf-8")
    # Malformed cursor must not raise; it re-initializes cleanly.
    idx = next_rotation_index(d, 3, random.Random(0))
    assert 0 <= idx < 3
    # And the next call advances by one from the re-seeded value.
    idx2 = next_rotation_index(d, 3, random.Random(0))
    assert idx2 == (idx + 1) % 3
