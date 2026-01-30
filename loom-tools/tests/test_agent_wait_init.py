"""Validation tests for agent-wait-bg.sh timing initialization patterns.

This module validates that timing-sensitive variables in agent-wait-bg.sh
are initialized correctly to prevent threshold detection bugs.

Background:
  - Issue #1670: Detection too slow (122s instead of 30s) - wrong initial value
  - Issue #1683: Detection too fast (immediate instead of 30s) - initialized to 0
  - PR #1716: Fixed by initializing to $start_time

The dangerous pattern: a `last_*_check` or `last_*_time` variable initialized
to 0 that is later compared with `$(date +%s) - last_*` to determine if a
threshold has been exceeded. With 0, the difference is ~epoch seconds,
triggering immediately.

Not all `=0` initializations are bugs:
  - Sentinel values (prompt_stuck_since=0, prompt_stuck_recovery_time=0):
    0 means "not yet occurred" and is explicitly checked with `== 0` or `> 0`
  - Counter/accumulator variables (last_contract_check=0):
    Uses adaptive interval gating that returns 0 during the initial delay,
    so the variable is never compared until it's been set to a real timestamp

Referenced bash script: defaults/scripts/agent-wait-bg.sh
Related issues: #1670, #1683, #1716, #1721
"""

from __future__ import annotations

import pathlib
import re


# Path to the bash script under test, relative to the repo root
SCRIPT_REL_PATH = "defaults/scripts/agent-wait-bg.sh"

# Variables that MUST be initialized to $start_time because they are used in
# `$(date +%s) - variable` comparisons to check if a time threshold has passed.
# Initializing these to 0 causes ~epoch-seconds difference, triggering immediately.
MUST_USE_START_TIME = {
    "last_prompt_stuck_check",
    "last_heartbeat_time",
}

# Variables that correctly use =0 as a sentinel value meaning "not yet occurred".
# These are explicitly checked with `== 0` or `> 0` guards before any arithmetic.
VALID_ZERO_SENTINELS = {
    "prompt_stuck_since",          # 0 = not stuck; checked with `-eq 0` / `-gt 0`
    "prompt_stuck_recovery_time",  # 0 = never attempted; checked with `-gt 0`
}

# Variables that correctly use =0 because they are gated by adaptive interval
# logic that returns 0 (skip) during the initial delay period.
VALID_ZERO_GATED = {
    "last_contract_check",  # Gated by get_adaptive_contract_interval() returning 0
}


def _get_script_path() -> pathlib.Path:
    """Find the agent-wait-bg.sh script from the repo root."""
    # Walk up from this test file to find the repo root
    test_dir = pathlib.Path(__file__).resolve().parent
    # tests/ -> loom-tools/ -> repo root
    repo_root = test_dir.parent.parent
    script = repo_root / SCRIPT_REL_PATH
    if not script.exists():
        msg = f"Script not found: {script}"
        raise FileNotFoundError(msg)
    return script


def _read_main_function(script_path: pathlib.Path) -> str:
    """Extract the main() function body from agent-wait-bg.sh."""
    content = script_path.read_text()
    # Find `main() {` and extract until the matching closing brace
    # For our purposes, we just need the variable declarations near the top
    # of main(), so reading the full file is fine.
    return content


def _find_local_last_declarations(source: str) -> dict[str, str]:
    """Find all `local last_*` variable declarations and their initializers.

    Returns a dict mapping variable name to its initialization expression.
    For example: {"last_heartbeat_time": "$start_time", "last_contract_check": "0"}
    """
    # Match patterns like:
    #   local last_foo=$start_time
    #   local last_foo=0
    #   local last_foo="$start_time"
    pattern = re.compile(r'^\s*local\s+(last_\w+)=(.+?)(?:\s*#.*)?$', re.MULTILINE)
    results = {}
    for match in pattern.finditer(source):
        var_name = match.group(1)
        init_value = match.group(2).strip().strip('"').strip("'")
        results[var_name] = init_value
    return results


class TestTimingInitializationPatterns:
    """Validate that timing variables in agent-wait-bg.sh are initialized safely."""

    def test_script_exists(self) -> None:
        """The bash script under test must exist."""
        script = _get_script_path()
        assert script.exists(), f"Expected script at {script}"

    def test_start_time_variables_use_start_time(self) -> None:
        """Variables in MUST_USE_START_TIME must be initialized to $start_time.

        These variables are used in `$(date +%s) - variable` comparisons.
        If initialized to 0, the difference would be ~epoch seconds (~1.7 billion),
        causing immediate threshold triggers.

        Bug history:
          - #1683: last_prompt_stuck_check=0 caused immediate stuck detection
          - #1670: Related timing initialization caused detection delays
        """
        source = _read_main_function(_get_script_path())
        declarations = _find_local_last_declarations(source)

        for var_name in MUST_USE_START_TIME:
            assert var_name in declarations, (
                f"Expected `local {var_name}=...` declaration in agent-wait-bg.sh. "
                f"Found declarations: {list(declarations.keys())}"
            )
            init_value = declarations[var_name]
            assert init_value == "$start_time", (
                f"`{var_name}` must be initialized to $start_time, "
                f"but found `{var_name}={init_value}`. "
                f"Initializing to 0 causes immediate threshold triggers "
                f"(see issues #1670, #1683)."
            )

    def test_sentinel_zero_values_are_documented(self) -> None:
        """Variables using =0 as sentinel must have inline comments explaining why.

        The =0 pattern is dangerous for timestamp variables but correct for
        sentinel values. Each sentinel must have a comment documenting its meaning.
        """
        script = _get_script_path()
        source = script.read_text()

        for var_name in VALID_ZERO_SENTINELS:
            # Find the line declaring this variable
            pattern = re.compile(rf'^\s*local\s+{re.escape(var_name)}=0\b.*$', re.MULTILINE)
            match = pattern.search(source)
            assert match is not None, (
                f"Expected `local {var_name}=0` declaration in agent-wait-bg.sh"
            )
            line = match.group(0)
            assert "#" in line, (
                f"`local {var_name}=0` should have an inline comment explaining "
                f"why =0 is correct (sentinel value meaning 'not yet occurred'). "
                f"Line: {line.strip()}"
            )

    def test_no_new_dangerous_zero_initializations(self) -> None:
        """Any new `local last_*` variable initialized to 0 must be explicitly allowed.

        This test catches regressions where a developer adds a new timing variable
        with =0 initialization. If a new `last_*=0` pattern appears, it must be
        added to either VALID_ZERO_SENTINELS or VALID_ZERO_GATED with documentation.
        """
        source = _read_main_function(_get_script_path())
        declarations = _find_local_last_declarations(source)

        allowed_zero = VALID_ZERO_SENTINELS | VALID_ZERO_GATED
        all_known = MUST_USE_START_TIME | allowed_zero

        for var_name, init_value in declarations.items():
            if init_value == "0" and var_name not in allowed_zero:
                msg = (
                    f"Found `local {var_name}=0` which is not in the allowed list. "
                    f"If this is a timestamp used in `$(date +%s) - {var_name}` comparisons, "
                    f"initialize it to $start_time and add it to MUST_USE_START_TIME. "
                    f"If 0 is a valid sentinel value, add it to VALID_ZERO_SENTINELS "
                    f"or VALID_ZERO_GATED with documentation."
                )
                raise AssertionError(msg)

            if var_name not in all_known:
                # New variable we haven't categorized - not necessarily wrong,
                # but should be reviewed and categorized
                assert init_value != "0", (
                    f"New variable `local {var_name}={init_value}` found. "
                    f"If this is a timing variable, add it to the appropriate "
                    f"category in test_agent_wait_init.py."
                )

    def test_start_time_is_initialized_before_use(self) -> None:
        """Verify start_time is set via `date +%s` before the timing variables."""
        source = _read_main_function(_get_script_path())

        # Find start_time initialization
        start_time_match = re.search(
            r'start_time=\$\(date \+%s\)', source
        )
        assert start_time_match is not None, (
            "Expected `start_time=$(date +%s)` in agent-wait-bg.sh"
        )

        # Find first use of $start_time in a local declaration
        first_use_match = re.search(
            r'local\s+last_\w+=\$start_time', source
        )
        assert first_use_match is not None, (
            "Expected at least one `local last_*=$start_time` declaration"
        )

        # Verify initialization comes before first use
        assert start_time_match.start() < first_use_match.start(), (
            "start_time must be initialized before it is used in variable declarations"
        )

    def test_contract_check_uses_adaptive_gating(self) -> None:
        """Verify last_contract_check=0 is safe because of adaptive interval gating.

        last_contract_check=0 is intentionally not $start_time because
        get_adaptive_contract_interval() returns 0 during the initial delay
        period (first 180s), which causes the contract check to be skipped entirely.
        The variable only participates in `now - last_contract_check` arithmetic
        after the adaptive interval returns a non-zero value, at which point
        the first check runs and sets last_contract_check=$now.
        """
        source = _read_main_function(_get_script_path())

        # Verify the adaptive interval gating pattern exists:
        # adaptive_interval of 0 means "skip this check"
        assert 'adaptive_interval" -gt 0' in source or "adaptive_interval -gt 0" in source, (
            "Expected adaptive interval gating (`adaptive_interval -gt 0`) "
            "that protects last_contract_check from immediate triggers"
        )

        # Verify get_adaptive_contract_interval returns 0 for early elapsed times
        assert re.search(
            r'get_adaptive_contract_interval.*\{', source
        ), "Expected get_adaptive_contract_interval function definition"

        # The function should return 0 for elapsed < CONTRACT_INITIAL_DELAY
        assert re.search(
            r'echo\s+0.*return', source,
            re.DOTALL
        ) or re.search(
            r'echo\s+"?0"?\s*$', source,
            re.MULTILINE
        ), "get_adaptive_contract_interval should return 0 during initial delay"
