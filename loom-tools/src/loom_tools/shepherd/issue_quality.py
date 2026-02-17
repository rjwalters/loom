"""Pre-flight validation for issue quality before Builder phase.

Checks issue body for quality indicators and returns findings at
configurable severity levels. When quality gates are configured,
BLOCK-level findings will cause the builder phase to fail.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from enum import Enum
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from loom_tools.shepherd.config import QualityGates


class Severity(Enum):
    """Severity level for quality findings.

    INFO: Nice-to-have quality indicators (logged with log_info)
    WARNING: Important but not blocking (logged with log_warning)
    BLOCK: Required before building (logged with log_error, blocks phase)
    """

    BLOCK = "block"
    WARNING = "warning"
    INFO = "info"


@dataclass
class QualityFinding:
    """A single quality finding about an issue."""

    severity: Severity
    message: str


@dataclass
class ValidationResult:
    """Result of issue quality validation."""

    findings: list[QualityFinding] = field(default_factory=list)

    @property
    def blocks(self) -> list[QualityFinding]:
        """Return only block-level findings."""
        return [f for f in self.findings if f.severity == Severity.BLOCK]

    @property
    def warnings(self) -> list[QualityFinding]:
        """Return only warning-level findings."""
        return [f for f in self.findings if f.severity == Severity.WARNING]

    @property
    def infos(self) -> list[QualityFinding]:
        """Return only info-level findings."""
        return [f for f in self.findings if f.severity == Severity.INFO]

    @property
    def has_blocking_findings(self) -> bool:
        """Return True if any findings have BLOCK severity."""
        return len(self.blocks) > 0


# Patterns that indicate vague/non-checkable acceptance criteria.
# Each entry is (compiled regex, description for the finding message).
_VAGUE_PATTERNS: list[tuple[re.Pattern[str], str]] = [
    (re.compile(r"\bmake\s+it\s+better\b", re.IGNORECASE), "make it better"),
    (re.compile(r"\bimprove\s+(?:the\s+)?performance\b", re.IGNORECASE), "improve performance"),
    (re.compile(r"\bfix\s+the\s+issues?\b", re.IGNORECASE), "fix the issue(s)"),
    (re.compile(r"\bshould\s+work\s+(?:well|properly|correctly)\b", re.IGNORECASE), "should work well"),
    (re.compile(r"\bclean\s*up\s+the\s+code\b", re.IGNORECASE), "clean up the code"),
]

# Heading patterns that indicate an acceptance criteria section.
_AC_HEADING_RE = re.compile(
    r"^#{1,3}\s+(?:acceptance\s+criteria|requirements|expected\s+behavio(?:u?r))",
    re.IGNORECASE | re.MULTILINE,
)

# Checkbox pattern used in acceptance criteria lists.
_CHECKBOX_RE = re.compile(r"^\s*-\s*\[[ x]\]", re.MULTILINE)

# Numbered list items with action verbs (e.g., "1. Ensure X", "2. Verify Y").
_NUMBERED_ACTION_RE = re.compile(
    r"^\s*\d+\.\s+(?:ensure|verify|confirm|check|validate|test|add|update|implement|create|remove|fix)\b",
    re.IGNORECASE | re.MULTILINE,
)

# Requirement statements using "should", "must", or "shall" with a subject.
_REQUIREMENT_STMT_RE = re.compile(
    r"(?:^|\.\s+)\S.{0,80}?\b(?:should|must|shall)\b\s+\S",
    re.IGNORECASE | re.MULTILINE,
)

# Bug report pattern: "Observed Behavior" + "Expected Behavior" headings.
_OBSERVED_BEHAVIOR_RE = re.compile(
    r"^#{1,3}\s+(?:observed|actual|current)\s+behavio(?:u?r)",
    re.IGNORECASE | re.MULTILINE,
)
_EXPECTED_BEHAVIOR_RE = re.compile(
    r"^#{1,3}\s+expected\s+behavio(?:u?r)",
    re.IGNORECASE | re.MULTILINE,
)

# Minimum number of file references to count as "well-researched".
_MIN_FILE_REFS_FOR_QUALITY = 3

# Heading pattern for test plan section.
_TEST_PLAN_RE = re.compile(
    r"^#{1,3}\s+test(?:ing)?\s+plan",
    re.IGNORECASE | re.MULTILINE,
)

# Pattern for file path or component references.
_FILE_REF_RE = re.compile(
    r"(?:"
    r"[\w/]+\.(?:py|ts|tsx|js|jsx|sh|rs|go|json|yaml|yml|toml|md)"  # file paths
    r"|`[^`]+\.(?:py|ts|tsx|js|jsx|sh|rs|go|json|yaml|yml|toml|md)`"  # backtick-quoted
    r")",
)


def _has_content_quality_signals(issue_body: str) -> bool:
    """Check if issue body has content-quality signals beyond formal headings.

    Recognizes well-specified issues that use alternative structures instead
    of formal ``## Acceptance Criteria`` headings or checkbox syntax.

    Returns True when the body contains at least one quality signal:
    - Numbered action items (e.g. "1. Ensure ...")
    - Requirement statements ("should", "must", "shall")
    - Bug report pattern (Observed + Expected Behavior headings)
    - Multiple file/code references (3+, indicating well-researched issue)
    """
    # Numbered action items
    if len(_NUMBERED_ACTION_RE.findall(issue_body)) >= 2:
        return True

    # Requirement statements
    if len(_REQUIREMENT_STMT_RE.findall(issue_body)) >= 2:
        return True

    # Bug report pattern (need both headings)
    if _OBSERVED_BEHAVIOR_RE.search(issue_body) and _EXPECTED_BEHAVIOR_RE.search(
        issue_body
    ):
        return True

    # Multiple file references indicate well-researched issue
    if len(_FILE_REF_RE.findall(issue_body)) >= _MIN_FILE_REFS_FOR_QUALITY:
        return True

    return False


def validate_issue_quality(issue_body: str) -> ValidationResult:
    """Check issue body for quality indicators before Builder phase.

    This is an informational-only check. The Builder should always proceed
    regardless of findings -- warnings are logged for observability.

    Args:
        issue_body: The raw markdown body of the GitHub issue.

    Returns:
        ValidationResult with any findings.
    """
    if not issue_body or not issue_body.strip():
        return ValidationResult(
            findings=[
                QualityFinding(
                    severity=Severity.WARNING,
                    message="Issue body is empty",
                ),
            ]
        )

    findings: list[QualityFinding] = []

    # Check 1: Has acceptance criteria section (Warning)
    has_ac_heading = bool(_AC_HEADING_RE.search(issue_body))
    has_checkboxes = bool(_CHECKBOX_RE.search(issue_body))
    has_quality_signals = _has_content_quality_signals(issue_body)

    if not has_ac_heading and not has_checkboxes and not has_quality_signals:
        findings.append(
            QualityFinding(
                severity=Severity.WARNING,
                message="No acceptance criteria section found",
            )
        )

    # Check 2: Vague acceptance criteria (Warning)
    for pattern, description in _VAGUE_PATTERNS:
        if pattern.search(issue_body):
            findings.append(
                QualityFinding(
                    severity=Severity.WARNING,
                    message=f"Potentially vague criterion: '{description}'",
                )
            )

    # Check 3: Has test plan section (Info)
    if not _TEST_PLAN_RE.search(issue_body):
        findings.append(
            QualityFinding(
                severity=Severity.INFO,
                message="No test plan section found",
            )
        )

    # Check 4: References specific files/components (Info)
    if not _FILE_REF_RE.search(issue_body):
        findings.append(
            QualityFinding(
                severity=Severity.INFO,
                message="No specific file or component references found",
            )
        )

    return ValidationResult(findings=findings)


def _gate_level_to_severity(gate_level: "QualityGateLevel") -> Severity:
    """Convert QualityGateLevel to Severity.

    Imported here to avoid circular imports.
    """
    from loom_tools.shepherd.config import QualityGateLevel

    mapping = {
        QualityGateLevel.INFO: Severity.INFO,
        QualityGateLevel.WARN: Severity.WARNING,
        QualityGateLevel.BLOCK: Severity.BLOCK,
    }
    return mapping.get(gate_level, Severity.INFO)


def validate_issue_quality_with_gates(
    issue_body: str, quality_gates: "QualityGates"
) -> ValidationResult:
    """Check issue body for quality indicators with configurable severity levels.

    Unlike validate_issue_quality(), this function uses the configured quality
    gates to determine the severity of each finding. Findings with BLOCK severity
    will cause the builder phase to fail.

    Args:
        issue_body: The raw markdown body of the GitHub issue.
        quality_gates: Configuration for quality gate severity levels.

    Returns:
        ValidationResult with findings at configured severity levels.
    """
    if not issue_body or not issue_body.strip():
        # Empty body is always a warning (not configurable)
        return ValidationResult(
            findings=[
                QualityFinding(
                    severity=Severity.WARNING,
                    message="Issue body is empty",
                ),
            ]
        )

    findings: list[QualityFinding] = []

    # Check 1: Has acceptance criteria section
    has_ac_heading = bool(_AC_HEADING_RE.search(issue_body))
    has_checkboxes = bool(_CHECKBOX_RE.search(issue_body))
    has_quality_signals = _has_content_quality_signals(issue_body)

    if not has_ac_heading and not has_checkboxes and not has_quality_signals:
        findings.append(
            QualityFinding(
                severity=_gate_level_to_severity(quality_gates.acceptance_criteria),
                message="No acceptance criteria section found",
            )
        )

    # Check 2: Vague acceptance criteria
    for pattern, description in _VAGUE_PATTERNS:
        if pattern.search(issue_body):
            findings.append(
                QualityFinding(
                    severity=_gate_level_to_severity(quality_gates.vague_criteria),
                    message=f"Potentially vague criterion: '{description}'",
                )
            )

    # Check 3: Has test plan section
    if not _TEST_PLAN_RE.search(issue_body):
        findings.append(
            QualityFinding(
                severity=_gate_level_to_severity(quality_gates.test_plan),
                message="No test plan section found",
            )
        )

    # Check 4: References specific files/components
    if not _FILE_REF_RE.search(issue_body):
        findings.append(
            QualityFinding(
                severity=_gate_level_to_severity(quality_gates.file_refs),
                message="No specific file or component references found",
            )
        )

    return ValidationResult(findings=findings)
