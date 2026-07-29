"""Tests for the local-only, opt-in semantic search module (#4339, #4370)."""

from __future__ import annotations

import json
import logging
import sqlite3
import subprocess
from pathlib import Path

import pytest

from loom_tools import embedders
from loom_tools import semantic_search as ss


class _StubEmbedder:
    """Fixed-vector test double for :class:`loom_tools.embedders.Embedder` (#4370).

    No real ``fastembed``/model download is needed in CI: ``embed()`` looks
    up a vector by whether one of ``vectors_by_key``'s keys appears as a
    substring of the input text (index-time calls embed the doc's
    ``"<title>\\n\\n<body>"``, which contains the source_id), falling back to
    ``query_vector`` (used for the raw query string at query time).
    """

    model_name = "stub-model"

    def __init__(self, vectors_by_key: dict[str, list[float]], query_vector: list[float]) -> None:
        self._vectors_by_key = vectors_by_key
        self._query_vector = query_vector

    def embed(self, text: str) -> list[float]:
        for key, vector in self._vectors_by_key.items():
            if key in text:
                return vector
        return self._query_vector


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


def _embeddings_rows(repo_root: Path) -> list[tuple[str, str, str, bytes]]:
    """Read all rows from the ``embeddings`` table directly (test helper)."""
    conn = sqlite3.connect(str(ss.index_db_path(repo_root)))
    try:
        return conn.execute("SELECT source_type, source_id, model, vector FROM embeddings").fetchall()
    finally:
        conn.close()


@pytest.fixture(autouse=True)
def _clean_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv(ss.SEARCH_ENABLED_ENV, raising=False)
    monkeypatch.delenv(ss.EMBEDDINGS_PROVIDER_ENV, raising=False)


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


# --------------------------------------------------------------------------
# Tier B: pluggable vector embeddings (#4370, follow-up to #4339)
# --------------------------------------------------------------------------


class TestEmbeddingsProviderPrecedence:
    def test_default_is_none(self, repo: Path) -> None:
        assert ss.resolve_embeddings_provider(repo) == "none"

    def test_config_true_enables_without_env(self, repo: Path) -> None:
        _write_config(repo, {"search": {"embeddings": {"provider": "local"}}})
        assert ss.resolve_embeddings_provider(repo) == "local"

    def test_env_overrides_config(self, repo: Path, monkeypatch: pytest.MonkeyPatch) -> None:
        _write_config(repo, {"search": {"embeddings": {"provider": "local"}}})
        monkeypatch.setenv(ss.EMBEDDINGS_PROVIDER_ENV, "none")
        assert ss.resolve_embeddings_provider(repo) == "none"


class TestProviderNoneUnchangedFromTierA:
    """`provider=none` (default): zero behavior change vs. Tier A."""

    def test_never_populates_embeddings_table(self, repo: Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv(ss.SEARCH_ENABLED_ENV, "1")
        monkeypatch.setattr(ss, "_run_gh_pr_list", lambda repo_root, limit=ss.PR_FETCH_LIMIT: [])
        _ignore_search_index(repo)
        _write_sweep_log(repo, 1, "database migration notes\n")

        ss.build_index(repo)

        assert _embeddings_rows(repo) == []

    def test_query_results_are_byte_identical_to_pure_bm25(
        self, repo: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(ss.SEARCH_ENABLED_ENV, "1")
        monkeypatch.setattr(ss, "_run_gh_pr_list", lambda repo_root, limit=ss.PR_FETCH_LIMIT: [])
        _ignore_search_index(repo)
        _write_sweep_log(repo, 101, "token exhaustion failure during dispatch\n")
        _write_sweep_log(repo, 102, "token was rotated; exhaustion of retries too\n")

        ss.build_index(repo)
        results = ss.query_index(repo, "token exhaustion")

        assert [r.source_id for r in results] == ["101", "102"]


class TestEmbeddingsIndexing:
    def test_provider_local_populates_embeddings_keyed_by_model(
        self, repo: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(ss.SEARCH_ENABLED_ENV, "1")
        monkeypatch.setenv(ss.EMBEDDINGS_PROVIDER_ENV, "local")
        monkeypatch.setattr(ss, "_run_gh_pr_list", lambda repo_root, limit=ss.PR_FETCH_LIMIT: [])
        monkeypatch.setattr(
            embedders,
            "create_embedder",
            lambda provider, **kwargs: _StubEmbedder(vectors_by_key={}, query_vector=[0.1, 0.2]),
        )
        _ignore_search_index(repo)
        _write_sweep_log(repo, 55, "some sweep body text\n")

        counts = ss.build_index(repo)
        assert counts["sweeps"] == 1

        rows = _embeddings_rows(repo)
        assert len(rows) == 1
        source_type, source_id, model, vector = rows[0]
        assert (source_type, source_id) == ("sweep", "55")
        assert model == "stub-model"
        assert len(vector) % 4 == 0  # a well-formed packed-float32 blob

    def test_reindex_with_unchanged_watermark_embeds_zero_new_rows(
        self, repo: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(ss.SEARCH_ENABLED_ENV, "1")
        monkeypatch.setenv(ss.EMBEDDINGS_PROVIDER_ENV, "local")
        monkeypatch.setattr(ss, "_run_gh_pr_list", lambda repo_root, limit=ss.PR_FETCH_LIMIT: [])
        monkeypatch.setattr(
            embedders,
            "create_embedder",
            lambda provider, **kwargs: _StubEmbedder(vectors_by_key={}, query_vector=[0.1, 0.2]),
        )
        _ignore_search_index(repo)
        _write_sweep_log(repo, 55, "some sweep body text\n")

        first = ss.build_index(repo)
        assert first["sweeps"] == 1
        assert len(_embeddings_rows(repo)) == 1

        second = ss.build_index(repo)
        assert second["sweeps"] == 0
        assert len(_embeddings_rows(repo)) == 1  # no re-embed of the unchanged document


class TestReciprocalRankFusion:
    def test_fusion_reorders_when_cosine_strongly_favors_a_weaker_bm25_match(
        self, repo: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(ss.SEARCH_ENABLED_ENV, "1")
        monkeypatch.setenv(ss.EMBEDDINGS_PROVIDER_ENV, "local")
        monkeypatch.setattr(ss, "_run_gh_pr_list", lambda repo_root, limit=ss.PR_FETCH_LIMIT: [])
        _ignore_search_index(repo)

        # 301: exact-phrase match -> ranked first by pure BM25.
        _write_sweep_log(repo, 301, "database migration completed successfully\n")
        # 302: both terms present but not adjacent -> weaker BM25 match.
        _write_sweep_log(repo, 302, "migration of the database happened yesterday\n")

        query_vector = [1.0, 0.0]
        vectors_by_key = {
            "301": [0.0, 1.0],  # orthogonal to the query vector -> cosine ~0
            "302": [1.0, 0.0],  # identical to the query vector -> cosine 1
        }
        monkeypatch.setattr(
            embedders,
            "create_embedder",
            lambda provider, **kwargs: _StubEmbedder(vectors_by_key=vectors_by_key, query_vector=query_vector),
        )

        ss.build_index(repo)

        # Sanity check: pure BM25 (provider=none) puts the exact-phrase doc first.
        monkeypatch.setenv(ss.EMBEDDINGS_PROVIDER_ENV, "none")
        bm25_only = ss.query_index(repo, "database migration")
        assert [r.source_id for r in bm25_only] == ["301", "302"]

        # Simulate "embeddings absent for a document" (e.g. 301 was indexed
        # before Tier B was enabled and hasn't changed since) so only 302
        # contributes a cosine rank — otherwise both docs would receive
        # symmetric BM25+cosine contributions and tie.
        conn = sqlite3.connect(str(ss.index_db_path(repo)))
        conn.execute("DELETE FROM embeddings WHERE source_id = '301'")
        conn.commit()
        conn.close()

        # Fusion (provider=local): 302's dominant cosine rank should flip the order.
        monkeypatch.setenv(ss.EMBEDDINGS_PROVIDER_ENV, "local")
        fused = ss.query_index(repo, "database migration")
        assert [r.source_id for r in fused] == ["302", "301"]

    def test_reciprocal_rank_fusion_helper_combines_scores(self) -> None:
        bm25_ranking = [("sweep", "1"), ("sweep", "2")]
        cosine_ranking = [("sweep", "2"), ("sweep", "1")]
        scores = ss._reciprocal_rank_fusion(bm25_ranking, cosine_ranking)
        # "2" is bm25-rank-2 + cosine-rank-1; "1" is bm25-rank-1 + cosine-rank-2.
        # Both accumulate the same pair of terms (1/61 + 1/62) -> tie.
        assert scores[("sweep", "1")] == pytest.approx(scores[("sweep", "2")])
        assert scores[("sweep", "1")] == pytest.approx(1 / 61 + 1 / 62)


class TestMissingEmbeddingDependency:
    def test_hard_errors_at_index_time(self, repo: Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv(ss.SEARCH_ENABLED_ENV, "1")
        monkeypatch.setenv(ss.EMBEDDINGS_PROVIDER_ENV, "local")
        monkeypatch.setattr(ss, "_run_gh_pr_list", lambda repo_root, limit=ss.PR_FETCH_LIMIT: [])
        _ignore_search_index(repo)
        _write_sweep_log(repo, 9, "hello world\n")

        def _boom(provider: str, **kwargs: object) -> None:
            raise embedders.MissingEmbeddingDependencyError(
                f"search.embeddings.provider=local requires fastembed. Install with: {embedders.INSTALL_HINT}"
            )

        monkeypatch.setattr(embedders, "create_embedder", _boom)

        with pytest.raises(embedders.MissingEmbeddingDependencyError, match=r"loom-tools\[search\]"):
            ss.build_index(repo)

    def test_degrades_to_bm25_with_warning_at_query_time(
        self, repo: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
    ) -> None:
        monkeypatch.setenv(ss.SEARCH_ENABLED_ENV, "1")
        monkeypatch.setattr(ss, "_run_gh_pr_list", lambda repo_root, limit=ss.PR_FETCH_LIMIT: [])
        _ignore_search_index(repo)
        _write_sweep_log(repo, 9, "token exhaustion during dispatch\n")
        ss.build_index(repo)  # indexed with provider=none; embeddings table stays empty

        monkeypatch.setenv(ss.EMBEDDINGS_PROVIDER_ENV, "local")

        def _boom(provider: str, **kwargs: object) -> None:
            raise embedders.MissingEmbeddingDependencyError(f"boom: {embedders.INSTALL_HINT}")

        monkeypatch.setattr(embedders, "create_embedder", _boom)

        results = ss.query_index(repo, "token exhaustion")

        assert results and results[0].source_id == "9"
        err = capsys.readouterr().err
        assert "loom-tools[search]" in err


class TestEverythingOffWhenSearchDisabled:
    def test_provider_local_with_search_disabled_is_still_fully_off(
        self, repo: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(ss.EMBEDDINGS_PROVIDER_ENV, "local")
        _write_sweep_log(repo, 1, "hello\n")

        counts = ss.build_index(repo)

        assert counts == {"sweeps": 0, "prs": 0}
        assert not ss.index_dir(repo).exists()


class TestCorruptEmbeddingBlob:
    def test_corrupt_or_short_blob_is_skipped_without_crashing(
        self, repo: Path, monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
    ) -> None:
        monkeypatch.setenv(ss.SEARCH_ENABLED_ENV, "1")
        monkeypatch.setenv(ss.EMBEDDINGS_PROVIDER_ENV, "local")
        monkeypatch.setattr(ss, "_run_gh_pr_list", lambda repo_root, limit=ss.PR_FETCH_LIMIT: [])
        monkeypatch.setattr(
            embedders,
            "create_embedder",
            lambda provider, **kwargs: _StubEmbedder(vectors_by_key={"1": [1.0, 0.0]}, query_vector=[1.0, 0.0]),
        )
        _ignore_search_index(repo)
        _write_sweep_log(repo, 1, "database migration notes\n")

        ss.build_index(repo)

        # Corrupt the stored blob: 3 bytes is not a multiple of 4 (one float32).
        conn = sqlite3.connect(str(ss.index_db_path(repo)))
        conn.execute("UPDATE embeddings SET vector = ? WHERE source_id = '1'", (b"\x01\x02\x03",))
        conn.commit()
        conn.close()

        with caplog.at_level(logging.WARNING):
            results = ss.query_index(repo, "database migration")

        assert results and results[0].source_id == "1"  # degrades to BM25-only, no crash
        assert any(
            "corrupt" in message.lower() or "short" in message.lower() for message in caplog.messages
        )


class TestCosineSimilarityEdgeCases:
    def test_zero_vector_returns_zero_not_a_crash(self) -> None:
        assert ss._cosine_similarity([], []) == 0.0
        assert ss._cosine_similarity([0.0, 0.0], [1.0, 2.0]) == 0.0

    def test_mismatched_dimensions_returns_zero(self) -> None:
        assert ss._cosine_similarity([1.0], [1.0, 2.0]) == 0.0

    def test_identical_vectors_return_one(self) -> None:
        assert ss._cosine_similarity([1.0, 2.0], [1.0, 2.0]) == pytest.approx(1.0)


class TestQueryIndexEmptyIndexNoDivisionByZero:
    def test_empty_index_with_provider_local_returns_no_results(
        self, repo: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(ss.EMBEDDINGS_PROVIDER_ENV, "local")
        assert ss.query_index(repo, "anything") == []
