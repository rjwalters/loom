"""Tests for loom_tools.common.usage."""

from __future__ import annotations

import json
import subprocess
from datetime import datetime, timezone
from pathlib import Path
from unittest import mock

import pytest

from loom_tools.common.usage import (
    _call_usage_api,
    _read_keychain_token,
    _transform_api_response,
    format_usage_status,
    get_usage,
)


# ---------------------------------------------------------------------------
# _read_keychain_token
# ---------------------------------------------------------------------------


class TestReadKeychainToken:
    """Tests for _read_keychain_token()."""

    def test_top_level_access_token(self):
        blob = json.dumps({"accessToken": "tok_abc"})
        result = subprocess.CompletedProcess([], 0, stdout=blob + "\n", stderr="")
        with mock.patch("subprocess.run", return_value=result):
            assert _read_keychain_token() == "tok_abc"

    def test_snake_case_key(self):
        blob = json.dumps({"access_token": "tok_snake"})
        result = subprocess.CompletedProcess([], 0, stdout=blob, stderr="")
        with mock.patch("subprocess.run", return_value=result):
            assert _read_keychain_token() == "tok_snake"

    def test_wrapper_format(self):
        blob = json.dumps({"claudeAiOauth": {"accessToken": "tok_wrap"}})
        result = subprocess.CompletedProcess([], 0, stdout=blob, stderr="")
        with mock.patch("subprocess.run", return_value=result):
            assert _read_keychain_token() == "tok_wrap"

    def test_wrapper_snake_case(self):
        blob = json.dumps({"claudeAiOauth": {"access_token": "tok_ws"}})
        result = subprocess.CompletedProcess([], 0, stdout=blob, stderr="")
        with mock.patch("subprocess.run", return_value=result):
            assert _read_keychain_token() == "tok_ws"

    def test_nonzero_exit_code(self):
        result = subprocess.CompletedProcess([], 1, stdout="", stderr="err")
        with mock.patch("subprocess.run", return_value=result):
            assert _read_keychain_token() is None

    def test_empty_stdout(self):
        result = subprocess.CompletedProcess([], 0, stdout="", stderr="")
        with mock.patch("subprocess.run", return_value=result):
            assert _read_keychain_token() is None

    def test_invalid_json(self):
        result = subprocess.CompletedProcess([], 0, stdout="not json", stderr="")
        with mock.patch("subprocess.run", return_value=result):
            assert _read_keychain_token() is None

    def test_missing_token_key(self):
        blob = json.dumps({"someOtherKey": "value"})
        result = subprocess.CompletedProcess([], 0, stdout=blob, stderr="")
        with mock.patch("subprocess.run", return_value=result):
            assert _read_keychain_token() is None

    def test_timeout(self):
        with mock.patch("subprocess.run", side_effect=subprocess.TimeoutExpired("cmd", 10)):
            assert _read_keychain_token() is None

    def test_oserror(self):
        with mock.patch("subprocess.run", side_effect=OSError("no such binary")):
            assert _read_keychain_token() is None


# ---------------------------------------------------------------------------
# _call_usage_api
# ---------------------------------------------------------------------------


class TestCallUsageApi:
    """Tests for _call_usage_api()."""

    def test_success(self):
        api_response = {"five_hour": {"utilization": 0.5}, "seven_day": {"utilization": 0.1}}
        result = subprocess.CompletedProcess([], 0, stdout=json.dumps(api_response), stderr="")
        with mock.patch("subprocess.run", return_value=result):
            data = _call_usage_api("tok_abc")
        assert data == api_response

    def test_curl_failure(self):
        result = subprocess.CompletedProcess([], 22, stdout="", stderr="404")
        with mock.patch("subprocess.run", return_value=result):
            assert _call_usage_api("tok_abc") is None

    def test_invalid_json_response(self):
        result = subprocess.CompletedProcess([], 0, stdout="not json", stderr="")
        with mock.patch("subprocess.run", return_value=result):
            assert _call_usage_api("tok_abc") is None

    def test_timeout(self):
        with mock.patch("subprocess.run", side_effect=subprocess.TimeoutExpired("cmd", 15)):
            assert _call_usage_api("tok_abc") is None

    def test_passes_bearer_token(self):
        result = subprocess.CompletedProcess([], 0, stdout="{}", stderr="")
        with mock.patch("subprocess.run", return_value=result) as m:
            _call_usage_api("my_token")
        args = m.call_args[0][0]
        assert "Authorization: Bearer my_token" in " ".join(args)


# ---------------------------------------------------------------------------
# _transform_api_response
# ---------------------------------------------------------------------------


class TestTransformApiResponse:
    """Tests for _transform_api_response()."""

    def test_full_response(self):
        api_data = {
            "five_hour": {"utilization": 42.0, "resets_at": "2026-01-23T15:00:00Z"},
            "seven_day": {"utilization": 15.0, "resets_at": "2026-01-27T00:00:00Z"},
        }
        result = _transform_api_response(api_data)
        assert result["session_percent"] == 42.0
        assert result["session_reset"] == "2026-01-23T15:00:00Z"
        assert result["weekly_all_percent"] == 15.0
        assert result["weekly_reset"] == "2026-01-27T00:00:00Z"
        assert result["data_age_seconds"] == 0
        assert "timestamp" in result

    def test_missing_five_hour(self):
        result = _transform_api_response({"seven_day": {"utilization": 30.0}})
        assert result["session_percent"] is None
        assert result["session_reset"] is None
        assert result["weekly_all_percent"] == 30.0

    def test_missing_utilization(self):
        result = _transform_api_response({"five_hour": {}, "seven_day": {}})
        assert result["session_percent"] is None
        assert result["weekly_all_percent"] is None

    def test_empty_response(self):
        result = _transform_api_response({})
        assert result["session_percent"] is None
        assert result["weekly_all_percent"] is None


# ---------------------------------------------------------------------------
# get_usage
# ---------------------------------------------------------------------------


class TestGetUsage:
    """Tests for get_usage()."""

    def test_cache_hit(self, tmp_path):
        now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        cached = {"session_percent": 50.0, "timestamp": now, "data_age_seconds": 0}
        cache_dir = tmp_path / ".loom"
        cache_dir.mkdir()
        (cache_dir / "usage-cache.json").write_text(json.dumps(cached))

        result = get_usage(tmp_path, ttl_seconds=120)
        assert result["session_percent"] == 50.0

    def test_cache_expired(self, tmp_path):
        old_ts = "2020-01-01T00:00:00Z"
        cached = {"session_percent": 50.0, "timestamp": old_ts, "data_age_seconds": 0}
        cache_dir = tmp_path / ".loom"
        cache_dir.mkdir()
        (cache_dir / "usage-cache.json").write_text(json.dumps(cached))

        # Cache expired → should try keychain → no token → error
        with mock.patch("loom_tools.common.usage._read_keychain_token", return_value=None):
            result = get_usage(tmp_path, ttl_seconds=60)
        assert result == {"error": "no_keychain_token"}

    def test_no_cache_no_token(self, tmp_path):
        (tmp_path / ".loom").mkdir()
        with mock.patch("loom_tools.common.usage._read_keychain_token", return_value=None):
            result = get_usage(tmp_path, ttl_seconds=60)
        assert result == {"error": "no_keychain_token"}

    def test_no_cache_api_failure(self, tmp_path):
        (tmp_path / ".loom").mkdir()
        with mock.patch("loom_tools.common.usage._read_keychain_token", return_value="tok"):
            with mock.patch("loom_tools.common.usage._call_usage_api", return_value=None):
                result = get_usage(tmp_path, ttl_seconds=60)
        assert result == {"error": "api_call_failed"}

    def test_success_writes_cache(self, tmp_path):
        (tmp_path / ".loom").mkdir()
        api_data = {
            "five_hour": {"utilization": 60.0, "resets_at": "2026-01-23T15:00:00Z"},
            "seven_day": {"utilization": 20.0, "resets_at": "2026-01-27T00:00:00Z"},
        }
        with mock.patch("loom_tools.common.usage._read_keychain_token", return_value="tok"):
            with mock.patch("loom_tools.common.usage._call_usage_api", return_value=api_data):
                result = get_usage(tmp_path, ttl_seconds=60)

        assert result["session_percent"] == 60.0
        assert result["weekly_all_percent"] == 20.0
        # Cache file should have been written
        cache = json.loads((tmp_path / ".loom" / "usage-cache.json").read_text())
        assert cache["session_percent"] == 60.0

    def test_cache_with_error_key_is_ignored(self, tmp_path):
        cache_dir = tmp_path / ".loom"
        cache_dir.mkdir()
        (cache_dir / "usage-cache.json").write_text(json.dumps({"error": "old_error"}))

        with mock.patch("loom_tools.common.usage._read_keychain_token", return_value=None):
            result = get_usage(tmp_path, ttl_seconds=60)
        assert result == {"error": "no_keychain_token"}

    def test_env_ttl_override(self, tmp_path):
        (tmp_path / ".loom").mkdir()
        with mock.patch("loom_tools.common.usage._read_keychain_token", return_value=None):
            with mock.patch.dict("os.environ", {"LOOM_USAGE_CACHE_TTL": "300"}):
                result = get_usage(tmp_path)
        assert result == {"error": "no_keychain_token"}


# ---------------------------------------------------------------------------
# format_usage_status
# ---------------------------------------------------------------------------


class TestFormatUsageStatus:
    """Tests for format_usage_status()."""

    def test_error(self):
        out = format_usage_status({"error": "no_keychain_token"})
        assert "ERROR" in out
        assert "no_keychain_token" in out

    def test_healthy(self):
        data = {
            "session_percent": 30.0,
            "session_reset": "2026-01-23T15:00:00Z",
            "weekly_all_percent": 10.0,
            "weekly_reset": "2026-01-27T00:00:00Z",
            "timestamp": "2026-01-23T12:00:00Z",
        }
        out = format_usage_status(data)
        assert "30.0%" in out
        assert "healthy" in out.lower()

    def test_warning(self):
        data = {"session_percent": 85.0, "timestamp": "now"}
        out = format_usage_status(data)
        assert "WARNING" in out

    def test_pause_recommendation(self):
        data = {"session_percent": 98.0, "timestamp": "now"}
        out = format_usage_status(data)
        assert "Pause" in out

    def test_none_percent(self):
        data = {"session_percent": None, "timestamp": "now"}
        out = format_usage_status(data)
        assert "N/A" in out
