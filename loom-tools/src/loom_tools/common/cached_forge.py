"""Caching decorator for ForgeClient implementations.

``CachedForgeClient`` wraps any :class:`ForgeClient` and adds TTL-based
LRU caching for read-only methods.  Mutation methods bypass the cache
and trigger targeted invalidation of related cached entries.

This replaces the CLI-level ``gh-cached`` script with a forge-neutral
solution that works identically for GitHub and Gitea backends.

Cache storage is file-backed in ``/tmp/forge-cache/`` (configurable via
``FORGE_CACHE_DIR``) for cross-process sharing, matching the strategy of
the legacy ``gh-cached`` script.

Environment variables
---------------------
``FORGE_CACHE_DIR``
    Cache directory (default ``/tmp/forge-cache``).
``FORGE_CACHE_TTL``
    Default TTL in seconds (default ``30``).
``FORGE_CACHE_MAX_SIZE``
    Maximum cached entries (default ``256``).
``FORGE_CACHE_DISABLE``
    Set to ``"1"`` to disable caching entirely.
``FORGE_CACHE_DEBUG``
    Set to ``"1"`` for debug logging to stderr.
"""

from __future__ import annotations

import hashlib
import json
import logging
import os
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Sequence

from loom_tools.common.forge import (
    EntityType,
    ForgeCIStatus,
    ForgeClient,
    ForgeIssue,
    ForgePullRequest,
)

logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

CACHE_DIR = os.environ.get("FORGE_CACHE_DIR", "/tmp/forge-cache")
DEFAULT_TTL = int(os.environ.get("FORGE_CACHE_TTL", "30"))
MAX_CACHE_SIZE = int(os.environ.get("FORGE_CACHE_MAX_SIZE", "256"))
CACHE_DISABLED = os.environ.get("FORGE_CACHE_DISABLE", "") == "1"
DEBUG = os.environ.get("FORGE_CACHE_DEBUG", "") == "1"

# TTL overrides by method name (seconds)
TTL_BY_METHOD: dict[str, int] = {
    "get_issue": 30,
    "list_issues": 30,
    "get_pull_request": 30,
    "list_pull_requests": 30,
    "get_pull_request_reviews": 30,
    "get_default_branch_ci_status": 30,
    "get_repo_nwo": 300,
    "get_repo_default_branch": 300,
    "get_issues_batch": 30,
    "find_pull_request_for_issue": 30,
}

# Declarative mapping: mutation method -> list of read methods to invalidate.
# When a mutation succeeds, cached entries from the listed read methods that
# reference the same entity are invalidated.
INVALIDATION_MAP: dict[str, list[str]] = {
    "create_issue": ["list_issues", "get_issues_batch"],
    "close_issue": ["get_issue", "list_issues", "get_issues_batch"],
    "comment_on_issue": ["get_issue"],
    "create_pull_request": [
        "list_pull_requests",
        "find_pull_request_for_issue",
    ],
    "close_pull_request": [
        "get_pull_request",
        "list_pull_requests",
        "find_pull_request_for_issue",
    ],
    "merge_pull_request": [
        "get_pull_request",
        "list_pull_requests",
        "find_pull_request_for_issue",
    ],
    "comment_on_pull_request": ["get_pull_request"],
    "add_labels": [
        "get_issue",
        "get_pull_request",
        "list_issues",
        "list_pull_requests",
        "get_issues_batch",
    ],
    "remove_labels": [
        "get_issue",
        "get_pull_request",
        "list_issues",
        "list_pull_requests",
        "get_issues_batch",
    ],
    "transition_labels": [
        "get_issue",
        "get_pull_request",
        "list_issues",
        "list_pull_requests",
        "get_issues_batch",
    ],
}

# Read-only methods that are safe to cache
CACHEABLE_METHODS = frozenset(TTL_BY_METHOD.keys())


# ---------------------------------------------------------------------------
# Cache statistics
# ---------------------------------------------------------------------------


@dataclass
class CacheStats:
    """Tracks cache performance counters."""

    hits: int = 0
    misses: int = 0
    bypasses: int = 0
    invalidations: int = 0

    def to_dict(self) -> dict[str, int]:
        return {
            "hits": self.hits,
            "misses": self.misses,
            "bypasses": self.bypasses,
            "invalidations": self.invalidations,
        }

    @property
    def hit_rate(self) -> float:
        total = self.hits + self.misses
        return (self.hits / total * 100) if total > 0 else 0.0


# ---------------------------------------------------------------------------
# File-backed cache implementation
# ---------------------------------------------------------------------------


def _debug(msg: str) -> None:
    if DEBUG:
        print(f"[forge-cache] {msg}", file=sys.stderr)


def _ensure_cache_dir(cache_dir: str) -> None:
    os.makedirs(cache_dir, mode=0o700, exist_ok=True)


def _cache_key(method: str, forge_type: str, args_repr: str) -> str:
    """Generate a semantic cache key.

    The key encodes the method name, forge type, and a hash of the
    arguments so that cache entries are human-debuggable while keeping
    filenames short.
    """
    raw = f"{method}:{forge_type}:{args_repr}"
    h = hashlib.sha256(raw.encode()).hexdigest()[:16]
    return f"{method}_{h}"


def _cache_path(cache_dir: str, key: str) -> str:
    return os.path.join(cache_dir, f"{key}.json")


def _cache_get(cache_dir: str, key: str) -> Any | None:
    """Read a cached entry.  Returns the stored value or ``None`` on miss."""
    path = _cache_path(cache_dir, key)
    try:
        with open(path) as f:
            entry = json.load(f)
        if time.time() - entry["time"] > entry["ttl"]:
            _debug(f"EXPIRED key={key}")
            os.unlink(path)
            return None
        # Update access time for LRU
        entry["accessed"] = time.time()
        with open(path, "w") as f:
            json.dump(entry, f)
        return entry["value"]
    except (FileNotFoundError, json.JSONDecodeError, KeyError):
        return None


# Sentinel to distinguish "not in cache" from "cached value is None"
_MISSING = object()


def _cache_get_or_missing(cache_dir: str, key: str) -> Any:
    """Like ``_cache_get`` but returns ``_MISSING`` sentinel on miss."""
    path = _cache_path(cache_dir, key)
    try:
        with open(path) as f:
            entry = json.load(f)
        if time.time() - entry["time"] > entry["ttl"]:
            _debug(f"EXPIRED key={key}")
            os.unlink(path)
            return _MISSING
        entry["accessed"] = time.time()
        with open(path, "w") as f:
            json.dump(entry, f)
        return entry["value"]
    except (FileNotFoundError, json.JSONDecodeError, KeyError):
        return _MISSING


def _cache_put(
    cache_dir: str,
    key: str,
    value: Any,
    ttl: int,
    method: str,
    entity_id: str | None = None,
    max_size: int = MAX_CACHE_SIZE,
) -> None:
    """Write a cache entry."""
    _ensure_cache_dir(cache_dir)
    _enforce_max_size(cache_dir, max_size)
    entry = {
        "time": time.time(),
        "accessed": time.time(),
        "ttl": ttl,
        "value": value,
        "method": method,
        "entity_id": entity_id,
    }
    path = _cache_path(cache_dir, key)
    try:
        with open(path, "w") as f:
            json.dump(entry, f)
    except OSError:
        pass


def _enforce_max_size(cache_dir: str, max_size: int = MAX_CACHE_SIZE) -> None:
    """Evict least-recently-accessed entries if cache exceeds max size."""
    try:
        entries: list[tuple[float, str]] = []
        for name in os.listdir(cache_dir):
            if name.startswith("_") or not name.endswith(".json"):
                continue
            path = os.path.join(cache_dir, name)
            try:
                with open(path) as f:
                    data = json.load(f)
                entries.append((data.get("accessed", 0), path))
            except (json.JSONDecodeError, OSError):
                # Corrupted -- remove
                try:
                    os.unlink(path)
                except OSError:
                    pass

        if len(entries) <= max_size:
            return

        entries.sort(key=lambda x: x[0])
        evict_count = len(entries) - max_size
        for _, path in entries[:evict_count]:
            _debug(f"EVICT {os.path.basename(path)}")
            try:
                os.unlink(path)
            except OSError:
                pass
    except OSError:
        pass


def _invalidate_by_methods(
    cache_dir: str, methods: Sequence[str], entity_id: str | None = None,
) -> int:
    """Invalidate cached entries matching the given method names.

    If *entity_id* is provided, only entries whose stored ``entity_id``
    matches are invalidated.  If *entity_id* is ``None``, all entries
    for the listed methods are invalidated.

    Returns the count of invalidated entries.
    """
    count = 0
    try:
        for name in os.listdir(cache_dir):
            if name.startswith("_") or not name.endswith(".json"):
                continue
            path = os.path.join(cache_dir, name)
            try:
                with open(path) as f:
                    data = json.load(f)
                stored_method = data.get("method", "")
                if stored_method not in methods:
                    continue
                # If we have a specific entity_id, only invalidate matching entries
                if entity_id is not None:
                    stored_entity = data.get("entity_id")
                    if stored_entity is not None and stored_entity != entity_id:
                        continue
                _debug(f"INVALIDATE {name} (method={stored_method})")
                os.unlink(path)
                count += 1
            except (json.JSONDecodeError, OSError):
                try:
                    os.unlink(path)
                except OSError:
                    pass
    except OSError:
        pass
    return count


def _clear_cache(cache_dir: str) -> None:
    """Remove all cached entries."""
    try:
        for name in os.listdir(cache_dir):
            if name.startswith("_"):
                continue
            path = os.path.join(cache_dir, name)
            try:
                os.unlink(path)
            except OSError:
                pass
    except OSError:
        pass


def _serialize_value(value: Any) -> Any:
    """Convert forge dataclasses to JSON-serializable dicts."""
    if isinstance(value, (ForgeIssue, ForgePullRequest, ForgeCIStatus)):
        d: dict[str, Any] = {}
        for k in value.__dataclass_fields__:
            d[k] = getattr(value, k)
        return {"__type__": type(value).__name__, **d}
    if isinstance(value, list):
        return [_serialize_value(v) for v in value]
    if isinstance(value, dict):
        return {k: _serialize_value(v) for k, v in value.items()}
    return value


def _deserialize_value(value: Any) -> Any:
    """Restore forge dataclasses from their serialized form."""
    if isinstance(value, dict) and "__type__" in value:
        type_name = value["__type__"]
        data = {k: v for k, v in value.items() if k != "__type__"}
        if type_name == "ForgeIssue":
            return ForgeIssue(**data)
        if type_name == "ForgePullRequest":
            return ForgePullRequest(**data)
        if type_name == "ForgeCIStatus":
            return ForgeCIStatus(**data)
        return data
    if isinstance(value, list):
        return [_deserialize_value(v) for v in value]
    if isinstance(value, dict):
        return {k: _deserialize_value(v) for k, v in value.items()}
    return value


# ---------------------------------------------------------------------------
# CachedForgeClient
# ---------------------------------------------------------------------------


class CachedForgeClient:
    """Caching decorator over any :class:`ForgeClient`.

    Read-only methods are cached with TTL-based expiry and LRU eviction.
    Mutation methods bypass the cache and invalidate related entries
    according to the declarative :data:`INVALIDATION_MAP`.

    Usage::

        from loom_tools.common.forge import get_forge
        from loom_tools.common.cached_forge import CachedForgeClient

        forge = CachedForgeClient(get_forge())
    """

    def __init__(
        self,
        inner: ForgeClient,
        *,
        cache_dir: str | None = None,
        default_ttl: int | None = None,
        max_size: int | None = None,
        disabled: bool | None = None,
    ) -> None:
        self._inner = inner
        self._cache_dir = cache_dir or CACHE_DIR
        self._default_ttl = default_ttl if default_ttl is not None else DEFAULT_TTL
        self._max_size = max_size if max_size is not None else MAX_CACHE_SIZE
        self._disabled = disabled if disabled is not None else CACHE_DISABLED
        self._stats = CacheStats()

    # --- Expose inner client and stats ---

    @property
    def inner(self) -> ForgeClient:
        """The wrapped ``ForgeClient``."""
        return self._inner

    @property
    def stats(self) -> CacheStats:
        """Cache performance statistics."""
        return self._stats

    # --- ForgeClient.forge_type ---

    @property
    def forge_type(self) -> str:
        return self._inner.forge_type

    # --- Internal helpers ---

    def _args_repr(self, *args: Any, **kwargs: Any) -> str:
        """Build a stable string representation of call arguments."""
        parts = [repr(a) for a in args]
        parts.extend(f"{k}={v!r}" for k, v in sorted(kwargs.items()))
        return ",".join(parts)

    def _get_cached(self, method: str, args_repr: str) -> Any:
        """Try to get a cached value.  Returns ``_MISSING`` on miss."""
        if self._disabled:
            self._stats.bypasses += 1
            return _MISSING
        key = _cache_key(method, self._inner.forge_type, args_repr)
        result = _cache_get_or_missing(self._cache_dir, key)
        if result is _MISSING:
            _debug(f"MISS {method} args={args_repr}")
            self._stats.misses += 1
        else:
            _debug(f"HIT {method} args={args_repr}")
            self._stats.hits += 1
            result = _deserialize_value(result)
        return result

    def _put_cached(
        self,
        method: str,
        args_repr: str,
        value: Any,
        entity_id: str | None = None,
    ) -> None:
        """Store a value in the cache."""
        if self._disabled:
            return
        key = _cache_key(method, self._inner.forge_type, args_repr)
        ttl = TTL_BY_METHOD.get(method, self._default_ttl)
        serialized = _serialize_value(value)
        _cache_put(
            self._cache_dir, key, serialized, ttl, method,
            entity_id=entity_id,
            max_size=self._max_size,
        )

    def _invalidate(self, mutation_method: str, entity_id: str | None = None) -> None:
        """Invalidate cache entries affected by a mutation."""
        if self._disabled:
            return
        methods = INVALIDATION_MAP.get(mutation_method, [])
        if not methods:
            return
        count = _invalidate_by_methods(self._cache_dir, methods, entity_id=entity_id)
        self._stats.invalidations += count
        if count:
            _debug(f"INVALIDATED {count} entries for {mutation_method}")

    # --- Issue operations (cached reads) ---

    def get_issue(self, number: int) -> ForgeIssue | None:
        ar = self._args_repr(number)
        cached = self._get_cached("get_issue", ar)
        if cached is not _MISSING:
            return cached
        result = self._inner.get_issue(number)
        self._put_cached("get_issue", ar, result, entity_id=str(number))
        return result

    def list_issues(
        self,
        *,
        labels: Sequence[str] | None = None,
        state: str = "open",
        limit: int | None = None,
    ) -> list[ForgeIssue]:
        ar = self._args_repr(labels=labels, state=state, limit=limit)
        cached = self._get_cached("list_issues", ar)
        if cached is not _MISSING:
            return cached
        result = self._inner.list_issues(labels=labels, state=state, limit=limit)
        self._put_cached("list_issues", ar, result)
        return result

    def create_issue(
        self,
        title: str,
        body: str,
        labels: Sequence[str] | None = None,
    ) -> ForgeIssue | None:
        result = self._inner.create_issue(title, body, labels=labels)
        if result is not None:
            self._invalidate("create_issue")
        return result

    def close_issue(self, number: int) -> bool:
        result = self._inner.close_issue(number)
        if result:
            self._invalidate("close_issue", entity_id=str(number))
        return result

    def comment_on_issue(self, number: int, body: str) -> bool:
        result = self._inner.comment_on_issue(number, body)
        if result:
            self._invalidate("comment_on_issue", entity_id=str(number))
        return result

    # --- Pull request operations ---

    def get_pull_request(self, number: int) -> ForgePullRequest | None:
        ar = self._args_repr(number)
        cached = self._get_cached("get_pull_request", ar)
        if cached is not _MISSING:
            return cached
        result = self._inner.get_pull_request(number)
        self._put_cached("get_pull_request", ar, result, entity_id=str(number))
        return result

    def list_pull_requests(
        self,
        *,
        labels: Sequence[str] | None = None,
        state: str = "open",
        head: str | None = None,
        search: str | None = None,
        limit: int | None = None,
    ) -> list[ForgePullRequest]:
        ar = self._args_repr(
            labels=labels, state=state, head=head, search=search, limit=limit,
        )
        cached = self._get_cached("list_pull_requests", ar)
        if cached is not _MISSING:
            return cached
        result = self._inner.list_pull_requests(
            labels=labels, state=state, head=head, search=search, limit=limit,
        )
        self._put_cached("list_pull_requests", ar, result)
        return result

    def create_pull_request(
        self,
        title: str,
        body: str,
        head: str,
        base: str | None = None,
        labels: Sequence[str] | None = None,
    ) -> ForgePullRequest | None:
        result = self._inner.create_pull_request(
            title, body, head, base=base, labels=labels,
        )
        if result is not None:
            self._invalidate("create_pull_request")
        return result

    def close_pull_request(
        self, number: int, comment: str | None = None,
    ) -> bool:
        result = self._inner.close_pull_request(number, comment=comment)
        if result:
            self._invalidate("close_pull_request", entity_id=str(number))
        return result

    def merge_pull_request(
        self, number: int, method: str = "squash",
    ) -> bool:
        result = self._inner.merge_pull_request(number, method=method)
        if result:
            self._invalidate("merge_pull_request", entity_id=str(number))
        return result

    def comment_on_pull_request(self, number: int, body: str) -> bool:
        result = self._inner.comment_on_pull_request(number, body)
        if result:
            self._invalidate("comment_on_pull_request", entity_id=str(number))
        return result

    def get_pull_request_reviews(
        self, number: int,
    ) -> list[dict[str, Any]]:
        ar = self._args_repr(number)
        cached = self._get_cached("get_pull_request_reviews", ar)
        if cached is not _MISSING:
            return cached
        result = self._inner.get_pull_request_reviews(number)
        self._put_cached(
            "get_pull_request_reviews", ar, result, entity_id=str(number),
        )
        return result

    # --- Label operations (mutations) ---

    def add_labels(
        self, entity_type: EntityType, number: int, labels: Sequence[str],
    ) -> bool:
        result = self._inner.add_labels(entity_type, number, labels)
        if result:
            self._invalidate("add_labels", entity_id=str(number))
        return result

    def remove_labels(
        self, entity_type: EntityType, number: int, labels: Sequence[str],
    ) -> bool:
        result = self._inner.remove_labels(entity_type, number, labels)
        if result:
            self._invalidate("remove_labels", entity_id=str(number))
        return result

    def transition_labels(
        self,
        entity_type: EntityType,
        number: int,
        add: Sequence[str] | None = None,
        remove: Sequence[str] | None = None,
    ) -> bool:
        result = self._inner.transition_labels(
            entity_type, number, add=add, remove=remove,
        )
        if result:
            self._invalidate("transition_labels", entity_id=str(number))
        return result

    # --- CI status (cached) ---

    def get_default_branch_ci_status(self) -> ForgeCIStatus:
        ar = self._args_repr()
        cached = self._get_cached("get_default_branch_ci_status", ar)
        if cached is not _MISSING:
            return cached
        result = self._inner.get_default_branch_ci_status()
        self._put_cached("get_default_branch_ci_status", ar, result)
        return result

    # --- Repository metadata (cached, long TTL) ---

    def get_repo_nwo(self) -> str | None:
        ar = self._args_repr()
        cached = self._get_cached("get_repo_nwo", ar)
        if cached is not _MISSING:
            return cached
        result = self._inner.get_repo_nwo()
        self._put_cached("get_repo_nwo", ar, result)
        return result

    def get_repo_default_branch(self) -> str | None:
        ar = self._args_repr()
        cached = self._get_cached("get_repo_default_branch", ar)
        if cached is not _MISSING:
            return cached
        result = self._inner.get_repo_default_branch()
        self._put_cached("get_repo_default_branch", ar, result)
        return result

    # --- Batch operations (cached) ---

    def get_issues_batch(
        self, numbers: Sequence[int],
    ) -> dict[int, ForgeIssue | None]:
        ar = self._args_repr(tuple(sorted(numbers)))
        cached = self._get_cached("get_issues_batch", ar)
        if cached is not _MISSING:
            # Deserialized dict may have string keys from JSON
            if isinstance(cached, dict):
                return {int(k): v for k, v in cached.items()}
            return cached
        result = self._inner.get_issues_batch(numbers)
        self._put_cached("get_issues_batch", ar, result)
        return result

    def find_pull_request_for_issue(
        self, issue: int, state: str = "open",
    ) -> int | None:
        ar = self._args_repr(issue, state=state)
        cached = self._get_cached("find_pull_request_for_issue", ar)
        if cached is not _MISSING:
            return cached
        result = self._inner.find_pull_request_for_issue(issue, state=state)
        self._put_cached(
            "find_pull_request_for_issue", ar, result, entity_id=str(issue),
        )
        return result

    # --- Cache management ---

    def clear_cache(self) -> None:
        """Clear all cached entries."""
        _clear_cache(self._cache_dir)

    def reset_stats(self) -> None:
        """Reset cache statistics."""
        self._stats = CacheStats()
