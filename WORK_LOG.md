# Work Log

Chronological record of completed work in this repository, maintained by the Guide role.

Entries are grouped by date, newest first. Each entry references the merged PR or closed issue.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

### 2026-02-01

- **PR #1912**: Add diagnostic output on judge validation failure
- **Issue #1908** (closed): Shepherd: judge worker silently fails without submitting review

### 2026-01-31

- **PR #1913**: Add force-mode fallback for changes-requested detection in judge phase
- **PR #1911**: Add judge retry mechanism in shepherd orchestrator
- **PR #1907**: Extract shared tmux session utilities to common/tmux_session.py
- **PR #1906**: Preserve worktree on builder test failure instead of cleaning up
- **PR #1905**: Extract stuck_detection.py formatting into dedicated stuck_formatting.py module
- **PR #1904**: Reduce CI minutes on pull requests
- **PR #1903**: Port detect-systematic-failure.sh and record-blocked-reason.sh to Python
- **PR #1892**: Add backwards-compatible clean.sh wrapper in defaults/scripts/
- **PR #1890**: Add Python loom-tools tests to CI via uv
- **PR #1889**: Issue #1884: Auto-recovered PR
- **PR #1877**: Add --clean flag to uninstall for clean reinstall
- **PR #1876**: Fix install PR creation failing silently on error
- **PR #1874**: Consolidate CuratorPhase validation to use validate_phase module
- **PR #1873**: Add loom-tools toolchain validation at daemon startup
- **PR #1872**: Add shared loom-tools.sh helper for consistent CLI error handling
- **PR #1871**: Unify GitHub label operations in labels.py
- **PR #1870**: Add generic gh_list() for GitHub CLI queries
- **PR #1869**: Add centralized environment variable parsing utilities
- **PR #1868**: Centralize path constants and naming conventions in common/paths.py
- **PR #1867**: Add phase result helpers to BasePhase
- **PR #1866**: Add SerializableMixin for automatic dataclass serialization
- **PR #1865**: Add centralized JSON I/O utilities to loom-tools
- **PR #1864**: Add phase_completed milestone event type
- **PR #1863**: Increase API rate limit threshold from 90% to 99%
- **PR #1853**: Fix flaky integration tests due to tmux server state
- **PR #1852**: Add worktree cleanup when builder phase fails
- **PR #1846**: Fix loom-shepherd ModuleNotFoundError on PEP 668 systems
- **PR #1845**: Add worktree safety checks to prevent destroying active sessions
- **PR #1844**: Fix shell stub scripts for non-interactive environments
- **PR #1843**: Fix daemon-cleanup.sh startup hang with many stale progress files
- **PR #1842**: Add CI health awareness to loom-tools snapshot
- **PR #1841**: Fix stale recommended_actions by reordering iteration sequence
- **PR #1840**: Fix state schema mismatch: support tmux_session for support roles
- **PR #1839**: Add --force flag to loom-clean calls in daemon_cleanup.py
- **PR #1836**: Fix daemon state rotation with Python fallback and better error logging
- **PR #1835**: Clean managed directories on reinstall to remove stale files
- **PR #1834**: Port health-check.sh proactive monitoring to loom-tools Python
- **PR #1832**: Add configurable champion auto-merge size limit
- **PR #1831**: Fix jq null handling in session-reflection.sh
- **PR #1830**: Port validate-phase.sh to loom-tools Python module
- **PR #1829**: Fix parent loop docs: Task() not Skill() for iteration spawning
- **PR #1828**: Port daemon-cleanup.sh to loom-tools Python
- **PR #1827**: Add check:ci:lite script excluding Tauri build for worktree verification
- **PR #1826**: Port loom-status.sh to loom-tools Python
- **PR #1824**: Port agent-metrics.sh to loom-tools Python
- **PR #1823**: Port report-milestone.sh to loom-tools Python module
- **PR #1822**: Port orphaned shepherd recovery from shell to Python module
- **PR #1821**: Port validate-daemon-state.sh to Python (loom-validate-state)
- **PR #1820**: Port agent-wait.sh to loom-tools Python module
- **Issue #1910** (closed): Shepherd: fallback approval detection should also detect changes-requested
- **Issue #1909** (closed): Shepherd: judge validation failure skips doctor loop entirely
- **Issue #1900** (closed): Reduce CI minutes on pull requests
- **Issue #1894** (closed): Phase 1: Config v3 & State Simplification for Single-Session Model
- **Issue #1891** (closed): Shepherd: preserve worktree on builder test failure instead of cleaning up
- **Issue #1887** (closed): Increase frontend test coverage thresholds
- **Issue #1886** (closed): Extract stuck_detection.py formatting into dedicated module
- **Issue #1885** (closed): loom-tools Python tests fail to import
- **Issue #1884** (closed): check:ci:lite fails on main: coverage thresholds exceed actual coverage
- **Issue #1882** (closed): Port detect-systematic-failure.sh to Python
- **Issue #1880** (closed): Extract shared tmux session utilities to common/tmux_session.py
- **Issue #1879** (closed): Remove unused prediction.ts module (658 LOC dead code)
- **Issue #1878** (closed): Add backwards-compatible clean.sh wrapper
- **Issue #1875** (closed): Clean reinstall preserves stale scripts
- **Issue #1862** (closed): loom-tools: Consolidate phase validation logic
- **Issue #1861** (closed): loom-tools: Add phase result helper to BasePhase
- **Issue #1860** (closed): loom-tools: Centralize environment variable parsing utilities
- **Issue #1859** (closed): loom-tools: Create generic gh_list() for GitHub CLI queries
- **Issue #1858** (closed): loom-tools: Unify GitHub label operations in labels.py
- **Issue #1857** (closed): loom-tools: Centralize path constants and naming conventions
- **Issue #1856** (closed): loom-tools: Create serialization mixin for dataclass models
- **Issue #1855** (closed): loom-tools: Create centralized JSON I/O utilities
- **Issue #1854** (closed): Increase API rate limit thresholds to 99%
- **Issue #1851** (closed): Refactor loom-tools: DRY opportunities and code consolidation
- **Issue #1850** (closed): Shepherd fails silently when loom-tools CLI commands not installed
- **Issue #1849** (closed): Milestone reporting shows 'Unknown event phase_completed' error
- **Issue #1848** (closed): Flaky integration tests fail on main due to tmux server state
- **Issue #1847** (closed): Builder phase should clean up worktree on failure
- **Issue #1838** (closed): Daemon should be resilient to missing/broken loom-tools commands
