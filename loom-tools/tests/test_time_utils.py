"""Tests for loom_tools.common.time_utils."""

from __future__ import annotations

from datetime import datetime, timezone
from unittest.mock import patch

from loom_tools.common.time_utils import (
    elapsed_seconds,
    format_duration,
    now_utc,
    parse_iso_timestamp,
)


def test_parse_iso_timestamp_z() -> None:
    dt = parse_iso_timestamp("2026-01-23T10:00:00Z")
    assert dt == datetime(2026, 1, 23, 10, 0, 0, tzinfo=timezone.utc)


def test_parse_iso_timestamp_offset() -> None:
    dt = parse_iso_timestamp("2026-01-23T10:00:00+00:00")
    assert dt == datetime(2026, 1, 23, 10, 0, 0, tzinfo=timezone.utc)


def test_now_utc_has_tzinfo() -> None:
    dt = now_utc()
    assert dt.tzinfo is not None
    assert dt.tzinfo == timezone.utc


def test_elapsed_seconds() -> None:
    fixed_now = datetime(2026, 1, 23, 10, 1, 30, tzinfo=timezone.utc)
    with patch("loom_tools.common.time_utils.now_utc", return_value=fixed_now):
        assert elapsed_seconds("2026-01-23T10:00:00Z") == 90


def test_format_duration_seconds_only() -> None:
    assert format_duration(5) == "5s"


def test_format_duration_minutes_seconds() -> None:
    assert format_duration(90) == "1m 30s"


def test_format_duration_hours_minutes_seconds() -> None:
    assert format_duration(3661) == "1h 1m 1s"


def test_format_duration_exact_hour() -> None:
    assert format_duration(3600) == "1h"


def test_format_duration_exact_minute() -> None:
    assert format_duration(60) == "1m"


def test_format_duration_zero() -> None:
    assert format_duration(0) == "0s"


def test_format_duration_negative() -> None:
    assert format_duration(-10) == "0s"
