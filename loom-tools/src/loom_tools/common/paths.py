"""Centralized path constants and naming conventions for .loom directory structure.

This module provides a single source of truth for:
- Directory paths within .loom/
- File paths for state files (spawn-loop-state.json, health-metrics.json, etc.)
- Naming conventions for branches and worktrees
"""

from __future__ import annotations

import os
import re
import warnings
from pathlib import Path


def _namespaced(override: str, repo_root: Path) -> Path:
    """Namespace an absolute override root by the repo basename.

    Mirrors bash ``${override%/}/<repo-basename>``: strip trailing slash(es)
    from the override, then join the repo's basename. If ``repo_root`` has no
    final component (e.g. ``/``), fall back to the trimmed override alone.

    Args:
        override: An absolute override path (validated by the caller).
        repo_root: The repository root whose basename provides the namespace.

    Returns:
        ``<override-without-trailing-slash>/<repo-basename>``.
    """
    trimmed = override.rstrip("/")
    base = Path(trimmed)
    name = repo_root.name
    if name:
        return base / name
    return base


def _resolve_worktree_root(repo_root: Path) -> Path:
    """Resolve the worktree base directory, honoring overrides.

    Resolution precedence (first match wins), mirroring
    ``defaults/scripts/lib/worktree-root.sh`` (``loom_worktree_root``) and
    ``loom-daemon/src/worktree_root.rs`` (``worktree_root``):

    1. ``LOOM_WORKTREE_ROOT`` env var          — highest priority
    2. ``.loom/config.json`` -> ``worktree.root`` — soft-fail JSON read
    3. ``${repo_root}/.loom/worktrees``         — default, UNCHANGED behavior

    When an override (env var or config key) is set, the returned path is
    namespaced by repo basename so multiple workspaces can share one external
    volume without colliding (``${override}/<repo-basename>``).

    A relative override (env var or config key) is rejected with a warning and
    the function falls back to the default — matching the bash stderr warning
    and Rust ``log::warn!`` behavior, not a hard error. An external worktree
    root must be absolute so cleanup/GC comparison sites (which compare
    absolute paths) match.

    With neither override set, the return value is byte-for-byte identical to
    the historical hardcoded ``${repo_root}/.loom/worktrees`` path.

    This helper never creates directories; callers ``mkdir -p`` as needed.

    Args:
        repo_root: The absolute repository root path.

    Returns:
        The absolute worktree base directory.
    """
    default = repo_root / LoomPaths.LOOM_DIR / LoomPaths.WORKTREES_DIR

    # 1. Env var override — highest priority.
    env_root = os.environ.get("LOOM_WORKTREE_ROOT")
    if env_root:
        if Path(env_root).is_absolute():
            return _namespaced(env_root, repo_root)
        warnings.warn(
            "LOOM_WORKTREE_ROOT must be an absolute path "
            f"(got: '{env_root}'); falling back to default",
            stacklevel=2,
        )
        return default

    # 2. Config key override — .loom/config.json -> worktree.root.
    cfg_root = _read_config_worktree_root(repo_root)
    if cfg_root:
        if Path(cfg_root).is_absolute():
            return _namespaced(cfg_root, repo_root)
        warnings.warn(
            "worktree.root in .loom/config.json must be an absolute path "
            f"(got: '{cfg_root}'); falling back to default",
            stacklevel=2,
        )
        return default

    # 3. Default — unchanged historical behavior.
    return default


def _read_config_worktree_root(repo_root: Path) -> str | None:
    """Read ``.loom/config.json`` -> ``worktree.root``, soft-failing to ``None``.

    Missing file, parse error, missing key, or a non-string/empty value all
    resolve to ``None`` (never a hard error), mirroring the soft-fail read in
    the bash/Rust ports.

    Args:
        repo_root: The absolute repository root path.

    Returns:
        The configured worktree root string, or ``None`` when unset/invalid.
    """
    # Imported lazily to avoid a module-level import cycle
    # (common.state imports from common.paths).
    from loom_tools.common.state import read_json_file

    config_path = repo_root / LoomPaths.LOOM_DIR / LoomPaths.CONFIG_FILE
    data = read_json_file(config_path, default={})
    if not isinstance(data, dict):
        return None
    worktree = data.get("worktree")
    if not isinstance(worktree, dict):
        return None
    root = worktree.get("root")
    if isinstance(root, str) and root:
        return root
    return None


def is_worktree_path(path: Path, repo_root: Path) -> bool:
    """Whether ``path`` is a Loom-managed worktree eligible for GC.

    Two-way match mirroring ``loom-daemon/src/worktree_root.rs::is_worktree_path``
    and ``defaults/scripts/agent-destroy.sh``: a path counts if it lives under
    the resolved worktree root for ``repo_root`` (override-aware) OR contains the
    historical ``.loom/worktrees`` substring. The substring branch preserves
    default-path detection unchanged and covers mixed setups where an override
    was configured after worktrees were already created under the default base.

    Args:
        path: The candidate worktree path (should be resolved by the caller).
        repo_root: The absolute repository root path.

    Returns:
        True if ``path`` is under the resolved root or matches the legacy
        substring, False otherwise.
    """
    root = LoomPaths(repo_root).worktrees_dir
    try:
        under_root = path == root or root in path.parents
    except Exception:
        under_root = False
    return under_root or ".loom/worktrees" in str(path)


class LoomPaths:
    """Centralized path constants for .loom directory structure.

    Usage:
        paths = LoomPaths(repo_root)
        print(paths.spawn_loop_state_file)  # repo_root/.loom/spawn-loop-state.json
        print(paths.worktree_path(42))       # repo_root/.loom/worktrees/issue-42
    """

    # Directory names (relative to .loom/)
    LOOM_DIR = ".loom"
    SCRIPTS_DIR = "scripts"
    WORKTREES_DIR = "worktrees"
    LOGS_DIR = "logs"
    DOCS_DIR = "docs"
    ROLES_DIR = "roles"
    DIAGNOSTICS_DIR = "diagnostics"
    METRICS_DIR = "metrics"
    CLAUDE_CONFIG_DIR = "claude-config"

    # State file names
    SPAWN_LOOP_STATE_FILE = "spawn-loop-state.json"
    HEALTH_METRICS_FILE = "health-metrics.json"
    ALERTS_FILE = "alerts.json"
    STUCK_HISTORY_FILE = "stuck-history.json"
    CONFIG_FILE = "config.json"
    STOP_DAEMON_FILE = "stop-daemon"
    STOP_SHEPHERDS_FILE = "stop-shepherds"
    RECOVERY_EVENTS_FILE = "recovery-events.json"
    BASELINE_HEALTH_FILE = "baseline-health.json"
    ISSUE_FAILURES_FILE = "issue-failures.json"
    USAGE_CACHE_FILE = "usage-cache.json"

    def __init__(self, repo_root: Path) -> None:
        """Initialize with repository root path.

        Args:
            repo_root: The root path of the repository.
        """
        self.repo_root = repo_root

    @property
    def loom_dir(self) -> Path:
        """Path to .loom directory."""
        return self.repo_root / self.LOOM_DIR

    @property
    def scripts_dir(self) -> Path:
        """Path to .loom/scripts directory."""
        return self.loom_dir / self.SCRIPTS_DIR

    @property
    def worktrees_dir(self) -> Path:
        """Path to the worktree base directory.

        Honors an overridden worktree root (``LOOM_WORKTREE_ROOT`` env var, then
        ``.loom/config.json`` -> ``worktree.root``), namespaced by repo basename,
        falling back to ``.loom/worktrees`` when no override is set. See
        :func:`_resolve_worktree_root` for the full precedence chain. Resolved at
        call time (matching the bash/Rust ports), so env/config changes between
        ``LoomPaths()`` construction and property access are always picked up.
        """
        return _resolve_worktree_root(self.repo_root)

    @property
    def logs_dir(self) -> Path:
        """Path to .loom/logs directory."""
        return self.loom_dir / self.LOGS_DIR

    @property
    def docs_dir(self) -> Path:
        """Path to .loom/docs directory."""
        return self.loom_dir / self.DOCS_DIR

    @property
    def roles_dir(self) -> Path:
        """Path to .loom/roles directory."""
        return self.loom_dir / self.ROLES_DIR

    @property
    def diagnostics_dir(self) -> Path:
        """Path to .loom/diagnostics directory."""
        return self.loom_dir / self.DIAGNOSTICS_DIR

    @property
    def metrics_dir(self) -> Path:
        """Path to .loom/metrics directory."""
        return self.loom_dir / self.METRICS_DIR

    @property
    def claude_config_base_dir(self) -> Path:
        """Path to .loom/claude-config/ directory."""
        return self.loom_dir / self.CLAUDE_CONFIG_DIR

    def agent_claude_config_dir(self, agent_name: str) -> Path:
        """Path to per-agent Claude config directory.

        Args:
            agent_name: The agent name (e.g., "builder-1", "shepherd-2").

        Returns:
            Path to .loom/claude-config/{agent_name}/
        """
        return self.claude_config_base_dir / agent_name

    @property
    def spawn_loop_state_file(self) -> Path:
        """Path to .loom/spawn-loop-state.json (Phase 1, #3374)."""
        return self.loom_dir / self.SPAWN_LOOP_STATE_FILE

    @property
    def health_metrics_file(self) -> Path:
        """Path to .loom/health-metrics.json."""
        return self.loom_dir / self.HEALTH_METRICS_FILE

    @property
    def alerts_file(self) -> Path:
        """Path to .loom/alerts.json."""
        return self.loom_dir / self.ALERTS_FILE

    @property
    def stuck_history_file(self) -> Path:
        """Path to .loom/stuck-history.json."""
        return self.loom_dir / self.STUCK_HISTORY_FILE

    @property
    def config_file(self) -> Path:
        """Path to .loom/config.json."""
        return self.loom_dir / self.CONFIG_FILE

    @property
    def stop_daemon_file(self) -> Path:
        """Path to .loom/stop-daemon (shutdown signal)."""
        return self.loom_dir / self.STOP_DAEMON_FILE

    @property
    def stop_shepherds_file(self) -> Path:
        """Path to .loom/stop-shepherds (shepherd shutdown signal)."""
        return self.loom_dir / self.STOP_SHEPHERDS_FILE

    @property
    def baseline_health_file(self) -> Path:
        """Path to .loom/baseline-health.json."""
        return self.loom_dir / self.BASELINE_HEALTH_FILE

    @property
    def issue_failures_file(self) -> Path:
        """Path to .loom/issue-failures.json."""
        return self.loom_dir / self.ISSUE_FAILURES_FILE

    @property
    def usage_cache_file(self) -> Path:
        """Path to .loom/usage-cache.json."""
        return self.loom_dir / self.USAGE_CACHE_FILE

    @property
    def recovery_events_file(self) -> Path:
        """Path to .loom/metrics/recovery-events.json."""
        return self.metrics_dir / self.RECOVERY_EVENTS_FILE

    def worktree_path(self, issue: int) -> Path:
        """Path to worktree for a specific issue.

        Args:
            issue: The issue number.

        Returns:
            Path to .loom/worktrees/issue-{N}
        """
        return self.worktrees_dir / NamingConventions.worktree_name(issue)

    def builder_log_file(self, issue: int) -> Path:
        """Path to builder log file for a specific issue.

        Args:
            issue: The issue number.

        Returns:
            Path to .loom/logs/loom-builder-issue-{N}.log
        """
        return self.logs_dir / f"loom-builder-issue-{issue}.log"

    def worker_log_file(self, role: str, issue: int) -> Path:
        """Path to worker log file for a specific role and issue.

        Args:
            role: The worker role (e.g., "judge", "builder").
            issue: The issue number.

        Returns:
            Path to .loom/logs/loom-{role}-issue-{N}.log
        """
        return self.logs_dir / f"loom-{role}-issue-{issue}.log"


class NamingConventions:
    """Naming conventions for branches, worktrees, and other identifiers.

    All methods are static - no instance needed.
    """

    BRANCH_PREFIX = "feature/issue-"
    WORKTREE_PREFIX = "issue-"

    @staticmethod
    def branch_name(issue: int) -> str:
        """Generate branch name for an issue.

        Args:
            issue: The issue number.

        Returns:
            Branch name in format "feature/issue-{N}"
        """
        return f"{NamingConventions.BRANCH_PREFIX}{issue}"

    @staticmethod
    def worktree_name(issue: int) -> str:
        """Generate worktree directory name for an issue.

        Args:
            issue: The issue number.

        Returns:
            Worktree directory name in format "issue-{N}"
        """
        return f"{NamingConventions.WORKTREE_PREFIX}{issue}"

    @staticmethod
    def issue_from_branch(branch: str) -> int | None:
        """Extract issue number from branch name.

        Args:
            branch: Branch name (e.g., "feature/issue-42")

        Returns:
            Issue number if branch matches pattern, None otherwise.
        """
        if branch.startswith(NamingConventions.BRANCH_PREFIX):
            try:
                return int(branch[len(NamingConventions.BRANCH_PREFIX) :])
            except ValueError:
                pass
        return None

    @staticmethod
    def issue_from_worktree(worktree_name: str) -> int | None:
        """Extract issue number from worktree directory name.

        Args:
            worktree_name: Worktree directory name (e.g., "issue-42")

        Returns:
            Issue number if name matches pattern, None otherwise.
        """
        if worktree_name.startswith(NamingConventions.WORKTREE_PREFIX):
            try:
                return int(worktree_name[len(NamingConventions.WORKTREE_PREFIX) :])
            except ValueError:
                pass
        return None

    _CC_PREFIX_RE = re.compile(
        r"^(fix|feat|refactor|docs|test|chore|perf)\s*:", re.IGNORECASE
    )

    @staticmethod
    def pr_title(issue_title: str, issue: int | None = None) -> str:
        """Generate a conventional-commit-style PR title from an issue title.

        If the issue title already starts with a conventional commit prefix,
        normalise it to lowercase.  Otherwise, prefix with ``feat:``.

        Falls back to ``feat: implement changes for issue #<N>`` when the
        title is empty/missing and an issue number is provided.

        Args:
            issue_title: The raw issue title from GitHub.
            issue: Optional issue number used for the fallback title.

        Returns:
            A PR title with a conventional commit prefix.
        """
        title = (issue_title or "").strip()
        if not title:
            if issue is not None:
                return f"feat: implement changes for issue #{issue}"
            return "feat: implement changes"

        m = NamingConventions._CC_PREFIX_RE.match(title)
        if m:
            # Already has a prefix – normalise the prefix to lowercase
            prefix = m.group(1).lower()
            rest = title[m.end():].strip()
            return f"{prefix}: {rest}"

        # No prefix – add one.  Use the title lowercased (first char).
        first = title[0].lower() + title[1:] if len(title) > 1 else title.lower()
        return f"feat: {first}"
