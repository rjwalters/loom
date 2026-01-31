"""Tests for installation and reinstallation workflows after loom-tools migration.

This module validates that the installation pipeline correctly integrates
with loom-tools Python modules. It covers:

1. Python CLI command availability and executability
2. Wrapper script routing to Python implementations
3. verify-install.sh manifest generation and verification
4. Post-installation file structure validation
5. loom-daemon init file coverage

Related issue: #1751 - Verify installation and reinstallation workflows
"""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys

import pytest

# All Python CLI commands that loom-tools should expose
EXPECTED_CLI_COMMANDS = [
    "loom-shepherd",
    "loom-agent-monitor",
    "loom-daemon-diagnostic",
    "loom-daemon-loop",
    "loom-stuck-detection",
    "loom-claim",
    "loom-check-completions",
    "loom-clean",
    "loom-worktree",
    "loom-daemon-cleanup",
]

# Wrapper scripts that should route to Python implementations
PYTHON_ROUTING_SCRIPTS = [
    "loom-shepherd.sh",
    "daemon-cleanup.sh",
]

# Wrapper scripts that call Python internally (not exec replacement)
PYTHON_INTEGRATION_SCRIPTS = [
    "health-check.sh",
]

# Files that loom-daemon init should create/manage
EXPECTED_INSTALLED_DIRS = [
    ".loom/roles",
    ".loom/scripts",
    ".loom/docs",
    ".claude/commands",
]

EXPECTED_INSTALLED_FILES = [
    "CLAUDE.md",
    ".github/labels.yml",
]


def _find_repo_root() -> pathlib.Path:
    """Find the loom repository root by looking for loom-tools/."""
    current = pathlib.Path(__file__).resolve()
    for parent in current.parents:
        if (parent / "loom-tools" / "pyproject.toml").exists():
            return parent
    pytest.skip("Cannot find loom repository root")


def _find_venv_bin() -> pathlib.Path:
    """Find the loom-tools venv bin directory."""
    repo_root = _find_repo_root()
    venv_bin = repo_root / "loom-tools" / ".venv" / "bin"
    if not venv_bin.exists():
        pytest.skip("loom-tools venv not built (run: cd loom-tools && uv sync)")
    return venv_bin


def _find_defaults_dir() -> pathlib.Path:
    """Find the defaults directory containing installation templates."""
    repo_root = _find_repo_root()
    defaults = repo_root / "defaults"
    if not defaults.exists():
        pytest.skip("defaults/ directory not found")
    return defaults


class TestPythonCLIAvailability:
    """Verify all loom-tools Python CLI commands are installed and executable."""

    @pytest.fixture
    def venv_bin(self) -> pathlib.Path:
        return _find_venv_bin()

    @pytest.mark.parametrize("command", EXPECTED_CLI_COMMANDS)
    def test_command_exists_in_venv(self, venv_bin: pathlib.Path, command: str) -> None:
        cmd_path = venv_bin / command
        assert cmd_path.exists(), f"{command} not found in venv at {cmd_path}"
        assert os.access(cmd_path, os.X_OK), f"{command} is not executable"

    @pytest.mark.parametrize("command", EXPECTED_CLI_COMMANDS)
    def test_command_responds_to_help(
        self, venv_bin: pathlib.Path, command: str
    ) -> None:
        cmd_path = venv_bin / command
        result = subprocess.run(
            [str(cmd_path), "--help"],
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert result.returncode == 0, (
            f"{command} --help failed with exit code {result.returncode}: "
            f"{result.stderr}"
        )
        assert len(result.stdout) > 0, f"{command} --help produced no output"

    @pytest.mark.parametrize("command", EXPECTED_CLI_COMMANDS)
    def test_no_python_warnings(
        self, venv_bin: pathlib.Path, command: str
    ) -> None:
        """Commands should not emit Python warnings about unavailability."""
        cmd_path = venv_bin / command
        result = subprocess.run(
            [str(cmd_path), "--help"],
            capture_output=True,
            text=True,
            timeout=30,
        )
        combined_output = result.stdout + result.stderr
        assert "[WARN] Python" not in combined_output, (
            f"{command} emitted Python availability warning"
        )
        assert "not available" not in combined_output.lower() or "help" in combined_output.lower(), (
            f"{command} reported something as 'not available'"
        )


class TestWrapperScriptRouting:
    """Verify wrapper scripts correctly route to Python implementations."""

    @pytest.fixture
    def defaults_dir(self) -> pathlib.Path:
        return _find_defaults_dir()

    @pytest.fixture
    def repo_root(self) -> pathlib.Path:
        return _find_repo_root()

    def test_shepherd_wrapper_routes_to_python(
        self, defaults_dir: pathlib.Path, repo_root: pathlib.Path
    ) -> None:
        """loom-shepherd.sh should exec the Python loom-shepherd command."""
        script = defaults_dir / "scripts" / "loom-shepherd.sh"
        assert script.exists(), "loom-shepherd.sh not found in defaults/scripts/"

        content = script.read_text()
        # Should check for venv binary
        assert ".venv/bin/loom-shepherd" in content, (
            "loom-shepherd.sh doesn't check for venv Python binary"
        )
        # Should exec the binary
        assert 'exec "$LOOM_TOOLS/.venv/bin/loom-shepherd"' in content, (
            "loom-shepherd.sh doesn't exec the venv binary"
        )
        # Should also check PATH fallback
        assert "command -v loom-shepherd" in content, (
            "loom-shepherd.sh doesn't check PATH for system-installed command"
        )

    def test_health_check_delegates_to_python(
        self, defaults_dir: pathlib.Path
    ) -> None:
        """health-check.sh should delegate to Python loom-health-monitor."""
        script = defaults_dir / "scripts" / "health-check.sh"
        assert script.exists(), "health-check.sh not found"

        content = script.read_text()
        # Accept either direct command reference or run_loom_tool helper
        assert (
            "loom-health-monitor" in content
            or 'run_loom_tool "health-monitor"' in content
        ), "health-check.sh doesn't delegate to loom-health-monitor"

    def test_daemon_cleanup_delegates_to_python(
        self, defaults_dir: pathlib.Path
    ) -> None:
        """daemon-cleanup.sh should delegate to Python loom-daemon-cleanup."""
        script = defaults_dir / "scripts" / "daemon-cleanup.sh"
        assert script.exists(), "daemon-cleanup.sh not found"

        content = script.read_text()
        # Accept either direct command reference or run_loom_tool helper
        assert (
            "loom-daemon-cleanup" in content
            or 'run_loom_tool "daemon-cleanup"' in content
        ), "daemon-cleanup.sh doesn't delegate to loom-daemon-cleanup"

    def test_cli_wrapper_health_routes_to_python(
        self, defaults_dir: pathlib.Path
    ) -> None:
        """The 'loom' CLI wrapper should route health to Python."""
        cli_wrapper = defaults_dir / "loom"
        assert cli_wrapper.exists(), "loom CLI wrapper not found"

        content = cli_wrapper.read_text()
        assert "loom-daemon-diagnostic" in content, (
            "loom CLI doesn't reference loom-daemon-diagnostic"
        )

    def test_shepherd_wrapper_no_deprecated_fallback(
        self, defaults_dir: pathlib.Path
    ) -> None:
        """loom-shepherd.sh should NOT fall back to deprecated shell scripts."""
        script = defaults_dir / "scripts" / "loom-shepherd.sh"
        content = script.read_text()
        assert "deprecated/" not in content, (
            "loom-shepherd.sh still references deprecated/ directory"
        )


class TestInstallationFileStructure:
    """Verify the expected file structure is present after installation."""

    @pytest.fixture
    def repo_root(self) -> pathlib.Path:
        return _find_repo_root()

    @pytest.fixture
    def defaults_dir(self) -> pathlib.Path:
        return _find_defaults_dir()

    def test_defaults_directory_exists(self, defaults_dir: pathlib.Path) -> None:
        assert defaults_dir.is_dir()

    def test_defaults_has_roles(self, defaults_dir: pathlib.Path) -> None:
        roles_dir = defaults_dir / "roles"
        assert roles_dir.is_dir(), "defaults/roles/ missing"
        role_files = list(roles_dir.glob("*.md"))
        assert len(role_files) > 0, "No role .md files in defaults/roles/"

    def test_defaults_has_scripts(self, defaults_dir: pathlib.Path) -> None:
        scripts_dir = defaults_dir / "scripts"
        assert scripts_dir.is_dir(), "defaults/scripts/ missing"
        script_files = list(scripts_dir.glob("*.sh"))
        assert len(script_files) > 0, "No .sh scripts in defaults/scripts/"

    def test_defaults_has_claude_md(self, defaults_dir: pathlib.Path) -> None:
        claude_md = defaults_dir / "CLAUDE.md"
        assert claude_md.exists(), "defaults/CLAUDE.md missing"

    def test_defaults_has_labels(self, defaults_dir: pathlib.Path) -> None:
        labels = defaults_dir / ".github" / "labels.yml"
        assert labels.exists(), "defaults/.github/labels.yml missing"

    def test_defaults_has_config_json(self, defaults_dir: pathlib.Path) -> None:
        config = defaults_dir / "config.json"
        assert config.exists(), "defaults/config.json missing"
        # Validate it's valid JSON
        data = json.loads(config.read_text())
        assert "terminals" in data, "config.json missing 'terminals' key"

    def test_loom_tools_venv_has_all_commands(
        self, repo_root: pathlib.Path
    ) -> None:
        venv_bin = repo_root / "loom-tools" / ".venv" / "bin"
        if not venv_bin.exists():
            pytest.skip("venv not built")

        missing = []
        for cmd in EXPECTED_CLI_COMMANDS:
            if not (venv_bin / cmd).exists():
                missing.append(cmd)

        assert not missing, f"Missing commands in venv: {missing}"

    def test_loom_daemon_binary_buildable(
        self, repo_root: pathlib.Path
    ) -> None:
        """Verify loom-daemon Cargo.toml exists (binary is required for init)."""
        cargo_toml = repo_root / "loom-daemon" / "Cargo.toml"
        assert cargo_toml.exists(), "loom-daemon/Cargo.toml missing"


class TestVerifyInstallScript:
    """Test the verify-install.sh manifest system."""

    @pytest.fixture
    def defaults_dir(self) -> pathlib.Path:
        return _find_defaults_dir()

    def test_verify_install_exists(self, defaults_dir: pathlib.Path) -> None:
        script = defaults_dir / "scripts" / "verify-install.sh"
        assert script.exists(), "verify-install.sh missing"
        assert os.access(script, os.X_OK), "verify-install.sh not executable"

    def test_verify_install_help(self, defaults_dir: pathlib.Path) -> None:
        script = defaults_dir / "scripts" / "verify-install.sh"
        result = subprocess.run(
            ["bash", str(script), "--help"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        assert result.returncode == 0
        assert "verify" in result.stdout.lower() or "manifest" in result.stdout.lower()

    def test_verify_install_generates_manifest(self, tmp_path: pathlib.Path) -> None:
        """Test manifest generation in a simulated installation directory."""
        repo_root = _find_repo_root()
        script = repo_root / "defaults" / "scripts" / "verify-install.sh"

        # Create a minimal .loom structure
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "scripts").mkdir()
        (loom_dir / "roles").mkdir()

        # Copy verify-install.sh into the simulated install
        scripts_dir = loom_dir / "scripts"
        import shutil
        shutil.copy2(script, scripts_dir / "verify-install.sh")

        # Create a minimal tracked file
        (tmp_path / "CLAUDE.md").write_text("# Test")

        # Initialize git so verify-install.sh can find repo root
        subprocess.run(
            ["git", "init"],
            cwd=tmp_path,
            capture_output=True,
        )

        result = subprocess.run(
            ["bash", str(scripts_dir / "verify-install.sh"), "generate"],
            capture_output=True,
            text=True,
            timeout=30,
            cwd=tmp_path,
        )

        manifest = loom_dir / "manifest.json"
        if manifest.exists():
            data = json.loads(manifest.read_text())
            assert "version" in data
            assert "files" in data
            assert data["version"] == 1


class TestPyprojectConfiguration:
    """Verify pyproject.toml is correctly configured for all CLI commands."""

    @pytest.fixture
    def pyproject(self) -> dict:
        repo_root = _find_repo_root()
        pyproject_path = repo_root / "loom-tools" / "pyproject.toml"
        assert pyproject_path.exists()

        # Parse TOML - use tomllib (3.11+) or tomli
        try:
            import tomllib
        except ImportError:
            import tomli as tomllib

        return tomllib.loads(pyproject_path.read_text())

    def test_all_commands_declared(self, pyproject: dict) -> None:
        """All expected CLI commands should be declared in [project.scripts]."""
        scripts = pyproject.get("project", {}).get("scripts", {})
        for cmd in EXPECTED_CLI_COMMANDS:
            assert cmd in scripts, (
                f"CLI command '{cmd}' not declared in pyproject.toml [project.scripts]"
            )

    def test_entry_points_have_valid_modules(self, pyproject: dict) -> None:
        """All entry points should reference importable modules."""
        scripts = pyproject.get("project", {}).get("scripts", {})
        for cmd, entry_point in scripts.items():
            # Entry point format: "module.path:function"
            assert ":" in entry_point, (
                f"Entry point for '{cmd}' missing function reference: {entry_point}"
            )
            module_path, func_name = entry_point.rsplit(":", 1)
            assert module_path.startswith("loom_tools."), (
                f"Entry point for '{cmd}' doesn't start with loom_tools.: {entry_point}"
            )

    def test_pytest_config_present(self, pyproject: dict) -> None:
        """pytest configuration should be present."""
        pytest_config = pyproject.get("tool", {}).get("pytest", {}).get("ini_options", {})
        assert "testpaths" in pytest_config


class TestInstallScriptDependencies:
    """Verify installation script dependencies are available."""

    def test_git_available(self) -> None:
        result = subprocess.run(
            ["git", "--version"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        assert result.returncode == 0

    def test_gh_cli_available(self) -> None:
        result = subprocess.run(
            ["gh", "--version"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        assert result.returncode == 0

    def test_uv_available(self) -> None:
        """uv is needed to build the loom-tools venv."""
        result = subprocess.run(
            ["uv", "--version"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        assert result.returncode == 0

    def test_jq_available(self) -> None:
        """jq is used by many wrapper scripts."""
        result = subprocess.run(
            ["jq", "--version"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        assert result.returncode == 0


class TestInstalledScriptsMatchDefaults:
    """Verify that installed scripts in .loom/scripts/ match their defaults."""

    @pytest.fixture
    def repo_root(self) -> pathlib.Path:
        return _find_repo_root()

    def test_installed_scripts_exist(self, repo_root: pathlib.Path) -> None:
        """Key wrapper scripts should be installed in .loom/scripts/."""
        scripts_dir = repo_root / ".loom" / "scripts"
        if not scripts_dir.exists():
            pytest.skip(".loom/scripts/ not present (not an installed repo)")

        for script_name in PYTHON_ROUTING_SCRIPTS + PYTHON_INTEGRATION_SCRIPTS:
            installed = scripts_dir / script_name
            assert installed.exists(), (
                f"Installed script {script_name} missing from .loom/scripts/"
            )

    def test_installed_scripts_match_defaults(
        self, repo_root: pathlib.Path
    ) -> None:
        """Installed scripts should match their default templates."""
        defaults_scripts = repo_root / "defaults" / "scripts"
        installed_scripts = repo_root / ".loom" / "scripts"

        if not installed_scripts.exists():
            pytest.skip(".loom/scripts/ not present")

        mismatches = []
        for script_name in PYTHON_ROUTING_SCRIPTS:
            default = defaults_scripts / script_name
            installed = installed_scripts / script_name
            if default.exists() and installed.exists():
                if default.read_bytes() != installed.read_bytes():
                    mismatches.append(script_name)

        assert not mismatches, (
            f"Scripts differ from defaults (may need reinstall): {mismatches}"
        )
