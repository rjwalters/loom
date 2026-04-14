"""Issue dependency parsing and filtering.

Parses ``## Dependencies`` sections from GitHub issue bodies to determine
which issues have unmet dependencies.  Used by :func:`spawn_shepherds` to
avoid scheduling issues whose prerequisites are still open.

Supported formats inside the ``## Dependencies`` section::

    - #10
    - #13, #14
    - Depends on #10
    - Requires #10 and #13
    - #10 (scaffold) must be completed first

The parser extracts all ``#N`` references found between the Dependencies
heading and the next ``##`` heading (or end of body).
"""

from __future__ import annotations

import re
from typing import Any

from loom_tools.common.logging import log_info

# Regex to extract the ## Dependencies section content.
# Matches from "## Dependencies" to the next "##" heading or end of string.
_DEPS_SECTION_RE = re.compile(
    r"##\s+Dependencies\s*\n(.*?)(?=\n##\s|\Z)",
    re.DOTALL | re.IGNORECASE,
)

# Regex to find issue references (#N) within the dependencies section.
_ISSUE_REF_RE = re.compile(r"#(\d+)")


def parse_dependencies(body: str | None) -> set[int]:
    """Extract dependency issue numbers from an issue body.

    Looks for a ``## Dependencies`` section and returns the set of
    referenced issue numbers.  Returns an empty set if there is no
    Dependencies section or the body is empty/None.
    """
    if not body:
        return set()

    match = _DEPS_SECTION_RE.search(body)
    if not match:
        return set()

    section_text = match.group(1)
    return {int(m.group(1)) for m in _ISSUE_REF_RE.finditer(section_text)}


def filter_issues_by_dependencies(
    ready_issues: list[dict[str, Any]],
    all_open_issue_numbers: set[int],
) -> list[dict[str, Any]]:
    """Filter out issues whose dependencies are not yet closed.

    Args:
        ready_issues: Issues with ``loom:issue`` label from the snapshot.
            Each dict must have ``number`` and ``body`` keys.
        all_open_issue_numbers: Set of all open issue numbers in the
            pipeline (ready + building + blocked).  A dependency is
            considered *unmet* if its number appears in this set (i.e.
            the issue is still open).

    Returns:
        The subset of *ready_issues* whose dependencies are all closed
        (not present in *all_open_issue_numbers*), preserving order.
    """
    schedulable: list[dict[str, Any]] = []
    skipped: list[tuple[int, set[int]]] = []

    for issue in ready_issues:
        issue_num = issue.get("number")
        body = issue.get("body") or ""
        deps = parse_dependencies(body)

        if not deps:
            # No dependencies declared -- always schedulable
            schedulable.append(issue)
            continue

        # A dependency is unmet if it is still open
        unmet = deps & all_open_issue_numbers
        if unmet:
            skipped.append((issue_num, unmet))
        else:
            schedulable.append(issue)

    if skipped:
        for issue_num, unmet in skipped:
            refs = ", ".join(f"#{n}" for n in sorted(unmet))
            log_info(
                f"Skipping issue #{issue_num}: unmet dependencies ({refs})"
            )

    return schedulable
