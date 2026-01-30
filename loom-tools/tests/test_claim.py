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


@pytest.fixture
def mock_repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a mock repo with .git and .loom directories."""
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
