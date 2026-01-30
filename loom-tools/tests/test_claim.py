"""Tests for loom_tools.claim."""

from __future__ import annotations

import json
import pathlib
import time

import pytest

from loom_tools.claim import (
    DEFAULT_TTL,
    ClaimInfo,
    _get_expiration,
    _get_timestamp,
    _is_expired,
    check_claim,
    claim_issue,
    cleanup_claims,
    extend_claim,
    list_claims,
    main,
    release_claim,
)
from loom_tools.common.repo import clear_repo_cache


@pytest.fixture
def mock_repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a mock repo with .git and .loom directories."""
    # Clear the repo root cache to ensure each test gets its own repo
    clear_repo_cache()
    (tmp_path / ".git").mkdir()
    (tmp_path / ".loom").mkdir()
    return tmp_path


class TestTimeFunctions:
    """Tests for time-related functions."""

    def test_get_timestamp_format(self) -> None:
        ts = _get_timestamp()
        # Should match ISO format: YYYY-MM-DDTHH:MM:SSZ
        assert len(ts) == 20
        assert ts.endswith("Z")
        assert "T" in ts

    def test_get_expiration_future(self) -> None:
        now = _get_timestamp()
        future = _get_expiration(3600)
        # Future should be greater than now
        assert future > now

    def test_is_expired_past(self) -> None:
        # A timestamp in the past should be expired
        assert _is_expired("2020-01-01T00:00:00Z")

    def test_is_expired_future(self) -> None:
        # A timestamp far in the future should not be expired
        assert not _is_expired("2099-01-01T00:00:00Z")


class TestClaimInfo:
    """Tests for ClaimInfo dataclass."""

    def test_from_dict(self) -> None:
        data = {
            "issue": 42,
            "agent_id": "test-agent",
            "claimed_at": "2026-01-01T00:00:00Z",
            "expires_at": "2026-01-01T01:00:00Z",
            "ttl_seconds": 3600,
        }
        claim = ClaimInfo.from_dict(data)
        assert claim.issue == 42
        assert claim.agent_id == "test-agent"
        assert claim.ttl_seconds == 3600

    def test_to_dict_roundtrip(self) -> None:
        claim = ClaimInfo(
            issue=42,
            agent_id="test-agent",
            claimed_at="2026-01-01T00:00:00Z",
            expires_at="2026-01-01T01:00:00Z",
            ttl_seconds=3600,
        )
        data = claim.to_dict()
        restored = ClaimInfo.from_dict(data)
        assert restored == claim


class TestClaimIssue:
    """Tests for claim_issue function."""

    def test_claim_success(self, mock_repo: pathlib.Path) -> None:
        result = claim_issue(mock_repo, 42, "test-agent", 3600)
        assert result == 0

        # Verify claim file was created
        claim_file = mock_repo / ".loom" / "claims" / "issue-42.lock" / "claim.json"
        assert claim_file.exists()

        data = json.loads(claim_file.read_text())
        assert data["issue"] == 42
        assert data["agent_id"] == "test-agent"
        assert data["ttl_seconds"] == 3600

    def test_claim_already_claimed(self, mock_repo: pathlib.Path) -> None:
        # First claim should succeed
        result1 = claim_issue(mock_repo, 42, "agent-1", 3600)
        assert result1 == 0

        # Second claim should fail
        result2 = claim_issue(mock_repo, 42, "agent-2", 3600)
        assert result2 == 1

    def test_claim_expired_reclaim(self, mock_repo: pathlib.Path) -> None:
        # Create an expired claim manually
        claim_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        claim_dir.mkdir(parents=True)
        claim_file = claim_dir / "claim.json"
        claim_file.write_text(
            json.dumps(
                {
                    "issue": 42,
                    "agent_id": "old-agent",
                    "claimed_at": "2020-01-01T00:00:00Z",
                    "expires_at": "2020-01-01T01:00:00Z",
                    "ttl_seconds": 3600,
                }
            )
        )

        # Should be able to reclaim expired issue
        result = claim_issue(mock_repo, 42, "new-agent", 3600)
        assert result == 0

        # New agent should own it
        data = json.loads(claim_file.read_text())
        assert data["agent_id"] == "new-agent"

    def test_claim_incomplete_cleanup(self, mock_repo: pathlib.Path) -> None:
        # Create a lock directory without claim file
        claim_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        claim_dir.mkdir(parents=True)

        # Should clean up and succeed
        result = claim_issue(mock_repo, 42, "test-agent", 3600)
        assert result == 0


class TestExtendClaim:
    """Tests for extend_claim function."""

    def test_extend_success(self, mock_repo: pathlib.Path) -> None:
        # First claim
        claim_issue(mock_repo, 42, "test-agent", 60)

        # Extend
        result = extend_claim(mock_repo, 42, "test-agent", 7200)
        assert result == 0

        # Verify extended TTL
        claim_file = mock_repo / ".loom" / "claims" / "issue-42.lock" / "claim.json"
        data = json.loads(claim_file.read_text())
        assert data["ttl_seconds"] == 7200

    def test_extend_not_found(self, mock_repo: pathlib.Path) -> None:
        result = extend_claim(mock_repo, 999, "test-agent", 3600)
        assert result == 3

    def test_extend_wrong_agent(self, mock_repo: pathlib.Path) -> None:
        claim_issue(mock_repo, 42, "agent-1", 3600)
        result = extend_claim(mock_repo, 42, "agent-2", 3600)
        assert result == 4


class TestReleaseClaim:
    """Tests for release_claim function."""

    def test_release_success(self, mock_repo: pathlib.Path) -> None:
        claim_issue(mock_repo, 42, "test-agent", 3600)
        result = release_claim(mock_repo, 42)
        assert result == 0

        # Verify claim was removed
        claim_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        assert not claim_dir.exists()

    def test_release_not_found(self, mock_repo: pathlib.Path) -> None:
        result = release_claim(mock_repo, 999)
        assert result == 3

    def test_release_wrong_agent(self, mock_repo: pathlib.Path) -> None:
        claim_issue(mock_repo, 42, "agent-1", 3600)
        result = release_claim(mock_repo, 42, "agent-2")
        assert result == 4

    def test_release_with_correct_agent(self, mock_repo: pathlib.Path) -> None:
        claim_issue(mock_repo, 42, "agent-1", 3600)
        result = release_claim(mock_repo, 42, "agent-1")
        assert result == 0


class TestCheckClaim:
    """Tests for check_claim function."""

    def test_check_not_claimed(self, mock_repo: pathlib.Path) -> None:
        result = check_claim(mock_repo, 42)
        assert result == 3

    def test_check_claimed(self, mock_repo: pathlib.Path) -> None:
        claim_issue(mock_repo, 42, "test-agent", 3600)
        result = check_claim(mock_repo, 42)
        assert result == 0

    def test_check_expired(self, mock_repo: pathlib.Path) -> None:
        # Create an expired claim
        claim_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        claim_dir.mkdir(parents=True)
        claim_file = claim_dir / "claim.json"
        claim_file.write_text(
            json.dumps(
                {
                    "issue": 42,
                    "agent_id": "test-agent",
                    "claimed_at": "2020-01-01T00:00:00Z",
                    "expires_at": "2020-01-01T01:00:00Z",
                    "ttl_seconds": 3600,
                }
            )
        )

        result = check_claim(mock_repo, 42)
        assert result == 3


class TestListClaims:
    """Tests for list_claims function."""

    def test_list_empty(self, mock_repo: pathlib.Path) -> None:
        result = list_claims(mock_repo)
        assert result == 0

    def test_list_with_claims(self, mock_repo: pathlib.Path) -> None:
        claim_issue(mock_repo, 42, "agent-1", 3600)
        claim_issue(mock_repo, 43, "agent-2", 3600)
        result = list_claims(mock_repo)
        assert result == 0


class TestCleanupClaims:
    """Tests for cleanup_claims function."""

    def test_cleanup_removes_expired(self, mock_repo: pathlib.Path) -> None:
        # Create an expired claim
        claim_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        claim_dir.mkdir(parents=True)
        claim_file = claim_dir / "claim.json"
        claim_file.write_text(
            json.dumps(
                {
                    "issue": 42,
                    "agent_id": "test-agent",
                    "claimed_at": "2020-01-01T00:00:00Z",
                    "expires_at": "2020-01-01T01:00:00Z",
                    "ttl_seconds": 3600,
                }
            )
        )

        result = cleanup_claims(mock_repo)
        assert result == 0
        assert not claim_dir.exists()

    def test_cleanup_preserves_active(self, mock_repo: pathlib.Path) -> None:
        claim_issue(mock_repo, 42, "test-agent", 3600)
        result = cleanup_claims(mock_repo)
        assert result == 0

        claim_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        assert claim_dir.exists()


class TestCLI:
    """Tests for CLI main function."""

    def test_cli_help(self) -> None:
        with pytest.raises(SystemExit) as exc_info:
            main(["--help"])
        assert exc_info.value.code == 0

    def test_cli_no_command(self) -> None:
        result = main([])
        assert result == 0

    def test_cli_invalid_command(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["invalid"])
        assert result == 2

    def test_cli_claim_no_issue(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["claim"])
        assert result == 2

    def test_cli_claim_invalid_issue(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["claim", "not-a-number"])
        assert result == 2

    def test_cli_claim_success(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["claim", "42", "test-agent"])
        assert result == 0

    def test_cli_extend_missing_args(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["extend", "42"])
        assert result == 2

    def test_cli_release_success(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        main(["claim", "42", "test-agent"])
        result = main(["release", "42"])
        assert result == 0

    def test_cli_check(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        main(["claim", "42", "test-agent"])
        result = main(["check", "42"])
        assert result == 0

    def test_cli_list(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["list"])
        assert result == 0

    def test_cli_cleanup(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["cleanup"])
        assert result == 0


# ============================================================================
# Bash Parity Validation Tests
# ============================================================================
# These tests validate that claim.py produces identical behavior to claim.sh.
# This is part of the loom-tools migration (#1630) behavior validation effort.
#
# Referenced bash script: .loom/scripts/claim.sh
# Related issue: #1702 - Validate claim.py behavior matches claim.sh
# ============================================================================


class TestBashParityExitCodes:
    """Validate exit codes match bash script behavior.

    Bash script (.loom/scripts/claim.sh) uses:
    - 0: Success
    - 1: Claim already exists (for claim), or general error
    - 2: Invalid arguments
    - 3: Claim not found (for release/check)
    - 4: Agent ID mismatch (for release)
    """

    def test_claim_success_exit_0(self, mock_repo: pathlib.Path) -> None:
        """Successful claim returns exit code 0 like bash."""
        # Bash: return 0 on line 134
        result = claim_issue(mock_repo, 42, "test-agent", 3600)
        assert result == 0

    def test_claim_already_claimed_exit_1(self, mock_repo: pathlib.Path) -> None:
        """Claim on already-claimed issue returns exit code 1 like bash."""
        # Bash: return 1 on line 156
        claim_issue(mock_repo, 42, "agent-1", 3600)
        result = claim_issue(mock_repo, 42, "agent-2", 3600)
        assert result == 1

    def test_claim_missing_issue_exit_2(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        """Missing issue number argument returns exit code 2 like bash."""
        # Bash: return 2 on line 103-104
        monkeypatch.chdir(mock_repo)
        result = main(["claim"])
        assert result == 2

    def test_extend_not_found_exit_3(self, mock_repo: pathlib.Path) -> None:
        """Extend on non-existent claim returns exit code 3 like bash."""
        # Bash: return 3 on line 191
        result = extend_claim(mock_repo, 999, "test-agent", 3600)
        assert result == 3

    def test_release_not_found_exit_3(self, mock_repo: pathlib.Path) -> None:
        """Release on non-existent claim returns exit code 3 like bash."""
        # Bash: return 3 on line 252-253
        result = release_claim(mock_repo, 999)
        assert result == 3

    def test_check_not_claimed_exit_3(self, mock_repo: pathlib.Path) -> None:
        """Check on unclaimed issue returns exit code 3 like bash."""
        # Bash: return 3 on line 288-289
        result = check_claim(mock_repo, 999)
        assert result == 3

    def test_extend_wrong_agent_exit_4(self, mock_repo: pathlib.Path) -> None:
        """Extend with wrong agent ID returns exit code 4 like bash."""
        # Bash: return 4 on line 207
        claim_issue(mock_repo, 42, "agent-1", 3600)
        result = extend_claim(mock_repo, 42, "agent-2", 3600)
        assert result == 4

    def test_release_wrong_agent_exit_4(self, mock_repo: pathlib.Path) -> None:
        """Release with wrong agent ID returns exit code 4 like bash."""
        # Bash: return 4 on line 265
        claim_issue(mock_repo, 42, "agent-1", 3600)
        result = release_claim(mock_repo, 42, "agent-2")
        assert result == 4

    def test_invalid_command_exit_2(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        """Unknown command returns exit code 2 like bash."""
        # Bash: exit 2 on line 462
        monkeypatch.chdir(mock_repo)
        result = main(["unknown_command"])
        assert result == 2

    def test_no_command_returns_0_shows_help(self) -> None:
        """No command returns 0 with help like bash help command."""
        # Bash: help command returns 0 after showing usage
        result = main([])
        assert result == 0


class TestBashParityDefaultValues:
    """Validate default values match bash script constants.

    Bash script uses:
    - DEFAULT_TTL=1800 (30 minutes)
    - Default agent ID: $(hostname)-$$
    """

    def test_default_ttl_matches_bash(self) -> None:
        """DEFAULT_TTL constant matches bash DEFAULT_TTL=1800."""
        # Bash line 34: DEFAULT_TTL=1800
        assert DEFAULT_TTL == 1800

    def test_claim_uses_default_ttl(self, mock_repo: pathlib.Path) -> None:
        """Claim without TTL uses default 1800 seconds like bash."""
        # Bash line 99: local ttl="${3:-$DEFAULT_TTL}"
        claim_issue(mock_repo, 42, "test-agent")  # No TTL specified

        claim_file = mock_repo / ".loom" / "claims" / "issue-42.lock" / "claim.json"
        data = json.loads(claim_file.read_text())
        assert data["ttl_seconds"] == 1800

    def test_default_agent_id_format(self, mock_repo: pathlib.Path) -> None:
        """Default agent ID uses hostname-pid format like bash."""
        # Bash lines 84-92: echo "$(hostname)-$$"
        import socket

        claim_issue(mock_repo, 42)  # No agent ID specified

        claim_file = mock_repo / ".loom" / "claims" / "issue-42.lock" / "claim.json"
        data = json.loads(claim_file.read_text())

        # Verify format: hostname-pid
        agent_id = data["agent_id"]
        hostname = socket.gethostname()
        assert agent_id.startswith(f"{hostname}-")
        # The PID part should be numeric
        pid_part = agent_id.split("-")[-1]
        assert pid_part.isdigit()


class TestBashParityFileStructure:
    """Validate file paths and structure match bash script.

    Bash uses:
    - Claims dir: $REPO_ROOT/.loom/claims
    - Claim lock dir: $CLAIMS_DIR/issue-<N>.lock
    - Claim file: $claim_dir/claim.json
    """

    def test_claims_dir_path(self, mock_repo: pathlib.Path) -> None:
        """Claims directory is at .loom/claims like bash."""
        # Bash line 51: CLAIMS_DIR="$REPO_ROOT/.loom/claims"
        claim_issue(mock_repo, 42, "test-agent", 3600)
        claims_dir = mock_repo / ".loom" / "claims"
        assert claims_dir.exists()
        assert claims_dir.is_dir()

    def test_claim_lock_dir_pattern(self, mock_repo: pathlib.Path) -> None:
        """Claim lock directory uses issue-<N>.lock pattern like bash."""
        # Bash line 110: local claim_dir="$CLAIMS_DIR/issue-${issue_number}.lock"
        claim_issue(mock_repo, 42, "test-agent", 3600)
        claim_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        assert claim_dir.exists()
        assert claim_dir.is_dir()

    def test_claim_file_name(self, mock_repo: pathlib.Path) -> None:
        """Claim file is named claim.json like bash."""
        # Bash line 111: local claim_file="$claim_dir/claim.json"
        claim_issue(mock_repo, 42, "test-agent", 3600)
        claim_file = mock_repo / ".loom" / "claims" / "issue-42.lock" / "claim.json"
        assert claim_file.exists()
        assert claim_file.is_file()


class TestBashParityClaimJsonStructure:
    """Validate claim JSON structure matches bash script output.

    Bash script produces JSON with:
    - issue: number
    - agent_id: string
    - claimed_at: ISO timestamp
    - expires_at: ISO timestamp
    - ttl_seconds: number
    """

    def test_claim_json_has_all_fields(self, mock_repo: pathlib.Path) -> None:
        """Claim JSON has all fields matching bash heredoc (lines 122-130)."""
        claim_issue(mock_repo, 42, "test-agent", 3600)
        claim_file = mock_repo / ".loom" / "claims" / "issue-42.lock" / "claim.json"
        data = json.loads(claim_file.read_text())

        # Verify all fields exist
        assert "issue" in data
        assert "agent_id" in data
        assert "claimed_at" in data
        assert "expires_at" in data
        assert "ttl_seconds" in data

    def test_claim_json_field_types(self, mock_repo: pathlib.Path) -> None:
        """Claim JSON field types match bash output."""
        claim_issue(mock_repo, 42, "test-agent", 3600)
        claim_file = mock_repo / ".loom" / "claims" / "issue-42.lock" / "claim.json"
        data = json.loads(claim_file.read_text())

        # Bash: "issue": $issue_number (number, not string)
        assert isinstance(data["issue"], int)
        # Bash: "agent_id": "$agent_id" (string)
        assert isinstance(data["agent_id"], str)
        # Bash: "claimed_at": "$timestamp" (string)
        assert isinstance(data["claimed_at"], str)
        # Bash: "expires_at": "$expiration" (string)
        assert isinstance(data["expires_at"], str)
        # Bash: "ttl_seconds": $ttl (number, not string)
        assert isinstance(data["ttl_seconds"], int)

    def test_timestamp_format_matches_bash(self, mock_repo: pathlib.Path) -> None:
        """Timestamps use ISO format with Z suffix like bash."""
        # Bash line 60-61: date -u +"%Y-%m-%dT%H:%M:%SZ"
        claim_issue(mock_repo, 42, "test-agent", 3600)
        claim_file = mock_repo / ".loom" / "claims" / "issue-42.lock" / "claim.json"
        data = json.loads(claim_file.read_text())

        # Format: YYYY-MM-DDTHH:MM:SSZ
        claimed_at = data["claimed_at"]
        expires_at = data["expires_at"]

        assert len(claimed_at) == 20  # e.g., "2026-01-30T22:30:00Z"
        assert claimed_at.endswith("Z")
        assert "T" in claimed_at

        assert len(expires_at) == 20
        assert expires_at.endswith("Z")
        assert "T" in expires_at


class TestBashParityAtomicOperations:
    """Validate atomic operations match bash mkdir-based approach."""

    def test_atomic_mkdir_for_claim(self, mock_repo: pathlib.Path) -> None:
        """Claim uses mkdir for atomic creation like bash."""
        # Bash line 115: if mkdir "$claim_dir" 2>/dev/null; then
        # Python uses Path.mkdir(exist_ok=False) which raises FileExistsError
        result1 = claim_issue(mock_repo, 42, "agent-1", 3600)
        assert result1 == 0

        # Second claim should fail atomically
        result2 = claim_issue(mock_repo, 42, "agent-2", 3600)
        assert result2 == 1

        # First agent should still own the claim
        claim_file = mock_repo / ".loom" / "claims" / "issue-42.lock" / "claim.json"
        data = json.loads(claim_file.read_text())
        assert data["agent_id"] == "agent-1"

    def test_expired_claim_cleanup_and_reclaim(self, mock_repo: pathlib.Path) -> None:
        """Expired claims are cleaned up and reclaimable like bash."""
        # Bash lines 140-148: if is_expired; clean up and retry
        claim_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        claim_dir.mkdir(parents=True)
        claim_file = claim_dir / "claim.json"
        claim_file.write_text(json.dumps({
            "issue": 42,
            "agent_id": "old-agent",
            "claimed_at": "2020-01-01T00:00:00Z",
            "expires_at": "2020-01-01T00:30:00Z",
            "ttl_seconds": 1800,
        }))

        # New claim should succeed after cleaning up expired claim
        result = claim_issue(mock_repo, 42, "new-agent", 3600)
        assert result == 0

        data = json.loads(claim_file.read_text())
        assert data["agent_id"] == "new-agent"

    def test_incomplete_claim_cleanup(self, mock_repo: pathlib.Path) -> None:
        """Incomplete claims (dir without file) are cleaned up like bash."""
        # Bash lines 159-163: if lock dir exists but no claim file
        claim_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        claim_dir.mkdir(parents=True)
        # No claim.json file created (incomplete claim)

        result = claim_issue(mock_repo, 42, "test-agent", 3600)
        assert result == 0

        claim_file = claim_dir / "claim.json"
        assert claim_file.exists()


class TestBashParityExtendBehavior:
    """Validate extend command behavior matches bash."""

    def test_extend_updates_expiration_from_now(self, mock_repo: pathlib.Path) -> None:
        """Extend calculates new expiration from current time like bash."""
        # Bash lines 209-211: new_expiration=$(get_expiration "$additional_seconds")
        claim_issue(mock_repo, 42, "test-agent", 60)

        # Wait a moment and extend
        time.sleep(0.1)
        result = extend_claim(mock_repo, 42, "test-agent", 7200)
        assert result == 0

        claim_file = mock_repo / ".loom" / "claims" / "issue-42.lock" / "claim.json"
        data = json.loads(claim_file.read_text())

        # TTL should be updated to new value
        assert data["ttl_seconds"] == 7200

        # expires_at should be approximately now + 7200 seconds
        # (We can't test exact time, but verify it's a valid future timestamp)
        assert data["expires_at"] > data["claimed_at"]

    def test_extend_preserves_claimed_at(self, mock_repo: pathlib.Path) -> None:
        """Extend preserves original claimed_at timestamp like bash."""
        # Bash line 217: claimed_at=$(grep ... "$claim_file")
        claim_issue(mock_repo, 42, "test-agent", 60)

        claim_file = mock_repo / ".loom" / "claims" / "issue-42.lock" / "claim.json"
        original_data = json.loads(claim_file.read_text())
        original_claimed_at = original_data["claimed_at"]

        extend_claim(mock_repo, 42, "test-agent", 7200)

        updated_data = json.loads(claim_file.read_text())
        assert updated_data["claimed_at"] == original_claimed_at


class TestBashParityReleaseBehavior:
    """Validate release command behavior matches bash."""

    def test_release_without_agent_succeeds(self, mock_repo: pathlib.Path) -> None:
        """Release without agent ID succeeds (no ownership check) like bash."""
        # Bash lines 255-266: only checks agent if provided
        claim_issue(mock_repo, 42, "agent-1", 3600)
        result = release_claim(mock_repo, 42)  # No agent ID
        assert result == 0

    def test_release_removes_entire_claim_dir(self, mock_repo: pathlib.Path) -> None:
        """Release removes entire claim directory like bash."""
        # Bash line 269: rm -rf "$claim_dir"
        claim_issue(mock_repo, 42, "test-agent", 3600)
        claim_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        assert claim_dir.exists()

        release_claim(mock_repo, 42)
        assert not claim_dir.exists()


class TestBashParityCheckBehavior:
    """Validate check command behavior matches bash."""

    def test_check_returns_expired_status_exit_3(self, mock_repo: pathlib.Path) -> None:
        """Check on expired claim returns exit 3 like bash."""
        # Bash lines 301-304: if is_expired; return 3
        claim_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        claim_dir.mkdir(parents=True)
        claim_file = claim_dir / "claim.json"
        claim_file.write_text(json.dumps({
            "issue": 42,
            "agent_id": "test-agent",
            "claimed_at": "2020-01-01T00:00:00Z",
            "expires_at": "2020-01-01T00:30:00Z",
            "ttl_seconds": 1800,
        }))

        result = check_claim(mock_repo, 42)
        assert result == 3


class TestBashParityListBehavior:
    """Validate list command behavior matches bash."""

    def test_list_shows_expired_status(self, mock_repo: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
        """List shows expired status for expired claims like bash."""
        # Bash lines 331-334: if is_expired; echo "(EXPIRED)"
        # Create an expired claim
        claim_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        claim_dir.mkdir(parents=True)
        claim_file = claim_dir / "claim.json"
        claim_file.write_text(json.dumps({
            "issue": 42,
            "agent_id": "test-agent",
            "claimed_at": "2020-01-01T00:00:00Z",
            "expires_at": "2020-01-01T00:30:00Z",
            "ttl_seconds": 1800,
        }))

        list_claims(mock_repo)
        captured = capsys.readouterr()
        assert "EXPIRED" in captured.out

    def test_list_shows_total_count(self, mock_repo: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
        """List shows total count like bash."""
        # Bash line 346: echo "Total: $count claim(s)"
        claim_issue(mock_repo, 42, "agent-1", 3600)
        claim_issue(mock_repo, 43, "agent-2", 3600)

        list_claims(mock_repo)
        captured = capsys.readouterr()
        assert "Total: 2 claim(s)" in captured.out

    def test_list_empty_shows_none(self, mock_repo: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
        """Empty list shows (none) like bash."""
        # Bash lines 341-342: if [[ $count -eq 0 ]]; echo "(none)"
        list_claims(mock_repo)
        captured = capsys.readouterr()
        assert "(none)" in captured.out


class TestBashParityCleanupBehavior:
    """Validate cleanup command behavior matches bash."""

    def test_cleanup_removes_expired_claims(self, mock_repo: pathlib.Path) -> None:
        """Cleanup removes only expired claims like bash."""
        # Bash lines 356-376
        # Create an expired claim
        expired_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        expired_dir.mkdir(parents=True)
        (expired_dir / "claim.json").write_text(json.dumps({
            "issue": 42,
            "agent_id": "test-agent",
            "claimed_at": "2020-01-01T00:00:00Z",
            "expires_at": "2020-01-01T00:30:00Z",
            "ttl_seconds": 1800,
        }))

        # Create an active claim
        claim_issue(mock_repo, 43, "active-agent", 3600)

        cleanup_claims(mock_repo)

        # Expired claim should be removed
        assert not expired_dir.exists()
        # Active claim should remain
        active_dir = mock_repo / ".loom" / "claims" / "issue-43.lock"
        assert active_dir.exists()

    def test_cleanup_removes_incomplete_claims(self, mock_repo: pathlib.Path) -> None:
        """Cleanup removes incomplete claims (dir without file) like bash."""
        # Bash lines 370-374: if no claim file, remove dir
        incomplete_dir = mock_repo / ".loom" / "claims" / "issue-99.lock"
        incomplete_dir.mkdir(parents=True)
        # No claim.json file

        cleanup_claims(mock_repo)
        assert not incomplete_dir.exists()


class TestBashParityCLIArgumentParsing:
    """Validate CLI argument parsing matches bash script.

    Bash uses positional arguments:
    - claim.sh claim <issue-number> [agent-id] [ttl-seconds]
    - claim.sh extend <issue-number> <agent-id> [additional-seconds]
    - claim.sh release <issue-number> [agent-id]
    - claim.sh check <issue-number>
    - claim.sh list
    - claim.sh cleanup
    """

    def test_claim_accepts_all_positional_args(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        """claim command accepts issue, agent-id, ttl-seconds like bash."""
        monkeypatch.chdir(mock_repo)
        result = main(["claim", "42", "my-agent", "7200"])
        assert result == 0

        claim_file = mock_repo / ".loom" / "claims" / "issue-42.lock" / "claim.json"
        data = json.loads(claim_file.read_text())
        assert data["issue"] == 42
        assert data["agent_id"] == "my-agent"
        assert data["ttl_seconds"] == 7200

    def test_extend_requires_agent_id(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        """extend command requires agent-id like bash."""
        # Bash lines 175-183: requires agent_id for extend
        monkeypatch.chdir(mock_repo)
        main(["claim", "42", "test-agent"])
        result = main(["extend", "42"])  # Missing agent-id
        assert result == 2

    def test_extend_accepts_additional_seconds(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        """extend command accepts additional-seconds like bash."""
        monkeypatch.chdir(mock_repo)
        main(["claim", "42", "test-agent"])
        result = main(["extend", "42", "test-agent", "9000"])
        assert result == 0

        claim_file = mock_repo / ".loom" / "claims" / "issue-42.lock" / "claim.json"
        data = json.loads(claim_file.read_text())
        assert data["ttl_seconds"] == 9000


class TestBashParityTimestampComparison:
    """Validate timestamp comparison uses lexicographic ordering like bash."""

    def test_is_expired_uses_string_comparison(self) -> None:
        """Expired check uses lexicographic string comparison like bash."""
        # Bash line 80: [[ "$current" > "$expiration" ]]
        # ISO timestamps are designed to be lexicographically sortable

        # Past timestamp should be expired
        assert _is_expired("2020-01-01T00:00:00Z") is True

        # Future timestamp should not be expired
        assert _is_expired("2099-12-31T23:59:59Z") is False

    def test_timestamp_format_is_sortable(self) -> None:
        """Timestamps are in ISO format that's lexicographically sortable."""
        ts1 = "2026-01-15T10:00:00Z"
        ts2 = "2026-01-15T10:00:01Z"
        ts3 = "2026-02-01T00:00:00Z"

        # Verify string comparison gives correct ordering
        assert ts1 < ts2
        assert ts2 < ts3
        assert ts1 < ts3


class TestDocumentedDivergences:
    """Tests documenting intentional differences between bash and Python.

    These differences exist for good reasons and are documented here.
    """

    def test_error_output_uses_logging_not_echo(self) -> None:
        """Python uses logging functions instead of echo with ANSI colors.

        Bash uses:
        - echo -e "${RED}Error: ...${NC}" >&2
        - echo -e "${GREEN}✓ ...${NC}"
        - echo -e "${YELLOW}⚠ ...${NC}"

        Python uses:
        - log_error(), log_success(), log_warning() from common.logging

        This difference is ACCEPTABLE:
        - Both produce human-readable output
        - Python version uses semantic logging
        - Terminal color handling is abstracted
        """
        pass  # Behavior is equivalent

    def test_json_library_vs_grep_parsing(self) -> None:
        """Python uses json library instead of grep for parsing.

        Bash parses JSON with grep:
        - grep -o '"expires_at": "[^"]*"' "$claim_file" | cut -d'"' -f4

        Python uses json library:
        - json.loads(claim_file.read_text())

        This difference is INTENTIONAL:
        - Python approach is more robust
        - Handles edge cases better (escaped quotes, etc.)
        - Same end result (correct field values extracted)
        """
        pass  # Behavior is equivalent

    def test_shutil_rmtree_vs_rm_rf(self) -> None:
        """Python uses shutil.rmtree instead of rm -rf.

        Bash: rm -rf "$claim_dir"
        Python: shutil.rmtree(claim_dir)

        This difference is ACCEPTABLE:
        - Both completely remove directory and contents
        - Python approach is more portable
        - No subprocess overhead
        """
        pass  # Behavior is equivalent

    def test_pathlib_mkdir_vs_bash_mkdir(self) -> None:
        """Python uses pathlib.Path.mkdir() for atomic creation.

        Bash: mkdir "$claim_dir" 2>/dev/null
        Python: claim_dir.mkdir(parents=False, exist_ok=False)

        Both raise/return error if directory already exists.
        This is the core atomic operation for claim coordination.

        This difference is ACCEPTABLE:
        - Both are atomic operations
        - Both fail if directory exists
        - Python uses exception instead of exit code
        """
        pass  # Behavior is equivalent
