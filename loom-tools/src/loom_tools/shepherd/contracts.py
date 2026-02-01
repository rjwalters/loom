"""Phase contract system for explicit precondition validation.

This module provides a contract-based approach to phase validation.
Each phase has preconditions that must be satisfied BEFORE the phase runs.
If preconditions are not met, the phase is skipped with a clear failure message.

Contract violations apply explicit failure labels (e.g., loom:failed:builder)
and add diagnostic comments to the issue for manual intervention.
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass
from typing import TYPE_CHECKING, Callable

if TYPE_CHECKING:
    from loom_tools.shepherd.context import ShepherdContext


@dataclass
class Contract:
    """A single precondition contract for a phase.

    Attributes:
        name: Short name for the contract (e.g., "issue_open")
        check: Function that returns True if contract is satisfied
        violation_message: Human-readable message when contract is violated
        failure_label: Optional label to apply on violation (e.g., "loom:failed:builder")
    """

    name: str
    check: Callable[[ShepherdContext], bool]
    violation_message: str
    failure_label: str | None = None


@dataclass
class ContractViolation:
    """Result of a failed contract check.

    Attributes:
        phase: Phase that was being checked
        contract: The contract that was violated
        details: Additional diagnostic details
    """

    phase: str
    contract: Contract
    details: str = ""

    @property
    def message(self) -> str:
        """Human-readable violation message."""
        msg = f"{self.phase} precondition failed: {self.contract.violation_message}"
        if self.details:
            msg += f" ({self.details})"
        return msg


# ---------------------------------------------------------------------------
# Contract check functions
# ---------------------------------------------------------------------------


def _check_issue_exists(ctx: ShepherdContext) -> bool:
    """Check that the issue exists."""
    result = subprocess.run(
        ["gh", "issue", "view", str(ctx.config.issue), "--json", "state"],
        cwd=ctx.repo_root,
        capture_output=True,
        text=True,
        check=False,
    )
    return result.returncode == 0


def _check_issue_open(ctx: ShepherdContext) -> bool:
    """Check that the issue is in OPEN state."""
    result = subprocess.run(
        ["gh", "issue", "view", str(ctx.config.issue), "--json", "state", "--jq", ".state"],
        cwd=ctx.repo_root,
        capture_output=True,
        text=True,
        check=False,
    )
    return result.returncode == 0 and result.stdout.strip().upper() == "OPEN"


def _check_issue_has_loom_issue_label(ctx: ShepherdContext) -> bool:
    """Check that issue has loom:issue label (ready for work)."""
    return ctx.has_issue_label("loom:issue")


def _check_no_existing_pr(ctx: ShepherdContext) -> bool:
    """Check that no open PR exists for this issue.

    Returns True if no PR exists (contract satisfied).
    """
    from loom_tools.shepherd.labels import get_pr_for_issue

    pr = get_pr_for_issue(ctx.config.issue, repo_root=ctx.repo_root)
    return pr is None


def _check_pr_exists(ctx: ShepherdContext) -> bool:
    """Check that a PR exists for this issue."""
    return ctx.pr_number is not None


def _check_pr_is_open(ctx: ShepherdContext) -> bool:
    """Check that the PR is in OPEN state."""
    if ctx.pr_number is None:
        return False

    result = subprocess.run(
        ["gh", "pr", "view", str(ctx.pr_number), "--json", "state", "--jq", ".state"],
        cwd=ctx.repo_root,
        capture_output=True,
        text=True,
        check=False,
    )
    return result.returncode == 0 and result.stdout.strip().upper() == "OPEN"


def _check_pr_has_review_requested(ctx: ShepherdContext) -> bool:
    """Check that PR has loom:review-requested label."""
    return ctx.has_pr_label("loom:review-requested")


def _check_pr_has_changes_requested(ctx: ShepherdContext) -> bool:
    """Check that PR has loom:changes-requested label."""
    return ctx.has_pr_label("loom:changes-requested")


def _check_pr_has_approved(ctx: ShepherdContext) -> bool:
    """Check that PR has loom:pr label (approved)."""
    return ctx.has_pr_label("loom:pr")


# ---------------------------------------------------------------------------
# Phase contracts
# ---------------------------------------------------------------------------

# Curator phase contracts
CURATOR_CONTRACTS = [
    Contract(
        name="issue_exists",
        check=_check_issue_exists,
        violation_message="Issue does not exist",
    ),
    Contract(
        name="issue_open",
        check=_check_issue_open,
        violation_message="Issue is not open",
    ),
]

# Builder phase contracts
BUILDER_CONTRACTS = [
    Contract(
        name="issue_exists",
        check=_check_issue_exists,
        violation_message="Issue does not exist",
        failure_label="loom:failed:builder",
    ),
    Contract(
        name="issue_open",
        check=_check_issue_open,
        violation_message="Issue is not open",
        failure_label="loom:failed:builder",
    ),
    Contract(
        name="issue_ready",
        check=_check_issue_has_loom_issue_label,
        violation_message="Issue does not have loom:issue label (not ready for work)",
        failure_label="loom:failed:builder",
    ),
    Contract(
        name="no_existing_pr",
        check=_check_no_existing_pr,
        violation_message="A PR already exists for this issue",
        # No failure label - this is an unexpected state, not a builder failure
    ),
]

# Judge phase contracts
JUDGE_CONTRACTS = [
    Contract(
        name="pr_exists",
        check=_check_pr_exists,
        violation_message="No PR exists for this issue",
        failure_label="loom:failed:judge",
    ),
    Contract(
        name="pr_open",
        check=_check_pr_is_open,
        violation_message="PR is not open",
        failure_label="loom:failed:judge",
    ),
    Contract(
        name="review_requested",
        check=_check_pr_has_review_requested,
        violation_message="PR does not have loom:review-requested label",
        failure_label="loom:failed:judge",
    ),
]

# Doctor phase contracts
DOCTOR_CONTRACTS = [
    Contract(
        name="pr_exists",
        check=_check_pr_exists,
        violation_message="No PR exists for this issue",
        failure_label="loom:failed:doctor",
    ),
    Contract(
        name="pr_open",
        check=_check_pr_is_open,
        violation_message="PR is not open",
        failure_label="loom:failed:doctor",
    ),
    Contract(
        name="changes_requested",
        check=_check_pr_has_changes_requested,
        violation_message="PR does not have loom:changes-requested label",
        failure_label="loom:failed:doctor",
    ),
]

# Merge phase contracts
MERGE_CONTRACTS = [
    Contract(
        name="pr_exists",
        check=_check_pr_exists,
        violation_message="No PR exists for this issue",
    ),
    Contract(
        name="pr_approved",
        check=_check_pr_has_approved,
        violation_message="PR does not have loom:pr label (not approved)",
    ),
]

# Phase name to contracts mapping
PHASE_CONTRACTS: dict[str, list[Contract]] = {
    "curator": CURATOR_CONTRACTS,
    "builder": BUILDER_CONTRACTS,
    "judge": JUDGE_CONTRACTS,
    "doctor": DOCTOR_CONTRACTS,
    "merge": MERGE_CONTRACTS,
}


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------


def check_preconditions(ctx: ShepherdContext, phase: str) -> ContractViolation | None:
    """Check all preconditions for a phase BEFORE running it.

    Args:
        ctx: Shepherd context with issue/PR state
        phase: Phase name (e.g., "builder", "judge")

    Returns:
        ContractViolation if any precondition fails, None if all pass
    """
    contracts = PHASE_CONTRACTS.get(phase, [])

    for contract in contracts:
        try:
            if not contract.check(ctx):
                return ContractViolation(
                    phase=phase,
                    contract=contract,
                )
        except Exception as e:
            # Contract check itself failed - treat as violation
            return ContractViolation(
                phase=phase,
                contract=contract,
                details=f"check raised exception: {e}",
            )

    return None


def apply_contract_violation(
    ctx: ShepherdContext,
    violation: ContractViolation,
) -> None:
    """Apply failure label and add diagnostic comment for a contract violation.

    Args:
        ctx: Shepherd context
        violation: The contract violation to apply
    """
    issue = ctx.config.issue

    # Apply failure label if specified
    if violation.contract.failure_label:
        subprocess.run(
            [
                "gh",
                "issue",
                "edit",
                str(issue),
                "--remove-label",
                "loom:building",
                "--add-label",
                violation.contract.failure_label,
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            check=False,
        )

    # Add diagnostic comment
    comment = (
        f"**Contract violation**: {violation.phase} phase precondition failed.\n\n"
        f"- **Contract**: `{violation.contract.name}`\n"
        f"- **Reason**: {violation.contract.violation_message}\n"
    )
    if violation.details:
        comment += f"- **Details**: {violation.details}\n"

    comment += (
        "\nThis indicates the shepherd was started in an unexpected state. "
        "Check the issue/PR labels and state before retrying."
    )

    subprocess.run(
        ["gh", "issue", "comment", str(issue), "--body", comment],
        cwd=ctx.repo_root,
        capture_output=True,
        check=False,
    )
