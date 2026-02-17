"""Tests for loom_tools.claim."""

from __future__ import annotations

import json
import pathlib
from datetime import datetime, timedelta, timezone

import pytest

from loom_tools.claim import (
    ClaimInfo,
    _get_expiration,
    _get_timestamp,
    _is_claim_abandoned,
    _is_expired,
    check_claim,
    claim_issue,
    cleanup_claims,
    extend_claim,
    has_valid_claim,
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


class TestHasValidClaim:
    """Tests for has_valid_claim function."""

    def test_no_claim_returns_false(self, mock_repo: pathlib.Path) -> None:
        assert has_valid_claim(mock_repo, 42) is False

    def test_active_claim_returns_true(self, mock_repo: pathlib.Path) -> None:
        claim_issue(mock_repo, 42, "test-agent", 3600)
        assert has_valid_claim(mock_repo, 42) is True

    def test_expired_claim_returns_false(self, mock_repo: pathlib.Path) -> None:
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
        assert has_valid_claim(mock_repo, 42) is False

    def test_incomplete_claim_returns_false(self, mock_repo: pathlib.Path) -> None:
        claim_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        claim_dir.mkdir(parents=True)
        # Directory exists but no claim.json
        assert has_valid_claim(mock_repo, 42) is False

    def test_abandoned_claim_returns_false(self, mock_repo: pathlib.Path) -> None:
        """A claim with a stale heartbeat should not be considered valid."""
        stale_time = (datetime.now(timezone.utc) - timedelta(seconds=600)).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        )
        claim_time = (datetime.now(timezone.utc) - timedelta(seconds=900)).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        )
        # Create claim by shepherd-abc123
        claim_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        claim_dir.mkdir(parents=True)
        claim_file = claim_dir / "claim.json"
        claim_file.write_text(
            json.dumps(
                {
                    "issue": 42,
                    "agent_id": "shepherd-abc123",
                    "claimed_at": claim_time,
                    "expires_at": "2099-01-01T00:00:00Z",
                    "ttl_seconds": 7200,
                }
            )
        )
        # Create progress file with stale heartbeat
        progress_dir = mock_repo / ".loom" / "progress"
        progress_dir.mkdir(parents=True, exist_ok=True)
        progress_file = progress_dir / "shepherd-abc123.json"
        progress_file.write_text(
            json.dumps(
                {
                    "task_id": "abc123",
                    "issue": 42,
                    "last_heartbeat": stale_time,
                    "status": "working",
                }
            )
        )
        assert has_valid_claim(mock_repo, 42) is False


def _fmt_ts(dt: datetime) -> str:
    """Format datetime as ISO-8601 timestamp for tests."""
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


class TestIsClaimAbandoned:
    """Tests for _is_claim_abandoned function."""

    def test_non_shepherd_claim_never_abandoned(self, mock_repo: pathlib.Path) -> None:
        claim = ClaimInfo(
            issue=42,
            agent_id="builder-1",
            claimed_at="2020-01-01T00:00:00Z",
            expires_at="2099-01-01T00:00:00Z",
            ttl_seconds=7200,
        )
        assert _is_claim_abandoned(mock_repo, claim) is False

    def test_stale_heartbeat_is_abandoned(self, mock_repo: pathlib.Path) -> None:
        """Shepherd with stale heartbeat (>300s) should be abandoned."""
        stale_time = _fmt_ts(datetime.now(timezone.utc) - timedelta(seconds=600))
        claim = ClaimInfo(
            issue=42,
            agent_id="shepherd-abc123",
            claimed_at=_fmt_ts(datetime.now(timezone.utc) - timedelta(seconds=900)),
            expires_at="2099-01-01T00:00:00Z",
            ttl_seconds=7200,
        )
        # Create progress file with stale heartbeat
        progress_dir = mock_repo / ".loom" / "progress"
        progress_dir.mkdir(parents=True, exist_ok=True)
        (progress_dir / "shepherd-abc123.json").write_text(
            json.dumps(
                {
                    "task_id": "abc123",
                    "issue": 42,
                    "last_heartbeat": stale_time,
                    "status": "working",
                }
            )
        )
        assert _is_claim_abandoned(mock_repo, claim) is True

    def test_fresh_heartbeat_not_abandoned(self, mock_repo: pathlib.Path) -> None:
        """Shepherd with recent heartbeat should NOT be abandoned."""
        fresh_time = _fmt_ts(datetime.now(timezone.utc) - timedelta(seconds=30))
        claim = ClaimInfo(
            issue=42,
            agent_id="shepherd-def456",
            claimed_at=_fmt_ts(datetime.now(timezone.utc) - timedelta(seconds=120)),
            expires_at="2099-01-01T00:00:00Z",
            ttl_seconds=7200,
        )
        progress_dir = mock_repo / ".loom" / "progress"
        progress_dir.mkdir(parents=True, exist_ok=True)
        (progress_dir / "shepherd-def456.json").write_text(
            json.dumps(
                {
                    "task_id": "def456",
                    "issue": 42,
                    "last_heartbeat": fresh_time,
                    "status": "working",
                }
            )
        )
        assert _is_claim_abandoned(mock_repo, claim) is False

    def test_no_progress_file_old_claim_abandoned(
        self, mock_repo: pathlib.Path
    ) -> None:
        """Shepherd with no progress file and old claim (>10min) is abandoned."""
        old_time = _fmt_ts(datetime.now(timezone.utc) - timedelta(seconds=900))
        claim = ClaimInfo(
            issue=42,
            agent_id="shepherd-ghi789",
            claimed_at=old_time,
            expires_at="2099-01-01T00:00:00Z",
            ttl_seconds=7200,
        )
        # No progress file created
        assert _is_claim_abandoned(mock_repo, claim) is True

    def test_no_progress_file_recent_claim_not_abandoned(
        self, mock_repo: pathlib.Path
    ) -> None:
        """Shepherd with no progress file but recent claim (<10min) is NOT abandoned."""
        recent_time = _fmt_ts(datetime.now(timezone.utc) - timedelta(seconds=60))
        claim = ClaimInfo(
            issue=42,
            agent_id="shepherd-jkl012",
            claimed_at=recent_time,
            expires_at="2099-01-01T00:00:00Z",
            ttl_seconds=7200,
        )
        assert _is_claim_abandoned(mock_repo, claim) is False


class TestClaimStealing:
    """Integration tests for claim stealing via stale heartbeat detection."""

    def test_steal_claim_from_dead_shepherd(self, mock_repo: pathlib.Path) -> None:
        """A new shepherd should be able to steal a claim from a dead one."""
        stale_time = _fmt_ts(datetime.now(timezone.utc) - timedelta(seconds=600))
        claim_time = _fmt_ts(datetime.now(timezone.utc) - timedelta(seconds=900))

        # Create claim by dead shepherd
        claim_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        claim_dir.mkdir(parents=True)
        (claim_dir / "claim.json").write_text(
            json.dumps(
                {
                    "issue": 42,
                    "agent_id": "shepherd-dead123",
                    "claimed_at": claim_time,
                    "expires_at": "2099-01-01T00:00:00Z",
                    "ttl_seconds": 7200,
                }
            )
        )

        # Create stale progress file
        progress_dir = mock_repo / ".loom" / "progress"
        progress_dir.mkdir(parents=True, exist_ok=True)
        (progress_dir / "shepherd-dead123.json").write_text(
            json.dumps(
                {
                    "task_id": "dead123",
                    "issue": 42,
                    "last_heartbeat": stale_time,
                    "status": "working",
                }
            )
        )

        # New shepherd should be able to claim the issue
        result = claim_issue(mock_repo, 42, "shepherd-new456", 7200)
        assert result == 0

        # Verify new agent owns the claim
        claim_file = claim_dir / "claim.json"
        data = json.loads(claim_file.read_text())
        assert data["agent_id"] == "shepherd-new456"

    def test_cannot_steal_from_active_shepherd(
        self, mock_repo: pathlib.Path
    ) -> None:
        """A new shepherd should NOT steal a claim from an active one."""
        fresh_time = _fmt_ts(datetime.now(timezone.utc) - timedelta(seconds=30))
        claim_time = _fmt_ts(datetime.now(timezone.utc) - timedelta(seconds=120))

        # Create claim by active shepherd
        claim_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        claim_dir.mkdir(parents=True)
        (claim_dir / "claim.json").write_text(
            json.dumps(
                {
                    "issue": 42,
                    "agent_id": "shepherd-active789",
                    "claimed_at": claim_time,
                    "expires_at": "2099-01-01T00:00:00Z",
                    "ttl_seconds": 7200,
                }
            )
        )

        # Create fresh progress file
        progress_dir = mock_repo / ".loom" / "progress"
        progress_dir.mkdir(parents=True, exist_ok=True)
        (progress_dir / "shepherd-active789.json").write_text(
            json.dumps(
                {
                    "task_id": "active789",
                    "issue": 42,
                    "last_heartbeat": fresh_time,
                    "status": "working",
                }
            )
        )

        # New shepherd should be blocked
        result = claim_issue(mock_repo, 42, "shepherd-intruder", 7200)
        assert result == 1

    def test_steal_claim_no_progress_file_old(
        self, mock_repo: pathlib.Path
    ) -> None:
        """Can steal a claim when there's no progress file and claim is >10min old."""
        old_time = _fmt_ts(datetime.now(timezone.utc) - timedelta(seconds=900))

        # Create old claim with no progress file
        claim_dir = mock_repo / ".loom" / "claims" / "issue-42.lock"
        claim_dir.mkdir(parents=True)
        (claim_dir / "claim.json").write_text(
            json.dumps(
                {
                    "issue": 42,
                    "agent_id": "shepherd-vanished",
                    "claimed_at": old_time,
                    "expires_at": "2099-01-01T00:00:00Z",
                    "ttl_seconds": 7200,
                }
            )
        )

        # Should be able to steal
        result = claim_issue(mock_repo, 42, "shepherd-replacement", 7200)
        assert result == 0
