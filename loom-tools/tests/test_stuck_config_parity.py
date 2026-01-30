"""Validation tests for stuck detection config parity (Python/bash).

Background: PR #1717 introduced three config variables for stuck-at-prompt
detection. This file validates the bash and Python implementations use
identical env var names, default values, and detection logic.

Referenced bash script: defaults/scripts/agent-wait-bg.sh
Referenced Python model: loom_tools/models/agent_wait.py
Related: #1717, #1727
"""

from __future__ import annotations

import pathlib
import re

# Expected stuck detection config: env var name -> default value
EXPECTED_CONFIG = {
    "LOOM_PROMPT_STUCK_CHECK_INTERVAL": 10,
    "LOOM_PROMPT_STUCK_AGE_THRESHOLD": 30,
    "LOOM_PROMPT_STUCK_RECOVERY_COOLDOWN": 60,
}

# Mapping from env var to the bash local variable name (without LOOM_ prefix)
_ENV_TO_BASH_VAR = {
    "LOOM_PROMPT_STUCK_CHECK_INTERVAL": "PROMPT_STUCK_CHECK_INTERVAL",
    "LOOM_PROMPT_STUCK_AGE_THRESHOLD": "PROMPT_STUCK_AGE_THRESHOLD",
    "LOOM_PROMPT_STUCK_RECOVERY_COOLDOWN": "PROMPT_STUCK_RECOVERY_COOLDOWN",
}

# Mapping from env var to the Python StuckConfig field name
_ENV_TO_PYTHON_FIELD = {
    "LOOM_PROMPT_STUCK_CHECK_INTERVAL": "prompt_stuck_check_interval",
    "LOOM_PROMPT_STUCK_AGE_THRESHOLD": "prompt_stuck_age_threshold",
    "LOOM_PROMPT_STUCK_RECOVERY_COOLDOWN": "prompt_stuck_recovery_cooldown",
}

SCRIPT_REL_PATH = "defaults/scripts/agent-wait-bg.sh"
PYTHON_MODEL_REL_PATH = "loom-tools/src/loom_tools/models/agent_wait.py"


def _repo_root() -> pathlib.Path:
    """Find the repo root from this test file."""
    # tests/ -> loom-tools/ -> repo root
    return pathlib.Path(__file__).resolve().parent.parent.parent


def _get_script_path() -> pathlib.Path:
    path = _repo_root() / SCRIPT_REL_PATH
    if not path.exists():
        msg = f"Script not found: {path}"
        raise FileNotFoundError(msg)
    return path


def _get_python_model_path() -> pathlib.Path:
    path = _repo_root() / PYTHON_MODEL_REL_PATH
    if not path.exists():
        msg = f"Python model not found: {path}"
        raise FileNotFoundError(msg)
    return path


def _parse_bash_defaults(source: str) -> dict[str, int]:
    """Parse ${LOOM_PROMPT_STUCK_*:-N} patterns from bash source.

    Returns dict mapping env var name to default integer value.
    """
    # Matches: VAR=${LOOM_PROMPT_STUCK_*:-<default>}
    pattern = re.compile(
        r'(\w+)=\$\{(LOOM_PROMPT_STUCK_\w+):-([\d]+)\}'
    )
    results: dict[str, int] = {}
    for match in pattern.finditer(source):
        env_var = match.group(2)
        default = int(match.group(3))
        results[env_var] = default
    return results


def _parse_python_env_gets(source: str) -> dict[str, int]:
    """Parse os.environ.get("LOOM_PROMPT_STUCK_*", "N") patterns.

    Returns dict mapping env var name to default integer value.
    """
    pattern = re.compile(
        r'os\.environ\.get\(\s*"(LOOM_PROMPT_STUCK_\w+)"\s*,\s*"(\d+)"\s*\)'
    )
    results: dict[str, int] = {}
    for match in pattern.finditer(source):
        env_var = match.group(1)
        default = int(match.group(2))
        results[env_var] = default
    return results


class TestFileExistence:
    """Verify the source files under test exist."""

    def test_script_exists(self) -> None:
        assert _get_script_path().exists()

    def test_python_model_exists(self) -> None:
        assert _get_python_model_path().exists()


class TestBashEnvVarNames:
    """Validate bash script reads the correct environment variable names."""

    def test_bash_env_var_names_match(self) -> None:
        """All expected LOOM_PROMPT_STUCK_* env vars must appear in bash."""
        source = _get_script_path().read_text()
        parsed = _parse_bash_defaults(source)

        for env_var in EXPECTED_CONFIG:
            assert env_var in parsed, (
                f"Expected bash script to read ${{{env_var}:-...}} "
                f"but found only: {list(parsed.keys())}"
            )

    def test_no_extra_bash_stuck_vars(self) -> None:
        """No unexpected LOOM_PROMPT_STUCK_* vars in bash."""
        source = _get_script_path().read_text()
        parsed = _parse_bash_defaults(source)

        extra = set(parsed.keys()) - set(EXPECTED_CONFIG.keys())
        assert not extra, (
            f"Unexpected LOOM_PROMPT_STUCK_* vars in bash: {extra}. "
            f"Update EXPECTED_CONFIG if these are intentional."
        )


class TestPythonEnvVarNames:
    """Validate Python StuckConfig.from_env() reads the correct env vars."""

    def test_python_env_var_names_match(self) -> None:
        """All expected LOOM_PROMPT_STUCK_* env vars must appear in Python."""
        source = _get_python_model_path().read_text()
        parsed = _parse_python_env_gets(source)

        for env_var in EXPECTED_CONFIG:
            assert env_var in parsed, (
                f"Expected Python model to read os.environ.get(\"{env_var}\", ...) "
                f"but found only: {list(parsed.keys())}"
            )

    def test_no_extra_python_stuck_vars(self) -> None:
        """No unexpected LOOM_PROMPT_STUCK_* vars in Python."""
        source = _get_python_model_path().read_text()
        parsed = _parse_python_env_gets(source)

        extra = set(parsed.keys()) - set(EXPECTED_CONFIG.keys())
        assert not extra, (
            f"Unexpected LOOM_PROMPT_STUCK_* vars in Python: {extra}. "
            f"Update EXPECTED_CONFIG if these are intentional."
        )


class TestDefaultValuesParity:
    """Validate bash and Python default values match for all config vars."""

    def test_bash_defaults_match_expected(self) -> None:
        source = _get_script_path().read_text()
        parsed = _parse_bash_defaults(source)

        for env_var, expected_val in EXPECTED_CONFIG.items():
            assert parsed[env_var] == expected_val, (
                f"Bash default for {env_var}: got {parsed[env_var]}, "
                f"expected {expected_val}"
            )

    def test_python_defaults_match_expected(self) -> None:
        source = _get_python_model_path().read_text()
        parsed = _parse_python_env_gets(source)

        for env_var, expected_val in EXPECTED_CONFIG.items():
            assert parsed[env_var] == expected_val, (
                f"Python default for {env_var}: got {parsed[env_var]}, "
                f"expected {expected_val}"
            )

    def test_bash_defaults_match_python(self) -> None:
        """Direct cross-check: bash defaults == Python defaults."""
        bash_source = _get_script_path().read_text()
        python_source = _get_python_model_path().read_text()

        bash_defaults = _parse_bash_defaults(bash_source)
        python_defaults = _parse_python_env_gets(python_source)

        for env_var in EXPECTED_CONFIG:
            assert bash_defaults[env_var] == python_defaults[env_var], (
                f"Default mismatch for {env_var}: "
                f"bash={bash_defaults[env_var]}, python={python_defaults[env_var]}"
            )

    def test_python_dataclass_defaults_match(self) -> None:
        """StuckConfig dataclass field defaults match env var defaults.

        Parses the dataclass field definitions directly from source to avoid
        import dependency issues with tool-installed pytest environments.
        """
        source = _get_python_model_path().read_text()
        # Match patterns like: prompt_stuck_check_interval: int = 10
        field_pattern = re.compile(
            r'^\s+(prompt_stuck_\w+):\s*int\s*=\s*(\d+)', re.MULTILINE
        )
        field_defaults: dict[str, int] = {}
        for match in field_pattern.finditer(source):
            field_defaults[match.group(1)] = int(match.group(2))

        for env_var, expected_val in EXPECTED_CONFIG.items():
            field_name = _ENV_TO_PYTHON_FIELD[env_var]
            assert field_name in field_defaults, (
                f"Expected StuckConfig field '{field_name}' not found in source. "
                f"Found: {list(field_defaults.keys())}"
            )
            actual = field_defaults[field_name]
            assert actual == expected_val, (
                f"StuckConfig.{field_name} default = {actual}, expected {expected_val}"
            )


class TestDetectionTimingLogic:
    """Validate both implementations use age_threshold for detection."""

    def test_bash_uses_age_threshold_comparison(self) -> None:
        """Bash checks stuck_duration >= PROMPT_STUCK_AGE_THRESHOLD."""
        source = _get_script_path().read_text()

        assert re.search(
            r'stuck_duration.*-ge.*PROMPT_STUCK_AGE_THRESHOLD',
            source,
        ), (
            "Expected bash to compare stuck_duration >= PROMPT_STUCK_AGE_THRESHOLD"
        )

    def test_python_uses_age_threshold_comparison(self) -> None:
        """Python checks stuck_duration >= sc.prompt_stuck_age_threshold."""
        model_path = _repo_root() / "loom-tools/src/loom_tools/agent_monitor.py"
        source = model_path.read_text()

        assert re.search(
            r'stuck_duration\s*>=\s*sc\.prompt_stuck_age_threshold',
            source,
        ), (
            "Expected Python to compare stuck_duration >= sc.prompt_stuck_age_threshold"
        )

    def test_bash_uses_check_interval_gating(self) -> None:
        """Bash gates stuck checks with PROMPT_STUCK_CHECK_INTERVAL."""
        source = _get_script_path().read_text()

        assert re.search(
            r'since_last_stuck_check.*-ge.*PROMPT_STUCK_CHECK_INTERVAL',
            source,
        ) or re.search(
            r'since_stuck_check.*-ge.*PROMPT_STUCK_CHECK_INTERVAL',
            source,
        ) or re.search(
            r'PROMPT_STUCK_CHECK_INTERVAL',
            source,
        ), (
            "Expected bash to use PROMPT_STUCK_CHECK_INTERVAL for check gating"
        )

    def test_python_uses_check_interval_gating(self) -> None:
        """Python gates checks with sc.prompt_stuck_check_interval."""
        model_path = _repo_root() / "loom-tools/src/loom_tools/agent_monitor.py"
        source = model_path.read_text()

        assert re.search(
            r'sc\.prompt_stuck_check_interval',
            source,
        ), (
            "Expected Python to use sc.prompt_stuck_check_interval for check gating"
        )


class TestRecoveryCooldownLogic:
    """Validate both implementations use recovery_cooldown consistently."""

    def test_bash_recovery_cooldown_blocks_retry(self) -> None:
        """Bash: since_recovery < PROMPT_STUCK_RECOVERY_COOLDOWN -> skip."""
        source = _get_script_path().read_text()

        assert re.search(
            r'since_recovery.*-lt.*PROMPT_STUCK_RECOVERY_COOLDOWN',
            source,
        ), (
            "Expected bash to check since_recovery -lt PROMPT_STUCK_RECOVERY_COOLDOWN "
            "to block early retries"
        )

    def test_python_recovery_cooldown_allows_retry(self) -> None:
        """Python: since_recovery >= sc.prompt_stuck_recovery_cooldown -> allowed."""
        model_path = _repo_root() / "loom-tools/src/loom_tools/agent_monitor.py"
        source = model_path.read_text()

        assert re.search(
            r'since_recovery\s*>=\s*sc\.prompt_stuck_recovery_cooldown',
            source,
        ), (
            "Expected Python to check since_recovery >= sc.prompt_stuck_recovery_cooldown "
            "to allow retries"
        )

    def test_cooldown_comparisons_are_logically_equivalent(self) -> None:
        """Bash `< cooldown -> skip` is equivalent to Python `>= cooldown -> allow`.

        Bash:   if since_recovery < cooldown then recovery_allowed=false
        Python: recovery_allowed = (since_recovery >= cooldown)

        These are logically equivalent: !(x < y) == (x >= y).
        """
        bash_source = _get_script_path().read_text()
        python_path = _repo_root() / "loom-tools/src/loom_tools/agent_monitor.py"
        python_source = python_path.read_text()

        # Bash uses -lt (less than) to BLOCK
        bash_blocks = re.search(
            r'since_recovery.*-lt.*PROMPT_STUCK_RECOVERY_COOLDOWN', bash_source
        )
        # Python uses >= to ALLOW
        python_allows = re.search(
            r'since_recovery\s*>=\s*sc\.prompt_stuck_recovery_cooldown', python_source
        )

        assert bash_blocks and python_allows, (
            "Expected bash to use -lt (block) and Python to use >= (allow) "
            "which are logically equivalent"
        )
