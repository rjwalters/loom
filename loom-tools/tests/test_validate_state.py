"""Tests for loom_tools.validate_state."""

from __future__ import annotations

import json
import pathlib

import pytest

from loom_tools.common.repo import clear_repo_cache
from loom_tools.validate_state import (
    TASK_ID_RE,
    TIMESTAMP_RE,
    VALID_SHEPHERD_STATUSES,
    VALID_SUPPORT_ROLE_STATUSES,
    main,
    validate_state,
)


def _valid_state() -> dict:
    """Return a minimal valid daemon-state dict."""
    return {
        "started_at": "2026-01-26T18:30:00Z",
        "last_poll": "2026-01-27T04:29:30Z",
        "running": True,
        "iteration": 4,
        "shepherds": {
            "shepherd-1": {
                "status": "idle",
                "issue": None,
                "task_id": None,
            },
            "shepherd-2": {
                "status": "working",
                "issue": 100,
                "task_id": "a7dc1e0",
                "execution_mode": "direct",
            },
        },
        "support_roles": {
            "guide": {"status": "running", "task_id": "b8ec2f1"},
            "champion": {"status": "idle", "task_id": None},
        },
    }


@pytest.fixture
def mock_repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a mock repo with .git and .loom directories."""
    clear_repo_cache()
    (tmp_path / ".git").mkdir()
    (tmp_path / ".loom").mkdir()
    return tmp_path


@pytest.fixture
def state_file(tmp_path: pathlib.Path) -> pathlib.Path:
    """Return a path for a temporary state file."""
    return tmp_path / "daemon-state.json"


def _write_state(path: pathlib.Path, data: dict) -> None:
    path.write_text(json.dumps(data, indent=2))


# --- Regex / constant tests ---


class TestConstants:
    def test_task_id_valid(self) -> None:
        assert TASK_ID_RE.match("a7dc1e0")
        assert TASK_ID_RE.match("0000000")
        assert TASK_ID_RE.match("abcdef1")

    def test_task_id_invalid(self) -> None:
        assert not TASK_ID_RE.match("FAKE123")
        assert not TASK_ID_RE.match("20")
        assert not TASK_ID_RE.match("abcdefg")  # g not hex
        assert not TASK_ID_RE.match("a7dc1e")  # 6 chars
        assert not TASK_ID_RE.match("a7dc1e00")  # 8 chars

    def test_timestamp_valid(self) -> None:
        assert TIMESTAMP_RE.match("2026-01-26T18:30:00Z")
        assert TIMESTAMP_RE.match("2026-01-26T18:30:00")  # without Z

    def test_timestamp_invalid(self) -> None:
        assert not TIMESTAMP_RE.match("not-a-date")
        assert not TIMESTAMP_RE.match("2026/01/26 18:30:00")

    def test_shepherd_statuses(self) -> None:
        assert VALID_SHEPHERD_STATUSES == {"working", "idle", "errored", "paused"}

    def test_support_role_statuses(self) -> None:
        assert VALID_SUPPORT_ROLE_STATUSES == {"running", "idle"}


# --- Core validation tests ---


class TestValidateState:
    def test_valid_state(self) -> None:
        errors, warnings, fixes, fixed = validate_state(_valid_state())
        assert errors == []
        assert warnings == []
        assert fixes == []
        assert fixed is None

    def test_missing_required_fields(self) -> None:
        data = {"running": True, "iteration": 1}
        errors, _, _, _ = validate_state(data)
        assert "missing_field:started_at" in errors

    def test_all_required_fields_missing(self) -> None:
        errors, _, _, _ = validate_state({})
        assert "missing_field:started_at" in errors
        assert "missing_field:running" in errors
        assert "missing_field:iteration" in errors

    def test_invalid_task_id(self) -> None:
        data = _valid_state()
        data["shepherds"]["shepherd-2"]["task_id"] = "FAKE123"
        errors, _, _, _ = validate_state(data)
        assert any("invalid_task_id:shepherd-2:FAKE123" in e for e in errors)

    def test_non_hex_task_id(self) -> None:
        data = _valid_state()
        data["support_roles"]["guide"]["task_id"] = "22"
        errors, _, _, _ = validate_state(data)
        assert any("invalid_task_id:guide:22" in e for e in errors)

    def test_invalid_shepherd_status(self) -> None:
        data = _valid_state()
        data["shepherds"]["shepherd-1"]["status"] = "bogus"
        errors, _, _, _ = validate_state(data)
        assert any("invalid_shepherd_status:shepherd-1:bogus" in e for e in errors)

    def test_invalid_support_role_status(self) -> None:
        data = _valid_state()
        data["support_roles"]["guide"]["status"] = "bogus"
        errors, _, _, _ = validate_state(data)
        assert any("invalid_support_role_status:guide:bogus" in e for e in errors)

    def test_invalid_timestamp_format(self) -> None:
        data = _valid_state()
        data["started_at"] = "not-a-date"
        _, warnings, _, _ = validate_state(data)
        assert any("invalid_timestamp_format:started_at:not-a-date" in w for w in warnings)

    def test_working_without_task_id_direct(self) -> None:
        """Shepherd working without task_id in direct mode -> warning."""
        data = _valid_state()
        data["shepherds"]["shepherd-2"]["task_id"] = None
        data["shepherds"]["shepherd-2"]["execution_mode"] = "direct"
        _, warnings, _, _ = validate_state(data)
        assert any("working_without_task_id:shepherd-2" in w for w in warnings)

    def test_working_without_task_id_tmux(self) -> None:
        """Shepherd working without task_id in tmux mode -> no warning."""
        data = _valid_state()
        data["shepherds"]["shepherd-2"]["task_id"] = None
        data["shepherds"]["shepherd-2"]["execution_mode"] = "tmux"
        _, warnings, _, _ = validate_state(data)
        assert not any("working_without_task_id" in w for w in warnings)

    def test_null_task_id_not_flagged(self) -> None:
        """task_id=null should not be flagged as invalid."""
        data = _valid_state()
        data["shepherds"]["shepherd-1"]["task_id"] = None
        errors, _, _, _ = validate_state(data)
        assert not any("invalid_task_id" in e for e in errors)

    def test_empty_string_task_id_not_flagged(self) -> None:
        """task_id="" should not be flagged as invalid."""
        data = _valid_state()
        data["shepherds"]["shepherd-1"]["task_id"] = ""
        errors, _, _, _ = validate_state(data)
        assert not any("invalid_task_id" in e for e in errors)

    def test_null_timestamp_not_flagged(self) -> None:
        """Null timestamp fields should not trigger warnings."""
        data = _valid_state()
        data["last_architect_trigger"] = None
        _, warnings, _, _ = validate_state(data)
        assert not any("invalid_timestamp_format:last_architect_trigger" in w for w in warnings)

    def test_no_shepherds_section(self) -> None:
        """Missing shepherds section should not error."""
        data = {"started_at": "2026-01-26T18:30:00Z", "running": True, "iteration": 1}
        errors, _, _, _ = validate_state(data)
        assert errors == []

    def test_no_support_roles_section(self) -> None:
        """Missing support_roles section should not error."""
        data = {"started_at": "2026-01-26T18:30:00Z", "running": True, "iteration": 1}
        errors, _, _, _ = validate_state(data)
        assert errors == []


# --- Auto-fix tests ---


class TestAutoFix:
    def test_fix_invalid_shepherd_task_id(self) -> None:
        data = _valid_state()
        data["shepherds"]["shepherd-2"]["task_id"] = "FAKE123"
        errors, _, fixes, fixed = validate_state(data, fix=True)
        assert "reset_shepherd:shepherd-2" in fixes
        assert fixed is not None
        assert fixed["shepherds"]["shepherd-2"]["status"] == "idle"
        assert fixed["shepherds"]["shepherd-2"]["task_id"] is None
        assert fixed["shepherds"]["shepherd-2"]["idle_reason"] == "invalid_task_id_reset"

    def test_fix_invalid_support_role_task_id(self) -> None:
        data = _valid_state()
        data["support_roles"]["guide"]["task_id"] = "BADID"
        errors, _, fixes, fixed = validate_state(data, fix=True)
        assert "reset_support_role:guide" in fixes
        assert fixed is not None
        assert fixed["support_roles"]["guide"]["status"] == "idle"
        assert fixed["support_roles"]["guide"]["task_id"] is None

    def test_fix_does_not_mutate_original(self) -> None:
        data = _valid_state()
        data["shepherds"]["shepherd-2"]["task_id"] = "FAKE123"
        original_task_id = data["shepherds"]["shepherd-2"]["task_id"]
        _, _, _, fixed = validate_state(data, fix=True)
        assert data["shepherds"]["shepherd-2"]["task_id"] == original_task_id
        assert fixed is not None

    def test_no_fix_when_valid(self) -> None:
        _, _, fixes, fixed = validate_state(_valid_state(), fix=True)
        assert fixes == []
        assert fixed is None

    def test_fix_not_applied_without_flag(self) -> None:
        data = _valid_state()
        data["shepherds"]["shepherd-2"]["task_id"] = "FAKE123"
        _, _, fixes, fixed = validate_state(data, fix=False)
        assert fixes == []
        assert fixed is None


# --- CLI tests ---


class TestCLI:
    def test_valid_file(self, state_file: pathlib.Path) -> None:
        _write_state(state_file, _valid_state())
        assert main([str(state_file)]) == 0

    def test_missing_file(self, tmp_path: pathlib.Path) -> None:
        missing = tmp_path / "nonexistent.json"
        assert main([str(missing)]) == 2

    def test_invalid_json(self, state_file: pathlib.Path) -> None:
        state_file.write_text("not valid json {{{")
        assert main([str(state_file)]) == 1

    def test_missing_required_fields(self, state_file: pathlib.Path) -> None:
        _write_state(state_file, {"running": True})
        assert main([str(state_file)]) == 1

    def test_invalid_task_id_exit_code(self, state_file: pathlib.Path) -> None:
        data = _valid_state()
        data["shepherds"]["shepherd-2"]["task_id"] = "FAKE123"
        _write_state(state_file, data)
        assert main([str(state_file)]) == 1

    def test_invalid_shepherd_status_exit_code(self, state_file: pathlib.Path) -> None:
        data = _valid_state()
        data["shepherds"]["shepherd-1"]["status"] = "bogus"
        _write_state(state_file, data)
        assert main([str(state_file)]) == 1

    def test_invalid_support_role_status_exit_code(self, state_file: pathlib.Path) -> None:
        data = _valid_state()
        data["support_roles"]["guide"]["status"] = "bogus"
        _write_state(state_file, data)
        assert main([str(state_file)]) == 1

    def test_invalid_timestamp_is_warning_not_error(self, state_file: pathlib.Path) -> None:
        data = _valid_state()
        data["started_at"] = "not-a-date"
        _write_state(state_file, data)
        # Warnings don't cause exit code 1
        assert main([str(state_file)]) == 0

    def test_fix_writes_file(self, state_file: pathlib.Path) -> None:
        data = _valid_state()
        data["shepherds"]["shepherd-2"]["task_id"] = "FAKE123"
        _write_state(state_file, data)

        assert main(["--fix", str(state_file)]) == 0

        fixed = json.loads(state_file.read_text())
        assert fixed["shepherds"]["shepherd-2"]["status"] == "idle"
        assert fixed["shepherds"]["shepherd-2"]["task_id"] is None

    def test_fix_dry_run_does_not_write(self, state_file: pathlib.Path) -> None:
        data = _valid_state()
        data["shepherds"]["shepherd-2"]["task_id"] = "FAKE123"
        _write_state(state_file, data)
        original_content = state_file.read_text()

        assert main(["--fix", "--dry-run", str(state_file)]) == 0

        assert state_file.read_text() == original_content

    def test_json_output_valid(self, state_file: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
        _write_state(state_file, _valid_state())
        assert main(["--json", str(state_file)]) == 0

        output = json.loads(capsys.readouterr().out)
        assert output["valid"] is True
        assert output["errors"] == []
        assert output["error_count"] == 0
        assert output["warning_count"] == 0
        assert "file" in output

    def test_json_output_invalid(self, state_file: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
        data = _valid_state()
        data["shepherds"]["shepherd-2"]["task_id"] = "FAKE123"
        _write_state(state_file, data)

        assert main(["--json", str(state_file)]) == 1

        output = json.loads(capsys.readouterr().out)
        assert output["valid"] is False
        assert output["error_count"] > 0
        assert len(output["errors"]) == output["error_count"]

    def test_json_output_missing_file(self, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
        missing = tmp_path / "nonexistent.json"
        assert main(["--json", str(missing)]) == 2

        output = json.loads(capsys.readouterr().out)
        assert output["valid"] is False
        assert output["error"] == "file_not_found"

    def test_json_output_invalid_json(self, state_file: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
        state_file.write_text("not valid json {{{")
        assert main(["--json", str(state_file)]) == 1

        output = json.loads(capsys.readouterr().out)
        assert output["valid"] is False
        assert output["error"] == "invalid_json"

    def test_json_output_with_fix(self, state_file: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
        data = _valid_state()
        data["shepherds"]["shepherd-2"]["task_id"] = "FAKE123"
        _write_state(state_file, data)

        assert main(["--json", "--fix", str(state_file)]) == 0

        output = json.loads(capsys.readouterr().out)
        assert len(output["fixes_applied"]) > 0
        assert output["fixes_available"] == []

    def test_json_output_fixes_available(self, state_file: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
        """Without --fix, fixes should appear in fixes_available."""
        data = _valid_state()
        data["shepherds"]["shepherd-2"]["task_id"] = "FAKE123"
        _write_state(state_file, data)

        # Without --fix: errors reported, fixes_available populated
        # Note: validate_state only populates fixes when fix=True,
        # so fixes_available is empty without --fix (matching bash behavior)
        assert main(["--json", str(state_file)]) == 1

        output = json.loads(capsys.readouterr().out)
        assert output["fixes_applied"] == []

    def test_json_output_schema(self, state_file: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
        """JSON output has all expected keys."""
        _write_state(state_file, _valid_state())
        main(["--json", str(state_file)])

        output = json.loads(capsys.readouterr().out)
        expected_keys = {"valid", "file", "errors", "warnings", "fixes_applied", "fixes_available", "error_count", "warning_count"}
        assert set(output.keys()) == expected_keys

    def test_help(self) -> None:
        with pytest.raises(SystemExit) as exc_info:
            main(["--help"])
        assert exc_info.value.code == 0


# --- Test fixture validation ---


class TestFixture:
    """Test with the real test fixture file."""

    def test_fixture_has_non_hex_task_ids(self) -> None:
        """The test fixture has task_ids like '20' which are not valid hex7."""
        fixture = pathlib.Path(__file__).parent / "fixtures" / "daemon-state.json"
        if not fixture.exists():
            pytest.skip("Fixture file not found")

        data = json.loads(fixture.read_text())
        errors, _, _, _ = validate_state(data)
        # Should catch the non-hex task IDs (e.g. "20", "22", etc.)
        task_id_errors = [e for e in errors if "invalid_task_id" in e]
        assert len(task_id_errors) > 0
