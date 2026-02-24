"""Tests for daemon configuration."""

import os
from unittest.mock import patch

import pytest

from loom_tools.daemon_v2.config import DaemonConfig


class TestDaemonConfig:
    """Tests for DaemonConfig."""

    def test_default_values(self):
        """Test that defaults are sensible."""
        config = DaemonConfig()
        assert config.poll_interval == 30
        assert config.max_shepherds == 10
        assert config.issue_threshold == 3
        assert config.force_mode is False
        assert config.debug_mode is False

    def test_from_env_defaults(self):
        """Test loading from environment with defaults."""
        with patch.dict(os.environ, {}, clear=True):
            config = DaemonConfig.from_env()
            assert config.poll_interval == 30
            assert config.max_shepherds == 10

    def test_from_env_with_overrides(self):
        """Test loading from environment with overrides."""
        env = {
            "LOOM_POLL_INTERVAL": "60",
            "LOOM_MAX_SHEPHERDS": "5",
            "LOOM_ISSUE_THRESHOLD": "10",
            "LOOM_ISSUE_STRATEGY": "lifo",
        }
        with patch.dict(os.environ, env, clear=True):
            config = DaemonConfig.from_env()
            assert config.poll_interval == 60
            assert config.max_shepherds == 5
            assert config.issue_threshold == 10
            assert config.issue_strategy == "lifo"

    def test_from_env_with_force_mode(self):
        """Test force mode passed as argument."""
        config = DaemonConfig.from_env(force_mode=True)
        assert config.force_mode is True

    def test_from_env_with_debug_mode(self):
        """Test debug mode passed as argument."""
        config = DaemonConfig.from_env(debug_mode=True)
        assert config.debug_mode is True

    def test_mode_display_normal(self):
        """Test mode display for normal mode."""
        config = DaemonConfig()
        assert config.mode_display() == "Normal"

    def test_mode_display_force(self):
        """Test mode display for force mode."""
        config = DaemonConfig(force_mode=True)
        assert config.mode_display() == "Force"

    def test_mode_display_debug(self):
        """Test mode display for debug mode."""
        config = DaemonConfig(debug_mode=True)
        assert config.mode_display() == "Debug"

    def test_mode_display_both(self):
        """Test mode display for force + debug mode."""
        config = DaemonConfig(force_mode=True, debug_mode=True)
        assert config.mode_display() == "Force + Debug"
