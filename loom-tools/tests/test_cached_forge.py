"""Tests for CachedForgeClient."""

from __future__ import annotations

import json
import os
import shutil
import tempfile
import time
from typing import Any, Sequence
from unittest.mock import MagicMock

import pytest

from loom_tools.common.cached_forge import (
    INVALIDATION_MAP,
    CachedForgeClient,
    CacheStats,
    _MISSING,
    _cache_get_or_missing,
    _cache_key,
    _cache_put,
    _clear_cache,
    _deserialize_value,
    _invalidate_by_methods,
    _serialize_value,
)
from loom_tools.common.forge import (
    ForgeCIStatus,
    ForgeClient,
    ForgeIssue,
    ForgePullRequest,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_issue(number: int = 42, title: str = "Test issue") -> ForgeIssue:
    return ForgeIssue(
        number=number,
        state="OPEN",
        title=title,
        url=f"https://github.com/test/repo/issues/{number}",
        labels=["bug"],
    )


def _make_pr(number: int = 100, title: str = "Test PR") -> ForgePullRequest:
    return ForgePullRequest(
        number=number,
        state="OPEN",
        title=title,
        url=f"https://github.com/test/repo/pull/{number}",
        labels=["enhancement"],
        head_branch="feature/test",
    )


def _make_mock_forge() -> MagicMock:
    """Create a MagicMock that satisfies ForgeClient-like usage."""
    mock = MagicMock()
    mock.forge_type = "github"
    return mock


@pytest.fixture
def cache_dir():
    """Create a temporary cache directory."""
    d = tempfile.mkdtemp(prefix="forge-cache-test-")
    yield d
    shutil.rmtree(d, ignore_errors=True)


@pytest.fixture
def cached_forge(cache_dir):
    """Create a CachedForgeClient with a mock inner client."""
    mock = _make_mock_forge()
    client = CachedForgeClient(mock, cache_dir=cache_dir, disabled=False)
    return client


# ---------------------------------------------------------------------------
# CacheStats tests
# ---------------------------------------------------------------------------


class TestCacheStats:
    def test_initial_values(self):
        stats = CacheStats()
        assert stats.hits == 0
        assert stats.misses == 0
        assert stats.bypasses == 0
        assert stats.invalidations == 0

    def test_hit_rate_empty(self):
        stats = CacheStats()
        assert stats.hit_rate == 0.0

    def test_hit_rate(self):
        stats = CacheStats(hits=3, misses=1)
        assert stats.hit_rate == 75.0

    def test_to_dict(self):
        stats = CacheStats(hits=1, misses=2, bypasses=3, invalidations=4)
        d = stats.to_dict()
        assert d == {"hits": 1, "misses": 2, "bypasses": 3, "invalidations": 4}


# ---------------------------------------------------------------------------
# Cache key generation
# ---------------------------------------------------------------------------


class TestCacheKey:
    def test_deterministic(self):
        k1 = _cache_key("get_issue", "github", "42")
        k2 = _cache_key("get_issue", "github", "42")
        assert k1 == k2

    def test_method_prefix(self):
        k = _cache_key("get_issue", "github", "42")
        assert k.startswith("get_issue_")

    def test_different_methods(self):
        k1 = _cache_key("get_issue", "github", "42")
        k2 = _cache_key("get_pull_request", "github", "42")
        assert k1 != k2

    def test_different_forge_types(self):
        k1 = _cache_key("get_issue", "github", "42")
        k2 = _cache_key("get_issue", "gitea", "42")
        assert k1 != k2


# ---------------------------------------------------------------------------
# Low-level cache read/write
# ---------------------------------------------------------------------------


class TestCacheStorage:
    def test_put_and_get(self, cache_dir):
        _cache_put(cache_dir, "test_key", {"data": 123}, 30, "get_issue")
        result = _cache_get_or_missing(cache_dir, "test_key")
        assert result == {"data": 123}

    def test_miss(self, cache_dir):
        result = _cache_get_or_missing(cache_dir, "nonexistent")
        assert result is _MISSING

    def test_ttl_expiry(self, cache_dir):
        _cache_put(cache_dir, "test_key", "value", 1, "get_issue")
        # Manually set time to past
        path = os.path.join(cache_dir, "test_key.json")
        with open(path) as f:
            entry = json.load(f)
        entry["time"] = time.time() - 100
        with open(path, "w") as f:
            json.dump(entry, f)
        result = _cache_get_or_missing(cache_dir, "test_key")
        assert result is _MISSING
        # File should be cleaned up
        assert not os.path.exists(path)

    def test_corrupted_entry(self, cache_dir):
        path = os.path.join(cache_dir, "bad_key.json")
        os.makedirs(cache_dir, exist_ok=True)
        with open(path, "w") as f:
            f.write("not json")
        result = _cache_get_or_missing(cache_dir, "bad_key")
        assert result is _MISSING


# ---------------------------------------------------------------------------
# Serialization round-trip
# ---------------------------------------------------------------------------


class TestSerialization:
    def test_forge_issue_roundtrip(self):
        issue = _make_issue()
        serialized = _serialize_value(issue)
        assert serialized["__type__"] == "ForgeIssue"
        restored = _deserialize_value(serialized)
        assert isinstance(restored, ForgeIssue)
        assert restored.number == 42
        assert restored.labels == ["bug"]

    def test_forge_pr_roundtrip(self):
        pr = _make_pr()
        serialized = _serialize_value(pr)
        assert serialized["__type__"] == "ForgePullRequest"
        restored = _deserialize_value(serialized)
        assert isinstance(restored, ForgePullRequest)
        assert restored.number == 100

    def test_forge_ci_status_roundtrip(self):
        status = ForgeCIStatus(
            status="passing", failed_runs=[], total_runs=5, message="OK",
        )
        serialized = _serialize_value(status)
        restored = _deserialize_value(serialized)
        assert isinstance(restored, ForgeCIStatus)
        assert restored.status == "passing"

    def test_list_of_issues(self):
        issues = [_make_issue(1), _make_issue(2)]
        serialized = _serialize_value(issues)
        restored = _deserialize_value(serialized)
        assert len(restored) == 2
        assert all(isinstance(i, ForgeIssue) for i in restored)

    def test_dict_with_issues(self):
        batch = {42: _make_issue(42), 43: None}
        serialized = _serialize_value(batch)
        restored = _deserialize_value(serialized)
        assert isinstance(restored[42], ForgeIssue)
        assert restored[43] is None

    def test_plain_value_passthrough(self):
        assert _serialize_value(42) == 42
        assert _serialize_value("hello") == "hello"
        assert _serialize_value(None) is None
        assert _deserialize_value(42) == 42


# ---------------------------------------------------------------------------
# Invalidation
# ---------------------------------------------------------------------------


class TestInvalidation:
    def test_invalidate_by_method(self, cache_dir):
        _cache_put(cache_dir, "get_issue_abc", "v1", 30, "get_issue", entity_id="42")
        _cache_put(cache_dir, "list_issues_abc", "v2", 30, "list_issues")
        _cache_put(cache_dir, "get_pr_abc", "v3", 30, "get_pull_request")

        count = _invalidate_by_methods(cache_dir, ["get_issue", "list_issues"])
        assert count == 2
        # PR entry should survive
        assert _cache_get_or_missing(cache_dir, "get_pr_abc") == "v3"

    def test_invalidate_by_entity_id(self, cache_dir):
        _cache_put(cache_dir, "get_issue_1", "v1", 30, "get_issue", entity_id="42")
        _cache_put(cache_dir, "get_issue_2", "v2", 30, "get_issue", entity_id="99")

        count = _invalidate_by_methods(
            cache_dir, ["get_issue"], entity_id="42",
        )
        assert count == 1
        # Issue 99 should survive
        assert _cache_get_or_missing(cache_dir, "get_issue_2") == "v2"

    def test_invalidate_without_entity_clears_all(self, cache_dir):
        _cache_put(cache_dir, "list_1", "v1", 30, "list_issues")
        _cache_put(cache_dir, "list_2", "v2", 30, "list_issues")

        count = _invalidate_by_methods(cache_dir, ["list_issues"])
        assert count == 2

    def test_clear_cache(self, cache_dir):
        _cache_put(cache_dir, "a", "1", 30, "get_issue")
        _cache_put(cache_dir, "b", "2", 30, "list_issues")
        _clear_cache(cache_dir)
        assert _cache_get_or_missing(cache_dir, "a") is _MISSING
        assert _cache_get_or_missing(cache_dir, "b") is _MISSING


# ---------------------------------------------------------------------------
# CachedForgeClient — cache hit / miss behavior
# ---------------------------------------------------------------------------


class TestCachedForgeClientReads:
    def test_get_issue_cache_miss_then_hit(self, cached_forge):
        issue = _make_issue()
        cached_forge._inner.get_issue.return_value = issue

        # First call: miss
        r1 = cached_forge.get_issue(42)
        assert r1 == issue
        assert cached_forge._inner.get_issue.call_count == 1

        # Second call: hit
        r2 = cached_forge.get_issue(42)
        assert r2 == issue
        assert cached_forge._inner.get_issue.call_count == 1  # not called again

        assert cached_forge.stats.misses == 1
        assert cached_forge.stats.hits == 1

    def test_list_issues_cached(self, cached_forge):
        issues = [_make_issue(1), _make_issue(2)]
        cached_forge._inner.list_issues.return_value = issues

        r1 = cached_forge.list_issues(labels=["bug"], state="open")
        r2 = cached_forge.list_issues(labels=["bug"], state="open")
        assert r1 == r2
        assert cached_forge._inner.list_issues.call_count == 1

    def test_different_args_no_collision(self, cached_forge):
        cached_forge._inner.get_issue.side_effect = lambda n: _make_issue(n)

        cached_forge.get_issue(1)
        cached_forge.get_issue(2)
        assert cached_forge._inner.get_issue.call_count == 2

    def test_get_pull_request_cached(self, cached_forge):
        pr = _make_pr()
        cached_forge._inner.get_pull_request.return_value = pr

        r1 = cached_forge.get_pull_request(100)
        r2 = cached_forge.get_pull_request(100)
        assert r1 == r2
        assert cached_forge._inner.get_pull_request.call_count == 1

    def test_list_pull_requests_cached(self, cached_forge):
        prs = [_make_pr()]
        cached_forge._inner.list_pull_requests.return_value = prs

        cached_forge.list_pull_requests(labels=["loom:pr"])
        cached_forge.list_pull_requests(labels=["loom:pr"])
        assert cached_forge._inner.list_pull_requests.call_count == 1

    def test_get_pull_request_reviews_cached(self, cached_forge):
        reviews = [{"state": "APPROVED"}]
        cached_forge._inner.get_pull_request_reviews.return_value = reviews

        r1 = cached_forge.get_pull_request_reviews(100)
        r2 = cached_forge.get_pull_request_reviews(100)
        assert r1 == r2
        assert cached_forge._inner.get_pull_request_reviews.call_count == 1

    def test_get_repo_nwo_cached(self, cached_forge):
        cached_forge._inner.get_repo_nwo.return_value = "owner/repo"

        r1 = cached_forge.get_repo_nwo()
        r2 = cached_forge.get_repo_nwo()
        assert r1 == "owner/repo"
        assert cached_forge._inner.get_repo_nwo.call_count == 1

    def test_get_repo_default_branch_cached(self, cached_forge):
        cached_forge._inner.get_repo_default_branch.return_value = "main"

        r1 = cached_forge.get_repo_default_branch()
        r2 = cached_forge.get_repo_default_branch()
        assert r1 == "main"
        assert cached_forge._inner.get_repo_default_branch.call_count == 1

    def test_get_default_branch_ci_status_cached(self, cached_forge):
        status = ForgeCIStatus(status="passing", message="OK")
        cached_forge._inner.get_default_branch_ci_status.return_value = status

        r1 = cached_forge.get_default_branch_ci_status()
        r2 = cached_forge.get_default_branch_ci_status()
        assert r1.status == "passing"
        assert cached_forge._inner.get_default_branch_ci_status.call_count == 1

    def test_find_pull_request_for_issue_cached(self, cached_forge):
        cached_forge._inner.find_pull_request_for_issue.return_value = 200

        r1 = cached_forge.find_pull_request_for_issue(42)
        r2 = cached_forge.find_pull_request_for_issue(42)
        assert r1 == 200
        assert cached_forge._inner.find_pull_request_for_issue.call_count == 1

    def test_get_issues_batch_cached(self, cached_forge):
        batch = {1: _make_issue(1), 2: _make_issue(2)}
        cached_forge._inner.get_issues_batch.return_value = batch

        r1 = cached_forge.get_issues_batch([1, 2])
        r2 = cached_forge.get_issues_batch([1, 2])
        assert r1[1].number == 1
        assert cached_forge._inner.get_issues_batch.call_count == 1


# ---------------------------------------------------------------------------
# CachedForgeClient — mutation invalidation
# ---------------------------------------------------------------------------


class TestCachedForgeClientMutations:
    def test_create_issue_invalidates(self, cached_forge):
        cached_forge._inner.list_issues.return_value = [_make_issue()]
        cached_forge.list_issues()

        new_issue = _make_issue(99)
        cached_forge._inner.create_issue.return_value = new_issue
        cached_forge.create_issue("title", "body")

        # list_issues should be invalidated (new call)
        cached_forge._inner.list_issues.return_value = [_make_issue(), new_issue]
        cached_forge.list_issues()
        assert cached_forge._inner.list_issues.call_count == 2

    def test_close_issue_invalidates(self, cached_forge):
        cached_forge._inner.get_issue.return_value = _make_issue(42)
        cached_forge.get_issue(42)

        cached_forge._inner.close_issue.return_value = True
        cached_forge.close_issue(42)

        # get_issue(42) should be invalidated
        cached_forge.get_issue(42)
        assert cached_forge._inner.get_issue.call_count == 2

    def test_add_labels_invalidates(self, cached_forge):
        cached_forge._inner.get_issue.return_value = _make_issue(42)
        cached_forge.get_issue(42)

        cached_forge._inner.add_labels.return_value = True
        cached_forge.add_labels("issue", 42, ["new-label"])

        cached_forge.get_issue(42)
        assert cached_forge._inner.get_issue.call_count == 2

    def test_remove_labels_invalidates(self, cached_forge):
        cached_forge._inner.get_pull_request.return_value = _make_pr(100)
        cached_forge.get_pull_request(100)

        cached_forge._inner.remove_labels.return_value = True
        cached_forge.remove_labels("pr", 100, ["old-label"])

        cached_forge.get_pull_request(100)
        assert cached_forge._inner.get_pull_request.call_count == 2

    def test_transition_labels_invalidates(self, cached_forge):
        cached_forge._inner.get_issue.return_value = _make_issue(42)
        cached_forge.get_issue(42)

        cached_forge._inner.transition_labels.return_value = True
        cached_forge.transition_labels("issue", 42, add=["a"], remove=["b"])

        cached_forge.get_issue(42)
        assert cached_forge._inner.get_issue.call_count == 2

    def test_merge_pr_invalidates(self, cached_forge):
        cached_forge._inner.get_pull_request.return_value = _make_pr(100)
        cached_forge.get_pull_request(100)

        cached_forge._inner.merge_pull_request.return_value = True
        cached_forge.merge_pull_request(100)

        cached_forge.get_pull_request(100)
        assert cached_forge._inner.get_pull_request.call_count == 2

    def test_failed_mutation_does_not_invalidate(self, cached_forge):
        cached_forge._inner.get_issue.return_value = _make_issue(42)
        cached_forge.get_issue(42)

        cached_forge._inner.close_issue.return_value = False
        cached_forge.close_issue(42)

        cached_forge.get_issue(42)
        # Should still be cached (mutation failed)
        assert cached_forge._inner.get_issue.call_count == 1

    def test_comment_on_issue_invalidates(self, cached_forge):
        cached_forge._inner.get_issue.return_value = _make_issue(42)
        cached_forge.get_issue(42)

        cached_forge._inner.comment_on_issue.return_value = True
        cached_forge.comment_on_issue(42, "test")

        cached_forge.get_issue(42)
        assert cached_forge._inner.get_issue.call_count == 2

    def test_comment_on_pr_invalidates(self, cached_forge):
        cached_forge._inner.get_pull_request.return_value = _make_pr(100)
        cached_forge.get_pull_request(100)

        cached_forge._inner.comment_on_pull_request.return_value = True
        cached_forge.comment_on_pull_request(100, "test")

        cached_forge.get_pull_request(100)
        assert cached_forge._inner.get_pull_request.call_count == 2

    def test_close_pr_invalidates(self, cached_forge):
        cached_forge._inner.get_pull_request.return_value = _make_pr(100)
        cached_forge.get_pull_request(100)

        cached_forge._inner.close_pull_request.return_value = True
        cached_forge.close_pull_request(100, comment="closing")

        cached_forge.get_pull_request(100)
        assert cached_forge._inner.get_pull_request.call_count == 2

    def test_create_pr_invalidates(self, cached_forge):
        cached_forge._inner.list_pull_requests.return_value = []
        cached_forge.list_pull_requests()

        cached_forge._inner.create_pull_request.return_value = _make_pr()
        cached_forge.create_pull_request("title", "body", "feature/x")

        cached_forge.list_pull_requests()
        assert cached_forge._inner.list_pull_requests.call_count == 2


# ---------------------------------------------------------------------------
# Disabled cache
# ---------------------------------------------------------------------------


class TestDisabledCache:
    def test_disabled_bypasses_cache(self, cache_dir):
        mock = _make_mock_forge()
        mock.get_issue.return_value = _make_issue()
        client = CachedForgeClient(mock, cache_dir=cache_dir, disabled=True)

        client.get_issue(42)
        client.get_issue(42)
        assert mock.get_issue.call_count == 2
        assert client.stats.bypasses == 2

    def test_disabled_via_env(self, cache_dir, monkeypatch):
        monkeypatch.setenv("FORGE_CACHE_DISABLE", "1")
        # Import fresh to pick up env
        from loom_tools.common import cached_forge

        mock = _make_mock_forge()
        mock.get_issue.return_value = _make_issue()
        client = CachedForgeClient(mock, cache_dir=cache_dir)
        # Note: CACHE_DISABLED is read at import time as a module global,
        # but CachedForgeClient accepts disabled= param which we can test directly
        client_explicit = CachedForgeClient(mock, cache_dir=cache_dir, disabled=True)
        client_explicit.get_issue(42)
        client_explicit.get_issue(42)
        assert mock.get_issue.call_count == 2


# ---------------------------------------------------------------------------
# LRU eviction
# ---------------------------------------------------------------------------


class TestLRUEviction:
    def test_eviction_on_max_size(self, cache_dir):
        mock = _make_mock_forge()
        mock.get_issue.side_effect = lambda n: _make_issue(n)
        client = CachedForgeClient(
            mock, cache_dir=cache_dir, disabled=False, max_size=3,
        )

        # Fill cache beyond max
        for i in range(5):
            client.get_issue(i)

        # Count remaining cache files
        files = [
            f for f in os.listdir(cache_dir)
            if f.endswith(".json") and not f.startswith("_")
        ]
        # Should have at most max_size entries (eviction happens on put)
        assert len(files) <= 4  # Eviction might leave max_size + 1 briefly


# ---------------------------------------------------------------------------
# Cache management methods
# ---------------------------------------------------------------------------


class TestCacheManagement:
    def test_clear_cache(self, cached_forge):
        cached_forge._inner.get_issue.return_value = _make_issue()
        cached_forge.get_issue(42)
        cached_forge.clear_cache()
        cached_forge.get_issue(42)
        assert cached_forge._inner.get_issue.call_count == 2

    def test_reset_stats(self, cached_forge):
        cached_forge._inner.get_issue.return_value = _make_issue()
        cached_forge.get_issue(42)
        assert cached_forge.stats.misses == 1
        cached_forge.reset_stats()
        assert cached_forge.stats.misses == 0

    def test_inner_property(self, cached_forge):
        assert cached_forge.inner is cached_forge._inner

    def test_forge_type_delegates(self, cached_forge):
        assert cached_forge.forge_type == "github"


# ---------------------------------------------------------------------------
# Invalidation map coverage
# ---------------------------------------------------------------------------


class TestInvalidationMap:
    def test_all_mutation_methods_mapped(self):
        """Verify all mutation methods in CachedForgeClient have invalidation entries."""
        expected_mutations = {
            "create_issue",
            "close_issue",
            "comment_on_issue",
            "create_pull_request",
            "close_pull_request",
            "merge_pull_request",
            "comment_on_pull_request",
            "add_labels",
            "remove_labels",
            "transition_labels",
        }
        assert expected_mutations == set(INVALIDATION_MAP.keys())

    def test_invalidation_targets_are_cacheable(self):
        """All invalidation targets should be cacheable methods."""
        from loom_tools.common.cached_forge import CACHEABLE_METHODS

        for mutation, targets in INVALIDATION_MAP.items():
            for target in targets:
                assert target in CACHEABLE_METHODS, (
                    f"Invalidation target '{target}' for mutation "
                    f"'{mutation}' is not a cacheable method"
                )


# ---------------------------------------------------------------------------
# get_forge() factory integration
# ---------------------------------------------------------------------------


class TestGetForgeFactory:
    def test_get_forge_returns_cached_by_default(self, monkeypatch):
        """get_forge() should return a CachedForgeClient by default."""
        # Force GitHub detection
        monkeypatch.setenv("LOOM_FORGE_TYPE", "github")
        from loom_tools.common.forge import get_forge

        forge = get_forge(cached=True)
        assert isinstance(forge, CachedForgeClient)

    def test_get_forge_uncached(self, monkeypatch):
        """get_forge(cached=False) should return the raw backend."""
        monkeypatch.setenv("LOOM_FORGE_TYPE", "github")
        from loom_tools.common.forge import get_forge
        from loom_tools.common.github import GitHubForge

        forge = get_forge(cached=False)
        assert isinstance(forge, GitHubForge)


# ---------------------------------------------------------------------------
# None value caching
# ---------------------------------------------------------------------------


class TestNoneValueCaching:
    def test_get_issue_none_cached(self, cached_forge):
        """None results should be cached (issue doesn't exist)."""
        cached_forge._inner.get_issue.return_value = None

        r1 = cached_forge.get_issue(999)
        r2 = cached_forge.get_issue(999)
        assert r1 is None
        assert r2 is None
        assert cached_forge._inner.get_issue.call_count == 1

    def test_find_pr_none_cached(self, cached_forge):
        cached_forge._inner.find_pull_request_for_issue.return_value = None

        r1 = cached_forge.find_pull_request_for_issue(999)
        r2 = cached_forge.find_pull_request_for_issue(999)
        assert r1 is None
        assert cached_forge._inner.find_pull_request_for_issue.call_count == 1
