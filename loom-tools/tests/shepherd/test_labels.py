"""Tests for label caching and manipulation."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, patch

from loom_tools.common.forge import ForgeIssue, ForgePullRequest
from loom_tools.shepherd.labels import (
    LabelCache,
    add_label,
    get_issue_metadata,
    get_pr_for_issue,
    remove_label,
    transition_labels,
    transition_issue_labels,
    transition_pr_labels,
)


def _make_forge_mock() -> MagicMock:
    """Create a mock ForgeClient."""
    mock = MagicMock()
    mock.add_labels.return_value = True
    mock.remove_labels.return_value = True
    mock.transition_labels.return_value = True
    return mock


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

    def test_fetch_labels_uses_forge_get_issue(self) -> None:
        """_fetch_labels for issues should delegate to ForgeClient.get_issue."""
        forge_mock = _make_forge_mock()
        forge_mock.get_issue.return_value = ForgeIssue(
            number=42, state="OPEN", title="Test", url="https://example.com/42",
            labels=["label1", "label2"],
        )
        cache = LabelCache(forge=forge_mock)
        labels = cache._fetch_labels("issue", 42)
        assert labels == {"label1", "label2"}
        forge_mock.get_issue.assert_called_once_with(42)

    def test_fetch_labels_uses_forge_get_pull_request(self) -> None:
        """_fetch_labels for PRs should delegate to ForgeClient.get_pull_request."""
        forge_mock = _make_forge_mock()
        forge_mock.get_pull_request.return_value = ForgePullRequest(
            number=100, state="OPEN", title="PR", url="https://example.com/pr/100",
            labels=["pr_label"],
        )
        cache = LabelCache(forge=forge_mock)
        labels = cache._fetch_labels("pr", 100)
        assert labels == {"pr_label"}
        forge_mock.get_pull_request.assert_called_once_with(100)

    def test_fetch_labels_returns_empty_set_on_none(self) -> None:
        """_fetch_labels returns empty set when entity not found."""
        forge_mock = _make_forge_mock()
        forge_mock.get_issue.return_value = None
        cache = LabelCache(forge=forge_mock)
        labels = cache._fetch_labels("issue", 999)
        assert labels == set()

    def test_constructor_accepts_forge_parameter(self) -> None:
        """LabelCache should accept an optional forge parameter."""
        forge_mock = _make_forge_mock()
        cache = LabelCache(forge=forge_mock)
        assert cache._forge is forge_mock

    def test_lazy_forge_creation(self) -> None:
        """LabelCache should lazily create ForgeClient from repo_root."""
        with patch("loom_tools.shepherd.labels._get_forge_client") as mock_get:
            mock_get.return_value = _make_forge_mock()
            mock_get.return_value.get_issue.return_value = ForgeIssue(
                number=42, state="OPEN", title="Test", url="",
                labels=["label1"],
            )
            cache = LabelCache(repo_root=Path("/fake/repo"))
            cache._fetch_labels("issue", 42)
            mock_get.assert_called_once_with(Path("/fake/repo"))


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


class TestAtomicLabelTransitions:
    """Tests for atomic label transition functions."""

    def test_transition_labels_add_and_remove(self) -> None:
        """transition_labels should delegate to ForgeClient.transition_labels."""
        forge_mock = _make_forge_mock()
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            result = transition_labels(
                "issue",
                42,
                add=["loom:building"],
                remove=["loom:issue"],
            )

            assert result is True
            forge_mock.transition_labels.assert_called_once_with(
                "issue", 42,
                add=["loom:building"],
                remove=["loom:issue"],
            )

    def test_transition_labels_add_only(self) -> None:
        """transition_labels should work with only add."""
        forge_mock = _make_forge_mock()
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            result = transition_labels("issue", 42, add=["loom:building"])

            assert result is True
            forge_mock.transition_labels.assert_called_once_with(
                "issue", 42,
                add=["loom:building"],
                remove=None,
            )

    def test_transition_labels_remove_only(self) -> None:
        """transition_labels should work with only remove."""
        forge_mock = _make_forge_mock()
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            result = transition_labels("issue", 42, remove=["loom:issue"])

            assert result is True
            forge_mock.transition_labels.assert_called_once_with(
                "issue", 42,
                add=None,
                remove=["loom:issue"],
            )

    def test_transition_labels_noop(self) -> None:
        """transition_labels should return True with no changes."""
        forge_mock = _make_forge_mock()
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            result = transition_labels("issue", 42)

            assert result is True
            forge_mock.transition_labels.assert_not_called()

    def test_transition_labels_failure(self) -> None:
        """transition_labels should return False on failure."""
        forge_mock = _make_forge_mock()
        forge_mock.transition_labels.return_value = False
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            result = transition_labels(
                "issue",
                42,
                add=["loom:building"],
                remove=["loom:issue"],
            )

            assert result is False

    def test_transition_labels_multiple_add_remove(self) -> None:
        """transition_labels should handle multiple labels."""
        forge_mock = _make_forge_mock()
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            result = transition_labels(
                "issue",
                42,
                add=["loom:building", "loom:wip"],
                remove=["loom:issue", "loom:curated"],
            )

            assert result is True
            forge_mock.transition_labels.assert_called_once()
            call_kwargs = forge_mock.transition_labels.call_args
            assert call_kwargs[1]["add"] == ["loom:building", "loom:wip"]
            assert set(call_kwargs[1]["remove"]) == {"loom:issue", "loom:curated"}

    def test_transition_labels_pr(self) -> None:
        """transition_labels should work for PRs."""
        forge_mock = _make_forge_mock()
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            result = transition_labels(
                "pr",
                100,
                add=["loom:pr"],
                remove=["loom:review-requested"],
            )

            assert result is True
            forge_mock.transition_labels.assert_called_once_with(
                "pr", 100,
                add=["loom:pr"],
                remove=["loom:review-requested"],
            )

    def test_transition_issue_labels_convenience(self) -> None:
        """transition_issue_labels should delegate to transition_labels."""
        with patch("loom_tools.shepherd.labels.transition_labels") as mock:
            mock.return_value = True
            result = transition_issue_labels(
                42,
                add=["loom:building"],
                remove=["loom:issue"],
                repo_root=Path("/repo"),
            )

            assert result is True
            mock.assert_called_once_with(
                "issue",
                42,
                add=["loom:building"],
                remove=["loom:issue"],
                repo_root=Path("/repo"),
            )

    def test_transition_pr_labels_convenience(self) -> None:
        """transition_pr_labels should delegate to transition_labels."""
        with patch("loom_tools.shepherd.labels.transition_labels") as mock:
            mock.return_value = True
            result = transition_pr_labels(
                100,
                add=["loom:pr"],
                remove=["loom:review-requested"],
                repo_root=Path("/repo"),
            )

            assert result is True
            mock.assert_called_once_with(
                "pr",
                100,
                add=["loom:pr"],
                remove=["loom:review-requested"],
                repo_root=Path("/repo"),
            )

    def test_transition_labels_with_repo_root(self) -> None:
        """transition_labels should pass repo_root to _get_forge_client."""
        forge_mock = _make_forge_mock()
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock) as mock_get:
            repo_root = Path("/fake/repo")
            transition_labels(
                "issue",
                42,
                add=["loom:building"],
                repo_root=repo_root,
            )

            mock_get.assert_called_once_with(repo_root)

    def test_transition_labels_enforce_exclusion(self) -> None:
        """enforce_exclusion should add conflicting labels to the remove set."""
        forge_mock = _make_forge_mock()
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            result = transition_labels(
                "pr",
                100,
                add=["loom:pr"],
                enforce_exclusion=True,
            )

            assert result is True
            call_kwargs = forge_mock.transition_labels.call_args
            remove_set = set(call_kwargs[1]["remove"])
            assert "loom:changes-requested" in remove_set
            assert "loom:review-requested" in remove_set
            assert "loom:pr" not in remove_set

    def test_transition_labels_enforce_exclusion_merges_with_explicit_remove(self) -> None:
        """enforce_exclusion should merge with explicitly provided remove labels."""
        forge_mock = _make_forge_mock()
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            result = transition_labels(
                "issue",
                42,
                add=["loom:building"],
                remove=["loom:curated"],
                enforce_exclusion=True,
            )

            assert result is True
            call_kwargs = forge_mock.transition_labels.call_args
            remove_set = set(call_kwargs[1]["remove"])
            # Should include both explicit and exclusion-derived removes
            assert "loom:curated" in remove_set
            assert "loom:issue" in remove_set
            assert "loom:blocked" in remove_set


class TestStandaloneLabelFunctions:
    """Tests for standalone add_label, remove_label functions."""

    def test_add_label_delegates_to_forge(self) -> None:
        """add_label should delegate to ForgeClient.add_labels."""
        forge_mock = _make_forge_mock()
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            result = add_label("issue", 42, "loom:building")
            assert result is True
            forge_mock.add_labels.assert_called_once_with("issue", 42, ["loom:building"])

    def test_add_label_returns_false_on_failure(self) -> None:
        """add_label should return False when ForgeClient fails."""
        forge_mock = _make_forge_mock()
        forge_mock.add_labels.return_value = False
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            result = add_label("issue", 42, "loom:building")
            assert result is False

    def test_remove_label_delegates_to_forge(self) -> None:
        """remove_label should delegate to ForgeClient.remove_labels."""
        forge_mock = _make_forge_mock()
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            result = remove_label("issue", 42, "loom:building")
            assert result is True
            forge_mock.remove_labels.assert_called_once_with("issue", 42, ["loom:building"])

    def test_remove_label_always_returns_true(self) -> None:
        """remove_label should always return True (label may not have existed)."""
        forge_mock = _make_forge_mock()
        forge_mock.remove_labels.return_value = False
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            result = remove_label("issue", 42, "loom:building")
            assert result is True

    def test_add_label_passes_repo_root(self) -> None:
        """add_label should pass repo_root to _get_forge_client."""
        forge_mock = _make_forge_mock()
        repo_root = Path("/fake/repo")
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock) as mock_get:
            add_label("issue", 42, "loom:building", repo_root=repo_root)
            mock_get.assert_called_once_with(repo_root)


class TestGetIssueMetadata:
    """Tests for get_issue_metadata."""

    def test_returns_metadata_dict(self) -> None:
        """get_issue_metadata should return a dict with expected keys."""
        forge_mock = _make_forge_mock()
        forge_mock.get_issue.return_value = ForgeIssue(
            number=42, state="OPEN", title="Test Issue",
            url="https://example.com/42",
            labels=["loom:issue", "loom:curated"],
        )
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            result = get_issue_metadata(42)

        assert result is not None
        assert result["url"] == "https://example.com/42"
        assert result["state"] == "OPEN"
        assert result["title"] == "Test Issue"
        assert result["labels"] == [{"name": "loom:issue"}, {"name": "loom:curated"}]

    def test_returns_none_when_not_found(self) -> None:
        """get_issue_metadata should return None when issue not found."""
        forge_mock = _make_forge_mock()
        forge_mock.get_issue.return_value = None
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            result = get_issue_metadata(999)

        assert result is None


class TestGetPrForIssue:
    """Tests for get_pr_for_issue delegating to ForgeClient."""

    def test_returns_pr_number_when_found(self) -> None:
        """get_pr_for_issue should return PR number from ForgeClient."""
        forge_mock = _make_forge_mock()
        forge_mock.find_pull_request_for_issue.return_value = 42
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            result = get_pr_for_issue(100, state="open")
        assert result == 42
        forge_mock.find_pull_request_for_issue.assert_called_once_with(100, state="open")

    def test_returns_none_when_not_found(self) -> None:
        """get_pr_for_issue should return None when no PR found."""
        forge_mock = _make_forge_mock()
        forge_mock.find_pull_request_for_issue.return_value = None
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            result = get_pr_for_issue(999, state="open")
        assert result is None

    def test_passes_state_to_forge(self) -> None:
        """get_pr_for_issue should pass state to ForgeClient."""
        forge_mock = _make_forge_mock()
        forge_mock.find_pull_request_for_issue.return_value = None
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock):
            get_pr_for_issue(42, state="merged")
        forge_mock.find_pull_request_for_issue.assert_called_once_with(42, state="merged")

    def test_passes_repo_root(self) -> None:
        """get_pr_for_issue should pass repo_root to _get_forge_client."""
        forge_mock = _make_forge_mock()
        forge_mock.find_pull_request_for_issue.return_value = None
        repo_root = Path("/some/repo")
        with patch("loom_tools.shepherd.labels._get_forge_client", return_value=forge_mock) as mock_get:
            get_pr_for_issue(42, repo_root=repo_root)
        mock_get.assert_called_once_with(repo_root)
