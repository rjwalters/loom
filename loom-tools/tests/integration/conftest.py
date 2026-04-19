"""Shared fixtures for Gitea integration tests.

Requires a running Gitea instance (see tests/integration/docker-compose.yml).
Tests are skipped automatically when Gitea is not reachable.

Environment variables:
    GITEA_URL   - Gitea base URL (default: http://localhost:3000)
    GITEA_TOKEN - API token for authentication
"""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
from pathlib import Path
from typing import Any, Generator

import pytest
import requests


# ---------------------------------------------------------------------------
# Markers
# ---------------------------------------------------------------------------

def pytest_configure(config: Any) -> None:
    """Register the ``integration`` marker."""
    config.addinivalue_line(
        "markers",
        "integration: marks tests as integration tests requiring a Gitea instance",
    )


# ---------------------------------------------------------------------------
# Skip logic
# ---------------------------------------------------------------------------

_GITEA_URL = os.environ.get("GITEA_URL", "http://localhost:3000")
_GITEA_TOKEN = os.environ.get("GITEA_TOKEN", "")


def _gitea_reachable() -> bool:
    """Return True if Gitea API is reachable."""
    if not _GITEA_TOKEN:
        return False
    try:
        resp = requests.get(
            f"{_GITEA_URL}/api/v1/version",
            timeout=5,
        )
        return resp.status_code == 200
    except (requests.ConnectionError, requests.Timeout):
        return False


_GITEA_AVAILABLE = _gitea_reachable()

_SKIP_REASON = "Gitea not reachable (set GITEA_URL and GITEA_TOKEN, run docker compose up)"


def pytest_collection_modifyitems(
    config: Any, items: list[Any],
) -> None:
    """Skip all integration tests when Gitea is not available."""
    if _GITEA_AVAILABLE:
        return
    skip_marker = pytest.mark.skip(reason=_SKIP_REASON)
    for item in items:
        # Only skip tests in this integration package
        if "integration" in str(item.fspath):
            item.add_marker(skip_marker)


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

def _require_gitea() -> None:
    """Skip the test if Gitea is not available."""
    if not _GITEA_AVAILABLE:
        pytest.skip(_SKIP_REASON)


@pytest.fixture(scope="session")
def gitea_url() -> str:
    """Return the Gitea base URL."""
    _require_gitea()
    return _GITEA_URL


@pytest.fixture(scope="session")
def gitea_token() -> str:
    """Return the Gitea API token."""
    _require_gitea()
    return _GITEA_TOKEN


@pytest.fixture(scope="session")
def gitea_nwo() -> str:
    """Return the test repository owner/name."""
    return os.environ.get("GITEA_REPO", "loom-test/test-repo")


@pytest.fixture(scope="session")
def gitea_api(gitea_url: str, gitea_token: str) -> requests.Session:
    """Return an authenticated requests.Session for direct API calls."""
    session = requests.Session()
    session.headers.update({
        "Authorization": f"token {gitea_token}",
        "Content-Type": "application/json",
        "Accept": "application/json",
    })
    session.base_url = gitea_url  # type: ignore[attr-defined]
    return session


@pytest.fixture(scope="session")
def gitea_forge(
    gitea_url: str,
    gitea_token: str,
    gitea_nwo: str,
    tmp_path_factory: pytest.TempPathFactory,
) -> Generator:
    """Create a GiteaForge instance pointed at the test Gitea server.

    Sets up a temporary git repo with a remote pointing at the Gitea test
    repository so that ``get_repo_nwo()`` works correctly.
    """
    from loom_tools.common.gitea import GiteaForge

    # Create a temporary directory with a git remote matching the Gitea repo
    tmp_dir = tmp_path_factory.mktemp("gitea-forge")

    # Initialize a git repo with the correct remote
    subprocess.run(
        ["git", "init"],
        cwd=tmp_dir,
        capture_output=True,
        check=True,
    )
    subprocess.run(
        ["git", "remote", "add", "origin", f"{gitea_url}/{gitea_nwo}.git"],
        cwd=tmp_dir,
        capture_output=True,
        check=True,
    )

    # Write a .loom/config.json with Gitea forge config
    loom_dir = tmp_dir / ".loom"
    loom_dir.mkdir()
    config = {
        "forge": {
            "type": "gitea",
            "gitea": {
                "url": gitea_url,
                "token": gitea_token,
            },
        },
    }
    (loom_dir / "config.json").write_text(json.dumps(config))

    # Set env var for token (GiteaForge checks GITEA_TOKEN)
    old_token = os.environ.get("GITEA_TOKEN")
    os.environ["GITEA_TOKEN"] = gitea_token

    forge = GiteaForge(cwd=tmp_dir)
    yield forge

    # Restore env
    if old_token is not None:
        os.environ["GITEA_TOKEN"] = old_token
    elif "GITEA_TOKEN" in os.environ:
        del os.environ["GITEA_TOKEN"]


@pytest.fixture()
def create_test_branch(
    gitea_url: str,
    gitea_token: str,
    gitea_nwo: str,
) -> Generator:
    """Factory fixture to create branches in the test repo.

    Returns a callable that creates a branch and returns its name.
    Cleans up created branches after the test.
    """
    created_branches: list[str] = []

    def _create(branch_name: str, from_ref: str = "main") -> str:
        resp = requests.post(
            f"{gitea_url}/api/v1/repos/{gitea_nwo}/branches",
            headers={
                "Authorization": f"token {gitea_token}",
                "Content-Type": "application/json",
            },
            json={
                "new_branch_name": branch_name,
                "old_branch_name": from_ref,
            },
            timeout=10,
        )
        resp.raise_for_status()
        created_branches.append(branch_name)
        return branch_name

    yield _create

    # Cleanup: delete created branches
    for branch in created_branches:
        requests.delete(
            f"{gitea_url}/api/v1/repos/{gitea_nwo}/branches/{branch}",
            headers={"Authorization": f"token {gitea_token}"},
            timeout=10,
        )
