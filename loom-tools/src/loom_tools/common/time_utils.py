"""Timestamp helpers for ISO-8601 parsing and human-readable durations."""

from __future__ import annotations

from datetime import datetime, timezone


def parse_iso_timestamp(s: str) -> datetime:
    """Parse an ISO-8601 timestamp like ``2026-01-23T10:00:00Z``.

    Accepts trailing ``Z`` (replaced with ``+00:00``) and the standard
    ``+HH:MM`` offset notation.
    """
    if s.endswith("Z"):
        s = s[:-1] + "+00:00"
    return datetime.fromisoformat(s)


def now_utc() -> datetime:
    """Return the current time in UTC with timezone info."""
    return datetime.now(timezone.utc)


def elapsed_seconds(ts: str) -> int:
    """Seconds elapsed since the ISO-8601 timestamp *ts*."""
    dt = parse_iso_timestamp(ts)
    delta = now_utc() - dt
    return int(delta.total_seconds())


def format_duration(seconds: int) -> str:
    """Format *seconds* as a human-readable string.

    Examples::

        format_duration(90)   -> "1m 30s"
        format_duration(3661) -> "1h 1m 1s"
        format_duration(5)    -> "5s"
    """
    if seconds < 0:
        return "0s"
    parts: list[str] = []
    hours, remainder = divmod(seconds, 3600)
    minutes, secs = divmod(remainder, 60)
    if hours:
        parts.append(f"{hours}h")
    if minutes:
        parts.append(f"{minutes}m")
    if secs or not parts:
        parts.append(f"{secs}s")
    return " ".join(parts)
