"""Tests for environment variable parsing utilities."""

from __future__ import annotations

import os

import pytest

from loom_tools.common.config import env_bool, env_float, env_int, env_list, env_str


class TestEnvStr:
    """Tests for env_str function."""

    def test_returns_value_when_set(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_STR", "hello")
        assert env_str("TEST_STR") == "hello"

    def test_returns_default_when_not_set(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.delenv("TEST_STR", raising=False)
        assert env_str("TEST_STR", default="fallback") == "fallback"

    def test_default_is_empty_string(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.delenv("TEST_STR", raising=False)
        assert env_str("TEST_STR") == ""

    def test_preserves_whitespace(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_STR", "  spaced  ")
        assert env_str("TEST_STR") == "  spaced  "

    def test_empty_string_value(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_STR", "")
        assert env_str("TEST_STR", default="fallback") == ""


class TestEnvBool:
    """Tests for env_bool function."""

    @pytest.mark.parametrize("value", ["true", "True", "TRUE", "1", "yes", "YES", "on", "ON"])
    def test_truthy_values(self, monkeypatch: pytest.MonkeyPatch, value: str) -> None:
        monkeypatch.setenv("TEST_BOOL", value)
        assert env_bool("TEST_BOOL") is True

    @pytest.mark.parametrize("value", ["false", "False", "FALSE", "0", "no", "NO", "off", "OFF"])
    def test_falsy_values(self, monkeypatch: pytest.MonkeyPatch, value: str) -> None:
        monkeypatch.setenv("TEST_BOOL", value)
        assert env_bool("TEST_BOOL", default=True) is False

    def test_returns_default_when_not_set(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.delenv("TEST_BOOL", raising=False)
        assert env_bool("TEST_BOOL", default=True) is True
        assert env_bool("TEST_BOOL", default=False) is False

    def test_returns_default_on_invalid_value(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_BOOL", "maybe")
        assert env_bool("TEST_BOOL", default=True) is True
        assert env_bool("TEST_BOOL", default=False) is False

    def test_default_is_false(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.delenv("TEST_BOOL", raising=False)
        assert env_bool("TEST_BOOL") is False

    def test_empty_string_returns_default(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_BOOL", "")
        assert env_bool("TEST_BOOL", default=True) is True


class TestEnvInt:
    """Tests for env_int function."""

    def test_returns_integer_when_valid(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_INT", "42")
        assert env_int("TEST_INT") == 42

    def test_handles_negative_numbers(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_INT", "-5")
        assert env_int("TEST_INT") == -5

    def test_returns_default_when_not_set(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.delenv("TEST_INT", raising=False)
        assert env_int("TEST_INT", default=100) == 100

    def test_returns_default_on_invalid_value(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_INT", "not-a-number")
        assert env_int("TEST_INT", default=100) == 100

    def test_returns_default_on_float_value(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_INT", "3.14")
        assert env_int("TEST_INT", default=100) == 100

    def test_default_is_zero(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.delenv("TEST_INT", raising=False)
        assert env_int("TEST_INT") == 0

    def test_empty_string_returns_default(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_INT", "")
        assert env_int("TEST_INT", default=50) == 50

    def test_whitespace_returns_default(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_INT", "  ")
        assert env_int("TEST_INT", default=50) == 50


class TestEnvFloat:
    """Tests for env_float function."""

    def test_returns_float_when_valid(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_FLOAT", "3.14")
        assert env_float("TEST_FLOAT") == 3.14

    def test_handles_integer_input(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_FLOAT", "42")
        assert env_float("TEST_FLOAT") == 42.0

    def test_handles_negative_numbers(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_FLOAT", "-2.5")
        assert env_float("TEST_FLOAT") == -2.5

    def test_returns_default_when_not_set(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.delenv("TEST_FLOAT", raising=False)
        assert env_float("TEST_FLOAT", default=1.5) == 1.5

    def test_returns_default_on_invalid_value(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_FLOAT", "not-a-number")
        assert env_float("TEST_FLOAT", default=1.5) == 1.5

    def test_default_is_zero(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.delenv("TEST_FLOAT", raising=False)
        assert env_float("TEST_FLOAT") == 0.0

    def test_empty_string_returns_default(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_FLOAT", "")
        assert env_float("TEST_FLOAT", default=2.5) == 2.5


class TestEnvList:
    """Tests for env_list function."""

    def test_returns_list_when_valid(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_LIST", "a,b,c")
        assert env_list("TEST_LIST") == ["a", "b", "c"]

    def test_strips_whitespace(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_LIST", " a , b , c ")
        assert env_list("TEST_LIST") == ["a", "b", "c"]

    def test_filters_empty_items(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_LIST", "a,,b,  ,c")
        assert env_list("TEST_LIST") == ["a", "b", "c"]

    def test_custom_separator(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_LIST", "a:b:c")
        assert env_list("TEST_LIST", sep=":") == ["a", "b", "c"]

    def test_returns_default_when_not_set(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.delenv("TEST_LIST", raising=False)
        assert env_list("TEST_LIST", default=["x", "y"]) == ["x", "y"]

    def test_default_is_empty_list(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.delenv("TEST_LIST", raising=False)
        assert env_list("TEST_LIST") == []

    def test_empty_string_returns_empty_list(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_LIST", "")
        assert env_list("TEST_LIST", default=["x"]) == []

    def test_single_item(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("TEST_LIST", "single")
        assert env_list("TEST_LIST") == ["single"]
