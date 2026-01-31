"""Validate daemon-state.json structure and task IDs.

Port of validate-daemon-state.sh to Python. Validates the daemon state file
to catch corruption and fabricated task IDs before they cause cascading
failures in the orchestration system.

Exit codes:
    0 - Valid (or fixed successfully with --fix)
    1 - Invalid (with error details)
    2 - File not found or unreadable
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import safe_parse_json, write_json_file

# Valid status values
VALID_SHEPHERD_STATUSES = {"working", "idle", "errored", "paused"}
VALID_SUPPORT_ROLE_STATUSES = {"running", "idle"}

# Task ID pattern: 7-char lowercase hex
TASK_ID_RE = re.compile(r"^[a-f0-9]{7}$")

# ISO 8601 timestamp pattern: YYYY-MM-DDTHH:MM:SSZ (Z optional)
TIMESTAMP_RE = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z?$")

# Required top-level fields
REQUIRED_FIELDS = ("started_at", "running", "iteration")

# Timestamp fields to validate
TIMESTAMP_FIELDS = ("started_at", "last_poll", "last_architect_trigger", "last_hermit_trigger")


def _now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def validate_state(
    data: dict[str, Any],
    *,
    fix: bool = False,
) -> tuple[list[str], list[str], list[str], dict[str, Any] | None]:
    """Validate a parsed daemon-state dict.

    Returns (errors, warnings, fixes, fixed_data).
    fixed_data is non-None only when fix=True and fixes were applied.
    """
    errors: list[str] = []
    warnings: list[str] = []
    fixes: list[str] = []

    # Rule 1: Required fields
    for field in REQUIRED_FIELDS:
        if field not in data:
            errors.append(f"missing_field:{field}")

    # Rule 2-4: Shepherds
    shepherds = data.get("shepherds", {})
    if isinstance(shepherds, dict):
        for sid, sdata in shepherds.items():
            if not isinstance(sdata, dict):
                errors.append(f"invalid_shepherd_data:{sid}")
                continue

            # Rule 4: Shepherd status
            status = sdata.get("status", "unknown")
            if status not in VALID_SHEPHERD_STATUSES:
                errors.append(f"invalid_shepherd_status:{sid}:{status}")

            # Rule 3: Task ID format
            task_id = sdata.get("task_id")
            if task_id is not None and task_id != "":
                if not TASK_ID_RE.match(str(task_id)):
                    errors.append(f"invalid_task_id:{sid}:{task_id}")
                    if fix:
                        fixes.append(f"reset_shepherd:{sid}")

            # Rule 7: Working without task_id warning
            execution_mode = sdata.get("execution_mode", "direct")
            if status == "working" and not task_id and execution_mode == "direct":
                warnings.append(f"working_without_task_id:{sid}")

    # Rule 5: Support roles
    support_roles = data.get("support_roles", {})
    if isinstance(support_roles, dict):
        for rname, rdata in support_roles.items():
            if not isinstance(rdata, dict):
                errors.append(f"invalid_support_role_data:{rname}")
                continue

            status = rdata.get("status", "unknown")
            if status not in VALID_SUPPORT_ROLE_STATUSES:
                errors.append(f"invalid_support_role_status:{rname}:{status}")

            task_id = rdata.get("task_id")
            if task_id is not None and task_id != "":
                if not TASK_ID_RE.match(str(task_id)):
                    errors.append(f"invalid_task_id:{rname}:{task_id}")
                    if fix:
                        fixes.append(f"reset_support_role:{rname}")

    # Rule 6: Timestamps
    for field in TIMESTAMP_FIELDS:
        value = data.get(field)
        if value is not None and value != "":
            if not TIMESTAMP_RE.match(str(value)):
                warnings.append(f"invalid_timestamp_format:{field}:{value}")

    # Apply fixes
    fixed_data: dict[str, Any] | None = None
    if fix and fixes:
        import copy

        fixed_data = copy.deepcopy(data)
        now = _now_iso()

        for fix_entry in fixes:
            fix_type, fix_target = fix_entry.split(":", 1)
            if fix_type == "reset_shepherd":
                fixed_data["shepherds"][fix_target] = {
                    "status": "idle",
                    "issue": None,
                    "task_id": None,
                    "output_file": None,
                    "idle_since": now,
                    "idle_reason": "invalid_task_id_reset",
                }
            elif fix_type == "reset_support_role":
                fixed_data["support_roles"][fix_target] = {
                    "status": "idle",
                    "task_id": None,
                    "output_file": None,
                    "last_completed": now,
                }

    return errors, warnings, fixes, fixed_data


def main(argv: list[str] | None = None) -> int:
    """CLI entry point for loom-validate-state."""
    parser = argparse.ArgumentParser(
        description="Validate daemon-state.json structure and task IDs",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Validations performed:
  - JSON syntax
  - Task IDs match ^[a-f0-9]{7}$ (real Task tool IDs)
  - Required fields: started_at, running, iteration
  - Shepherd status: working, idle, errored, paused
  - Support role status: running, idle
  - ISO 8601 timestamp format

Examples:
  loom-validate-state                       # Validate default state file
  loom-validate-state --fix                 # Fix and write back
  loom-validate-state --fix --dry-run       # Preview fixes without writing
  loom-validate-state --json                # Machine-readable output
  loom-validate-state /path/to/state.json   # Validate specific file
""",
    )
    parser.add_argument(
        "state_file",
        nargs="?",
        default=None,
        help="Path to state file (default: .loom/daemon-state.json)",
    )
    parser.add_argument(
        "--fix",
        action="store_true",
        help="Auto-fix common issues (resets invalid entries to idle)",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        dest="json_output",
        help="Output JSON for programmatic use",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Show what would be fixed without making changes",
    )

    args = parser.parse_args(argv)

    # Resolve state file path
    if args.state_file:
        state_path = Path(args.state_file)
    else:
        try:
            repo_root = find_repo_root()
        except FileNotFoundError:
            if args.json_output:
                print(json.dumps({"valid": False, "error": "repo_not_found"}))
            else:
                log_error("Not in a git repository with .loom directory")
            return 2
        state_path = repo_root / ".loom" / "daemon-state.json"

    # Check file exists
    if not state_path.exists():
        if args.json_output:
            print(json.dumps({"valid": False, "error": "file_not_found", "file": str(state_path)}))
        else:
            log_error(f"State file not found: {state_path}")
        return 2

    # Check file readable
    if not state_path.is_file():
        if args.json_output:
            print(json.dumps({"valid": False, "error": "file_not_readable", "file": str(state_path)}))
        else:
            log_error(f"State file not readable: {state_path}")
        return 2

    # Parse JSON
    try:
        raw_text = state_path.read_text()
    except OSError:
        if args.json_output:
            print(json.dumps({"valid": False, "error": "file_not_readable", "file": str(state_path)}))
        else:
            log_error(f"Could not read {state_path}")
        return 2

    # Use safe_parse_json with a sentinel to detect parse failure
    sentinel: dict[str, Any] = {"__parse_failed__": True}
    data = safe_parse_json(raw_text, default=sentinel)
    if data is sentinel or not isinstance(data, dict):
        if args.json_output:
            print(json.dumps({"valid": False, "error": "invalid_json", "file": str(state_path)}))
        else:
            if data is sentinel:
                log_error(f"Invalid JSON in {state_path}")
            else:
                log_error(f"Expected JSON object, got {type(data).__name__}")
        return 1

    # Validate
    if not args.json_output:
        log_info("Validating JSON syntax...")
        log_info("Validating required fields...")
        log_info("Validating shepherds...")
        log_info("Validating support roles...")
        log_info("Validating timestamps...")

    errors, warnings, fixes, fixed_data = validate_state(data, fix=args.fix)

    # Apply fixes
    if args.fix and fixed_data is not None:
        if args.dry_run:
            if not args.json_output:
                log_info(f"Dry run - would write fixed state to {state_path}")
        else:
            write_json_file(state_path, fixed_data)
            if not args.json_output:
                log_success(f"Fixed state written to {state_path}")

    # Output
    if args.json_output:
        valid = len(errors) == 0
        fixes_applied = fixes if (args.fix and not args.dry_run) else []
        fixes_available = fixes if not args.fix else []
        result = {
            "valid": valid,
            "file": str(state_path),
            "errors": errors,
            "warnings": warnings,
            "fixes_applied": fixes_applied,
            "fixes_available": fixes_available,
            "error_count": len(errors),
            "warning_count": len(warnings),
        }
        print(json.dumps(result, indent=2))
    else:
        if not errors:
            log_success(f"State file is valid: {state_path}")
        else:
            log_error(f"State file has {len(errors)} error(s):")
            for error in errors:
                print(f"  - {error}", file=sys.stderr)

        if warnings:
            log_warning(f"Warnings ({len(warnings)}):")
            for warning in warnings:
                print(f"  - {warning}", file=sys.stderr)

        if fixes:
            if args.fix:
                if args.dry_run:
                    log_info(f"Would apply {len(fixes)} fix(es) (dry run)")
                else:
                    log_success(f"Applied {len(fixes)} fix(es)")
            else:
                log_info(f"Available fixes (run with --fix): {len(fixes)}")
                for fix in fixes:
                    print(f"  - {fix}", file=sys.stderr)

    # Exit code
    if errors and not args.fix:
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
