"""Tests for loom_tools.tokens.paths — the shared-pool resolver (issue #3938).

The autouse ``_isolate_home_master`` fixture (conftest) pins
``LOOM_SHARED_TOKENS_DIR`` at a non-existent tmp path, so unless a test opts in
by pointing it at a materialized pool, the shared fallback is effectively empty.
"""

from __future__ import annotations

from pathlib import Path

from loom_tools.tokens.paths import (
    has_token_files,
    per_repo_tokens_dir,
    resolve_tokens_dir,
    shared_tokens_dir,
)


def _make_pool(dir_path: Path, names: list[str]) -> Path:
    dir_path.mkdir(parents=True, exist_ok=True)
    for name in names:
        (dir_path / f"{name}.token").write_text("sk-ant-oat01-fake", encoding="utf-8")
    return dir_path


# ---------- per_repo_tokens_dir ----------


def test_per_repo_tokens_dir_shape(tmp_path):
    assert per_repo_tokens_dir(tmp_path) == tmp_path / ".loom" / "tokens"


# ---------- shared_tokens_dir ----------


def test_shared_dir_from_env(tmp_path, monkeypatch):
    monkeypatch.setenv("LOOM_SHARED_TOKENS_DIR", str(tmp_path / "pool"))
    assert shared_tokens_dir() == tmp_path / "pool"


def test_shared_dir_empty_env_disables(monkeypatch):
    monkeypatch.setenv("LOOM_SHARED_TOKENS_DIR", "")
    assert shared_tokens_dir() is None


def test_shared_dir_default_is_home(monkeypatch):
    monkeypatch.delenv("LOOM_SHARED_TOKENS_DIR", raising=False)
    assert shared_tokens_dir() == Path.home() / ".loom" / "tokens"


def test_shared_dir_expands_tilde(monkeypatch):
    monkeypatch.setenv("LOOM_SHARED_TOKENS_DIR", "~/custom-pool")
    assert shared_tokens_dir() == Path.home() / "custom-pool"


# ---------- has_token_files ----------


def test_has_token_files_true(tmp_path):
    _make_pool(tmp_path / "p", ["a"])
    assert has_token_files(tmp_path / "p") is True


def test_has_token_files_false_when_missing(tmp_path):
    assert has_token_files(tmp_path / "nope") is False


def test_has_token_files_false_when_only_bookkeeping(tmp_path):
    d = tmp_path / "p"
    d.mkdir()
    (d / ".bad_tokens").write_text("x", encoding="utf-8")
    (d / "index.json").write_text("{}", encoding="utf-8")
    assert has_token_files(d) is False


# ---------- resolve_tokens_dir ----------


def test_resolve_prefers_per_repo(tmp_path, monkeypatch):
    repo = tmp_path / "repo"
    per_repo = _make_pool(repo / ".loom" / "tokens", ["r1"])
    shared = _make_pool(tmp_path / "shared", ["s1"])
    monkeypatch.setenv("LOOM_SHARED_TOKENS_DIR", str(shared))
    assert resolve_tokens_dir(repo) == per_repo


def test_resolve_falls_back_to_shared_when_per_repo_absent(tmp_path, monkeypatch):
    repo = tmp_path / "repo"
    repo.mkdir()
    shared = _make_pool(tmp_path / "shared", ["s1", "s2"])
    monkeypatch.setenv("LOOM_SHARED_TOKENS_DIR", str(shared))
    assert resolve_tokens_dir(repo) == shared


def test_resolve_falls_back_when_per_repo_empty(tmp_path, monkeypatch):
    repo = tmp_path / "repo"
    (repo / ".loom" / "tokens").mkdir(parents=True)  # exists but no *.token
    shared = _make_pool(tmp_path / "shared", ["s1"])
    monkeypatch.setenv("LOOM_SHARED_TOKENS_DIR", str(shared))
    assert resolve_tokens_dir(repo) == shared


def test_resolve_returns_per_repo_when_nothing_anywhere(tmp_path, monkeypatch):
    repo = tmp_path / "repo"
    repo.mkdir()
    # Shared points at a non-existent dir (default conftest behavior too).
    monkeypatch.setenv("LOOM_SHARED_TOKENS_DIR", str(tmp_path / "empty-shared"))
    assert resolve_tokens_dir(repo) == repo / ".loom" / "tokens"


def test_resolve_ignores_shared_when_disabled(tmp_path, monkeypatch):
    repo = tmp_path / "repo"
    repo.mkdir()
    # A shared pool exists on disk, but the operator disabled the fallback.
    _make_pool(tmp_path / "shared", ["s1"])
    monkeypatch.setenv("LOOM_SHARED_TOKENS_DIR", "")
    assert resolve_tokens_dir(repo) == repo / ".loom" / "tokens"
