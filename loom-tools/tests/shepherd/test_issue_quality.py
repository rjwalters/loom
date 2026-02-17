"""Tests for issue quality pre-flight validation."""

from __future__ import annotations

import os
from unittest import mock

from loom_tools.shepherd.config import QualityGateLevel, QualityGates
from loom_tools.shepherd.issue_quality import (
    QualityFinding,
    Severity,
    ValidationResult,
    validate_issue_quality,
    validate_issue_quality_with_gates,
)


class TestValidateIssueQuality:
    """Test validate_issue_quality function."""

    def test_good_issue_no_warnings(self) -> None:
        """Issue with acceptance criteria, test plan, and file refs produces no warnings."""
        body = """## Summary

Add pre-flight validation for issue quality.

## Acceptance Criteria

- [ ] Validation function checks issue body
- [ ] Warnings logged to shepherd output
- [ ] Does not block builder

## Test Plan

- [ ] Unit test with good issue
- [ ] Unit test with bad issue

## Implementation

Modify `builder.py` to call validation before spawning worker.
"""
        result = validate_issue_quality(body)
        assert len(result.warnings) == 0
        assert len(result.infos) == 0

    def test_empty_body_returns_warning(self) -> None:
        """Empty issue body should produce a warning."""
        result = validate_issue_quality("")
        assert len(result.warnings) == 1
        assert "empty" in result.warnings[0].message.lower()

    def test_none_body_returns_warning(self) -> None:
        """None-ish body should produce a warning."""
        result = validate_issue_quality("   ")
        assert len(result.warnings) == 1
        assert "empty" in result.warnings[0].message.lower()

    def test_missing_acceptance_criteria_warning(self) -> None:
        """Issue without acceptance criteria section or checkboxes should produce a warning."""
        body = """## Summary

Fix the login button.

## Test Plan

Run manual testing to verify button works.
"""
        result = validate_issue_quality(body)
        warnings = [f.message for f in result.warnings]
        assert any("acceptance criteria" in w.lower() for w in warnings)

    def test_checkboxes_satisfy_acceptance_criteria(self) -> None:
        """Checkboxes without heading should still count as acceptance criteria."""
        body = """## Summary

Fix the login button.

- [ ] Button renders correctly
- [ ] Click triggers login flow
- [x] Already has error handling
"""
        result = validate_issue_quality(body)
        warnings = [f.message for f in result.warnings]
        assert not any("acceptance criteria" in w.lower() for w in warnings)

    def test_requirements_heading_satisfies_ac_check(self) -> None:
        """'Requirements' heading should be recognized as acceptance criteria."""
        body = """## Summary

Add feature X.

## Requirements

- Must support Y
- Must handle Z
"""
        result = validate_issue_quality(body)
        warnings = [f.message for f in result.warnings]
        assert not any("acceptance criteria" in w.lower() for w in warnings)

    def test_expected_behavior_heading_satisfies_ac_check(self) -> None:
        """'Expected Behavior' heading should be recognized as acceptance criteria."""
        body = """## Summary

Fix bug.

## Expected Behavior

The button should render.
"""
        result = validate_issue_quality(body)
        warnings = [f.message for f in result.warnings]
        assert not any("acceptance criteria" in w.lower() for w in warnings)

    def test_vague_make_it_better(self) -> None:
        """Vague 'make it better' should produce a warning."""
        body = """## Acceptance Criteria

- [ ] Make it better
"""
        result = validate_issue_quality(body)
        vague_warnings = [f for f in result.warnings if "vague" in f.message.lower()]
        assert len(vague_warnings) >= 1
        assert any("make it better" in w.message for w in vague_warnings)

    def test_vague_improve_performance(self) -> None:
        """Vague 'improve performance' should produce a warning."""
        body = """## Acceptance Criteria

- [ ] Improve performance of the API
"""
        result = validate_issue_quality(body)
        vague_warnings = [f for f in result.warnings if "vague" in f.message.lower()]
        assert len(vague_warnings) >= 1

    def test_vague_fix_the_issues(self) -> None:
        """Vague 'fix the issues' should produce a warning."""
        body = """## Acceptance Criteria

- [ ] Fix the issues with deployment
"""
        result = validate_issue_quality(body)
        vague_warnings = [f for f in result.warnings if "vague" in f.message.lower()]
        assert len(vague_warnings) >= 1

    def test_vague_should_work_properly(self) -> None:
        """Vague 'should work properly' should produce a warning."""
        body = """## Acceptance Criteria

- [ ] The feature should work properly
"""
        result = validate_issue_quality(body)
        vague_warnings = [f for f in result.warnings if "vague" in f.message.lower()]
        assert len(vague_warnings) >= 1

    def test_vague_clean_up_the_code(self) -> None:
        """Vague 'clean up the code' should produce a warning."""
        body = """## Acceptance Criteria

- [ ] Clean up the code
"""
        result = validate_issue_quality(body)
        vague_warnings = [f for f in result.warnings if "vague" in f.message.lower()]
        assert len(vague_warnings) >= 1

    def test_missing_test_plan_info(self) -> None:
        """Missing test plan should produce an info finding."""
        body = """## Acceptance Criteria

- [ ] Add validation function

Modify `builder.py` to call it.
"""
        result = validate_issue_quality(body)
        info_messages = [f.message for f in result.infos]
        assert any("test plan" in m.lower() for m in info_messages)

    def test_has_test_plan_no_info(self) -> None:
        """Issue with test plan should not produce test plan info."""
        body = """## Acceptance Criteria

- [ ] Add validation function

## Test Plan

- [ ] Unit test
"""
        result = validate_issue_quality(body)
        info_messages = [f.message for f in result.infos]
        assert not any("test plan" in m.lower() for m in info_messages)

    def test_testing_plan_heading_recognized(self) -> None:
        """'Testing Plan' heading should be recognized."""
        body = """## Acceptance Criteria

- [ ] Add feature

## Testing Plan

- [ ] Integration test
"""
        result = validate_issue_quality(body)
        info_messages = [f.message for f in result.infos]
        assert not any("test plan" in m.lower() for m in info_messages)

    def test_no_file_references_info(self) -> None:
        """Missing file references should produce an info finding."""
        body = """## Acceptance Criteria

- [ ] Add validation

## Test Plan

- [ ] Test it
"""
        result = validate_issue_quality(body)
        info_messages = [f.message for f in result.infos]
        assert any("file" in m.lower() for m in info_messages)

    def test_file_reference_recognized(self) -> None:
        """File references should be detected."""
        body = """## Acceptance Criteria

- [ ] Update builder.py

## Test Plan

- [ ] Test it
"""
        result = validate_issue_quality(body)
        info_messages = [f.message for f in result.infos]
        assert not any("file" in m.lower() for m in info_messages)

    def test_backtick_file_reference_recognized(self) -> None:
        """Backtick-quoted file references should be detected."""
        body = """## Acceptance Criteria

- [ ] Modify `src/shepherd/phases/builder.py`

## Test Plan

- [ ] Test it
"""
        result = validate_issue_quality(body)
        info_messages = [f.message for f in result.infos]
        assert not any("file" in m.lower() for m in info_messages)

    def test_multiple_vague_patterns_detected(self) -> None:
        """Multiple vague patterns should each produce their own warning."""
        body = """## Acceptance Criteria

- [ ] Make it better
- [ ] Fix the issues
- [ ] Should work properly
"""
        result = validate_issue_quality(body)
        vague_warnings = [f for f in result.warnings if "vague" in f.message.lower()]
        assert len(vague_warnings) >= 3

    def test_case_insensitive_vague_patterns(self) -> None:
        """Vague pattern matching should be case-insensitive."""
        body = """## Acceptance Criteria

- [ ] MAKE IT BETTER
"""
        result = validate_issue_quality(body)
        vague_warnings = [f for f in result.warnings if "vague" in f.message.lower()]
        assert len(vague_warnings) >= 1

    def test_numbered_action_items_satisfy_ac_check(self) -> None:
        """Numbered action items with verbs should count as acceptance criteria."""
        body = """## Summary

Refactor the authentication module.

## Implementation

1. Ensure the login endpoint validates tokens correctly
2. Verify that expired tokens return 401
3. Update the middleware to check roles

Modify `auth/middleware.py` to implement.
"""
        result = validate_issue_quality(body)
        warnings = [f.message for f in result.warnings]
        assert not any("acceptance criteria" in w.lower() for w in warnings)

    def test_requirement_statements_satisfy_ac_check(self) -> None:
        """Should/must/shall statements should count as acceptance criteria."""
        body = """## Summary

Fix the dashboard widget rendering.

The widget should display the correct count when data is loaded.
The API response must include pagination metadata.
The cache should be invalidated after updates.

See `dashboard/widgets.py` for the rendering logic.
"""
        result = validate_issue_quality(body)
        warnings = [f.message for f in result.warnings]
        assert not any("acceptance criteria" in w.lower() for w in warnings)

    def test_bug_report_pattern_satisfies_ac_check(self) -> None:
        """Observed + Expected Behavior headings should count as acceptance criteria."""
        body = """## Summary

Login button does not respond on mobile.

## Observed Behavior

Clicking the login button on mobile does nothing. No network request is made.

## Expected Behavior

Clicking the login button should trigger the OAuth flow and redirect to the provider.

## Steps to Reproduce

1. Open the app on a mobile device
2. Click "Login"
"""
        result = validate_issue_quality(body)
        warnings = [f.message for f in result.warnings]
        assert not any("acceptance criteria" in w.lower() for w in warnings)

    def test_multiple_file_refs_satisfy_ac_check(self) -> None:
        """3+ file references should indicate a well-researched issue."""
        body = """## Summary

Update the shepherd configuration to support new timeout settings.

The changes affect `config.py`, `builder.py`, and `issue_quality.py`.
Also update the tests in `test_config.py`.
"""
        result = validate_issue_quality(body)
        warnings = [f.message for f in result.warnings]
        assert not any("acceptance criteria" in w.lower() for w in warnings)

    def test_single_numbered_item_not_enough(self) -> None:
        """A single numbered action item should not satisfy the check."""
        body = """## Summary

Fix the crash bug.

1. Ensure it doesn't crash anymore.
"""
        result = validate_issue_quality(body)
        warnings = [f.message for f in result.warnings]
        assert any("acceptance criteria" in w.lower() for w in warnings)

    def test_single_requirement_not_enough(self) -> None:
        """A single should/must statement should not satisfy the check."""
        body = """## Summary

The system should not crash.
"""
        result = validate_issue_quality(body)
        warnings = [f.message for f in result.warnings]
        assert any("acceptance criteria" in w.lower() for w in warnings)

    def test_vague_issue_still_warns(self) -> None:
        """Genuinely vague issues without quality signals should still warn."""
        body = "Fix the crash bug."
        result = validate_issue_quality(body)
        warnings = [f.message for f in result.warnings]
        assert any("acceptance criteria" in w.lower() for w in warnings)

    def test_observed_without_expected_not_enough(self) -> None:
        """Only Observed Behavior without Expected Behavior should not satisfy the check."""
        body = """## Summary

Bug report.

## Observed Behavior

The app crashes on startup.
"""
        result = validate_issue_quality(body)
        warnings = [f.message for f in result.warnings]
        assert any("acceptance criteria" in w.lower() for w in warnings)

    def test_severity_classification(self) -> None:
        """Warnings and infos should have correct severity levels."""
        # Issue with no AC, no test plan, no file refs
        body = "Just a vague description."
        result = validate_issue_quality(body)

        for finding in result.warnings:
            assert finding.severity == Severity.WARNING
        for finding in result.infos:
            assert finding.severity == Severity.INFO


class TestValidationResult:
    """Test ValidationResult dataclass."""

    def test_empty_result(self) -> None:
        """Empty result should have no warnings or infos."""
        result = ValidationResult()
        assert len(result.warnings) == 0
        assert len(result.infos) == 0
        assert len(result.findings) == 0

    def test_filters_warnings(self) -> None:
        """warnings property should only return WARNING severity."""
        from loom_tools.shepherd.issue_quality import QualityFinding

        result = ValidationResult(
            findings=[
                QualityFinding(severity=Severity.WARNING, message="w1"),
                QualityFinding(severity=Severity.INFO, message="i1"),
                QualityFinding(severity=Severity.WARNING, message="w2"),
            ]
        )
        assert len(result.warnings) == 2
        assert len(result.infos) == 1

    def test_filters_infos(self) -> None:
        """infos property should only return INFO severity."""
        result = ValidationResult(
            findings=[
                QualityFinding(severity=Severity.INFO, message="i1"),
                QualityFinding(severity=Severity.INFO, message="i2"),
            ]
        )
        assert len(result.infos) == 2
        assert len(result.warnings) == 0

    def test_filters_blocks(self) -> None:
        """blocks property should only return BLOCK severity."""
        result = ValidationResult(
            findings=[
                QualityFinding(severity=Severity.BLOCK, message="b1"),
                QualityFinding(severity=Severity.WARNING, message="w1"),
                QualityFinding(severity=Severity.BLOCK, message="b2"),
            ]
        )
        assert len(result.blocks) == 2
        assert len(result.warnings) == 1

    def test_has_blocking_findings_true(self) -> None:
        """has_blocking_findings returns True when BLOCK findings exist."""
        result = ValidationResult(
            findings=[
                QualityFinding(severity=Severity.BLOCK, message="b1"),
                QualityFinding(severity=Severity.WARNING, message="w1"),
            ]
        )
        assert result.has_blocking_findings is True

    def test_has_blocking_findings_false(self) -> None:
        """has_blocking_findings returns False when no BLOCK findings."""
        result = ValidationResult(
            findings=[
                QualityFinding(severity=Severity.WARNING, message="w1"),
                QualityFinding(severity=Severity.INFO, message="i1"),
            ]
        )
        assert result.has_blocking_findings is False


class TestValidateIssueQualityWithGates:
    """Test validate_issue_quality_with_gates function with configurable gates."""

    def test_default_gates_no_blocks(self) -> None:
        """Default quality gates should not produce BLOCK findings."""
        body = "Just a vague description without any sections."
        gates = QualityGates()  # Default: info/warn levels only
        result = validate_issue_quality_with_gates(body, gates)
        assert len(result.blocks) == 0

    def test_strict_gates_missing_ac_blocks(self) -> None:
        """Strict gates should produce BLOCK finding for missing acceptance criteria."""
        body = """## Summary

Some description without acceptance criteria.

## Test Plan

Test steps:
1. Run the command
2. Verify output
"""
        gates = QualityGates.strict()
        result = validate_issue_quality_with_gates(body, gates)
        assert len(result.blocks) == 1
        assert "acceptance criteria" in result.blocks[0].message.lower()

    def test_strict_gates_with_ac_no_block(self) -> None:
        """Strict gates should not block when acceptance criteria exist."""
        body = """## Summary

Some description.

## Acceptance Criteria

- [ ] Feature works correctly

## Test Plan

- [ ] Unit test
"""
        gates = QualityGates.strict()
        result = validate_issue_quality_with_gates(body, gates)
        assert len(result.blocks) == 0

    def test_custom_gates_test_plan_blocks(self) -> None:
        """Custom gates can make test plan check block."""
        body = """## Acceptance Criteria

- [ ] Feature works

Modify `builder.py` to implement.
"""
        gates = QualityGates(test_plan=QualityGateLevel.BLOCK)
        result = validate_issue_quality_with_gates(body, gates)
        assert result.has_blocking_findings is True
        assert any("test plan" in f.message.lower() for f in result.blocks)

    def test_custom_gates_file_refs_blocks(self) -> None:
        """Custom gates can make file references check block."""
        body = """## Acceptance Criteria

- [ ] Feature works

## Test Plan

- [ ] Test it
"""
        gates = QualityGates(file_refs=QualityGateLevel.BLOCK)
        result = validate_issue_quality_with_gates(body, gates)
        assert result.has_blocking_findings is True
        assert any("file" in f.message.lower() for f in result.blocks)

    def test_custom_gates_vague_criteria_blocks(self) -> None:
        """Custom gates can make vague criteria check block."""
        body = """## Acceptance Criteria

- [ ] Make it better

## Test Plan

- [ ] Test it

Modify `builder.py`.
"""
        gates = QualityGates(vague_criteria=QualityGateLevel.BLOCK)
        result = validate_issue_quality_with_gates(body, gates)
        assert result.has_blocking_findings is True
        assert any("vague" in f.message.lower() for f in result.blocks)

    def test_strict_gates_with_quality_signals_no_block(self) -> None:
        """Strict gates should not block when content-quality signals are present."""
        body = """## Summary

Refactor the auth module.

1. Ensure tokens are validated properly
2. Verify expired tokens return 401
3. Check that role-based access works

See `auth/middleware.py` and `auth/tokens.py`.
"""
        gates = QualityGates.strict()
        result = validate_issue_quality_with_gates(body, gates)
        assert not any("acceptance criteria" in f.message.lower() for f in result.blocks)

    def test_all_info_gates_no_warnings(self) -> None:
        """When all gates are INFO, findings should be INFO level."""
        body = "Vague description."
        gates = QualityGates(
            test_plan=QualityGateLevel.INFO,
            file_refs=QualityGateLevel.INFO,
            acceptance_criteria=QualityGateLevel.INFO,
            vague_criteria=QualityGateLevel.INFO,
        )
        result = validate_issue_quality_with_gates(body, gates)
        assert len(result.blocks) == 0
        assert len(result.warnings) == 0
        assert len(result.infos) > 0


class TestQualityGatesEnvironment:
    """Test quality gates environment variable configuration."""

    def test_env_var_test_plan_block(self) -> None:
        """LOOM_QUALITY_TEST_PLAN=block should set test_plan to BLOCK."""
        with mock.patch.dict(os.environ, {"LOOM_QUALITY_TEST_PLAN": "block"}):
            gates = QualityGates()
            assert gates.test_plan == QualityGateLevel.BLOCK

    def test_env_var_acceptance_block(self) -> None:
        """LOOM_QUALITY_ACCEPTANCE=block should set acceptance_criteria to BLOCK."""
        with mock.patch.dict(os.environ, {"LOOM_QUALITY_ACCEPTANCE": "block"}):
            gates = QualityGates()
            assert gates.acceptance_criteria == QualityGateLevel.BLOCK

    def test_env_var_file_refs_warn(self) -> None:
        """LOOM_QUALITY_FILE_REFS=warn should set file_refs to WARN."""
        with mock.patch.dict(os.environ, {"LOOM_QUALITY_FILE_REFS": "warn"}):
            gates = QualityGates()
            assert gates.file_refs == QualityGateLevel.WARN

    def test_env_var_vague_info(self) -> None:
        """LOOM_QUALITY_VAGUE=info should set vague_criteria to INFO."""
        with mock.patch.dict(os.environ, {"LOOM_QUALITY_VAGUE": "info"}):
            gates = QualityGates()
            assert gates.vague_criteria == QualityGateLevel.INFO

    def test_env_var_case_insensitive(self) -> None:
        """Environment variable values should be case-insensitive."""
        with mock.patch.dict(os.environ, {"LOOM_QUALITY_TEST_PLAN": "BLOCK"}):
            gates = QualityGates()
            assert gates.test_plan == QualityGateLevel.BLOCK

    def test_env_var_invalid_uses_default(self) -> None:
        """Invalid environment variable value should use default."""
        with mock.patch.dict(os.environ, {"LOOM_QUALITY_TEST_PLAN": "invalid"}):
            gates = QualityGates()
            assert gates.test_plan == QualityGateLevel.INFO  # Default


class TestSeverityEnum:
    """Test Severity enum."""

    def test_block_severity_exists(self) -> None:
        """BLOCK severity level should exist."""
        assert Severity.BLOCK.value == "block"

    def test_severity_values(self) -> None:
        """All severity values should be correct."""
        assert Severity.BLOCK.value == "block"
        assert Severity.WARNING.value == "warning"
        assert Severity.INFO.value == "info"
