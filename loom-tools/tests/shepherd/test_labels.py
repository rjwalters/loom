"""Tests for label caching and manipulation."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

from loom_tools.shepherd.labels import (
    LabelCache,
    get_pr_for_issue,
    transition_labels,
    transition_issue_labels,
    transition_pr_labels,
)


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

    def test_run_gh_delegates_to_common_gh_run(self) -> None:
        """_run_gh should delegate to common.github.gh_run."""
        cache = LabelCache(Path("/fake/repo"))
        with patch("loom_tools.shepherd.labels.gh_run") as mock_gh:
            mock_gh.return_value.returncode = 0
            mock_gh.return_value.stdout = "label1\nlabel2\n"
            result = cache._run_gh(["issue", "view", "42", "--json", "labels"])
        assert result == "label1\nlabel2"
        mock_gh.assert_called_once()
        assert mock_gh.call_args[1]["cwd"] == Path("/fake/repo")


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
        """transition_labels should use single gh command with both flags."""
        with patch("loom_tools.shepherd.labels.gh_run") as mock_run:
            mock_run.return_value.returncode = 0
            result = transition_labels(
                "issue",
                42,
                add=["loom:building"],
                remove=["loom:issue"],
            )

            assert result is True
            mock_run.assert_called_once()
            call_args = mock_run.call_args[0][0]
            assert call_args == [
                "issue", "edit", "42",
                "--remove-label", "loom:issue",
                "--add-label", "loom:building",
            ]

    def test_transition_labels_add_only(self) -> None:
        """transition_labels should work with only add."""
        with patch("loom_tools.shepherd.labels.gh_run") as mock_run:
            mock_run.return_value.returncode = 0
            result = transition_labels("issue", 42, add=["loom:building"])

            assert result is True
            call_args = mock_run.call_args[0][0]
            assert "--add-label" in call_args
            assert "loom:building" in call_args
            assert "--remove-label" not in call_args

    def test_transition_labels_remove_only(self) -> None:
        """transition_labels should work with only remove."""
        with patch("loom_tools.shepherd.labels.gh_run") as mock_run:
            mock_run.return_value.returncode = 0
            result = transition_labels("issue", 42, remove=["loom:issue"])

            assert result is True
            call_args = mock_run.call_args[0][0]
            assert "--remove-label" in call_args
            assert "loom:issue" in call_args
            assert "--add-label" not in call_args

    def test_transition_labels_noop(self) -> None:
        """transition_labels should return True with no changes."""
        with patch("loom_tools.shepherd.labels.gh_run") as mock_run:
            result = transition_labels("issue", 42)

            assert result is True
            mock_run.assert_not_called()

    def test_transition_labels_failure(self) -> None:
        """transition_labels should return False on failure."""
        with patch("loom_tools.shepherd.labels.gh_run") as mock_run:
            mock_run.return_value.returncode = 1
            result = transition_labels(
                "issue",
                42,
                add=["loom:building"],
                remove=["loom:issue"],
            )

            assert result is False

    def test_transition_labels_multiple_add_remove(self) -> None:
        """transition_labels should handle multiple labels."""
        with patch("loom_tools.shepherd.labels.gh_run") as mock_run:
            mock_run.return_value.returncode = 0
            result = transition_labels(
                "issue",
                42,
                add=["loom:building", "loom:wip"],
                remove=["loom:issue", "loom:curated"],
            )

            assert result is True
            call_args = mock_run.call_args[0][0]
            # Check all labels are present
            assert call_args.count("--add-label") == 2
            assert call_args.count("--remove-label") == 2
            assert "loom:building" in call_args
            assert "loom:wip" in call_args
            assert "loom:issue" in call_args
            assert "loom:curated" in call_args

    def test_transition_labels_pr(self) -> None:
        """transition_labels should work for PRs."""
        with patch("loom_tools.shepherd.labels.gh_run") as mock_run:
            mock_run.return_value.returncode = 0
            result = transition_labels(
                "pr",
                100,
                add=["loom:pr"],
                remove=["loom:review-requested"],
            )

            assert result is True
            call_args = mock_run.call_args[0][0]
            assert call_args[:3] == ["pr", "edit", "100"]

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
        """transition_labels should pass repo_root to gh_run."""
        with patch("loom_tools.shepherd.labels.gh_run") as mock_run:
            mock_run.return_value.returncode = 0
            repo_root = Path("/fake/repo")
            transition_labels(
                "issue",
                42,
                add=["loom:building"],
                repo_root=repo_root,
            )

            mock_run.assert_called_once()
            assert mock_run.call_args[1]["cwd"] == repo_root


class TestGetPrForIssue:
    """Tests for get_pr_for_issue false-positive prevention."""

    def _make_run(self, branch_result: str, body_result: str):
        """Return a mock gh_run side_effect that simulates branch and body searches."""

        def _side_effect(args, check=True, cwd=None):
            mock = type("MockResult", (), {})()
            mock.returncode = 0
            # Branch-based lookup uses --head flag
            if "--head" in args:
                mock.stdout = branch_result + "\n"
            else:
                mock.stdout = body_result + "\n"
            return mock

        return _side_effect

    def test_merged_state_skips_body_search_when_branch_found(self) -> None:
        """For state='merged', branch lookup succeeds and body search is never called."""
        call_args_list = []

        def _side_effect(args, check=True, cwd=None):
            call_args_list.append(args)
            mock = type("MockResult", (), {})()
            mock.returncode = 0
            # Branch lookup returns a PR number
            mock.stdout = "848\n"
            return mock

        with patch("loom_tools.shepherd.labels.gh_run", side_effect=_side_effect):
            result = get_pr_for_issue(2858, state="merged")

        assert result == 848
        # Only one call (branch lookup) — no body search calls
        assert len(call_args_list) == 1
        assert "--head" in call_args_list[0]

    def test_merged_state_skips_body_search_when_branch_not_found(self) -> None:
        """For state='merged', body search is skipped even when branch lookup returns nothing.

        This is the core fix for issue #2915: a dependabot PR with a cross-repo
        reference to issue #2858 would previously be returned as a false positive.
        """
        call_args_list = []

        def _side_effect(args, check=True, cwd=None):
            call_args_list.append(args)
            mock = type("MockResult", (), {})()
            mock.returncode = 0
            # Branch lookup finds nothing; body search would find 848 (dependabot false positive)
            if "--head" in args:
                mock.stdout = "\n"
            else:
                mock.stdout = "848\n"
            return mock

        with patch("loom_tools.shepherd.labels.gh_run", side_effect=_side_effect):
            result = get_pr_for_issue(2858, state="merged")

        # Must return None, not 848 (the false positive)
        assert result is None
        # Only one call (branch lookup) — body search never called for merged state
        assert len(call_args_list) == 1
        assert "--head" in call_args_list[0]

    def test_open_state_still_uses_body_search(self) -> None:
        """For state='open', body search is used as a fallback when branch lookup fails."""
        call_args_list = []

        def _side_effect(args, check=True, cwd=None):
            call_args_list.append(args)
            mock = type("MockResult", (), {})()
            mock.returncode = 0
            if "--head" in args:
                mock.stdout = "\n"  # Branch lookup finds nothing
            else:
                mock.stdout = "123\n"  # Body search finds PR 123
            return mock

        with patch("loom_tools.shepherd.labels.gh_run", side_effect=_side_effect):
            result = get_pr_for_issue(42, state="open")

        assert result == 123
        # Should have called branch lookup + at least one body search
        assert len(call_args_list) >= 2
        assert "--head" in call_args_list[0]
        # Second call is body search (no --head flag)
        assert "--head" not in call_args_list[1]

    def test_branch_lookup_takes_precedence_over_body_search(self) -> None:
        """Branch-based lookup result is returned before body search is attempted."""
        call_args_list = []

        def _side_effect(args, check=True, cwd=None):
            call_args_list.append(args)
            mock = type("MockResult", (), {})()
            mock.returncode = 0
            if "--head" in args:
                mock.stdout = "999\n"  # Branch lookup succeeds
            else:
                mock.stdout = "888\n"  # Body search would return different PR
            return mock

        with patch("loom_tools.shepherd.labels.gh_run", side_effect=_side_effect):
            result = get_pr_for_issue(42, state="open")

        # Branch result (999) takes precedence over body search result (888)
        assert result == 999
        # Only one call made (branch lookup)
        assert len(call_args_list) == 1

    def test_returns_none_when_no_pr_found(self) -> None:
        """Returns None when all search methods find nothing."""

        def _side_effect(args, check=True, cwd=None):
            mock = type("MockResult", (), {})()
            mock.returncode = 0
            mock.stdout = "\n"
            return mock

        with patch("loom_tools.shepherd.labels.gh_run", side_effect=_side_effect):
            result = get_pr_for_issue(42, state="open")

        assert result is None
