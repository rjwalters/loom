"""Tests for label caching and manipulation."""

from __future__ import annotations

import json
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
    """Tests for get_pr_for_issue using closingIssuesReferences validation."""

    def _make_result(self, returncode: int = 0, stdout: str = "") -> object:
        """Create a mock subprocess result."""
        result = object.__new__(object)
        result.__class__ = type(
            "Result",
            (),
            {"returncode": returncode, "stdout": stdout},
        )
        return result

    def _mock_run(self, outputs: list[tuple[int, str]]):  # type: ignore[return]
        """Return a mock that yields (returncode, stdout) pairs in sequence."""
        from unittest.mock import MagicMock
        mock = MagicMock()
        results = []
        for returncode, stdout in outputs:
            r = MagicMock()
            r.returncode = returncode
            r.stdout = stdout
            results.append(r)
        mock.side_effect = results
        return mock

    def test_branch_lookup_returns_pr_number(self) -> None:
        """Method 1 (branch-based) returns PR number on match."""
        with patch("loom_tools.shepherd.labels.gh_run") as mock_run:
            mock_run.return_value.returncode = 0
            mock_run.return_value.stdout = "42"
            result = get_pr_for_issue(100, state="open")
        assert result == 42

    def test_branch_lookup_null_falls_through_to_body_search(self) -> None:
        """When branch lookup returns null, falls through to body search."""
        closing_refs = [{"number": 100}]
        body_search_output = json.dumps([
            {"number": 99, "closingIssuesReferences": closing_refs}
        ])
        mock = self._mock_run([
            (0, "null"),           # Method 1: branch lookup → null
            (0, body_search_output),  # Method 2: "Closes #100" → match
        ])
        with patch("loom_tools.shepherd.labels.gh_run", mock):
            result = get_pr_for_issue(100, state="open")
        assert result == 99

    def test_body_search_validates_closing_refs(self) -> None:
        """Body search result must appear in closingIssuesReferences to be accepted."""
        # PR 848 has "Closes" in body but closingIssuesReferences references issue 2839
        # on a different repo — not matching our issue 100.
        false_positive_output = json.dumps([
            {
                "number": 848,
                "closingIssuesReferences": [
                    {"number": 2839}  # different issue — false positive
                ],
            }
        ])
        mock = self._mock_run([
            (0, "null"),               # Method 1: branch lookup → null
            (0, false_positive_output),  # Method 2: "Closes #100" → false positive
            (0, "[]"),                 # Method 3: "Fixes #100" → empty
            (0, "[]"),                 # Method 4: "Resolves #100" → empty
        ])
        with patch("loom_tools.shepherd.labels.gh_run", mock):
            result = get_pr_for_issue(100, state="merged")
        assert result is None

    def test_body_search_accepts_correct_closing_ref(self) -> None:
        """Body search accepts PR when closingIssuesReferences contains the target issue."""
        closing_refs = [{"number": 100}]
        output = json.dumps([
            {"number": 55, "closingIssuesReferences": closing_refs}
        ])
        mock = self._mock_run([
            (0, "null"),   # Method 1: branch lookup → null
            (0, output),   # Method 2: "Closes #100" → valid match
        ])
        with patch("loom_tools.shepherd.labels.gh_run", mock):
            result = get_pr_for_issue(100, state="open")
        assert result == 55

    def test_fixes_pattern_validated_by_closing_refs(self) -> None:
        """'Fixes' pattern also validated via closingIssuesReferences."""
        closes_output = json.dumps([])
        fixes_output = json.dumps([
            {"number": 77, "closingIssuesReferences": [{"number": 200}]}
        ])
        mock = self._mock_run([
            (0, "null"),        # Method 1: branch lookup → null
            (0, closes_output), # Method 2: "Closes #200" → empty
            (0, fixes_output),  # Method 3: "Fixes #200" → valid match
        ])
        with patch("loom_tools.shepherd.labels.gh_run", mock):
            result = get_pr_for_issue(200, state="open")
        assert result == 77

    def test_resolves_pattern_validated_by_closing_refs(self) -> None:
        """'Resolves' pattern also validated via closingIssuesReferences."""
        resolves_output = json.dumps([
            {"number": 88, "closingIssuesReferences": [{"number": 300}]}
        ])
        mock = self._mock_run([
            (0, "null"),     # Method 1: branch lookup → null
            (0, "[]"),       # Method 2: "Closes #300" → empty
            (0, "[]"),       # Method 3: "Fixes #300" → empty
            (0, resolves_output),  # Method 4: "Resolves #300" → valid match
        ])
        with patch("loom_tools.shepherd.labels.gh_run", mock):
            result = get_pr_for_issue(300, state="open")
        assert result == 88

    def test_returns_none_when_no_pr_found(self) -> None:
        """Returns None when no method finds a matching PR."""
        mock = self._mock_run([
            (0, "null"),  # Method 1: branch lookup → null
            (0, "[]"),    # Method 2: "Closes" → empty
            (0, "[]"),    # Method 3: "Fixes" → empty
            (0, "[]"),    # Method 4: "Resolves" → empty
        ])
        with patch("loom_tools.shepherd.labels.gh_run", mock):
            result = get_pr_for_issue(999, state="open")
        assert result is None

    def test_multiple_candidates_first_valid_wins(self) -> None:
        """When multiple PRs match a search, first one with correct closing ref wins."""
        output = json.dumps([
            {"number": 10, "closingIssuesReferences": [{"number": 999}]},  # wrong issue
            {"number": 20, "closingIssuesReferences": [{"number": 42}]},   # correct
            {"number": 30, "closingIssuesReferences": [{"number": 42}]},   # also correct
        ])
        mock = self._mock_run([
            (0, "null"),   # Method 1: branch lookup → null
            (0, output),   # Method 2: "Closes #42" → multiple candidates
        ])
        with patch("loom_tools.shepherd.labels.gh_run", mock):
            result = get_pr_for_issue(42, state="open")
        assert result == 20  # first valid candidate

    def test_invalid_json_from_body_search_skipped(self) -> None:
        """Invalid JSON from body search is skipped gracefully."""
        mock = self._mock_run([
            (0, "null"),           # Method 1: branch lookup → null
            (0, "not valid json"), # Method 2: invalid JSON
            (0, "[]"),             # Method 3: empty
            (0, "[]"),             # Method 4: empty
        ])
        with patch("loom_tools.shepherd.labels.gh_run", mock):
            result = get_pr_for_issue(42, state="open")
        assert result is None

    def test_gh_command_failure_skips_that_method(self) -> None:
        """gh command failure (non-zero exit) is skipped gracefully."""
        mock = self._mock_run([
            (1, ""),    # Method 1: branch lookup fails
            (1, ""),    # Method 2: command fails
            (1, ""),    # Method 3: command fails
            (1, ""),    # Method 4: command fails
        ])
        with patch("loom_tools.shepherd.labels.gh_run", mock):
            result = get_pr_for_issue(42, state="open")
        assert result is None

    def test_empty_closing_refs_list_not_accepted(self) -> None:
        """PR with empty closingIssuesReferences is not accepted as a match."""
        output = json.dumps([
            {"number": 55, "closingIssuesReferences": []}  # no closing refs
        ])
        mock = self._mock_run([
            (0, "null"),   # Method 1: branch lookup → null
            (0, output),   # Method 2: candidate with empty closingIssuesReferences
            (0, "[]"),     # Method 3: empty
            (0, "[]"),     # Method 4: empty
        ])
        with patch("loom_tools.shepherd.labels.gh_run", mock):
            result = get_pr_for_issue(42, state="open")
        assert result is None

    def test_passes_state_to_gh_command(self) -> None:
        """The state parameter is forwarded to gh pr list commands."""
        mock = self._mock_run([
            (0, "null"),  # Method 1: branch lookup → null
            (0, "[]"),    # Method 2: empty
            (0, "[]"),    # Method 3: empty
            (0, "[]"),    # Method 4: empty
        ])
        with patch("loom_tools.shepherd.labels.gh_run", mock):
            get_pr_for_issue(42, state="merged")

        # Check that "merged" appears in the call args
        calls = mock.call_args_list
        for call in calls:
            args_list = call[0][0]
            assert "merged" in args_list

    def test_passes_repo_root_to_gh_run(self) -> None:
        """repo_root is forwarded as cwd to gh_run."""
        repo_root = Path("/some/repo")
        mock = self._mock_run([
            (0, "null"),  # Method 1: branch lookup → null
            (0, "[]"),    # Method 2: empty
            (0, "[]"),    # Method 3: empty
            (0, "[]"),    # Method 4: empty
        ])
        with patch("loom_tools.shepherd.labels.gh_run", mock):
            get_pr_for_issue(42, repo_root=repo_root)

        for call in mock.call_args_list:
            assert call[1].get("cwd") == repo_root
