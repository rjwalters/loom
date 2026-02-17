"""Atomic file-based claiming system for parallel agent orchestration.

Uses mkdir for atomic claim creation (succeeds or fails atomically on all platforms).
Claims are stored in .loom/claims/issue-<N>.lock directories with metadata.

Exit codes:
    0 - Success
    1 - Claim already exists (for claim), or general error
    2 - Invalid arguments
    3 - Claim not found (for release/check)
    4 - Agent ID mismatch (for release)
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import socket
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any

from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_json_file, write_json_file

# Default TTL: 30 minutes
DEFAULT_TTL = 1800

# Default heartbeat staleness threshold: 5 minutes (300 seconds)
DEFAULT_HEARTBEAT_STALE_THRESHOLD = int(
    os.environ.get("LOOM_HEARTBEAT_STALE_THRESHOLD", "300")
)

# Threshold for claims without progress files: 10 minutes (600 seconds)
NO_PROGRESS_FILE_THRESHOLD = 600


@dataclass
class ClaimInfo:
    """Information about a claim."""

    issue: int
    agent_id: str
    claimed_at: str
    expires_at: str
    ttl_seconds: int

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> ClaimInfo:
        return cls(
            issue=data.get("issue", 0),
            agent_id=data.get("agent_id", ""),
            claimed_at=data.get("claimed_at", ""),
            expires_at=data.get("expires_at", ""),
            ttl_seconds=data.get("ttl_seconds", DEFAULT_TTL),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "issue": self.issue,
            "agent_id": self.agent_id,
            "claimed_at": self.claimed_at,
            "expires_at": self.expires_at,
            "ttl_seconds": self.ttl_seconds,
        }


def _now_utc() -> datetime:
    """Return current time in UTC with timezone info."""
    return datetime.now(timezone.utc)


def _format_iso_timestamp(dt: datetime) -> str:
    """Format datetime as ISO-8601 timestamp with Z suffix."""
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


def _get_timestamp() -> str:
    """Get current timestamp in ISO format."""
    return _format_iso_timestamp(_now_utc())


def _get_expiration(ttl: int) -> str:
    """Get expiration timestamp (current time + TTL seconds)."""
    from datetime import timedelta

    expiration = _now_utc() + timedelta(seconds=ttl)
    return _format_iso_timestamp(expiration)


def _is_expired(expiration: str) -> bool:
    """Check if a claim has expired by comparing ISO timestamps."""
    current = _get_timestamp()
    # ISO timestamps are lexicographically sortable
    return current > expiration


def _get_agent_id(provided: str | None) -> str:
    """Generate default agent ID if not provided."""
    if provided:
        return provided
    # Use hostname-pid as default agent ID
    return f"{socket.gethostname()}-{os.getpid()}"


def _get_claims_dir(repo_root: pathlib.Path) -> pathlib.Path:
    """Get the claims directory path."""
    return repo_root / ".loom" / "claims"


def _get_claim_dir(repo_root: pathlib.Path, issue_number: int) -> pathlib.Path:
    """Get the claim directory path for an issue."""
    return _get_claims_dir(repo_root) / f"issue-{issue_number}.lock"


def _ensure_claims_dir(repo_root: pathlib.Path) -> None:
    """Ensure the claims directory exists."""
    _get_claims_dir(repo_root).mkdir(parents=True, exist_ok=True)


def _read_claim(claim_file: pathlib.Path) -> ClaimInfo | None:
    """Read claim info from a claim file."""
    data = read_json_file(claim_file)
    if not data or not isinstance(data, dict):
        return None
    return ClaimInfo.from_dict(data)


def _is_claim_abandoned(
    repo_root: pathlib.Path,
    claim: ClaimInfo,
) -> bool:
    """Check if a claim is abandoned by inspecting the owner's progress file heartbeat.

    A claim is considered abandoned if:
    1. The owner's progress file exists and last_heartbeat is stale
       (older than LOOM_HEARTBEAT_STALE_THRESHOLD, default 300s).
    2. No progress file exists and the claim is older than 10 minutes.

    Returns True if the claim should be treated as abandoned.
    """
    # Extract task_id from agent_id (format: "shepherd-{task_id}")
    if not claim.agent_id.startswith("shepherd-"):
        # Non-shepherd claims can't be checked via progress files
        return False

    task_id = claim.agent_id[len("shepherd-"):]
    progress_file = repo_root / ".loom" / "progress" / f"shepherd-{task_id}.json"

    now = _now_utc()

    if progress_file.exists():
        data = read_json_file(progress_file)
        if data and isinstance(data, dict):
            last_heartbeat = data.get("last_heartbeat")
            if last_heartbeat:
                try:
                    hb_time = datetime.strptime(
                        last_heartbeat, "%Y-%m-%dT%H:%M:%SZ"
                    ).replace(tzinfo=timezone.utc)
                    age = (now - hb_time).total_seconds()
                    if age > DEFAULT_HEARTBEAT_STALE_THRESHOLD:
                        return True
                except (ValueError, TypeError):
                    pass
        # Progress file exists but no valid heartbeat — treat as stale
        # if the claim itself is old enough
        return False

    # No progress file: treat as abandoned if claim is older than 10 minutes
    claimed_at = claim.claimed_at
    if claimed_at:
        try:
            claim_time = datetime.strptime(
                claimed_at, "%Y-%m-%dT%H:%M:%SZ"
            ).replace(tzinfo=timezone.utc)
            claim_age = (now - claim_time).total_seconds()
            if claim_age > NO_PROGRESS_FILE_THRESHOLD:
                return True
        except (ValueError, TypeError):
            pass

    return False


def claim_issue(
    repo_root: pathlib.Path,
    issue_number: int,
    agent_id: str | None = None,
    ttl: int = DEFAULT_TTL,
) -> int:
    """Claim an issue atomically.

    Returns:
        0 on success, 1 if already claimed, 2 on invalid args.
    """
    agent = _get_agent_id(agent_id)
    _ensure_claims_dir(repo_root)

    claim_dir = _get_claim_dir(repo_root, issue_number)
    claim_file = claim_dir / "claim.json"

    # Attempt atomic directory creation
    # mkdir will raise FileExistsError if directory already exists
    try:
        claim_dir.mkdir(parents=False, exist_ok=False)

        # Successfully created - write metadata
        timestamp = _get_timestamp()
        expiration = _get_expiration(ttl)

        claim_info = ClaimInfo(
            issue=issue_number,
            agent_id=agent,
            claimed_at=timestamp,
            expires_at=expiration,
            ttl_seconds=ttl,
        )
        write_json_file(claim_file, claim_info.to_dict())

        log_success(f"Claimed issue #{issue_number}")
        log_info(f"  Agent: {agent}")
        log_info(f"  Expires: {expiration}")
        return 0

    except FileExistsError:
        # Directory already exists - check if claim is expired
        if claim_file.exists():
            existing = _read_claim(claim_file)
            if existing and _is_expired(existing.expires_at):
                # Expired claim - clean up and retry
                log_warning("Found expired claim, cleaning up...")
                _remove_claim_dir(claim_dir)
                return claim_issue(repo_root, issue_number, agent_id, ttl)
            elif existing and _is_claim_abandoned(repo_root, existing):
                # Claim is still within TTL but owner's heartbeat is stale
                log_warning(
                    f"Claim by {existing.agent_id} appears abandoned "
                    f"(stale heartbeat), stealing claim..."
                )
                _remove_claim_dir(claim_dir)
                return claim_issue(repo_root, issue_number, agent_id, ttl)
            elif existing:
                # Active claim by another agent
                log_error(f"Issue #{issue_number} already claimed")
                log_error(f"  By: {existing.agent_id}")
                log_error(f"  Expires: {existing.expires_at}")
                return 1
            else:
                # Lock dir exists but claim file is empty/corrupt - clean up
                log_warning("Found incomplete claim, cleaning up...")
                _remove_claim_dir(claim_dir)
                return claim_issue(repo_root, issue_number, agent_id, ttl)
        else:
            # Lock dir exists but no claim file - clean up and retry
            log_warning("Found incomplete claim, cleaning up...")
            _remove_claim_dir(claim_dir)
            return claim_issue(repo_root, issue_number, agent_id, ttl)


def _remove_claim_dir(claim_dir: pathlib.Path) -> None:
    """Remove a claim directory and its contents."""
    import shutil

    if claim_dir.exists():
        shutil.rmtree(claim_dir)


def extend_claim(
    repo_root: pathlib.Path,
    issue_number: int,
    agent_id: str,
    additional_seconds: int = DEFAULT_TTL,
) -> int:
    """Extend a claim's TTL.

    Returns:
        0 on success, 3 if no claim exists, 4 if agent mismatch.
    """
    claim_dir = _get_claim_dir(repo_root, issue_number)
    claim_file = claim_dir / "claim.json"

    if not claim_dir.exists():
        log_warning(f"No claim found for issue #{issue_number}")
        return 3

    if not claim_file.exists():
        log_warning(f"Incomplete claim found for issue #{issue_number}")
        return 3

    existing = _read_claim(claim_file)
    if not existing:
        log_warning(f"Incomplete claim found for issue #{issue_number}")
        return 3

    # Verify agent owns the claim
    if existing.agent_id != agent_id:
        log_error("Cannot extend: claim owned by different agent")
        log_error(f"  Owner: {existing.agent_id}")
        log_error(f"  Requested by: {agent_id}")
        return 4

    # Calculate new expiration from now + additional_seconds
    new_expiration = _get_expiration(additional_seconds)

    # Update claim with new expiration
    updated_claim = ClaimInfo(
        issue=existing.issue,
        agent_id=existing.agent_id,
        claimed_at=existing.claimed_at,
        expires_at=new_expiration,
        ttl_seconds=additional_seconds,
    )
    write_json_file(claim_file, updated_claim.to_dict())

    log_success(f"Extended claim for issue #{issue_number}")
    log_info(f"  New expiration: {new_expiration}")
    log_info(f"  Extended by: {additional_seconds} seconds")
    return 0


def release_claim(
    repo_root: pathlib.Path,
    issue_number: int,
    agent_id: str | None = None,
) -> int:
    """Release a claim.

    Returns:
        0 on success, 3 if no claim exists, 4 if agent mismatch.
    """
    claim_dir = _get_claim_dir(repo_root, issue_number)
    claim_file = claim_dir / "claim.json"

    if not claim_dir.exists():
        log_warning(f"No claim found for issue #{issue_number}")
        return 3

    # If agent_id provided, verify it matches
    if agent_id and claim_file.exists():
        existing = _read_claim(claim_file)
        if existing and existing.agent_id != agent_id:
            log_error("Cannot release: claim owned by different agent")
            log_error(f"  Owner: {existing.agent_id}")
            log_error(f"  Requested by: {agent_id}")
            return 4

    # Remove the claim
    _remove_claim_dir(claim_dir)
    log_success(f"Released claim for issue #{issue_number}")
    return 0


def check_claim(repo_root: pathlib.Path, issue_number: int) -> int:
    """Check if an issue is claimed and print claim metadata.

    Returns:
        0 if claimed, 3 if not claimed or expired.
    """
    claim_dir = _get_claim_dir(repo_root, issue_number)
    claim_file = claim_dir / "claim.json"

    if not claim_dir.exists():
        log_info(f"Issue #{issue_number} is not claimed")
        return 3

    if not claim_file.exists():
        log_warning(f"Incomplete claim found for issue #{issue_number}")
        return 3

    existing = _read_claim(claim_file)
    if not existing:
        log_warning(f"Incomplete claim found for issue #{issue_number}")
        return 3

    if _is_expired(existing.expires_at):
        log_warning(f"Issue #{issue_number} has an expired claim")
        print(json.dumps(existing.to_dict(), indent=2))
        return 3

    log_success(f"Issue #{issue_number} is claimed")
    print(json.dumps(existing.to_dict(), indent=2))
    return 0


def has_valid_claim(repo_root: pathlib.Path, issue_number: int) -> bool:
    """Check if an issue has a valid (non-expired) claim.

    Unlike ``check_claim``, this returns a boolean and does not print
    anything, making it suitable for programmatic checks.
    """
    claim_dir = _get_claim_dir(repo_root, issue_number)
    claim_file = claim_dir / "claim.json"

    if not claim_dir.exists() or not claim_file.exists():
        return False

    existing = _read_claim(claim_file)
    if not existing:
        return False

    if _is_expired(existing.expires_at):
        return False
    if _is_claim_abandoned(repo_root, existing):
        return False
    return True


def list_claims(repo_root: pathlib.Path) -> int:
    """List all active claims."""
    _ensure_claims_dir(repo_root)
    claims_dir = _get_claims_dir(repo_root)

    count = 0
    print("Active claims:\n")

    for claim_dir in sorted(claims_dir.glob("issue-*.lock")):
        if not claim_dir.is_dir():
            continue

        claim_file = claim_dir / "claim.json"
        if not claim_file.exists():
            continue

        existing = _read_claim(claim_file)
        if not existing:
            continue

        if _is_expired(existing.expires_at):
            print(f"  Issue #{existing.issue} (EXPIRED)")
        else:
            print(
                f"  Issue #{existing.issue} - "
                f"Agent: {existing.agent_id}, "
                f"Expires: {existing.expires_at}"
            )
        count += 1

    if count == 0:
        print("  (none)")

    print(f"\nTotal: {count} claim(s)")
    return 0


def cleanup_claims(repo_root: pathlib.Path) -> int:
    """Remove all expired claims."""
    _ensure_claims_dir(repo_root)
    claims_dir = _get_claims_dir(repo_root)

    cleaned = 0
    log_info("Cleaning up expired claims...")

    for claim_dir in sorted(claims_dir.glob("issue-*.lock")):
        if not claim_dir.is_dir():
            continue

        claim_file = claim_dir / "claim.json"
        if claim_file.exists():
            existing = _read_claim(claim_file)
            if existing and _is_expired(existing.expires_at):
                _remove_claim_dir(claim_dir)
                log_success(f"Removed expired claim for issue #{existing.issue}")
                cleaned += 1
        else:
            # No claim file - incomplete claim, remove it
            _remove_claim_dir(claim_dir)
            cleaned += 1

    if cleaned == 0:
        log_info("No expired claims found")
    else:
        print(f"\nCleaned up {cleaned} expired claim(s)")

    return 0


def main(argv: list[str] | None = None) -> int:
    """Main entry point for the claim CLI."""
    parser = argparse.ArgumentParser(
        description="Atomic file-based claiming system for parallel agent orchestration",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Commands:
  claim <issue-number> [agent-id] [ttl-seconds]
      Atomically claim an issue. Default TTL is 30 minutes (1800 seconds).
      Exits 0 on success, 1 if already claimed.

  extend <issue-number> <agent-id> [additional-seconds]
      Extend an existing claim's TTL. Agent must own the claim.
      Default extension is 30 minutes (1800 seconds) from now.
      Exits 0 on success, 3 if no claim exists, 4 if agent mismatch.

  release <issue-number> [agent-id]
      Release a claim. If agent-id is provided, verifies ownership.
      Exits 0 on success, 3 if no claim exists, 4 if agent mismatch.

  check <issue-number>
      Check if an issue is claimed and print claim metadata.
      Exits 0 if claimed, 3 if not claimed or expired.

  list
      List all active claims.

  cleanup
      Remove all expired claims.

Exit codes:
  0 - Success
  1 - Claim already exists (for claim), or general error
  2 - Invalid arguments
  3 - Claim not found (for release/check)
  4 - Agent ID mismatch (for release)

Examples:
  loom-claim claim 123                    # Claim issue with default agent ID
  loom-claim claim 123 builder-1 3600     # Claim for 1 hour
  loom-claim extend 123 builder-1         # Extend by default 30 minutes
  loom-claim extend 123 builder-1 7200    # Extend by 2 hours
  loom-claim release 123 builder-1        # Release with ownership check
  loom-claim check 123                    # Check claim status
  loom-claim list                         # List all claims
  loom-claim cleanup                      # Clean expired claims
""",
    )

    parser.add_argument("command", nargs="?", help="Command to execute")
    parser.add_argument("args", nargs="*", help="Command arguments")

    args = parser.parse_args(argv)

    if not args.command or args.command in ("-h", "--help", "help"):
        parser.print_help()
        return 0

    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return 2

    cmd = args.command
    cmd_args = args.args

    if cmd == "claim":
        if not cmd_args:
            log_error("Issue number required")
            return 2
        try:
            issue_number = int(cmd_args[0])
        except ValueError:
            log_error(f"Invalid issue number: {cmd_args[0]}")
            return 2
        agent_id = cmd_args[1] if len(cmd_args) > 1 else None
        ttl = int(cmd_args[2]) if len(cmd_args) > 2 else DEFAULT_TTL
        return claim_issue(repo_root, issue_number, agent_id, ttl)

    elif cmd == "extend":
        if len(cmd_args) < 2:
            log_error("Issue number and agent ID required for extend")
            return 2
        try:
            issue_number = int(cmd_args[0])
        except ValueError:
            log_error(f"Invalid issue number: {cmd_args[0]}")
            return 2
        agent_id = cmd_args[1]
        additional = int(cmd_args[2]) if len(cmd_args) > 2 else DEFAULT_TTL
        return extend_claim(repo_root, issue_number, agent_id, additional)

    elif cmd == "release":
        if not cmd_args:
            log_error("Issue number required")
            return 2
        try:
            issue_number = int(cmd_args[0])
        except ValueError:
            log_error(f"Invalid issue number: {cmd_args[0]}")
            return 2
        agent_id = cmd_args[1] if len(cmd_args) > 1 else None
        return release_claim(repo_root, issue_number, agent_id)

    elif cmd == "check":
        if not cmd_args:
            log_error("Issue number required")
            return 2
        try:
            issue_number = int(cmd_args[0])
        except ValueError:
            log_error(f"Invalid issue number: {cmd_args[0]}")
            return 2
        return check_claim(repo_root, issue_number)

    elif cmd == "list":
        return list_claims(repo_root)

    elif cmd == "cleanup":
        return cleanup_claims(repo_root)

    else:
        log_error(f"Unknown command '{cmd}'")
        parser.print_help()
        return 2


if __name__ == "__main__":
    sys.exit(main())
