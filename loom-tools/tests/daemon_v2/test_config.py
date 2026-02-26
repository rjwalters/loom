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
        assert config.auto_build is False
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

    def test_from_env_with_auto_build_flag(self):
        """Test auto_build passed as argument."""
        config = DaemonConfig.from_env(auto_build=True)
        assert config.auto_build is True
        assert config.force_mode is False

    def test_from_env_auto_build_env_var(self):
        """Test LOOM_AUTO_BUILD environment variable enables auto_build."""
        with patch.dict(os.environ, {"LOOM_AUTO_BUILD": "true"}, clear=True):
            config = DaemonConfig.from_env()
            assert config.auto_build is True

    def test_from_env_auto_build_false_by_default(self):
        """Test auto_build is False by default (no env var)."""
        with patch.dict(os.environ, {}, clear=True):
            config = DaemonConfig.from_env()
            assert config.auto_build is False

    def test_force_mode_implies_auto_build(self):
        """Test that force_mode=True implies auto_build=True."""
        config = DaemonConfig.from_env(force_mode=True)
        assert config.force_mode is True
        assert config.auto_build is True

    def test_merge_mode_implies_auto_build_via_env(self):
        """Test LOOM_FORCE_MODE env var implies auto_build."""
        with patch.dict(os.environ, {"LOOM_FORCE_MODE": "true"}, clear=True):
            config = DaemonConfig.from_env()
            assert config.force_mode is True
            assert config.auto_build is True

    def test_mode_display_support_only(self):
        """Test mode display for default mode (no auto_build, no force)."""
        config = DaemonConfig()
        assert config.mode_display() == "Support-only"

    def test_mode_display_auto_build(self):
        """Test mode display for auto-build mode."""
        config = DaemonConfig(auto_build=True)
        assert config.mode_display() == "Auto-build"

    def test_mode_display_force(self):
        """Test mode display for force mode."""
        config = DaemonConfig(force_mode=True, auto_build=True)
        assert config.mode_display() == "Force"

    def test_mode_display_debug(self):
        """Test mode display for debug mode."""
        config = DaemonConfig(debug_mode=True)
        assert config.mode_display() == "Debug"

    def test_mode_display_force_debug(self):
        """Test mode display for force + debug mode."""
        config = DaemonConfig(force_mode=True, auto_build=True, debug_mode=True)
        assert config.mode_display() == "Force + Debug"

    def test_mode_display_auto_build_debug(self):
        """Test mode display for auto-build + debug mode."""
        config = DaemonConfig(auto_build=True, debug_mode=True)
        assert config.mode_display() == "Auto-build + Debug"
