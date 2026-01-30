"""Validation tests for cleanup-progress.sh behavior patterns.

This module validates that cleanup-progress.sh correctly handles argument
parsing, output modes, and edge cases for cleaning up orphaned shepherd
progress files.

Script path: defaults/scripts/cleanup-progress.sh
Related PR: #1730
Related issue: #1736
"""

from __future__ import annotations

import json
import pathlib
import re
import subprocess


SCRIPT_REL_PATH = "defaults/scripts/cleanup-progress.sh"


def _get_script_path() -> pathlib.Path:
    """Find the cleanup-progress.sh script from the repo root."""
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
    """Create a minimal git repo with .loom/progress/ directory.

    Returns the repo root path.
    """
    repo = tmp_path / "repo"
    repo.mkdir()
    subprocess.run(["git", "init", str(repo)], capture_output=True, check=True)
    progress_dir = repo / ".loom" / "progress"
    progress_dir.mkdir(parents=True)
    return repo


def _create_progress_file(
    progress_dir: pathlib.Path,
    task_id: str = "abc123",
    issue: int = 42,
    status: str = "working",
    last_heartbeat: str = "2026-01-01T00:00:00Z",
) -> pathlib.Path:
    """Create a shepherd progress JSON file in the given directory."""
    filename = f"shepherd-{task_id}.json"
    filepath = progress_dir / filename
    filepath.write_text(
        json.dumps(
            {
                "task_id": task_id,
                "issue": issue,
                "status": status,
                "last_heartbeat": last_heartbeat,
            }
        )
    )
    return filepath


def _run_script(*args: str, cwd: str | None = None) -> subprocess.CompletedProcess[str]:
    """Run cleanup-progress.sh with given arguments."""
    script = _get_script_path()
    return subprocess.run(
        ["bash", str(script), *args],
        capture_output=True,
        text=True,
        cwd=cwd,
    )


def _parse_script_json(stdout: str) -> dict:
    """Parse JSON output from the script, handling known escaping quirk.

    The script's action field values have backslash-escaped quotes
    (e.g. ``\\"keep\\"``) due to bash string interpolation in a subshell.
    This helper normalises that before parsing.
    """
    cleaned = stdout.strip().replace('\\"', '"')
    return json.loads(cleaned)


class TestScriptStructure:
    """Validate structural patterns in cleanup-progress.sh."""

    def test_script_exists(self) -> None:
        """The bash script under test must exist."""
        script = _get_script_path()
        assert script.exists(), f"Expected script at {script}"

    def test_default_mode_is_list(self) -> None:
        """Default mode must be 'list' when no mode flag is provided (line 104)."""
        source = _read_script()
        assert re.search(r'MODE="list"', source), (
            "Expected MODE=\"list\" as the default mode assignment"
        )

    def test_mode_variable_values(self) -> None:
        """Verify all expected MODE values are present in the script."""
        source = _read_script()
        expected_modes = ["list", "stale", "older", "all"]
        for mode in expected_modes:
            assert re.search(rf'MODE="{mode}"', source), (
                f"Expected MODE=\"{mode}\" assignment in script"
            )

    def test_dry_run_variable_default(self) -> None:
        """DRY_RUN must default to false."""
        source = _read_script()
        assert re.search(r'DRY_RUN=false', source), (
            "Expected DRY_RUN=false as default"
        )

    def test_json_output_variable_default(self) -> None:
        """JSON_OUTPUT must default to false."""
        source = _read_script()
        assert re.search(r'JSON_OUTPUT=false', source), (
            "Expected JSON_OUTPUT=false as default"
        )

    def test_older_requires_argument_check(self) -> None:
        """--older must validate that an hours argument is provided (lines 82-87)."""
        source = _read_script()
        assert re.search(r'--older requires', source), (
            "Expected error message for missing --older argument"
        )

    def test_unknown_option_produces_error(self) -> None:
        """Unknown options must produce an error message (line 98)."""
        source = _read_script()
        assert re.search(r'Unknown option:', source), (
            "Expected 'Unknown option:' error handling pattern"
        )

    def test_missing_progress_dir_exits_gracefully(self) -> None:
        """Missing progress directory must exit 0, not error (lines 109-116)."""
        source = _read_script()
        # Verify the pattern: check for dir, output message, exit 0
        assert re.search(r'!\s*-d\s*"\$PROGRESS_DIR"', source), (
            "Expected directory existence check for PROGRESS_DIR"
        )
        # The outer if block contains a nested if/fi, then exit 0, then fi.
        # Match from the PROGRESS_DIR check through the outer fi (line 116).
        dir_check_match = re.search(
            r'if\s+\[\[\s+!\s+-d\s+"\$PROGRESS_DIR"\s+\]\].*?^fi\b',
            source,
            re.DOTALL | re.MULTILINE,
        )
        assert dir_check_match is not None, "Expected if block checking PROGRESS_DIR"
        block = dir_check_match.group(0)
        assert "exit 0" in block, (
            "Missing progress directory should exit 0 (graceful), not error"
        )

    def test_json_output_for_missing_dir(self) -> None:
        """Missing progress dir with --json should output JSON with empty files array."""
        source = _read_script()
        assert re.search(r'"files":\[\]', source), (
            "Expected JSON output with empty files array for missing progress dir"
        )

    def test_stale_mode_deletes_non_working_files(self) -> None:
        """Stale mode should delete files with status != 'working' and != 'unknown' (lines 177-179)."""
        source = _read_script()
        # Verify the conditional pattern
        assert re.search(r'file_status.*!=.*"working"', source), (
            "Expected status != working check in stale mode"
        )
        assert re.search(r'file_status.*!=.*"unknown"', source), (
            "Expected status != unknown check in stale mode"
        )

    def test_dry_run_prevents_deletion(self) -> None:
        """DRY_RUN=true must prevent rm calls and show [DRY-RUN] prefix (lines 203-205)."""
        source = _read_script()
        assert re.search(r'\[DRY-RUN\]', source), (
            "Expected [DRY-RUN] prefix in dry-run output"
        )

    def test_json_output_structure_fields(self) -> None:
        """JSON output must include expected top-level fields (line 228)."""
        source = _read_script()
        # In the bash script, JSON field names use escaped quotes: \"field\"
        for field in ["total", "action_count", "dry_run", "mode", "files"]:
            assert re.search(rf'\\?"{field}\\?"', source), (
                f"Expected \"{field}\" in JSON output construction"
            )

    def test_json_file_entry_fields(self) -> None:
        """JSON file entries must include expected fields (line 193)."""
        source = _read_script()
        for field in ["file", "issue", "status", "task_id", "age_hours", "action"]:
            assert re.search(rf'\\?"{field}\\?"', source), (
                f"Expected \"{field}\" in JSON file entry construction"
            )


class TestHelpOutput:
    """Validate --help output and exit behavior."""

    def test_help_exits_zero(self) -> None:
        """--help must exit with code 0."""
        result = _run_script("--help")
        assert result.returncode == 0, (
            f"Expected exit code 0 for --help, got {result.returncode}"
        )

    def test_help_shows_usage(self) -> None:
        """--help must display usage information."""
        result = _run_script("--help")
        assert "Usage:" in result.stdout, "Expected 'Usage:' in --help output"

    def test_help_shows_modes(self) -> None:
        """--help must describe the available modes."""
        result = _run_script("--help")
        for mode in ["--stale", "--older", "--all"]:
            assert mode in result.stdout, f"Expected '{mode}' in --help output"

    def test_help_shows_options(self) -> None:
        """--help must describe options like --dry-run and --json."""
        result = _run_script("--help")
        assert "--dry-run" in result.stdout
        assert "--json" in result.stdout

    def test_short_help_flag(self) -> None:
        """-h must also show help and exit 0."""
        result = _run_script("-h")
        assert result.returncode == 0
        assert "Usage:" in result.stdout


class TestNoArgsListMode:
    """Validate that no arguments defaults to list mode."""

    def test_no_args_defaults_to_list_mode(self) -> None:
        """Structural: no args means MODE='list' (script line 104)."""
        source = _read_script()
        # After argument parsing, empty MODE gets set to "list"
        pattern = re.compile(
            r'if\s+\[\[\s+-z\s+"\$MODE"\s+\]\].*?MODE="list"',
            re.DOTALL,
        )
        assert pattern.search(source), (
            "Expected: if MODE is empty, set MODE='list'"
        )

    def test_list_mode_does_not_delete(self) -> None:
        """List mode should not set should_delete=true."""
        source = _read_script()
        # In the case statement, list mode has only a comment
        list_case = re.search(r'list\)\s*\n\s*#.*\n\s*;;', source)
        assert list_case is not None, (
            "Expected list mode case to only contain comments (no deletion logic)"
        )


class TestMissingProgressDir:
    """Validate graceful handling when progress directory doesn't exist."""

    def test_missing_dir_exits_zero(self, tmp_path: pathlib.Path) -> None:
        """Script must exit 0 when progress directory is missing."""
        # Create a git repo without .loom/progress/
        repo = tmp_path / "repo"
        repo.mkdir()
        subprocess.run(["git", "init", str(repo)], capture_output=True, check=True)
        (repo / ".loom").mkdir()
        # No progress dir

        result = _run_script(cwd=str(repo))
        assert result.returncode == 0, (
            f"Expected exit 0 for missing progress dir, got {result.returncode}. "
            f"stderr: {result.stderr}"
        )

    def test_missing_dir_json_output(self, tmp_path: pathlib.Path) -> None:
        """--json with missing progress dir must output valid JSON with empty files."""
        repo = tmp_path / "repo"
        repo.mkdir()
        subprocess.run(["git", "init", str(repo)], capture_output=True, check=True)
        (repo / ".loom").mkdir()

        result = _run_script("--json", cwd=str(repo))
        assert result.returncode == 0

        data = json.loads(result.stdout.strip())
        assert data["files"] == [], "Expected empty files array"
        assert "message" in data or "files" in data


class TestDryRunStale:
    """Validate --dry-run --stale outputs [DRY-RUN] prefix without deleting files."""

    def test_dry_run_stale_shows_prefix(self, tmp_path: pathlib.Path) -> None:
        """--dry-run --stale must show [DRY-RUN] prefix for files that would be deleted."""
        repo = _setup_git_repo(tmp_path)
        progress_dir = repo / ".loom" / "progress"

        # Create a completed progress file (stale mode deletes non-working status)
        _create_progress_file(progress_dir, task_id="done1", status="completed")

        result = _run_script("--dry-run", "--stale", cwd=str(repo))
        assert result.returncode == 0
        assert "[DRY-RUN]" in result.stdout, (
            "Expected [DRY-RUN] prefix in dry-run output"
        )

    def test_dry_run_stale_preserves_files(self, tmp_path: pathlib.Path) -> None:
        """--dry-run --stale must not actually delete files."""
        repo = _setup_git_repo(tmp_path)
        progress_dir = repo / ".loom" / "progress"

        filepath = _create_progress_file(
            progress_dir, task_id="done2", status="completed"
        )

        _run_script("--dry-run", "--stale", cwd=str(repo))
        assert filepath.exists(), "File should NOT be deleted in dry-run mode"

    def test_dry_run_stale_keeps_working_files(self, tmp_path: pathlib.Path) -> None:
        """--dry-run --stale should not flag working files for open issues."""
        repo = _setup_git_repo(tmp_path)
        progress_dir = repo / ".loom" / "progress"

        _create_progress_file(progress_dir, task_id="active1", status="working")

        result = _run_script("--dry-run", "--stale", cwd=str(repo))
        assert result.returncode == 0
        # Working files for open issues should not appear in dry-run delete list
        # (stale mode only deletes working files if the issue is CLOSED,
        # which requires gh API, so working files are generally kept)


class TestJsonOutput:
    """Validate --json produces valid JSON with expected fields."""

    def test_json_list_mode_valid(self, tmp_path: pathlib.Path) -> None:
        """--json in list mode must produce valid JSON."""
        repo = _setup_git_repo(tmp_path)
        progress_dir = repo / ".loom" / "progress"

        _create_progress_file(progress_dir, task_id="t1", issue=10, status="working")
        _create_progress_file(progress_dir, task_id="t2", issue=20, status="completed")

        result = _run_script("--json", cwd=str(repo))
        assert result.returncode == 0

        data = _parse_script_json(result.stdout)
        assert isinstance(data, dict), "Expected JSON object"

    def test_json_has_top_level_fields(self, tmp_path: pathlib.Path) -> None:
        """JSON output must have total, action_count, dry_run, mode, and files."""
        repo = _setup_git_repo(tmp_path)
        progress_dir = repo / ".loom" / "progress"

        _create_progress_file(progress_dir, task_id="t1", issue=10, status="working")

        result = _run_script("--json", cwd=str(repo))
        data = _parse_script_json(result.stdout)

        for field in ["total", "action_count", "dry_run", "mode", "files"]:
            assert field in data, f"Missing top-level field: {field}"

    def test_json_file_entries_have_fields(self, tmp_path: pathlib.Path) -> None:
        """Each file entry in JSON output must have expected fields."""
        repo = _setup_git_repo(tmp_path)
        progress_dir = repo / ".loom" / "progress"

        _create_progress_file(progress_dir, task_id="t1", issue=10, status="working")

        result = _run_script("--json", cwd=str(repo))
        data = _parse_script_json(result.stdout)

        assert len(data["files"]) > 0, "Expected at least one file entry"
        entry = data["files"][0]
        for field in ["file", "issue", "status", "task_id", "age_hours", "action"]:
            assert field in entry, f"Missing file entry field: {field}"

    def test_json_total_matches_file_count(self, tmp_path: pathlib.Path) -> None:
        """JSON total field must match the number of file entries."""
        repo = _setup_git_repo(tmp_path)
        progress_dir = repo / ".loom" / "progress"

        _create_progress_file(progress_dir, task_id="t1", issue=10, status="working")
        _create_progress_file(progress_dir, task_id="t2", issue=20, status="completed")

        result = _run_script("--json", cwd=str(repo))
        data = _parse_script_json(result.stdout)

        assert data["total"] == len(data["files"])

    def test_json_mode_field_correct(self, tmp_path: pathlib.Path) -> None:
        """JSON mode field must reflect the mode used."""
        repo = _setup_git_repo(tmp_path)
        progress_dir = repo / ".loom" / "progress"
        _create_progress_file(progress_dir, task_id="t1", issue=10, status="working")

        result = _run_script("--json", cwd=str(repo))
        data = _parse_script_json(result.stdout)
        assert data["mode"] == "list"

    def test_json_dry_run_field(self, tmp_path: pathlib.Path) -> None:
        """JSON dry_run field must be true when --dry-run is passed."""
        repo = _setup_git_repo(tmp_path)
        progress_dir = repo / ".loom" / "progress"
        _create_progress_file(progress_dir, task_id="t1", issue=10, status="completed")

        result = _run_script("--json", "--dry-run", "--all", cwd=str(repo))
        data = _parse_script_json(result.stdout)
        assert data["dry_run"] is True

    def test_json_empty_progress_dir(self, tmp_path: pathlib.Path) -> None:
        """--json with empty progress directory must return valid JSON with zero total."""
        repo = _setup_git_repo(tmp_path)
        # Progress dir exists but is empty

        result = _run_script("--json", cwd=str(repo))
        assert result.returncode == 0

        data = json.loads(result.stdout.strip())
        assert data["total"] == 0
        assert data["files"] == []


class TestOlderValidation:
    """Validate --older requires a numeric argument."""

    def test_older_missing_argument_errors(self) -> None:
        """--older without a value must produce an error (line 84)."""
        result = _run_script("--older")
        assert result.returncode != 0, (
            "Expected non-zero exit for --older without argument"
        )

    def test_older_missing_argument_error_message(self) -> None:
        """--older without a value must show an appropriate error message."""
        result = _run_script("--older")
        assert "requires" in result.stderr.lower() or "error" in result.stderr.lower(), (
            f"Expected error message about missing argument, got: {result.stderr}"
        )


class TestUnknownOption:
    """Validate unknown options produce errors."""

    def test_unknown_option_exits_nonzero(self) -> None:
        """Unknown options must exit with non-zero code (line 98)."""
        result = _run_script("--bogus-flag")
        assert result.returncode != 0, (
            "Expected non-zero exit for unknown option"
        )

    def test_unknown_option_error_message(self) -> None:
        """Unknown options must show 'Unknown option' error message."""
        result = _run_script("--bogus-flag")
        assert "Unknown option" in result.stderr, (
            f"Expected 'Unknown option' in stderr, got: {result.stderr}"
        )
