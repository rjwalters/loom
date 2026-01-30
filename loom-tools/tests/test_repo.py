"""Tests for loom_tools.common.repo."""

from __future__ import annotations

import pathlib

import pytest

import loom_tools.common.repo as repo_mod
from loom_tools.common.repo import find_repo_root


@pytest.fixture(autouse=True)
def _clear_cache():
    """Reset the module-level cache between tests."""
    repo_mod._cached_root = None
    yield
    repo_mod._cached_root = None


def _make_repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a minimal repo structure with .git/ dir and .loom/ dir."""
    (tmp_path / ".git").mkdir()
    (tmp_path / ".loom").mkdir()
    return tmp_path


def test_find_repo_root_basic(tmp_path: pathlib.Path) -> None:
    root = _make_repo(tmp_path)
    assert find_repo_root(start=root) == root


def test_find_repo_root_from_subdir(tmp_path: pathlib.Path) -> None:
    root = _make_repo(tmp_path)
    subdir = root / "src" / "deep"
    subdir.mkdir(parents=True)
    assert find_repo_root(start=subdir) == root


def test_find_repo_root_caches(tmp_path: pathlib.Path) -> None:
    root = _make_repo(tmp_path)
    first = find_repo_root(start=root)
    second = find_repo_root(start=root)
    assert first is second


def test_find_repo_root_worktree(tmp_path: pathlib.Path) -> None:
    """Simulate a worktree where .git is a file pointing to the main repo."""
    # Main repo
    main = tmp_path / "main-repo"
    main.mkdir()
    git_dir = main / ".git"
    git_dir.mkdir()
    (main / ".loom").mkdir()

    # Worktree
    wt = tmp_path / "worktree"
    wt.mkdir()
    wt_gitdir = git_dir / "worktrees" / "issue-42"
    wt_gitdir.mkdir(parents=True)

    # .git file in worktree points to the worktree gitdir
    (wt / ".git").write_text(f"gitdir: {wt_gitdir}\n")

    assert find_repo_root(start=wt) == main


def test_find_repo_root_no_repo(tmp_path: pathlib.Path) -> None:
    with pytest.raises(FileNotFoundError):
        find_repo_root(start=tmp_path)


def test_find_repo_root_git_without_loom(tmp_path: pathlib.Path) -> None:
    """A .git dir without .loom/ should not match."""
    (tmp_path / ".git").mkdir()
    with pytest.raises(FileNotFoundError):
        find_repo_root(start=tmp_path)
