"""Validation tests for loom-shepherd.sh pre-flight checks.

Validates that loom-shepherd.sh correctly detects unmerged files (merge
conflicts) and exits with a clear error message before attempting to run
the Python shepherd implementation.

Also tests the Python-level dirty-repo check that filters .loom/ runtime files.

Script path: defaults/scripts/loom-shepherd.sh
Related issues: #1747, #2129
"""

from __future__ import annotations

import pathlib
import re
import subprocess
from unittest.mock import patch


SCRIPT_REL_PATH = "defaults/scripts/loom-shepherd.sh"


def _get_script_path() -> pathlib.Path:
    """Find the loom-shepherd.sh script from the repo root."""
    test_dir = pathlib.Path(__file__).resolve().parent
    # tests/ -> loom-tools/ -> repo root
    repo_root = test_dir.parent.parent
    script = repo_root / SCRIPT_REL_PATH
    if not script.exists():
        msg = f"Script not found: {script}"
        raise FileNotFoundError(msg)
    return script


def _read_script() -> str:
    """Read the full script source."""
    return _get_script_path().read_text()


def _setup_git_repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a minimal git repo for testing.

    Returns the repo root path.
    """
    repo = tmp_path / "repo"
    repo.mkdir()
    subprocess.run(
        ["git", "init", "-b", "main", str(repo)],
        capture_output=True,
        check=True,
    )
    subprocess.run(
        ["git", "-C", str(repo), "commit", "--allow-empty", "-m", "init"],
        capture_output=True,
        check=True,
    )
    return repo


def _simulate_unmerged_files(repo: pathlib.Path) -> None:
    """Simulate unmerged (UU) entries in git status output.

    Creates a real merge conflict by making conflicting changes on two
    branches and attempting to merge them.
    """
    # Create a file on main
    test_file = repo / "conflict.py"
    test_file.write_text("original content\n")
    subprocess.run(
        ["git", "-C", str(repo), "add", "conflict.py"],
        capture_output=True,
        check=True,
    )
    subprocess.run(
        ["git", "-C", str(repo), "commit", "-m", "add file"],
        capture_output=True,
        check=True,
    )

    # Create a branch with different content
    subprocess.run(
        ["git", "-C", str(repo), "checkout", "-b", "conflict-branch"],
        capture_output=True,
        check=True,
    )
    test_file.write_text("branch content\n")
    subprocess.run(
        ["git", "-C", str(repo), "add", "conflict.py"],
        capture_output=True,
        check=True,
    )
    subprocess.run(
        ["git", "-C", str(repo), "commit", "-m", "branch change"],
        capture_output=True,
        check=True,
    )

    # Go back to main and make a conflicting change
    subprocess.run(
        ["git", "-C", str(repo), "checkout", "main"],
        capture_output=True,
        check=True,
    )
    test_file.write_text("main content\n")
    subprocess.run(
        ["git", "-C", str(repo), "add", "conflict.py"],
        capture_output=True,
        check=True,
    )
    subprocess.run(
        ["git", "-C", str(repo), "commit", "-m", "main change"],
        capture_output=True,
        check=True,
    )

    # Attempt merge (will fail with conflict)
    subprocess.run(
        ["git", "-C", str(repo), "merge", "conflict-branch"],
        capture_output=True,
    )


def _run_preflight_snippet(repo: pathlib.Path) -> subprocess.CompletedProcess[str]:
    """Run just the pre-flight check portion of loom-shepherd.sh.

    Extracts and runs the unmerged file check logic in the context of the
    given repo, avoiding the need for a real Python shepherd installation.
    """
    snippet = f"""\
set -euo pipefail
REPO_ROOT="{repo}"
unmerged=$(git -C "$REPO_ROOT" status --porcelain 2>/dev/null | grep '^UU' | cut -c4- || true)
if [[ -n "$unmerged" ]]; then
    echo "[ERROR] Cannot run shepherd: repository has unmerged files:" >&2
    echo "$unmerged" | sed 's/^/  /' >&2
    echo "Resolve merge conflicts before running shepherd." >&2
    exit 1
fi
echo "OK"
"""
    return subprocess.run(
        ["bash", "-c", snippet],
        capture_output=True,
        text=True,
    )


class TestScriptStructure:
    """Validate the script contains the pre-flight check."""

    def test_unmerged_check_present(self) -> None:
        source = _read_script()
        assert re.search(r"git.*status --porcelain.*grep.*\^UU", source)

    def test_error_message_present(self) -> None:
        source = _read_script()
        assert "Cannot run shepherd: repository has unmerged files" in source

    def test_resolution_guidance_present(self) -> None:
        source = _read_script()
        assert "Resolve merge conflicts before running shepherd" in source

    def test_error_goes_to_stderr(self) -> None:
        source = _read_script()
        # All three echo lines in the unmerged check should go to stderr
        error_section = source[
            source.index("unmerged=") : source.index("# Try Python")
        ]
        echo_lines = [
            line.strip()
            for line in error_section.splitlines()
            if line.strip().startswith("echo")
        ]
        for line in echo_lines:
            assert line.endswith(">&2"), f"Expected stderr redirect: {line}"

    def test_exits_with_nonzero(self) -> None:
        source = _read_script()
        error_section = source[
            source.index("unmerged=") : source.index("# Try Python")
        ]
        assert "exit 1" in error_section

    def test_check_before_python_execution(self) -> None:
        """Pre-flight check must appear before the Python exec block."""
        source = _read_script()
        unmerged_pos = source.index("unmerged=")
        exec_pos = source.index("exec ")
        assert unmerged_pos < exec_pos

    def test_uses_repo_root_for_git(self) -> None:
        """git command should use -C $REPO_ROOT for correct directory."""
        source = _read_script()
        assert re.search(r'git -C "\$REPO_ROOT" status', source)


class TestUnmergedFileDetection:
    """Validate behavior when unmerged files exist."""

    def test_exits_nonzero_with_conflicts(self, tmp_path: pathlib.Path) -> None:
        repo = _setup_git_repo(tmp_path)
        _simulate_unmerged_files(repo)
        result = _run_preflight_snippet(repo)
        assert result.returncode == 1

    def test_error_message_on_stderr(self, tmp_path: pathlib.Path) -> None:
        repo = _setup_git_repo(tmp_path)
        _simulate_unmerged_files(repo)
        result = _run_preflight_snippet(repo)
        assert "[ERROR] Cannot run shepherd" in result.stderr

    def test_lists_conflicting_files(self, tmp_path: pathlib.Path) -> None:
        repo = _setup_git_repo(tmp_path)
        _simulate_unmerged_files(repo)
        result = _run_preflight_snippet(repo)
        assert "conflict.py" in result.stderr

    def test_resolution_guidance_in_output(self, tmp_path: pathlib.Path) -> None:
        repo = _setup_git_repo(tmp_path)
        _simulate_unmerged_files(repo)
        result = _run_preflight_snippet(repo)
        assert "Resolve merge conflicts" in result.stderr

    def test_nothing_on_stdout(self, tmp_path: pathlib.Path) -> None:
        repo = _setup_git_repo(tmp_path)
        _simulate_unmerged_files(repo)
        result = _run_preflight_snippet(repo)
        assert result.stdout.strip() == ""


class TestCleanRepository:
    """Validate behavior when no conflicts exist."""

    def test_exits_zero_without_conflicts(self, tmp_path: pathlib.Path) -> None:
        repo = _setup_git_repo(tmp_path)
        result = _run_preflight_snippet(repo)
        assert result.returncode == 0

    def test_no_error_output(self, tmp_path: pathlib.Path) -> None:
        repo = _setup_git_repo(tmp_path)
        result = _run_preflight_snippet(repo)
        assert result.stderr == ""

    def test_passes_through(self, tmp_path: pathlib.Path) -> None:
        repo = _setup_git_repo(tmp_path)
        result = _run_preflight_snippet(repo)
        assert result.stdout.strip() == "OK"

    def test_modified_files_not_flagged(self, tmp_path: pathlib.Path) -> None:
        """Modified (M) files should not trigger the check."""
        repo = _setup_git_repo(tmp_path)
        (repo / "modified.py").write_text("new content\n")
        result = _run_preflight_snippet(repo)
        assert result.returncode == 0

    def test_untracked_files_not_flagged(self, tmp_path: pathlib.Path) -> None:
        """Untracked (??) files should not trigger the check."""
        repo = _setup_git_repo(tmp_path)
        (repo / "untracked.py").write_text("content\n")
        result = _run_preflight_snippet(repo)
        assert result.returncode == 0


# ---------------------------------------------------------------------------
# Python-level dirty-repo check tests (issue #2129)
# ---------------------------------------------------------------------------

from loom_tools.shepherd.cli import _check_main_repo_clean, _is_loom_runtime


class TestIsLoomRuntime:
    """Unit tests for _is_loom_runtime helper."""

    def test_untracked_loom_file(self) -> None:
        assert _is_loom_runtime("?? .loom/daemon-state.json") is True

    def test_modified_loom_file(self) -> None:
        assert _is_loom_runtime(" M .loom/config.json") is True

    def test_added_loom_file(self) -> None:
        assert _is_loom_runtime("A  .loom/progress/shepherd-abc.json") is True

    def test_regular_source_file(self) -> None:
        assert _is_loom_runtime(" M src/main.py") is False

    def test_untracked_source_file(self) -> None:
        assert _is_loom_runtime("?? new_file.py") is False

    def test_rename_into_loom(self) -> None:
        assert _is_loom_runtime("R  old.json -> .loom/new.json") is True

    def test_rename_out_of_loom(self) -> None:
        assert _is_loom_runtime("R  .loom/old.json -> new.json") is False

    def test_loom_prefix_not_in_dir(self) -> None:
        """Files named .loom-something (not in .loom/) should NOT be filtered."""
        assert _is_loom_runtime("?? .loom-config") is False

    def test_short_line(self) -> None:
        """Handles pathologically short lines without crashing."""
        assert _is_loom_runtime("??") is False

    def test_node_modules_not_filtered(self) -> None:
        assert _is_loom_runtime("?? node_modules") is False


class TestCheckMainRepoCleanLoomFiltering:
    """Tests that _check_main_repo_clean filters .loom/ runtime files."""

    def _mock_uncommitted(self, files: list[str]):
        """Return a patch that makes get_uncommitted_files return *files*."""
        return patch(
            "loom_tools.shepherd.cli.get_uncommitted_files",
            return_value=files,
        )

    def test_only_loom_files_is_clean(self) -> None:
        """Only .loom/ files -> repo treated as clean."""
        with self._mock_uncommitted(
            ["?? .loom/daemon-state.json", "?? .loom/progress/foo.json"]
        ):
            assert _check_main_repo_clean(pathlib.Path("/fake"), allow_dirty=False) is True

    def test_only_source_files_is_dirty(self) -> None:
        """Only source files -> repo treated as dirty."""
        with self._mock_uncommitted([" M src/main.py"]):
            assert _check_main_repo_clean(pathlib.Path("/fake"), allow_dirty=False) is False

    def test_mix_filters_loom_shows_source(self) -> None:
        """Mix of .loom/ and source files -> dirty, only source files reported."""
        with self._mock_uncommitted(
            ["?? .loom/daemon-state.json", " M src/main.py", "?? .loom/config.json"]
        ):
            assert _check_main_repo_clean(pathlib.Path("/fake"), allow_dirty=False) is False

    def test_empty_list_is_clean(self) -> None:
        """No uncommitted files -> clean."""
        with self._mock_uncommitted([]):
            assert _check_main_repo_clean(pathlib.Path("/fake"), allow_dirty=False) is True

    def test_allow_dirty_with_source_files(self) -> None:
        """With allow_dirty=True, source files still return True."""
        with self._mock_uncommitted([" M src/main.py"]):
            assert _check_main_repo_clean(pathlib.Path("/fake"), allow_dirty=True) is True

    def test_allow_dirty_with_loom_files(self) -> None:
        """With allow_dirty=True and only .loom/ files -> clean (filtered before allow_dirty)."""
        with self._mock_uncommitted(["?? .loom/daemon-state.json"]):
            assert _check_main_repo_clean(pathlib.Path("/fake"), allow_dirty=True) is True

    def test_dot_loom_prefix_file_not_filtered(self) -> None:
        """A file named .loom-config (not in .loom/) should NOT be filtered."""
        with self._mock_uncommitted(["?? .loom-config"]):
            assert _check_main_repo_clean(pathlib.Path("/fake"), allow_dirty=False) is False
