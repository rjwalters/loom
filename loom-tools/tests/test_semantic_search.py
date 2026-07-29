"""Tests for the local-only, opt-in semantic search module (#4339)."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

from loom_tools import semantic_search as ss


def _init_repo(tmp_path: Path) -> Path:
    """Init a git repo at tmp_path with a minimal .loom/logs/ dir."""
    subprocess.run(["git", "init", "-q"], cwd=tmp_path, check=True, capture_output=True)
    subprocess.run(
        ["git", "config", "user.email", "test@example.com"], cwd=tmp_path, check=True, capture_output=True
    )
    subprocess.run(["git", "config", "user.name", "Test"], cwd=tmp_path, check=True, capture_output=True)
    (tmp_path / ".loom" / "logs").mkdir(parents=True)
    return tmp_path


def _write_config(repo_root: Path, config: dict) -> None:
    (repo_root / ".loom").mkdir(exist_ok=True)
    (repo_root / ".loom" / "config.json").write_text(json.dumps(config))


def _write_sweep_log(repo_root: Path, issue: int, body: str) -> Path:
    path = repo_root / ".loom" / "logs" / f"sweep-issue-{issue}.log"
    path.write_text(body)
    return path


def _ignore_search_index(repo_root: Path) -> None:
    """Add the standard gitignore entry so the gitignore-or-refuse guard passes.

    Real repos already ship this entry (see .gitignore); tests that exercise
    indexing (not the guard itself) need it present so build_index proceeds.
    """
    (repo_root / ".gitignore").write_text(".loom/search-index/\n")


@pytest.fixture(autouse=True)
def _clean_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv(ss.SEARCH_ENABLED_ENV, raising=False)


@pytest.fixture()
def repo(tmp_path: Path) -> Path:
    return _init_repo(tmp_path)


class TestDisabledByDefault:
    def test_no_config_no_env_is_disabled(self, repo: Path) -> None:
        assert ss.is_search_enabled(repo) is False

    def test_index_is_noop_when_disabled(self, repo: Path) -> None:
        _write_sweep_log(repo, 1, "some sweep summary text")
        counts = ss.build_index(repo)
        assert counts == {"sweeps": 0, "prs": 0}
        assert not ss.index_dir(repo).exists()

    def test_query_falls_back_to_grep_with_note(
        self, repo: Path, capsys: pytest.CaptureFixture[str]
    ) -> None:
        _write_sweep_log(repo, 7, "line one\nTOKEN EXHAUSTION detected\nline three\n")
        rc = ss.main(["token exhaustion", "--repo-root", str(repo)])
        assert rc == 0
        out = capsys.readouterr().out
        assert "disabled" in out.lower()
        assert "sweep-issue-7.log" in out


class TestEnvOverConfigPrecedence:
    def test_env_true_overrides_config_false(self, repo: Path, monkeypatch: pytest.MonkeyPatch) -> None:
        _write_config(repo, {"search": {"enabled": False}})
        monkeypatch.setenv(ss.SEARCH_ENABLED_ENV, "1")
        assert ss.is_search_enabled(repo) is True

    def test_env_false_overrides_config_true(self, repo: Path, monkeypatch: pytest.MonkeyPatch) -> None:
        _write_config(repo, {"search": {"enabled": True}})
        monkeypatch.setenv(ss.SEARCH_ENABLED_ENV, "0")
        assert ss.is_search_enabled(repo) is False

    def test_config_true_enables_without_env(self, repo: Path) -> None:
        _write_config(repo, {"search": {"enabled": True}})
        assert ss.is_search_enabled(repo) is True


class TestIndexingAndRanking:
    def test_indexes_fixture_sweep_logs_and_ranks_exact_phrase_first(
        self, repo: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(ss.SEARCH_ENABLED_ENV, "1")
        monkeypatch.setattr(ss, "_run_gh_pr_list", lambda repo_root, limit=ss.PR_FETCH_LIMIT: [])
        _ignore_search_index(repo)

        _write_sweep_log(
            repo, 101, "sweep summary: hit a token exhaustion failure during dispatch\n"
        )
        _write_sweep_log(
            repo, 102, "sweep summary: an unrelated token was rotated; exhaustion of retries too\n"
        )

        counts = ss.build_index(repo)
        assert counts["sweeps"] == 2
        assert ss.index_db_path(repo).exists()

        results = ss.query_index(repo, "token exhaustion")
        assert results, "expected at least one hit"
        assert results[0].source_id == "101"
        ids_in_order = [r.source_id for r in results]
        assert ids_in_order.index("101") < ids_in_order.index("102")

    def test_incremental_reindex_indexes_zero_new_rows(
        self, repo: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(ss.SEARCH_ENABLED_ENV, "1")
        monkeypatch.setattr(ss, "_run_gh_pr_list", lambda repo_root, limit=ss.PR_FETCH_LIMIT: [])
        _ignore_search_index(repo)

        _write_sweep_log(repo, 5, "first pass summary text\n")
        first = ss.build_index(repo)
        assert first["sweeps"] == 1

        second = ss.build_index(repo)
        assert second["sweeps"] == 0
        assert second["prs"] == 0

    def test_empty_index_query_returns_no_results_not_traceback(self, repo: Path) -> None:
        assert ss.query_index(repo, "anything") == []


class TestPrIngestMocked:
    def test_pr_ingest_uses_mocked_gh_and_is_queryable(
        self, repo: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(ss.SEARCH_ENABLED_ENV, "1")
        _ignore_search_index(repo)

        fake_prs = [
            {
                "number": 4339,
                "title": "feat: local-only opt-in semantic search",
                "body": "Adds loom-search with FTS5 BM25 ranking.",
                "mergedAt": "2026-07-29T00:00:00Z",
                "url": "https://github.com/rjwalters/loom/pull/4339",
            }
        ]

        def _fake_gh_pr_list(repo_root: Path, limit: int = ss.PR_FETCH_LIMIT) -> list[dict]:
            assert repo_root == repo
            return fake_prs

        monkeypatch.setattr(ss, "_run_gh_pr_list", _fake_gh_pr_list)

        counts = ss.build_index(repo)
        assert counts["prs"] == 1

        results = ss.query_index(repo, "semantic search")
        assert any(r.source_type == "pr" and r.source_id == "4339" for r in results)

        # Second run with the same fixture data indexes nothing new.
        counts2 = ss.build_index(repo)
        assert counts2["prs"] == 0

    def test_gh_failure_does_not_raise(self, repo: Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv(ss.SEARCH_ENABLED_ENV, "1")
        _ignore_search_index(repo)

        def _boom(repo_root: Path, limit: int = ss.PR_FETCH_LIMIT) -> list[dict]:
            return []

        monkeypatch.setattr(ss, "_run_gh_pr_list", _boom)
        counts = ss.build_index(repo)
        assert counts["prs"] == 0


class TestGitignoreOrRefuseGuard:
    def test_untracked_and_unignored_dest_refuses(self, repo: Path) -> None:
        dest = repo / ".loom" / "search-index"
        with pytest.raises(ss.IndexDestinationNotIgnoredError):
            ss.guard_index_dest_not_tracked(dest)

    def test_ignored_dest_proceeds(self, repo: Path) -> None:
        (repo / ".gitignore").write_text(".loom/search-index/\n")
        dest = repo / ".loom" / "search-index"
        ss.guard_index_dest_not_tracked(dest)  # must not raise

    def test_dest_outside_any_repo_proceeds(self, tmp_path: Path) -> None:
        outside = tmp_path / "not-a-repo" / "search-index"
        ss.guard_index_dest_not_tracked(outside)  # must not raise

    def test_build_index_refuses_when_dest_untracked_and_unignored(
        self, repo: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(ss.SEARCH_ENABLED_ENV, "1")
        monkeypatch.setattr(ss, "_run_gh_pr_list", lambda repo_root, limit=ss.PR_FETCH_LIMIT: [])
        with pytest.raises(ss.IndexDestinationNotIgnoredError):
            ss.build_index(repo)

    def test_build_index_proceeds_when_dest_ignored(
        self, repo: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        (repo / ".gitignore").write_text(".loom/search-index/\n")
        monkeypatch.setenv(ss.SEARCH_ENABLED_ENV, "1")
        monkeypatch.setattr(ss, "_run_gh_pr_list", lambda repo_root, limit=ss.PR_FETCH_LIMIT: [])
        counts = ss.build_index(repo)
        assert ss.index_db_path(repo).exists()
        assert counts == {"sweeps": 0, "prs": 0}


class TestFtsQueryBuilder:
    def test_multi_term_query_includes_phrase_and_and_clause(self) -> None:
        expr = ss._build_fts_query("token exhaustion")
        assert '"token exhaustion"' in expr
        assert "token AND exhaustion" in expr

    def test_single_term_query_is_just_the_phrase(self) -> None:
        expr = ss._build_fts_query("token")
        assert expr == '"token"'


class TestCliIndexCommand:
    def test_index_command_noop_message_when_disabled(
        self, repo: Path, capsys: pytest.CaptureFixture[str]
    ) -> None:
        rc = ss.main(["index", "--repo-root", str(repo)])
        assert rc == 0
        assert not ss.index_dir(repo).exists()
        out = capsys.readouterr().out
        assert "disabled" in out.lower()

    def test_index_command_builds_when_enabled(
        self, repo: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
    ) -> None:
        monkeypatch.setenv(ss.SEARCH_ENABLED_ENV, "1")
        monkeypatch.setattr(ss, "_run_gh_pr_list", lambda repo_root, limit=ss.PR_FETCH_LIMIT: [])
        _ignore_search_index(repo)
        _write_sweep_log(repo, 9, "hello world\n")
        rc = ss.main(["index", "--repo-root", str(repo)])
        assert rc == 0
        assert ss.index_db_path(repo).exists()
        out = capsys.readouterr().out
        assert "Indexed" in out
