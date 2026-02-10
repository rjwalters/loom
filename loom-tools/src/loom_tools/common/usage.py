"""Claude API usage checking via Anthropic OAuth API.

Reads the Claude Code OAuth token from the macOS Keychain and queries
``GET https://api.anthropic.com/api/oauth/usage`` to get current session
and weekly utilization.  Results are cached to ``.loom/usage-cache.json``
to avoid hammering the API on every snapshot/rate-limit check.

Falls back gracefully on any failure (missing keychain entry, expired
token, network issues) by returning ``{"error": "..."}`` dicts.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any

from loom_tools.common.state import read_json_file, write_json_file

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

_KEYCHAIN_SERVICE = "Claude Code-credentials"
_USAGE_API_URL = "https://api.anthropic.com/api/oauth/usage"
_ANTHROPIC_BETA = "oauth-2025-04-20"
_USER_AGENT = "claude-code/2.0.32"

_DEFAULT_CACHE_TTL = 60  # seconds


# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------


def _read_keychain_token() -> str | None:
    """Read the Claude Code OAuth access token from the macOS Keychain.

    Returns the access token string, or ``None`` if the credential
    cannot be read or parsed.
    """
    try:
        result = subprocess.run(
            ["security", "find-generic-password", "-s", _KEYCHAIN_SERVICE, "-w"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode != 0:
            return None
        raw = result.stdout.strip()
        if not raw:
            return None
    except (subprocess.TimeoutExpired, OSError):
        return None

    # Parse the JSON credential blob
    try:
        data = json.loads(raw)
    except json.JSONDecodeError:
        return None

    # Handle wrapper format: {"claudeAiOauth": {... "accessToken": "..."}}
    if "claudeAiOauth" in data:
        data = data["claudeAiOauth"]

    # Accept both camelCase and snake_case key variants
    return data.get("accessToken") or data.get("access_token")


def _call_usage_api(token: str) -> dict | None:
    """Call the Anthropic OAuth usage API.

    Returns parsed JSON dict on success, ``None`` on any failure.
    """
    try:
        result = subprocess.run(
            [
                "curl", "-s", "-f",
                "-H", f"Authorization: Bearer {token}",
                "-H", f"anthropic-beta: {_ANTHROPIC_BETA}",
                "-H", f"User-Agent: {_USER_AGENT}",
                _USAGE_API_URL,
            ],
            capture_output=True,
            text=True,
            timeout=15,
        )
        if result.returncode != 0:
            return None
        return json.loads(result.stdout)
    except (subprocess.TimeoutExpired, OSError, json.JSONDecodeError):
        return None


def _transform_api_response(api_data: dict[str, Any]) -> dict[str, Any]:
    """Map the Anthropic usage API response to our backward-compatible format.

    API shape (simplified)::

        {
          "five_hour": {"utilization": 42.0, "resets_at": "2026-01-23T15:00:00Z"},
          "seven_day": {"utilization": 15.0, "resets_at": "2026-01-27T00:00:00Z"}
        }

    Output shape::

        {
          "session_percent": 42.0,
          "session_reset": "2026-01-23T15:00:00Z",
          "weekly_all_percent": 15.0,
          "weekly_reset": "2026-01-27T00:00:00Z",
          "timestamp": "2026-01-23T12:34:56Z",
          "data_age_seconds": 0
        }
    """
    from datetime import datetime, timezone

    five = api_data.get("five_hour") or {}
    seven = api_data.get("seven_day") or {}

    session_util = five.get("utilization")
    weekly_util = seven.get("utilization")

    return {
        "session_percent": round(session_util, 1) if session_util is not None else None,
        "session_reset": five.get("resets_at"),
        "weekly_all_percent": round(weekly_util, 1) if weekly_util is not None else None,
        "weekly_reset": seven.get("resets_at"),
        "timestamp": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "data_age_seconds": 0,
    }


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------


def get_usage(
    repo_root: Path | str,
    ttl_seconds: int | None = None,
) -> dict[str, Any]:
    """Return current Claude API usage data.

    Checks a file cache first (``.loom/usage-cache.json``).  If the
    cache is fresh (< *ttl_seconds*), returns the cached value.
    Otherwise queries the Anthropic API via the macOS Keychain token.

    On any failure returns ``{"error": "<reason>"}``.

    Args:
        repo_root: Repository root path.
        ttl_seconds: Cache TTL in seconds.  Defaults to the
            ``LOOM_USAGE_CACHE_TTL`` env var, or 60.
    """
    repo_root = Path(repo_root)
    if ttl_seconds is None:
        ttl_seconds = int(os.environ.get("LOOM_USAGE_CACHE_TTL", _DEFAULT_CACHE_TTL))

    cache_path = repo_root / ".loom" / "usage-cache.json"

    # Check file cache
    cached = read_json_file(cache_path)
    if isinstance(cached, dict) and "error" not in cached:
        ts = cached.get("timestamp")
        if ts:
            try:
                from datetime import datetime, timezone

                cached_time = datetime.fromisoformat(ts.replace("Z", "+00:00"))
                age = (datetime.now(timezone.utc) - cached_time).total_seconds()
                if age < ttl_seconds:
                    cached["data_age_seconds"] = int(age)
                    return cached
            except (ValueError, TypeError):
                pass  # stale or unparseable — fetch fresh

    # Read token from Keychain
    token = _read_keychain_token()
    if not token:
        return {"error": "no_keychain_token"}

    # Call the API
    api_data = _call_usage_api(token)
    if not api_data:
        return {"error": "api_call_failed"}

    # Transform and cache
    result = _transform_api_response(api_data)
    try:
        write_json_file(cache_path, result)
    except OSError:
        pass  # non-fatal — data is still valid

    return result


def format_usage_status(data: dict[str, Any]) -> str:
    """Return a human-readable usage status string."""
    if "error" in data:
        return f"ERROR: {data['error']}"

    lines: list[str] = []

    ts = data.get("timestamp", "unknown")
    lines.append(f"Claude Usage Status (as of {ts})")
    lines.append("=" * 40)
    lines.append("")

    session_pct = data.get("session_percent")
    lines.append(f"Session:     {session_pct}% used" if session_pct is not None else "Session:     N/A")
    if data.get("session_reset"):
        lines.append(f"  Resets:    {data['session_reset']}")
    lines.append("")

    weekly_pct = data.get("weekly_all_percent")
    lines.append(f"Weekly:      {weekly_pct}% used" if weekly_pct is not None else "Weekly:      N/A")
    if data.get("weekly_reset"):
        lines.append(f"  Resets:    {data['weekly_reset']}")
    lines.append("")

    if session_pct is not None:
        if session_pct >= 97:
            lines.append("RECOMMENDATION: Pause operations until session resets")
        elif session_pct >= 80:
            lines.append("WARNING: Approaching session limit")
        else:
            lines.append("Session usage is healthy")

    return "\n".join(lines)


# ---------------------------------------------------------------------------
# CLI entry point
# ---------------------------------------------------------------------------


def main() -> None:
    """CLI entry point for ``loom-usage``."""
    from loom_tools.common.repo import find_repo_root

    repo_root = find_repo_root()
    if repo_root is None:
        print('{"error": "not_in_repo"}', file=sys.stderr)
        sys.exit(1)

    status_mode = "--status" in sys.argv

    data = get_usage(repo_root)

    if status_mode:
        print(format_usage_status(data))
    else:
        json.dump(data, sys.stdout)
        print()  # trailing newline

    if "error" in data:
        sys.exit(1)


if __name__ == "__main__":
    main()
