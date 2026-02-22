"""Validate shepherd phase contracts.

Ports the logic from ``defaults/scripts/validate-phase.sh`` to a Python module
with both a programmatic API and a CLI (``loom-validate-phase``).

Phase contract validators check that the expected artifacts exist after a
shepherd phase completes (e.g. the builder created a PR with the correct
label). When a contract is not satisfied, the validator marks the issue with
the ``loom:blocked`` label and provides
diagnostic information for manual intervention.

Note: Auto-recovery was removed in favor of explicit failure visibility.
Failures now result in clear labels and diagnostic comments instead of
attempting to commit/push/create PRs automatically.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from enum import Enum
from pathlib import Path
from typing import Any

from loom_tools.common.git import derive_commit_message, parse_porcelain_path
from loom_tools.common.logging import log_warning, strip_ansi
from loom_tools.common.paths import LoomPaths
from loom_tools.common.state import find_progress_for_issue


class ValidationStatus(Enum):
    """Outcome of a phase contract check."""

    SATISFIED = "satisfied"
    RECOVERED = "recovered"
    FAILED = "failed"


@dataclass
class ValidationResult:
    """Result of a phase contract validation."""

    phase: str
    issue: int
    status: ValidationStatus
    message: str
    recovery_action: str = "none"

    @property
    def satisfied(self) -> bool:
        """True when the contract is met (either initially or after recovery)."""
        return self.status in (ValidationStatus.SATISFIED, ValidationStatus.RECOVERED)

    def to_dict(self) -> dict[str, Any]:
        """JSON-serialisable dict matching the bash script output shape."""
        return {
            "phase": self.phase,
            "issue": self.issue,
            "status": self.status.value,
            "message": self.message,
            "recovery_action": self.recovery_action,
        }

    def to_json(self) -> str:
        return json.dumps(self.to_dict())


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _find_repo_root(start: Path | None = None) -> Path:
    """Walk up from *start* (default cwd) to find the git repo root.

    Handles worktrees where ``.git`` is a file with a ``gitdir:`` pointer.
    """
    current = (start or Path.cwd()).resolve()
    while True:
        git_path = current / ".git"
        if git_path.exists():
            if git_path.is_file():
                text = git_path.read_text().strip()
                if text.startswith("gitdir:"):
                    gitdir = text.split(":", 1)[1].strip()
                    resolved = (current / gitdir).resolve()
                    p = resolved
                    while p.name != ".git" and p != p.parent:
                        p = p.parent
                    if p.name == ".git":
                        return p.parent
            else:
                return current
        parent = current.parent
        if parent == current:
            break
        current = parent
    return Path.cwd()


def _gh_cmd(repo_root: Path) -> str:
    """Return ``gh-cached`` when available, else plain ``gh``."""
    cached = repo_root / ".loom" / "scripts" / "gh-cached"
    if cached.is_file() and cached.stat().st_mode & 0o111:
        return str(cached)
    return "gh"


def _run_gh(
    args: list[str],
    repo_root: Path,
    *,
    use_cache: bool = True,
) -> subprocess.CompletedProcess[str]:
    """Run a ``gh`` (or ``gh-cached``) command."""
    gh = _gh_cmd(repo_root) if use_cache else "gh"
    return subprocess.run(
        [gh, *args],
        capture_output=True,
        text=True,
        check=False,
        cwd=repo_root,
    )


def _report_milestone(
    event: str,
    task_id: str | None,
    repo_root: Path,
    **kwargs: str,
) -> None:
    """Call ``report-milestone.sh`` if *task_id* is set."""
    if not task_id:
        return
    script = repo_root / ".loom" / "scripts" / "report-milestone.sh"
    if not script.is_file():
        return
    cmd: list[str] = [str(script), event, "--task-id", task_id]
    for key, value in kwargs.items():
        cmd.extend([f"--{key}", value])
    try:
        subprocess.run(cmd, capture_output=True, check=False)
    except OSError:
        pass


def _log_recovery_event(
    issue: int,
    recovery_type: str,
    reason: str,
    repo_root: Path,
    *,
    elapsed_seconds: int | None = None,
    worktree_had_changes: bool = False,
    commits_recovered: int = 0,
    pr_number: int | None = None,
    builder_exit_reason: str | None = None,
) -> None:
    """Log a recovery event to .loom/metrics/recovery-events.json.

    Args:
        issue: Issue number being recovered.
        recovery_type: Type of recovery performed (commit_and_pr, pr_only, add_label).
        reason: Reason for recovery (validation_failed, timeout, stuck, rate_limited, etc.).
        repo_root: Repository root path.
        elapsed_seconds: Time elapsed since builder started (if known).
        worktree_had_changes: Whether worktree had uncommitted changes.
        commits_recovered: Number of commits recovered/pushed.
        pr_number: PR number if one was created or updated.
        builder_exit_reason: Why the builder exited (e.g. "rate_limited"), if known.
    """
    paths = LoomPaths(repo_root)
    metrics_dir = paths.metrics_dir
    recovery_file = paths.recovery_events_file

    # Ensure metrics directory exists
    metrics_dir.mkdir(parents=True, exist_ok=True)

    # Build event record
    event: dict[str, Any] = {
        "timestamp": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "issue": issue,
        "recovery_type": recovery_type,
        "reason": reason,
        "elapsed_seconds": elapsed_seconds,
        "worktree_had_changes": worktree_had_changes,
        "commits_recovered": commits_recovered,
        "pr_number": pr_number,
    }
    if builder_exit_reason:
        event["builder_exit_reason"] = builder_exit_reason

    # Append to existing events or create new file
    events: list[dict[str, Any]] = []
    if recovery_file.is_file():
        try:
            with open(recovery_file) as f:
                data = json.load(f)
                if isinstance(data, list):
                    events = data
        except (json.JSONDecodeError, OSError):
            pass

    events.append(event)

    # Write back (keep last 1000 events to prevent unbounded growth)
    events = events[-1000:]
    try:
        with open(recovery_file, "w") as f:
            json.dump(events, f, indent=2)
    except OSError:
        log_warning(f"Failed to write recovery event to {recovery_file}")


def _is_rate_limited_builder_exit(issue: int, repo_root: Path) -> bool:
    """Check if the builder exited due to Claude CLI rate limiting.

    When the Claude CLI hits an Anthropic API rate limit mid-session, it
    prompts the user with ``/rate-limit-options``.  If the builder chose to
    stop, the session log will contain this prompt.  This is distinct from
    GitHub API rate limits (handled elsewhere) and degraded sessions (rate
    limits + Crystallizing loops).

    Returns True if the most recent builder log for the given issue contains
    the rate-limit prompt pattern.
    """
    logs_dir = repo_root / ".loom" / "logs"
    if not logs_dir.is_dir():
        return False

    # Find builder logs for this issue (may include retry suffixes like -a1, -a2)
    pattern = f"loom-builder-issue-{issue}*.log"
    log_files = sorted(logs_dir.glob(pattern), key=lambda p: p.stat().st_mtime)
    if not log_files:
        return False

    # Check the most recent log
    log_path = log_files[-1]
    try:
        content = log_path.read_text(errors="replace")
        return "/rate-limit-options" in content
    except OSError:
        return False


def _build_recovery_pr_body(issue: int, worktree: str, *, rate_limited: bool = False) -> str:
    """Build a descriptive PR body for recovery-created PRs.

    Checks for a pre-written PR body at .loom/pr-body.md in the worktree
    (written by the builder before running tests, while context is fresh).
    Falls back to generating a body from diff stats.

    Args:
        rate_limited: If True, the builder was rate-limited after completing
            work.  Uses less cautionary messaging since the work itself is
            complete — only the PR creation step was interrupted.
    """
    # Check for pre-written PR body from builder (written before tests,
    # while the builder still has full context about the changes).
    # This produces richer PR descriptions than the fallback below.
    pr_body_path = Path(worktree) / ".loom" / "pr-body.md"
    if pr_body_path.is_file():
        pr_body = pr_body_path.read_text().strip()
        close_keywords = (
            f"Closes #{issue}",
            f"Fixes #{issue}",
            f"Resolves #{issue}",
        )
        if not any(kw in pr_body for kw in close_keywords):
            pr_body += f"\n\nCloses #{issue}"
        return pr_body

    lines: list[str] = []

    lines.append(f"Closes #{issue}")
    lines.append("")
    if rate_limited:
        lines.append("> **Note:** Builder was rate-limited after completing work. "
                     "PR created via recovery path.")
    else:
        lines.append("> **Note:** This PR was created automatically via the builder "
                     "recovery path. The builder produced changes but exited before "
                     "creating a PR. Reviewers should examine the diff carefully.")
    lines.append("")

    # Look up the default branch dynamically
    r = subprocess.run(
        ["git", "-C", worktree, "rev-parse", "--abbrev-ref", "origin/HEAD"],
        capture_output=True, text=True, check=False,
    )
    default_branch = r.stdout.strip() if r.returncode == 0 and r.stdout.strip() else "origin/main"

    # Gather diff stats (committed changes vs default branch)
    r = subprocess.run(
        ["git", "-C", worktree, "diff", "--stat", f"{default_branch}...HEAD"],
        capture_output=True, text=True, check=False,
    )
    if r.returncode == 0 and r.stdout.strip():
        lines.append("## Changes")
        lines.append("")
        lines.append("```")
        lines.append(r.stdout.strip())
        lines.append("```")
        lines.append("")

    # Gather shortlog of commits
    r = subprocess.run(
        ["git", "-C", worktree, "log", "--oneline", f"{default_branch}..HEAD"],
        capture_output=True, text=True, check=False,
    )
    if r.returncode == 0 and r.stdout.strip():
        commits = r.stdout.strip().splitlines()
        lines.append("## Commits")
        lines.append("")
        for commit in commits:
            lines.append(f"- `{commit}`")
        lines.append("")

    lines.append("## Test plan")
    lines.append("")
    if rate_limited:
        lines.append("- [ ] Verify changes match issue requirements")
        lines.append("- [ ] Confirm tests pass (builder completed tests before rate limit)")
    else:
        lines.append("- [ ] Review diff carefully (recovery-created PR)")
        lines.append("- [ ] Verify changes match issue requirements")
        lines.append("- [ ] Run tests locally if needed")

    return "\n".join(lines)


def _mark_phase_failed(
    issue: int,
    phase: str,
    reason: str,
    repo_root: Path,
    diagnostics: str = "",
    *,
    failure_label: str | None = None,
    quiet: bool = False,
) -> None:
    """Mark issue with phase-specific failure label and add comment.

    Args:
        issue: Issue number
        phase: Phase name (e.g., "builder", "judge")
        reason: Human-readable failure reason
        repo_root: Repository root path
        diagnostics: Optional diagnostic markdown to append
        failure_label: Specific failure label to apply (e.g., "loom:blocked")
                      If None, uses "loom:blocked" as fallback
        quiet: If True, skip label changes and diagnostic comment.
               Used during intermediate recovery attempts to avoid noisy
               comments that persist even when the shepherd later recovers
               (see issue #2609).
    """
    if quiet:
        return

    # Determine label to apply
    target_label = failure_label or "loom:blocked"

    subprocess.run(
        [
            "gh", "issue", "edit", str(issue),
            "--remove-label", "loom:building",
            "--add-label", target_label,
        ],
        capture_output=True,
        check=False,
        cwd=repo_root,
    )

    body = (
        f"**Phase contract failed**: `{phase}` phase did not produce expected outcome. "
        f"{reason}\n\n"
        "For label state documentation and manual recovery steps, see "
        "[`.claude/commands/shepherd-lifecycle.md`]"
        "(../blob/main/.claude/commands/shepherd-lifecycle.md#label-state-machine)."
    )
    if diagnostics:
        body += f"\n\n{diagnostics}"

    subprocess.run(
        ["gh", "issue", "comment", str(issue), "--body", body],
        capture_output=True,
        check=False,
        cwd=repo_root,
    )


# Keep old name for backwards compatibility
_mark_blocked = _mark_phase_failed


# ---------------------------------------------------------------------------
# Builder diagnostics
# ---------------------------------------------------------------------------

@dataclass
class BuilderDiagnostics:
    """Diagnostic information gathered when builder validation fails."""

    worktree_path: str
    worktree_exists: bool = False
    branch: str = "unknown"
    commits_ahead: str = "?"
    commits_behind: str = "?"
    has_remote_tracking: bool = False
    log_tail: str = ""
    log_path: str = ""
    issue_labels: str = ""
    main_uncommitted: str = ""
    issue: int = 0
    # New fields for enhanced diagnostics
    worktree_mtime: str = ""  # ISO timestamp of worktree last modification
    progress_status: str = ""  # Current phase from progress file
    progress_started_at: str = ""  # When shepherd started (ISO timestamp)
    progress_last_heartbeat: str = ""  # Last heartbeat time (ISO timestamp)
    progress_milestones: list[str] | None = None  # Recent milestone events

    def to_markdown(self) -> str:
        parts: list[str] = ["<details>\n<summary>Diagnostic Information</summary>\n"]

        # Previous attempt timing section
        if self.progress_started_at or self.worktree_mtime:
            parts.append("### Previous Attempt")
            if self.progress_started_at:
                parts.append(f"**Started**: {self.progress_started_at}")
            if self.worktree_mtime:
                parts.append(f"**Worktree last modified**: {self.worktree_mtime}")
            if self.progress_status:
                parts.append(f"**Last phase**: `{self.progress_status}`")
            if self.progress_last_heartbeat:
                parts.append(f"**Last heartbeat**: {self.progress_last_heartbeat}")
            if self.progress_milestones:
                parts.append("**Recent milestones**:")
                for ms in self.progress_milestones[-5:]:  # Show last 5
                    parts.append(f"  - {ms}")
            parts.append("")

        # Worktree state section
        parts.append("### Worktree State")
        if self.worktree_exists:
            parts.append(f"**Worktree**: `{self.worktree_path}` exists")
            parts.append(f"**Branch**: `{self.branch}`")
            parts.append(f"**Commits ahead of main**: {self.commits_ahead}")
            parts.append(f"**Commits behind main**: {self.commits_behind}")
            tracking = "configured" if self.has_remote_tracking else "not configured (branch never pushed)"
            parts.append(f"**Remote tracking**: {tracking}")
        else:
            parts.append(f"**Worktree**: `{self.worktree_path}` does not exist")

        if self.log_tail:
            parts.append(f"\n**Last 15 lines from session log** (`{self.log_path}`):")
            parts.append(f"```\n{self.log_tail}\n```")

        if self.issue_labels:
            parts.append(f"\n**Current issue labels**: {self.issue_labels}")

        if self.main_uncommitted:
            parts.append(
                "\n**\u26a0\ufe0f WARNING: Uncommitted changes detected on main branch**:"
            )
            parts.append(f"```\n{self.main_uncommitted}\n```")
            parts.append(
                "This suggests the builder may have worked directly on main instead of in a worktree.\n"
                "This is a workflow violation - builders MUST work in worktrees."
            )

        # Possible causes
        parts.append("\n### Possible Causes")
        if not self.worktree_exists:
            parts.append("- Worktree was never created (agent may have failed early)")
            parts.append("- Worktree creation script failed")
            parts.append("- **Agent worked on main instead of worktree** (check for uncommitted changes on main)")
        elif self.commits_ahead in ("0", "?"):
            parts.append("- Builder exited without making any commits")
            parts.append("- Builder may have determined issue was invalid or already resolved")
            parts.append("- Builder may have encountered an error during implementation")
            parts.append("- Builder may have timed out before completing work")
            parts.append("- **Agent may have worked on main instead of worktree** (check for uncommitted changes on main)")

        issue = self.issue
        parts.append(f"""
### Recovery Options

**Option A: Clean worktree and retry** (recommended if worktree has no valuable changes)
```bash
# Navigate to repo root first (worktree removal breaks shell CWD)
cd "$(git rev-parse --show-toplevel)"
# Remove stale worktree
git worktree remove .loom/worktrees/issue-{issue} --force 2>/dev/null || true
git branch -D feature/issue-{issue} 2>/dev/null || true
# Reset labels and retry
gh issue edit {issue} --remove-label loom:blocked --add-label loom:issue
./.loom/scripts/loom-shepherd.sh {issue} --merge
```

**Option B: Retry preserving worktree** (if worktree may have partial work)
```bash
gh issue edit {issue} --remove-label loom:blocked --add-label loom:issue
./.loom/scripts/loom-shepherd.sh {issue} --merge
```

**Option C: Complete manually**
1. Create worktree: `./.loom/scripts/worktree.sh {issue}`
2. Navigate: `cd .loom/worktrees/issue-{issue}`
3. Implement the fix, commit changes
4. Push and create PR:
   ```bash
   git push -u origin feature/issue-{issue}
   gh pr create --label loom:review-requested --body "Closes #{issue}"
   ```
5. Remove blocked label: `gh issue edit {issue} --remove-label loom:blocked`

### Investigation Tips
- Check the issue description for clarity - is it actionable?
- Review any curator comments for implementation guidance
- If log file is large, use: `cat {self.log_path} | ./.loom/scripts/strip-ansi.sh | tail -100`

</details>""")
        return "\n".join(parts)


def _gather_builder_diagnostics(
    issue: int,
    worktree: str,
    repo_root: Path,
) -> BuilderDiagnostics:
    """Gather diagnostic info about a failed builder phase."""
    diag = BuilderDiagnostics(worktree_path=worktree, issue=issue)
    wt = Path(worktree)

    if wt.is_dir():
        diag.worktree_exists = True

        # Get worktree modification time
        try:
            mtime = wt.stat().st_mtime
            mtime_dt = datetime.fromtimestamp(mtime, tz=timezone.utc)
            diag.worktree_mtime = mtime_dt.strftime("%Y-%m-%dT%H:%M:%SZ")
        except OSError:
            pass

        r = subprocess.run(
            ["git", "-C", worktree, "rev-parse", "--abbrev-ref", "HEAD"],
            capture_output=True, text=True, check=False,
        )
        diag.branch = r.stdout.strip() if r.returncode == 0 else "unknown"

        # Detect default branch name
        r = subprocess.run(
            ["git", "-C", worktree, "symbolic-ref", "refs/remotes/origin/HEAD"],
            capture_output=True, text=True, check=False,
        )
        main_branch = "main"
        if r.returncode == 0:
            main_branch = r.stdout.strip().replace("refs/remotes/origin/", "")

        r = subprocess.run(
            ["git", "-C", worktree, "rev-list", "--count", f"origin/{main_branch}..HEAD"],
            capture_output=True, text=True, check=False,
        )
        diag.commits_ahead = r.stdout.strip() if r.returncode == 0 else "?"

        r = subprocess.run(
            ["git", "-C", worktree, "rev-list", "--count", f"HEAD..origin/{main_branch}"],
            capture_output=True, text=True, check=False,
        )
        diag.commits_behind = r.stdout.strip() if r.returncode == 0 else "?"

        r = subprocess.run(
            ["git", "-C", worktree, "rev-parse", "--abbrev-ref", "@{upstream}"],
            capture_output=True, text=True, check=False,
        )
        diag.has_remote_tracking = r.returncode == 0

    # Look up progress file for this issue
    progress = find_progress_for_issue(repo_root, issue)
    if progress:
        diag.progress_status = progress.current_phase
        diag.progress_started_at = progress.started_at
        diag.progress_last_heartbeat = progress.last_heartbeat or ""
        # Format milestones as human-readable strings
        if progress.milestones:
            diag.progress_milestones = [
                f"{m.event} at {m.timestamp}" + (f" ({m.data})" if m.data else "")
                for m in progress.milestones
            ]

    # Session log
    session_name = f"loom-builder-issue-{issue}"
    log_patterns = [
        f"/tmp/loom-{session_name}.out",
        str(repo_root / ".loom" / "logs" / f"{session_name}.log"),
    ]
    for path in log_patterns:
        if Path(path).is_file():
            diag.log_path = path
            try:
                lines = Path(path).read_text().splitlines()
                raw_tail = "\n".join(lines[-15:])
                # Strip ANSI escape sequences for human readability
                diag.log_tail = strip_ansi(raw_tail)
            except OSError:
                pass
            break

    # Issue labels
    r = _run_gh(
        ["issue", "view", str(issue), "--json", "labels", "--jq", ".labels[].name"],
        repo_root,
    )
    if r.returncode == 0 and r.stdout.strip():
        diag.issue_labels = r.stdout.strip().replace("\n", ", ")

    # Main branch uncommitted changes
    r = subprocess.run(
        ["git", "-C", str(repo_root), "status", "--porcelain"],
        capture_output=True, text=True, check=False,
    )
    if r.returncode == 0 and r.stdout.strip():
        lines = r.stdout.strip().splitlines()
        diag.main_uncommitted = "\n".join(lines[:10])

    return diag


# ---------------------------------------------------------------------------
# Phase validators
# ---------------------------------------------------------------------------

def validate_curator(
    issue: int,
    repo_root: Path,
    *,
    task_id: str | None = None,
    check_only: bool = False,
    quiet: bool = False,
) -> ValidationResult:
    """Curator contract: issue must have ``loom:curated`` label."""
    r = _run_gh(
        ["issue", "view", str(issue), "--json", "labels", "--jq", ".labels[].name"],
        repo_root,
    )
    if r.returncode != 0:
        return ValidationResult("curator", issue, ValidationStatus.FAILED, "Could not fetch issue labels")

    labels = r.stdout.strip()
    if "loom:curated" in labels.splitlines():
        return ValidationResult("curator", issue, ValidationStatus.SATISFIED, "Issue has loom:curated label")

    if check_only:
        return ValidationResult(
            "curator", issue, ValidationStatus.FAILED,
            "Issue missing loom:curated label (check-only mode, no recovery attempted)",
        )

    # Recovery: apply label
    r2 = subprocess.run(
        ["gh", "issue", "edit", str(issue), "--remove-label", "loom:curating", "--add-label", "loom:curated"],
        capture_output=True, text=True, check=False, cwd=repo_root,
    )
    if r2.returncode == 0:
        _report_milestone("heartbeat", task_id, repo_root, action="recovery: applied loom:curated label")
        return ValidationResult(
            "curator", issue, ValidationStatus.RECOVERED,
            "Applied loom:curated label", "apply_label",
        )

    return ValidationResult("curator", issue, ValidationStatus.FAILED, "Could not apply loom:curated label")


def validate_builder(
    issue: int,
    repo_root: Path,
    *,
    worktree: str | None = None,
    pr_number: int | None = None,
    task_id: str | None = None,
    check_only: bool = False,
    quiet: bool = False,
) -> ValidationResult:
    """Builder contract: PR with ``loom:review-requested`` must exist for the issue.

    Args:
        quiet: If True, attempt recovery but suppress diagnostic comments and
               label changes on failure.  Used by retry loops to avoid posting
               noisy intermediate-failure comments that persist even when the
               shepherd later recovers (see issue #2609).
    """

    # Pre-check: workflow violation detection
    if worktree and not Path(worktree).is_dir():
        r = subprocess.run(
            ["git", "-C", str(repo_root), "status", "--porcelain"],
            capture_output=True, text=True, check=False,
        )
        if r.returncode == 0 and r.stdout.strip():
            log_warning(
                f"WORKFLOW VIOLATION: Builder appears to have worked on main "
                f"instead of in worktree '{worktree}'. "
                f"Uncommitted changes on main: {r.stdout.strip()[:200]}"
            )

    # Check if issue is already closed
    r = _run_gh(
        ["issue", "view", str(issue), "--json", "state", "--jq", ".state"],
        repo_root,
    )
    if r.returncode == 0 and r.stdout.strip() == "CLOSED":
        # Verify a PR actually exists — closing without a PR means the builder
        # abandoned the issue rather than completing it legitimately.
        pr = _find_pr_for_issue(issue, repo_root, pr_number)
        if pr is not None:
            return ValidationResult(
                "builder", issue, ValidationStatus.SATISFIED,
                f"Issue #{issue} is closed with associated PR #{pr[0]}",
            )
        # Also check for merged PRs (closed PRs won't show in open search)
        r2 = _run_gh(
            ["pr", "list", "--head", f"feature/issue-{issue}",
             "--state", "merged", "--json", "number", "--jq", ".[0].number"],
            repo_root,
        )
        merged_pr = _parse_pr_number(r2.stdout)
        if merged_pr is not None:
            return ValidationResult(
                "builder", issue, ValidationStatus.SATISFIED,
                f"Issue #{issue} is closed with merged PR #{merged_pr}",
            )
        # No PR found — builder closed the issue without implementing anything.
        # Reopen the issue to prevent destruction of legitimate feature requests.
        if not check_only:
            _run_gh(["issue", "reopen", str(issue)], repo_root)
            _mark_phase_failed(
                issue, "builder",
                "Issue was closed without an associated PR. "
                "Builder may have abandoned the issue instead of implementing it. "
                "Issue has been automatically reopened.",
                repo_root,
                failure_label="loom:blocked",
                quiet=quiet,
            )
        return ValidationResult(
            "builder", issue, ValidationStatus.FAILED,
            f"Issue #{issue} was closed without a PR — builder abandoned issue (reopened)",
        )

    # Find existing PR
    pr = _find_pr_for_issue(issue, repo_root, pr_number)

    # Checkpoint-aware retry: if no PR found but the builder checkpoint
    # indicates one was just created, GitHub API eventual consistency may
    # not have propagated it yet.  Wait briefly and retry once (#2710).
    if pr is None and worktree:
        wt_path = Path(worktree)
        if wt_path.is_dir():
            from loom_tools.checkpoints import read_checkpoint

            checkpoint = read_checkpoint(wt_path)
            if checkpoint and checkpoint.stage == "pr_created":
                log_warning(
                    f"No PR found for issue #{issue} but checkpoint indicates "
                    f"pr_created — retrying after 2s for API propagation"
                )
                time.sleep(2)
                pr = _find_pr_for_issue(issue, repo_root, pr_number)

    pr_found_by = pr[1] if pr else None
    pr_num = pr[0] if pr else None

    if pr_num is not None:
        # Ensure PR body references the correct issue and remove wrong
        # closing keywords.  Runs for all PRs (not just branch-name
        # discoveries) because the builder may solve the wrong issue.
        if not check_only:
            _ensure_pr_body_references_issue(pr_num, issue, repo_root, task_id)

        # Validate PR title is not generic (anti-pattern detection)
        if not check_only:
            _warn_generic_pr_title(pr_num, issue, repo_root, task_id)
            _recover_minimal_pr_body(pr_num, issue, repo_root, task_id)

        # Check for loom:review-requested label
        r = _run_gh(
            ["pr", "view", str(pr_num), "--json", "labels", "--jq", ".labels[].name"],
            repo_root,
        )
        pr_labels = r.stdout.strip().splitlines() if r.returncode == 0 else []
        if "loom:review-requested" in pr_labels:
            return ValidationResult(
                "builder", issue, ValidationStatus.SATISFIED,
                f"PR #{pr_num} exists with loom:review-requested",
            )

        if check_only:
            return ValidationResult(
                "builder", issue, ValidationStatus.FAILED,
                f"PR #{pr_num} exists but missing loom:review-requested (check-only mode, no recovery attempted)",
            )

        # Recovery: add missing label
        r = subprocess.run(
            ["gh", "pr", "edit", str(pr_num), "--add-label", "loom:review-requested"],
            capture_output=True, text=True, check=False, cwd=repo_root,
        )
        if r.returncode == 0:
            _report_milestone(
                "heartbeat", task_id, repo_root,
                action=f"recovery: added loom:review-requested to PR #{pr_num}",
            )
            _log_recovery_event(
                issue=issue,
                recovery_type="add_label",
                reason="validation_failed",
                repo_root=repo_root,
                pr_number=pr_num,
            )
            return ValidationResult(
                "builder", issue, ValidationStatus.RECOVERED,
                f"Added loom:review-requested to existing PR #{pr_num}", "add_label",
            )

    # No PR found
    if check_only:
        return ValidationResult(
            "builder", issue, ValidationStatus.FAILED,
            f"No PR found for issue #{issue} (check-only mode, no recovery attempted)",
        )

    # No PR found and no worktree - fail with clear message
    if not worktree:
        msg = (
            f"No PR found (searched by branch 'feature/issue-{issue}' and keywords) "
            "and no worktree path provided"
        )
        _mark_phase_failed(
            issue, "builder",
            f"Builder did not create a PR. Searched for: branch 'feature/issue-{issue}' "
            f"and 'Closes/Fixes/Resolves #{issue}' in PR body. No worktree available.",
            repo_root,
            failure_label="loom:blocked",
            quiet=quiet,
        )
        return ValidationResult("builder", issue, ValidationStatus.FAILED, msg)

    wt = Path(worktree)
    if not wt.is_dir():
        diag = _gather_builder_diagnostics(issue, worktree, repo_root)
        _mark_phase_failed(
            issue, "builder",
            "Builder did not create a PR and worktree path does not exist.",
            repo_root, diag.to_markdown(),
            failure_label="loom:blocked",
            quiet=quiet,
        )
        return ValidationResult(
            "builder", issue, ValidationStatus.FAILED,
            f"Worktree path does not exist: {worktree}",
        )

    # Check worktree status
    r = subprocess.run(
        ["git", "-C", worktree, "status", "--porcelain"],
        capture_output=True, text=True, check=False,
    )
    if r.returncode != 0:
        _mark_phase_failed(
            issue, "builder",
            "Builder did not create a PR and worktree is not a valid git directory.",
            repo_root,
            failure_label="loom:blocked",
            quiet=quiet,
        )
        return ValidationResult(
            "builder", issue, ValidationStatus.FAILED, "Could not check worktree status",
        )

    status_output = r.stdout.strip()

    if not status_output:
        # No uncommitted changes — check for unpushed commits
        r = subprocess.run(
            ["git", "-C", worktree, "log", "--oneline", "@{upstream}..HEAD"],
            capture_output=True, text=True, check=False,
        )
        if not (r.stdout.strip() if r.returncode == 0 else ""):
            diag = _gather_builder_diagnostics(issue, worktree, repo_root)
            _mark_phase_failed(
                issue, "builder",
                "Builder did not create a PR. Worktree had no uncommitted or unpushed changes.",
                repo_root, diag.to_markdown(),
                failure_label="loom:blocked",
                quiet=quiet,
            )
            return ValidationResult(
                "builder", issue, ValidationStatus.FAILED,
                "No PR found and no changes in worktree.",
            )

    # Guard: only .no-changes-needed committed (no uncommitted changes)?
    # Mirrors builder.py's `only_marker_committed` detection.
    # When status_output is empty but we reach here, there ARE unpushed commits
    # (we didn't early-return above).  Check if those commits only add the
    # no-changes-needed marker file (".no-changes-needed" — see builder.py).
    if not status_output:
        r = subprocess.run(
            ["git", "-C", worktree, "diff", "--name-only", "@{upstream}..HEAD"],
            capture_output=True, text=True, check=False,
        )
        committed_files = (
            [f.strip() for f in r.stdout.splitlines() if f.strip()]
            if r.returncode == 0 else []
        )
        if committed_files == [".no-changes-needed"]:
            diag = _gather_builder_diagnostics(issue, worktree, repo_root)
            _mark_phase_failed(
                issue, "builder",
                "Builder committed only the .no-changes-needed marker — "
                "treating as 'no changes needed', skipping recovery PR.",
                repo_root, diag.to_markdown(),
                failure_label="loom:blocked",
                quiet=quiet,
            )
            return ValidationResult(
                "builder", issue, ValidationStatus.FAILED,
                "No substantive changes to recover (only .no-changes-needed committed).",
            )

    # Guard: only marker files?
    if status_output:
        substantive = [
            line for line in status_output.splitlines()
            if not line.rstrip().endswith(".loom-in-use")
            and ".loom/" not in line
        ]
        if not substantive:
            diag = _gather_builder_diagnostics(issue, worktree, repo_root)
            _mark_phase_failed(
                issue, "builder",
                "Builder did not produce substantive changes. "
                "Only marker/infrastructure files were found in the worktree.",
                repo_root, diag.to_markdown(),
                failure_label="loom:blocked",
                quiet=quiet,
            )
            return ValidationResult(
                "builder", issue, ValidationStatus.FAILED,
                "No substantive changes to recover (only marker files found).",
            )

    # Attempt mechanical recovery: stage, commit, push, create PR.
    # The builder produced substantive changes but exited before completing
    # the git/PR workflow.  We can finish the mechanical steps directly.
    branch = f"feature/issue-{issue}"

    # Step 1: Stage and commit if there are uncommitted changes
    if status_output:
        # Extract meaningful file paths from porcelain output
        files_to_stage = []
        for line in substantive:
            path = parse_porcelain_path(line)
            if path:
                files_to_stage.append(path)

        if files_to_stage:
            r = subprocess.run(
                ["git", "-C", worktree, "add", "--"] + files_to_stage,
                capture_output=True, text=True, check=False,
            )
            if r.returncode != 0:
                diag = _gather_builder_diagnostics(issue, worktree, repo_root)
                _mark_phase_failed(
                    issue, "builder",
                    f"Recovery failed: git add failed: {r.stderr.strip()[:200]}",
                    repo_root, diag.to_markdown(),
                    failure_label="loom:blocked",
                    quiet=quiet,
                )
                return ValidationResult(
                    "builder", issue, ValidationStatus.FAILED,
                    "Recovery failed: could not stage changes.",
                )

            commit_msg = derive_commit_message(
                issue, worktree, repo_root, staged_files=files_to_stage,
            )
            r = subprocess.run(
                ["git", "-C", worktree, "commit", "-m", commit_msg],
                capture_output=True, text=True, check=False,
            )
            if r.returncode != 0:
                diag = _gather_builder_diagnostics(issue, worktree, repo_root)
                _mark_phase_failed(
                    issue, "builder",
                    f"Recovery failed: git commit failed: {r.stderr.strip()[:200]}",
                    repo_root, diag.to_markdown(),
                    failure_label="loom:blocked",
                    quiet=quiet,
                )
                return ValidationResult(
                    "builder", issue, ValidationStatus.FAILED,
                    "Recovery failed: could not commit changes.",
                )

    # Step 2: Push the branch
    r = subprocess.run(
        ["git", "-C", worktree, "push", "-u", "origin", branch],
        capture_output=True, text=True, check=False,
    )
    if r.returncode != 0:
        diag = _gather_builder_diagnostics(issue, worktree, repo_root)
        _mark_phase_failed(
            issue, "builder",
            f"Recovery failed: git push failed: {r.stderr.strip()[:200]}",
            repo_root, diag.to_markdown(),
            failure_label="loom:blocked",
            quiet=quiet,
        )
        return ValidationResult(
            "builder", issue, ValidationStatus.FAILED,
            "Recovery failed: could not push branch.",
        )

    # Step 3: Create PR
    # Detect whether the builder was rate-limited (affects PR messaging)
    rate_limited = _is_rate_limited_builder_exit(issue, repo_root)

    # Fetch issue title and generate a conventional PR title
    from loom_tools.common.paths import NamingConventions

    r_title = _run_gh(
        ["issue", "view", str(issue), "--json", "title", "--jq", ".title"],
        repo_root,
    )
    raw_title = r_title.stdout.strip() if r_title.returncode == 0 and r_title.stdout.strip() else None
    pr_title = NamingConventions.pr_title(raw_title, issue)

    pr_body = _build_recovery_pr_body(issue, worktree, rate_limited=rate_limited)

    r = subprocess.run(
        [
            "gh", "pr", "create",
            "--head", branch,
            "--title", pr_title,
            "--label", "loom:review-requested",
            "--body", pr_body,
        ],
        cwd=repo_root,
        capture_output=True, text=True, check=False,
    )
    if r.returncode != 0:
        diag = _gather_builder_diagnostics(issue, worktree, repo_root)
        _mark_phase_failed(
            issue, "builder",
            f"Recovery failed: gh pr create failed: {r.stderr.strip()[:200]}",
            repo_root, diag.to_markdown(),
            failure_label="loom:blocked",
            quiet=quiet,
        )
        return ValidationResult(
            "builder", issue, ValidationStatus.FAILED,
            "Recovery failed: could not create PR.",
        )

    # Extract PR number from output (format: "https://github.com/.../pull/123")
    pr_url = r.stdout.strip()
    recovered_pr = _parse_pr_number(pr_url.split("/")[-1] if "/" in pr_url else pr_url)

    recovery_reason = "rate_limited" if rate_limited else "validation_failed"
    _report_milestone(
        "heartbeat", task_id, repo_root,
        action=f"recovery: created PR from {'rate-limited' if rate_limited else 'uncommitted'} worktree changes for issue #{issue}",
    )
    _log_recovery_event(
        issue=issue,
        recovery_type="commit_and_pr",
        reason=recovery_reason,
        repo_root=repo_root,
        worktree_had_changes=bool(status_output),
        pr_number=recovered_pr,
        builder_exit_reason="rate_limited" if rate_limited else None,
    )
    return ValidationResult(
        "builder", issue, ValidationStatus.RECOVERED,
        f"Recovered: staged, committed, pushed, and created PR from worktree changes"
        f"{' (builder was rate-limited)' if rate_limited else ''}",
        "commit_and_pr",
    )


def validate_judge(
    issue: int,
    repo_root: Path,
    *,
    pr_number: int | None = None,
    task_id: str | None = None,
    check_only: bool = False,
    quiet: bool = False,
) -> ValidationResult:
    """Judge contract: PR must have ``loom:pr`` or ``loom:changes-requested``."""
    if pr_number is None:
        return ValidationResult(
            "judge", issue, ValidationStatus.FAILED,
            "PR number required for judge phase validation",
        )

    r = _run_gh(
        ["pr", "view", str(pr_number), "--json", "labels", "--jq", ".labels[].name"],
        repo_root,
    )
    if r.returncode != 0:
        return ValidationResult("judge", issue, ValidationStatus.FAILED, "Could not fetch PR labels")

    labels = r.stdout.strip().splitlines()

    if "loom:pr" in labels:
        return ValidationResult(
            "judge", issue, ValidationStatus.SATISFIED,
            f"PR #{pr_number} approved (loom:pr)",
        )
    if "loom:changes-requested" in labels:
        return ValidationResult(
            "judge", issue, ValidationStatus.SATISFIED,
            f"PR #{pr_number} has changes requested (loom:changes-requested)",
        )

    # Issue #1998: Check for intermediate state after Doctor fixes
    # When Doctor applies fixes, it removes loom:changes-requested and adds
    # loom:review-requested. If judge worker just ran but hasn't applied its
    # outcome label yet, we're in an expected intermediate state.
    if "loom:review-requested" in labels:
        msg = (
            f"PR #{pr_number} has loom:review-requested (Doctor applied fixes) "
            "but judge did not produce outcome label yet"
        )
    else:
        msg = f"Judge did not produce loom:pr or loom:changes-requested on PR #{pr_number}"

    if not check_only:
        _mark_phase_failed(
            issue, "judge",
            f"Judge phase did not produce a review decision on PR #{pr_number}.",
            repo_root,
            failure_label="loom:blocked",
            quiet=quiet,
        )

    return ValidationResult("judge", issue, ValidationStatus.FAILED, msg)


def validate_doctor(
    issue: int,
    repo_root: Path,
    *,
    pr_number: int | None = None,
    task_id: str | None = None,
    check_only: bool = False,
    quiet: bool = False,
) -> ValidationResult:
    """Doctor contract: PR must have ``loom:review-requested``."""
    if pr_number is None:
        return ValidationResult(
            "doctor", issue, ValidationStatus.FAILED,
            "PR number required for doctor phase validation",
        )

    r = _run_gh(
        ["pr", "view", str(pr_number), "--json", "labels", "--jq", ".labels[].name"],
        repo_root,
    )
    if r.returncode != 0:
        return ValidationResult("doctor", issue, ValidationStatus.FAILED, "Could not fetch PR labels")

    labels = r.stdout.strip().splitlines()
    if "loom:review-requested" in labels:
        return ValidationResult(
            "doctor", issue, ValidationStatus.SATISFIED,
            f"PR #{pr_number} has loom:review-requested",
        )

    msg = f"Doctor did not re-request review on PR #{pr_number}"
    if not check_only:
        _mark_phase_failed(
            issue, "doctor",
            f"Doctor phase did not apply loom:review-requested to PR #{pr_number}.",
            repo_root,
            failure_label="loom:blocked",
            quiet=quiet,
        )

    return ValidationResult("doctor", issue, ValidationStatus.FAILED, msg)


# ---------------------------------------------------------------------------
# Internal: PR search helpers
# ---------------------------------------------------------------------------

def _find_pr_for_issue(
    issue: int,
    repo_root: Path,
    cached_pr: int | None = None,
) -> tuple[int, str] | None:
    """Find an open PR for *issue*.  Returns ``(pr_number, found_by)`` or None."""
    if cached_pr is not None:
        r = _run_gh(
            ["pr", "view", str(cached_pr), "--json", "state", "--jq", ".state"],
            repo_root,
        )
        if r.returncode == 0 and r.stdout.strip() == "OPEN":
            return (cached_pr, "caller_cached")

    # Method 1: branch name
    r = _run_gh(
        ["pr", "list", "--head", f"feature/issue-{issue}",
         "--state", "open", "--json", "number", "--jq", ".[0].number"],
        repo_root,
    )
    pr = _parse_pr_number(r.stdout)
    if pr is not None:
        return (pr, "branch_name")

    # Methods 2-4: body search
    for keyword, found_by in [("Closes", "closes_keyword"), ("Fixes", "fixes_keyword"), ("Resolves", "resolves_keyword")]:
        r = _run_gh(
            ["pr", "list", "--search", f"{keyword} #{issue}",
             "--state", "open", "--json", "number", "--jq", ".[0].number"],
            repo_root,
        )
        pr = _parse_pr_number(r.stdout)
        if pr is not None:
            return (pr, found_by)

    return None


def _parse_pr_number(output: str) -> int | None:
    """Parse a PR number from ``gh`` output, returning None for empty/null."""
    text = output.strip()
    if not text or text == "null":
        return None
    try:
        return int(text)
    except ValueError:
        return None


def _ensure_pr_body_references_issue(
    pr: int,
    issue: int,
    repo_root: Path,
    task_id: str | None,
) -> None:
    """Ensure the PR body contains a ``Closes #N`` reference.

    Also detects and removes closing keywords that reference wrong issues.
    """
    r = _run_gh(
        ["pr", "view", str(pr), "--json", "body", "--jq", ".body"],
        repo_root,
    )
    body = r.stdout.strip() if r.returncode == 0 else ""

    # Detect wrong-issue references (Closes/Fixes/Resolves #N where N != issue)
    wrong_refs = re.findall(
        r"(Closes|Fixes|Resolves)\s+#(\d+)", body, re.IGNORECASE,
    )
    wrong_issues = [
        (kw, int(num)) for kw, num in wrong_refs if int(num) != issue
    ]

    has_correct_ref = bool(
        re.search(rf"(Closes|Fixes|Resolves)\s+#{issue}\b", body),
    )
    needs_edit = False

    if wrong_issues:
        for kw, num in wrong_issues:
            body = re.sub(
                rf"{kw}\s+#{num}\b",
                f"~~{kw} #{num}~~ (removed: wrong issue)",
                body,
                count=1,
                flags=re.IGNORECASE,
            )
        wrong_list = ", ".join(f"#{num}" for _, num in wrong_issues)
        _report_milestone(
            "heartbeat", task_id, repo_root,
            action=(
                f"warning: PR #{pr} referenced wrong issue(s) {wrong_list} "
                f"instead of #{issue} -- removed closing keywords"
            ),
        )
        needs_edit = True

    if not has_correct_ref:
        body = (
            f"Closes #{issue}"
            if not body or body == "null"
            else f"{body}\n\nCloses #{issue}"
        )
        needs_edit = True

    if needs_edit:
        r = subprocess.run(
            ["gh", "pr", "edit", str(pr), "--body", body],
            capture_output=True, text=True, check=False, cwd=repo_root,
        )
        if r.returncode == 0:
            action = f"recovery: ensured PR #{pr} body references #{issue}"
            if wrong_issues:
                wrong_list = ", ".join(
                    f"#{num}" for _, num in wrong_issues
                )
                action += f" (removed wrong refs: {wrong_list})"
            _report_milestone(
                "heartbeat", task_id, repo_root,
                action=action,
            )


# Generic PR title patterns that indicate the builder didn't derive a
# meaningful title from its diff.  Each regex is matched case-insensitively.
_GENERIC_TITLE_PATTERNS: list[re.Pattern[str]] = [
    re.compile(r"implement\s+changes?\s+for\s+issue", re.IGNORECASE),
    re.compile(r"address\s+issue\s+#?\d+", re.IGNORECASE),
    re.compile(r"implement\s+feature\s+from\s+issue", re.IGNORECASE),
    re.compile(r"^issue\s+#?\d+\s*$", re.IGNORECASE),
]


def _warn_generic_pr_title(
    pr: int,
    issue: int,
    repo_root: Path,
    task_id: str | None,
) -> None:
    """Log a warning if the PR title matches a known generic anti-pattern.

    This is a *warning* (logged via milestone), not a hard failure, because
    the builder already created the PR and blocking validation here would
    disrupt the shepherd pipeline.  The warning surfaces in logs and
    milestones so the issue can be tracked and the builder role docs
    improved.
    """
    r = _run_gh(
        ["pr", "view", str(pr), "--json", "title", "--jq", ".title"],
        repo_root,
    )
    if r.returncode != 0:
        return

    title = r.stdout.strip()
    if not title:
        return

    for pattern in _GENERIC_TITLE_PATTERNS:
        if pattern.search(title):
            _report_milestone(
                "heartbeat",
                task_id,
                repo_root,
                action=(
                    f"warning: PR #{pr} has generic title matching "
                    f"anti-pattern /{pattern.pattern}/: {title!r}"
                ),
            )
            return


def _recover_minimal_pr_body(
    pr: int,
    issue: int,
    repo_root: Path,
    task_id: str | None,
) -> None:
    """Enrich a PR body that contains no meaningful summary.

    Detects "minimal" bodies where the only content (after stripping
    ``Closes/Fixes/Resolves #N`` references) is less than 80 characters and
    there is no ``## Summary`` section.  When detected, fetches the file
    change list from the PR and prepends an auto-generated summary.
    """
    r = _run_gh(
        ["pr", "view", str(pr), "--json", "body", "--jq", ".body"],
        repo_root,
        use_cache=False,
    )
    if r.returncode != 0:
        return

    body = r.stdout.strip() if r.stdout.strip() and r.stdout.strip() != "null" else ""

    # Already has a summary section — nothing to do.
    if re.search(r"^## Summary", body, re.MULTILINE):
        return

    # Strip close-keyword lines to measure remaining content.
    stripped = re.sub(
        r"(?mi)^(Closes|Fixes|Resolves)\s+#\d+\s*$", "", body,
    ).strip()

    if len(stripped) >= 80:
        return

    # Body is minimal — fetch file list to build an enriched body.
    r = _run_gh(
        ["pr", "view", str(pr), "--json", "files",
         "--jq", '.files[] | "\\(.path) (+\\(.additions)/-\\(.deletions))"'],
        repo_root,
        use_cache=False,
    )
    file_lines: list[str] = []
    if r.returncode == 0 and r.stdout.strip():
        for line in r.stdout.strip().splitlines()[:25]:
            file_lines.append(f"- `{line}`")

    new_parts: list[str] = [
        "## Summary",
        "",
        "> **Note:** This summary was auto-generated because the builder "
        "created a PR with a minimal body.",
        "",
    ]

    if file_lines:
        new_parts.append("## Changes")
        new_parts.append("")
        new_parts.extend(file_lines)
        new_parts.append("")

    # Preserve original body at the end.
    if body:
        new_parts.append(body)

    new_body = "\n".join(new_parts)

    r = subprocess.run(
        ["gh", "pr", "edit", str(pr), "--body", new_body],
        capture_output=True, text=True, check=False, cwd=repo_root,
    )
    if r.returncode == 0:
        _report_milestone(
            "heartbeat", task_id, repo_root,
            action=f"recovery: enriched minimal PR #{pr} body for issue #{issue}",
        )
        _log_recovery_event(
            issue=issue,
            recovery_type="enrich_pr_body",
            reason="minimal_pr_body",
            repo_root=repo_root,
            pr_number=pr,
        )


# ---------------------------------------------------------------------------
# Main dispatch
# ---------------------------------------------------------------------------

_VALIDATORS = {
    "curator": validate_curator,
    "builder": validate_builder,
    "judge": validate_judge,
    "doctor": validate_doctor,
}

VALID_PHASES = tuple(_VALIDATORS.keys())


def validate_phase(
    phase: str,
    issue: int,
    repo_root: Path | None = None,
    *,
    worktree: str | None = None,
    pr_number: int | None = None,
    task_id: str | None = None,
    check_only: bool = False,
    quiet: bool = False,
) -> ValidationResult:
    """Validate a shepherd phase contract.

    This is the main Python API — importable from other modules.

    Args:
        quiet: If True, attempt recovery but suppress diagnostic comments and
               label changes on failure.  Used by retry loops to avoid posting
               noisy intermediate-failure comments (see issue #2609).
    """
    if phase not in _VALIDATORS:
        return ValidationResult(
            phase, issue, ValidationStatus.FAILED,
            f"Invalid phase '{phase}'. Must be one of: {', '.join(VALID_PHASES)}",
        )

    if repo_root is None:
        repo_root = _find_repo_root()

    kwargs: dict[str, Any] = {
        "task_id": task_id,
        "check_only": check_only,
        "quiet": quiet,
    }

    if phase == "builder":
        kwargs["worktree"] = worktree
        kwargs["pr_number"] = pr_number
    elif phase in ("judge", "doctor"):
        kwargs["pr_number"] = pr_number

    return _VALIDATORS[phase](issue, repo_root, **kwargs)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def _parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="loom-validate-phase",
        description="Validate shepherd phase contracts and attempt recovery",
    )
    parser.add_argument("phase", choices=VALID_PHASES, help="Phase to validate")
    parser.add_argument("issue", type=int, help="Issue number")
    parser.add_argument("--worktree", help="Worktree path (required for builder recovery)")
    parser.add_argument("--pr", type=int, dest="pr_number", help="PR number (for judge/doctor)")
    parser.add_argument("--task-id", help="Shepherd task ID for milestone reporting")
    parser.add_argument("--json", action="store_true", dest="json_output", help="Output as JSON")
    parser.add_argument(
        "--check-only", action="store_true",
        help="Only check contract status, skip all side effects",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> None:
    args = _parse_args(argv)

    result = validate_phase(
        phase=args.phase,
        issue=args.issue,
        worktree=args.worktree,
        pr_number=args.pr_number,
        task_id=args.task_id,
        check_only=args.check_only,
    )

    if args.json_output:
        print(result.to_json())
    else:
        status = result.status.value
        if status == "satisfied":
            prefix = "\u2713"
        elif status == "recovered":
            prefix = "\u27f3"
        else:
            prefix = "\u2717"
        print(f"{prefix} {result.phase} phase contract {status}: {result.message}")

    sys.exit(0 if result.satisfied else 1)


if __name__ == "__main__":
    main()
