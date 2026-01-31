"""Tests for label caching and manipulation."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.shepherd.labels import LabelCache


class TestLabelCache:
    """Test LabelCache class."""

    def test_cache_miss_fetches_labels(self) -> None:
        """Cache miss should fetch labels from API."""
        cache = LabelCache()

        with patch.object(cache, "_fetch_labels", return_value={"label1", "label2"}) as mock:
            labels = cache.get_issue_labels(42)
            assert labels == {"label1", "label2"}
            mock.assert_called_once_with("issue", 42)

    def test_cache_hit_returns_cached(self) -> None:
        """Cache hit should return cached labels without API call."""
        cache = LabelCache()
        cache._issue_labels[42] = {"cached"}

        with patch.object(cache, "_fetch_labels") as mock:
            labels = cache.get_issue_labels(42)
            assert labels == {"cached"}
            mock.assert_not_called()

    def test_refresh_bypasses_cache(self) -> None:
        """refresh=True should bypass cache and fetch fresh labels."""
        cache = LabelCache()
        cache._issue_labels[42] = {"old_label"}

        with patch.object(cache, "_fetch_labels", return_value={"new_label"}) as mock:
            labels = cache.get_issue_labels(42, refresh=True)
            assert labels == {"new_label"}
            mock.assert_called_once_with("issue", 42)

    def test_has_issue_label_true(self) -> None:
        """has_issue_label should return True when label exists."""
        cache = LabelCache()
        cache._issue_labels[42] = {"loom:issue", "loom:curated"}

        assert cache.has_issue_label(42, "loom:issue") is True

    def test_has_issue_label_false(self) -> None:
        """has_issue_label should return False when label doesn't exist."""
        cache = LabelCache()
        cache._issue_labels[42] = {"loom:issue"}

        assert cache.has_issue_label(42, "loom:blocked") is False

    def test_set_issue_labels(self) -> None:
        """set_issue_labels should pre-populate cache."""
        cache = LabelCache()
        cache.set_issue_labels(42, {"label1", "label2"})

        assert cache._issue_labels[42] == {"label1", "label2"}

    def test_invalidate_issue_specific(self) -> None:
        """invalidate_issue should clear specific issue cache."""
        cache = LabelCache()
        cache._issue_labels[42] = {"label1"}
        cache._issue_labels[43] = {"label2"}

        cache.invalidate_issue(42)

        assert 42 not in cache._issue_labels
        assert 43 in cache._issue_labels

    def test_invalidate_issue_all(self) -> None:
        """invalidate_issue(None) should clear all issue caches."""
        cache = LabelCache()
        cache._issue_labels[42] = {"label1"}
        cache._issue_labels[43] = {"label2"}

        cache.invalidate_issue(None)

        assert cache._issue_labels == {}

    def test_invalidate_all(self) -> None:
        """invalidate should clear all caches."""
        cache = LabelCache()
        cache._issue_labels[42] = {"label1"}
        cache._pr_labels[100] = {"pr_label"}

        cache.invalidate()

        assert cache._issue_labels == {}
        assert cache._pr_labels == {}

    def test_pr_cache_separate(self) -> None:
        """PR cache should be separate from issue cache."""
        cache = LabelCache()
        cache._issue_labels[42] = {"issue_label"}
        cache._pr_labels[42] = {"pr_label"}

        assert cache.has_issue_label(42, "issue_label") is True
        assert cache.has_issue_label(42, "pr_label") is False
        assert cache.has_pr_label(42, "pr_label") is True
        assert cache.has_pr_label(42, "issue_label") is False

    def test_finds_gh_cached_when_available(self) -> None:
        """Should use gh-cached when available."""
        repo_root = Path("/fake/repo")

        with patch.object(Path, "is_file", return_value=True):
            with patch.object(Path, "stat") as mock_stat:
                mock_stat.return_value.st_mode = 0o755
                cache = LabelCache(repo_root)
                assert "gh-cached" in cache._gh_cmd

    def test_falls_back_to_gh(self) -> None:
        """Should fall back to gh when gh-cached not available."""
        cache = LabelCache()
        assert cache._gh_cmd == "gh"


class TestGenericLabelOperations:
    """Test generic (unified) label operations."""

    def test_get_labels_generic(self) -> None:
        """get_labels should work with entity_type parameter."""
        cache = LabelCache()

        with patch.object(cache, "_fetch_labels", return_value={"label1"}) as mock:
            labels = cache.get_labels("issue", 42)
            assert labels == {"label1"}
            mock.assert_called_once_with("issue", 42)

    def test_get_labels_pr(self) -> None:
        """get_labels should work for PRs."""
        cache = LabelCache()

        with patch.object(cache, "_fetch_labels", return_value={"pr_label"}) as mock:
            labels = cache.get_labels("pr", 100)
            assert labels == {"pr_label"}
            mock.assert_called_once_with("pr", 100)

    def test_has_label_generic(self) -> None:
        """has_label should work with entity_type parameter."""
        cache = LabelCache()
        cache.set_labels("issue", 42, {"loom:issue"})

        assert cache.has_label("issue", 42, "loom:issue") is True
        assert cache.has_label("issue", 42, "loom:blocked") is False

    def test_set_labels_generic(self) -> None:
        """set_labels should work with entity_type parameter."""
        cache = LabelCache()
        cache.set_labels("pr", 100, {"pr_label"})

        assert cache.get_labels("pr", 100) == {"pr_label"}

    def test_invalidate_entity_specific(self) -> None:
        """invalidate_entity should clear specific entity cache."""
        cache = LabelCache()
        cache.set_labels("issue", 42, {"label1"})
        cache.set_labels("issue", 43, {"label2"})
        cache.set_labels("pr", 42, {"pr_label"})

        cache.invalidate_entity("issue", 42)

        # Issue 42 should be gone, but issue 43 and PR 42 should remain
        assert ("issue", 42) not in cache._labels
        assert ("issue", 43) in cache._labels
        assert ("pr", 42) in cache._labels

    def test_invalidate_entity_all_of_type(self) -> None:
        """invalidate_entity(type, None) should clear all of that type."""
        cache = LabelCache()
        cache.set_labels("issue", 42, {"label1"})
        cache.set_labels("issue", 43, {"label2"})
        cache.set_labels("pr", 42, {"pr_label"})

        cache.invalidate_entity("issue")

        # All issues should be gone, but PRs should remain
        assert ("issue", 42) not in cache._labels
        assert ("issue", 43) not in cache._labels
        assert ("pr", 42) in cache._labels

    def test_cache_key_prevents_collisions(self) -> None:
        """Issue and PR with same number should have separate cache entries."""
        cache = LabelCache()
        cache.set_labels("issue", 42, {"issue_label"})
        cache.set_labels("pr", 42, {"pr_label"})

        assert cache.get_labels("issue", 42) == {"issue_label"}
        assert cache.get_labels("pr", 42) == {"pr_label"}
        assert ("issue", 42) in cache._labels
        assert ("pr", 42) in cache._labels
