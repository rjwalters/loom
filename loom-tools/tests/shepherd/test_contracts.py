"""Tests for the phase contracts system."""

from __future__ import annotations

from unittest.mock import MagicMock, patch

import pytest

from loom_tools.shepherd.contracts import (
    Contract,
    ContractViolation,
    check_preconditions,
    apply_contract_violation,
    PHASE_CONTRACTS,
)


class TestContract:
    """Test Contract dataclass."""

    def test_contract_creation(self) -> None:
        """Contract can be created with all fields."""
        c = Contract(
            name="test_contract",
            check=lambda ctx: True,
            violation_message="Test failed",
            failure_label="loom:failed:test",
        )
        assert c.name == "test_contract"
        assert c.violation_message == "Test failed"
        assert c.failure_label == "loom:failed:test"

    def test_contract_without_failure_label(self) -> None:
        """Contract can be created without failure_label."""
        c = Contract(
            name="test",
            check=lambda ctx: True,
            violation_message="Test",
        )
        assert c.failure_label is None


class TestContractViolation:
    """Test ContractViolation dataclass."""

    def test_message_without_details(self) -> None:
        """Violation message should include phase and reason."""
        c = Contract(
            name="test",
            check=lambda ctx: True,
            violation_message="PR not found",
        )
        v = ContractViolation(phase="judge", contract=c)
        assert "judge" in v.message
        assert "PR not found" in v.message

    def test_message_with_details(self) -> None:
        """Violation message should include details when provided."""
        c = Contract(
            name="test",
            check=lambda ctx: True,
            violation_message="PR not found",
        )
        v = ContractViolation(phase="judge", contract=c, details="API error")
        assert "API error" in v.message


class TestPhaseContracts:
    """Test that phase contracts are properly defined."""

    def test_all_phases_have_contracts(self) -> None:
        """Verify contracts exist for all expected phases."""
        expected_phases = ["curator", "builder", "judge", "doctor", "merge"]
        for phase in expected_phases:
            assert phase in PHASE_CONTRACTS
            assert len(PHASE_CONTRACTS[phase]) > 0

    def test_builder_has_failure_labels(self) -> None:
        """Builder contracts should specify failure labels."""
        for contract in PHASE_CONTRACTS["builder"]:
            # Most builder contracts should have failure labels
            if contract.name != "no_existing_pr":
                assert contract.failure_label == "loom:failed:builder"

    def test_judge_has_failure_labels(self) -> None:
        """Judge contracts should specify failure labels."""
        for contract in PHASE_CONTRACTS["judge"]:
            assert contract.failure_label == "loom:failed:judge"

    def test_doctor_has_failure_labels(self) -> None:
        """Doctor contracts should specify failure labels."""
        for contract in PHASE_CONTRACTS["doctor"]:
            assert contract.failure_label == "loom:failed:doctor"


class TestCheckPreconditions:
    """Test check_preconditions function."""

    def test_returns_none_when_all_pass(self) -> None:
        """Should return None when all contracts pass."""
        ctx = MagicMock()
        ctx.has_issue_label.return_value = True

        with patch(
            "loom_tools.shepherd.contracts.PHASE_CONTRACTS",
            {"test": [
                Contract(name="always_pass", check=lambda c: True, violation_message="pass"),
            ]},
        ):
            result = check_preconditions(ctx, "test")
            assert result is None

    def test_returns_violation_when_fails(self) -> None:
        """Should return ContractViolation when a contract fails."""
        ctx = MagicMock()

        with patch(
            "loom_tools.shepherd.contracts.PHASE_CONTRACTS",
            {"test": [
                Contract(name="always_fail", check=lambda c: False, violation_message="failed"),
            ]},
        ):
            result = check_preconditions(ctx, "test")
            assert result is not None
            assert isinstance(result, ContractViolation)
            assert result.contract.name == "always_fail"

    def test_returns_first_violation(self) -> None:
        """Should return the first failing contract."""
        ctx = MagicMock()

        with patch(
            "loom_tools.shepherd.contracts.PHASE_CONTRACTS",
            {"test": [
                Contract(name="first", check=lambda c: False, violation_message="first failed"),
                Contract(name="second", check=lambda c: False, violation_message="second failed"),
            ]},
        ):
            result = check_preconditions(ctx, "test")
            assert result is not None
            assert result.contract.name == "first"

    def test_handles_unknown_phase(self) -> None:
        """Should return None for unknown phases (no contracts)."""
        ctx = MagicMock()
        result = check_preconditions(ctx, "nonexistent_phase")
        assert result is None

    def test_handles_exception_in_check(self) -> None:
        """Should return violation when check raises exception."""
        ctx = MagicMock()

        def bad_check(c):
            raise RuntimeError("check failed")

        with patch(
            "loom_tools.shepherd.contracts.PHASE_CONTRACTS",
            {"test": [
                Contract(name="bad", check=bad_check, violation_message="msg"),
            ]},
        ):
            result = check_preconditions(ctx, "test")
            assert result is not None
            assert "exception" in result.details


class TestApplyContractViolation:
    """Test apply_contract_violation function."""

    @patch("subprocess.run")
    def test_applies_failure_label(self, mock_run: MagicMock) -> None:
        """Should apply the failure label from the contract."""
        mock_run.return_value = MagicMock(returncode=0)

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = "/fake/repo"

        contract = Contract(
            name="test",
            check=lambda c: True,
            violation_message="test failed",
            failure_label="loom:failed:test",
        )
        violation = ContractViolation(phase="test", contract=contract)

        apply_contract_violation(ctx, violation)

        # Should have made two calls: edit labels and add comment
        assert mock_run.call_count == 2
        label_call = mock_run.call_args_list[0]
        assert "loom:failed:test" in label_call[0][0]

    @patch("subprocess.run")
    def test_skips_label_when_not_specified(self, mock_run: MagicMock) -> None:
        """Should not apply label when failure_label is None."""
        mock_run.return_value = MagicMock(returncode=0)

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = "/fake/repo"

        contract = Contract(
            name="test",
            check=lambda c: True,
            violation_message="test failed",
            failure_label=None,
        )
        violation = ContractViolation(phase="test", contract=contract)

        apply_contract_violation(ctx, violation)

        # Should only make one call: add comment (no label edit)
        assert mock_run.call_count == 1
        comment_call = mock_run.call_args_list[0]
        assert "comment" in comment_call[0][0]

    @patch("subprocess.run")
    def test_adds_diagnostic_comment(self, mock_run: MagicMock) -> None:
        """Should add a comment with diagnostic information."""
        mock_run.return_value = MagicMock(returncode=0)

        ctx = MagicMock()
        ctx.config.issue = 42
        ctx.repo_root = "/fake/repo"

        contract = Contract(
            name="pr_exists",
            check=lambda c: True,
            violation_message="No PR exists",
            failure_label="loom:failed:judge",
        )
        violation = ContractViolation(
            phase="judge",
            contract=contract,
            details="API returned 404",
        )

        apply_contract_violation(ctx, violation)

        # Check comment was added
        comment_call = mock_run.call_args_list[1]
        comment_body = comment_call[0][0][comment_call[0][0].index("--body") + 1]
        assert "pr_exists" in comment_body
        assert "No PR exists" in comment_body
        assert "API returned 404" in comment_body
