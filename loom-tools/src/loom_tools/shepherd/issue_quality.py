"""Pre-flight validation for issue quality before Builder phase.

Checks issue body for quality indicators and returns informational
warnings. Validation is advisory only -- it never blocks the Builder.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from enum import Enum


class Severity(Enum):
    """Severity level for quality findings."""

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
    def warnings(self) -> list[QualityFinding]:
        """Return only warning-level findings."""
        return [f for f in self.findings if f.severity == Severity.WARNING]

    @property
    def infos(self) -> list[QualityFinding]:
        """Return only info-level findings."""
        return [f for f in self.findings if f.severity == Severity.INFO]


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

    if not has_ac_heading and not has_checkboxes:
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
