"""Tests for issue quality pre-flight validation."""

from __future__ import annotations

from loom_tools.shepherd.issue_quality import (
    Severity,
    ValidationResult,
    validate_issue_quality,
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
        from loom_tools.shepherd.issue_quality import QualityFinding

        result = ValidationResult(
            findings=[
                QualityFinding(severity=Severity.INFO, message="i1"),
                QualityFinding(severity=Severity.INFO, message="i2"),
            ]
        )
        assert len(result.infos) == 2
        assert len(result.warnings) == 0
