"""Preflight phase: check baseline health before Builder.

Consults .loom/baseline-health.json (maintained by the Auditor) to
determine whether main branch tests are healthy.  If they are known
to be failing and the cache is still fresh, the shepherd can skip the
builder phase and report the issue as blocked rather than wasting time
creating a worktree, running baseline tests independently, and
discovering the same breakage every other shepherd already found.

The check is intentionally **non-blocking** on errors:
- Missing file -> proceed (cold start, Auditor hasn't run yet)
- Corrupted file -> proceed (log warning)
- Stale cache -> proceed (Auditor will refresh on next run)

Only a *fresh* "failing" status prevents the builder from starting.
"""

from __future__ import annotations

import subprocess
from datetime import datetime, timezone

from loom_tools.common.logging import log_info, log_warning
from loom_tools.common.state import read_baseline_health
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.phases.base import BasePhase, PhaseResult


class PreflightPhase(BasePhase):
    """Pre-flight baseline health check.

    Reads .loom/baseline-health.json and determines whether the
    shepherd should proceed to the builder phase or wait for the
    Auditor to resolve main-branch failures.
    """

    phase_name = "preflight"

    def should_skip(self, ctx: ShepherdContext) -> tuple[bool, str]:
        """Check if the preflight phase should be skipped.

        Skip if:
        - --from argument skips past builder (preflight is only relevant
          for builder protection)
        """
        from loom_tools.shepherd.config import Phase

        if ctx.config.should_skip_phase(Phase.BUILDER):
            return True, "builder phase skipped"
        return False, ""

    def run(self, ctx: ShepherdContext) -> PhaseResult:
        """Check baseline health status.

        Returns:
            SUCCESS if baseline is healthy or unknown (proceed to builder).
            FAILED if baseline is failing and cache is fresh (block builder).
        """
        try:
            health = read_baseline_health(ctx.repo_root)
        except (TypeError, OSError) as exc:
            log_warning(f"Could not read baseline health: {exc}")
            return self.success("read error", data={"baseline_status": "unknown"})

        # Unknown status: no cache exists or Auditor hasn't run.
        if health.status == "unknown":
            log_info("No baseline health cache found, proceeding to builder")
            return self.success("no cache", data={"baseline_status": "unknown"})

        # Healthy: proceed immediately.
        if health.status == "healthy":
            log_info(
                f"Baseline healthy (checked {health.checked_at}, "
                f"commit {health.main_commit[:8] if health.main_commit else 'unknown'})"
            )
            return self.success("baseline healthy", data={"baseline_status": "healthy"})

        # Failing: check if cache is still fresh.
        if health.status == "failing":
            if _is_cache_stale(health.checked_at, health.cache_ttl_minutes):
                log_warning(
                    f"Baseline health cache is stale (checked {health.checked_at}, "
                    f"TTL {health.cache_ttl_minutes}min), proceeding to builder"
                )
                return self.success(
                    "stale cache",
                    data={"baseline_status": "failing", "cache_stale": True},
                )

            # Check if current main HEAD matches the cached commit.
            # If main has moved forward, the cache might be outdated.
            current_commit = _get_main_head(ctx)
            if current_commit and health.main_commit and current_commit != health.main_commit:
                log_warning(
                    f"Main has advanced since baseline check "
                    f"(cached: {health.main_commit[:8]}, "
                    f"current: {current_commit[:8]}), proceeding to builder"
                )
                return self.success(
                    "commit mismatch",
                    data={
                        "baseline_status": "failing",
                        "commit_mismatch": True,
                        "cached_commit": health.main_commit,
                        "current_commit": current_commit,
                    },
                )

            # Fresh, matching cache says failing -> block.
            failing_names = [t.name for t in health.failing_tests if t.name]
            issue_ref = f", tracked in {health.issue_tracking}" if health.issue_tracking else ""
            test_info = f" ({', '.join(failing_names)})" if failing_names else ""

            log_warning(
                f"Main branch tests failing{test_info}{issue_ref}. "
                "Blocking builder to avoid redundant failure."
            )

            return self.failed(
                f"baseline tests failing on main{issue_ref}",
                data={
                    "baseline_status": "failing",
                    "failing_tests": failing_names,
                    "issue_tracking": health.issue_tracking,
                    "checked_at": health.checked_at,
                    "main_commit": health.main_commit,
                },
            )

        # Unexpected status value: treat as unknown and proceed.
        log_warning(f"Unexpected baseline health status: {health.status!r}, proceeding")
        return self.success(
            f"unexpected status: {health.status}",
            data={"baseline_status": health.status},
        )

    def validate(self, ctx: ShepherdContext) -> bool:
        """Validate phase contract (always True for preflight)."""
        return True


def _is_cache_stale(checked_at: str, ttl_minutes: int) -> bool:
    """Return True if the cache is older than *ttl_minutes*.

    Returns True (stale) if the timestamp cannot be parsed.
    """
    if not checked_at:
        return True
    try:
        checked = datetime.fromisoformat(checked_at)
        # Ensure timezone-aware comparison
        if checked.tzinfo is None:
            checked = checked.replace(tzinfo=timezone.utc)
        now = datetime.now(timezone.utc)
        age_minutes = (now - checked).total_seconds() / 60
        return age_minutes > ttl_minutes
    except (ValueError, TypeError):
        return True


def _get_main_head(ctx: ShepherdContext) -> str | None:
    """Get the current HEAD commit of the main branch."""
    try:
        result = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=ctx.repo_root,
            text=True,
            capture_output=True,
            check=False,
        )
        if result.returncode == 0:
            return result.stdout.strip()
    except OSError:
        pass
    return None
