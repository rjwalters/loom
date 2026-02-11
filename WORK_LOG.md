# Work Log

Chronological record of completed work in this repository, maintained by the Guide role.

Entries are grouped by date, newest first. Each entry references the merged PR or closed issue.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

### 2026-02-10

- **PR #2219**: feat: extend guard-destructive hook with system and infrastructure patterns
- **PR #2218**: feat: detect and surface human-input-needed blockers in daemon
- **PR #2217**: fix: remove dead A/B testing module (1,341 LOC)
- **PR #2215**: fix: copy guard-destructive.sh hook to target repos during install
- **PR #2214**: feat: two-tier startup detection and diagnostic capture for stalled shepherds
- **PR #2213**: docs: Guide document maintenance update
- **PR #2212**: feat: two-tier heartbeat grace period for faster stale shepherd detection
- **PR #2211**: feat: capture terminal scrollback before killing stuck sessions
- **PR #2210**: feat: classify budget-exhausted shepherds and trigger architect decomposition
- **PR #2209**: feat: add -t/--timeout-min flag for time-bounded daemon runs
- **PR #2208**: fix: remove incorrect 100x scaling of usage API utilization
- **PR #2206**: feat: replace SQLite usage checking with direct Anthropic OAuth API
- **PR #2195**: Issue #2194: write loom-source-path to target repo root
- **PR #2193**: feat: detect editable pip installs before worktree cleanup
- **PR #2191**: Fix label sync script logic
- **PR #2190**: Bump the all-dependencies group with 6 updates
- **PR #2189**: Bump the production-dependencies group with 2 updates
- **PR #2188**: Bump the dev-dependencies group with 4 updates
- **Issue #2216** (closed): Add prompt hook to prevent agents from restarting servers/infrastructure
- **Issue #2207** (closed): Add -t/--timeout-min flag to /loom for time-bounded daemon runs
- **Issue #2205** (closed): Daemon should report when stalled waiting on human input
- **Issue #2203** (closed): Champion should be able to promote or close any open issue
- **Issue #2202** (closed): Remove orphaned ab_testing.rs backend (1,341 LOC dead code)
- **Issue #2201** (closed): Daemon needs strategy for issues that exceed single-session context budget
- **Issue #2200** (closed): Installer doesn't copy guard-destructive.sh hook to target repo
- **Issue #2199** (closed): Daemon should capture shepherd output on kill for post-mortem debugging
- **Issue #2198** (closed): Shepherd spawns without writing progress file -- silent failure mode
- **Issue #2197** (closed): Stale heartbeat detection too slow -- 8+ minutes to reclaim stuck shepherd
- **Issue #2196** (closed): Daemon should auto-resolve contradictory labels
- **Issue #2194** (closed): Installation does not create .loom/loom-source-path file
- **Issue #2192** (closed): loom-daemon breaks after worktree cleanup: editable install points to deleted path

### 2026-02-06

- **PR #2187**: Add tmux liveness detection for support role completion
- **PR #2186**: Add idempotency checks to Loom installation pipeline
- **PR #2185**: Add retry_blocked_issues action handler for blocked issue recovery
- **PR #2184**: Increase shepherd no-progress grace period from 300s to 600s
- **PR #2183**: Revert shepherd issue labels during daemon graceful shutdown
- **PR #2182**: Terminate child tmux sessions during daemon graceful shutdown
- **PR #2175**: Add spinning issue detection: auto-escalate after N review cycles
- **PR #2174**: Add .loom/issue-failures.json to .gitignore
- **PR #2172**: Add persistent cross-session failure tracking for daemon issues
- **PR #2171**: Detect shepherds stuck without progress files
- **PR #2167**: Fix judge/shepherd review workflow mismatch causing systematic judge_exhausted
- **PR #2166**: Fix PreToolUse hook error infinite retry loop
- **PR #2165**: Prevent .loom runtime files from triggering dirty-repo check
- **PR #2164**: Add heartbeat grace period for newly spawned shepherds
- **PR #2163**: Add contradictory label detection and exclusion group enforcement
- **PR #2162**: Extend pipe-pane sed filter to strip CR, BS, and bare escapes
- **PR #2155**: Downgrade ci_failing to info-level to prevent spurious stall escalation
- **PR #2154**: Clear systematic failure state on L3 pool restart
- **PR #2153**: Reset stall counter after L3 pool restart
- **PR #2149**: Guard against missing .loom/scripts when branch predates Loom install
- **PR #2148**: Add escalating stall recovery to daemon iteration loop
- **PR #2146**: Dispatch targeted doctor/judge agents for orphaned PRs
- **PR #2145**: Fix stale shepherd count in iteration summary and spawning decisions
- **PR #2141**: Add create_pr to direct completion mechanical steps
- **PR #2140**: Handle exit code 6 (instant-exit) in judge phase
- **PR #2137**: Fix shepherd Judge phase silent failure with 0s session duration
- **PR #2136**: Add test suite for log_filter module
- **PR #2133**: Filter .loom/ runtime files from shepherd dirty-repo check
- **PR #2132**: Replace sed ANSI stripping with Python log filter for cleaner agent logs
- **PR #2131**: Fix daemon runtime files missing from .gitignore template
- **Issue #2181** (closed): Support roles have no completion mechanism and run indefinitely
- **Issue #2180** (closed): Loom installation creates redundant 'Install Loom' PRs on every reinstall
- **Issue #2179** (closed): Blocked issues are retried endlessly without escalation or backoff
- **Issue #2178** (closed): Shepherds frequently stall with no progress file within grace period
- **Issue #2177** (closed): Daemon shutdown does not revert GitHub labels on in-progress issues
- **Issue #2176** (closed): Daemon shutdown does not kill child tmux sessions
- **Issue #2173** (closed): Add .loom/issue-failures.json to .gitignore
- **Issue #2170** (closed): Spinning issue detection: auto-escalate after N shepherd cycles
- **Issue #2169** (closed): Cross-session failure tracking and exponential backoff
- **Issue #2168** (closed): Stuck shepherd detection: heartbeat-based liveness checks
- **Issue #2161** (closed): Contradictory label state allowed on same PR
- **Issue #2160** (closed): Stale heartbeat detection too aggressive
- **Issue #2159** (closed): Repeated PreToolUse hook errors block shepherd progress
- **Issue #2158** (closed): Shepherds error on uncommitted .loom/ files in main repo
- **Issue #2157** (closed): Judge/Shepherd review workflow mismatch causes systematic judge_exhausted
- **Issue #2156** (closed): Logs unreadable: pipe-pane ANSI stripping missing or broken
- **Issue #2152** (closed): Systematic failure suppresses spawning even after L3 pool restart
- **Issue #2151** (closed): L3 pool restart does not reset stall counter
- **Issue #2150** (closed): ci_failing warning creates unrecoverable stall loop
- **Issue #2147** (closed): Shepherd judge phase crashes when branch predates Loom installation
- **Issue #2144** (closed): Daemon 'stalled' health status persists without corrective action
- **Issue #2143** (closed): Daemon reports stale shepherd count due to timing gap
- **Issue #2142** (closed): Daemon should dispatch targeted agents for orphaned PRs
- **Issue #2139** (closed): Judge CLI sessions exit immediately (0s duration)
- **Issue #2138** (closed): Builder completion phase fails to create PR
- **Issue #2135** (closed): Shepherd Judge phase fails silently with 0s session duration
- **Issue #2134** (closed): Add unit tests for loom_tools.log_filter module
- **Issue #2130** (closed): Shepherd tmux logs are raw terminal output
- **Issue #2129** (closed): Shepherd dirty-repo check too strict for .loom/ runtime files
- **Issue #2128** (closed): Daemon runtime files not included in .gitignore template

### 2026-02-05

- **PR #2127**: Fix --clean install leaving staged deletions in main
- **PR #2125**: Fix security audit failures: update bytes and MCP SDK
- **PR #2124**: Fix E2E terminal-management tests with proper mock terminal data
- **PR #2123**: Fix default mode not auto-approving past approval gate
- **PR #2122**: Add transient API error recovery for autonomous agents
- **PR #2121**: Add test failure analysis tooling for shepherd block rate investigation
- **PR #2120**: Add PreToolUse hook to block destructive agent commands
- **PR #2112**: Fix daemon self-sabotage: gitignore runtime files and fix empty args
- **Issue #2126** (closed): Clean reinstall leaves target repo in dirty state
- **Issue #2119** (closed): Implement PreToolUse hooks to block destructive agent commands
- **Issue #2118** (closed): Implement API error recovery for shepherd/daemon orchestration
- **Issue #2116** (closed): Shepherd: default mode should auto-promote past approval gate
- **Issue #2109** (closed): Daemon runtime files missing from .gitignore
- **Issue #2105** (closed): E2E terminal-management tests failing on main
- **Issue #2100** (closed): Investigate shepherd test failure patterns (20% block rate)

### 2026-02-04

- **PR #2117**: Add scoped test execution to Judge role
- **Issue #2114** (closed): Judge: scope test execution to changed files

### 2026-02-03

- **PR #2115**: Add approval phase timeout and heartbeat reporting
- **PR #2113**: Fix daemon-shepherd approval deadlock
- **PR #2108**: Add Python daemon implementation (daemon_v2) for deterministic orchestration
- **PR #2107**: Fix CI: workflow YAML syntax and E2E test mocks
- **PR #2106**: Fix scoped test detection for nested pyproject.toml
- **PR #2104**: Shepherd: Structured builder checkpoints to detect partial progress
- **PR #2103**: Add WIP commit preservation when builder exits with uncommitted changes
- **PR #2102**: Remove dead multi-terminal and prediction code (Phase 7)
- **Issue #2111** (closed): Approval phase has no timeout or heartbeat reporting
- **Issue #2110** (closed): Shepherd approval gate deadlocks daemon-spawned shepherds
- **Issue #2099** (closed): Scoped test detection misses nested pyproject.toml
- **Issue #2056** (closed): Shepherd: Structured builder checkpoints

### 2026-02-02

- **PR #2101**: Add stale worktree recovery to builder phase
- **PR #2098**: Extend name-based test comparison to line-based fallback path
- **PR #2097**: Add graceful 'no changes needed' pathway to shepherd
- **PR #2094**: Add output-based test ecosystem detection for umbrella commands
- **PR #2093**: Add scoped test verification to builder phase
- **PR #2092**: Add CI-aware validation to doctor phase
- **PR #2091**: Add diagnostics to 'no PR created' shepherd failure
- **PR #2090**: Update shepherd CLI tests to use ShepherdExitCode enum values
- **PR #2089**: Prefer loom-tools source over installed CLI in development
- **PR #2087**: Fix race condition in judge phase fallback approval
- **PR #2086**: Prevent curator from closing issues during curation
- **PR #2081**: Document judge_retry and phase_completed milestone events
- **PR #2080**: Phase 6: Rewrite main.ts for single-session analytics-first model
- **PR #2079**: Distinguish doctor failure modes for better label state recovery
- **PR #2078**: Improve builder completion retry prompting with diagnostic context
- **PR #2077**: Add granular exit codes for shepherd partial success states
- **PR #2076**: Fix PR creation gap after doctor test-fix loop
- **PR #2075**: Bump clap from 4.5.54 to 4.5.56
- **PR #2074**: Bump the dev-dependencies group with 2 updates
- **Issue #2096** (closed): Shepherd: Handle 'no changes needed' gracefully
- **Issue #2095** (closed): Builder: Verify problem exists before attempting fix
- **Issue #2088** (closed): Fix shepherd CLI tests for granular exit codes
- **Issue #2085** (closed): Detect stale loom-tools installation
- **Issue #2084** (closed): Curator agent should not close issues
- **Issue #2083** (closed): Shepherd fallback label application has race condition
- **Issue #2082** (closed): Doctor validation fails when CI is still pending
- **Issue #2068** (closed): Distinguish doctor failure modes
- **Issue #2067** (closed): Increase builder completion retry limit
- **Issue #2066** (closed): Extend name-based test comparison to line-based fallback
- **Issue #2065** (closed): Add diagnostics to 'no PR created' shepherd failure
- **Issue #2064** (closed): Fix PR creation gap after doctor test-fix loop
- **Issue #2045** (closed): Shepherd: Granular exit codes for partial success states
- **Issue #2044** (closed): Shepherd: Scoped test execution based on changed files
- **Issue #2031** (closed): Fix unused variable warning in loom-daemon scaffolding.rs

### 2026-02-01

- **PR #2073**: Judge: rename review terminology to judge/evaluate
- **PR #2071**: Add supplemental Python test verification when pipeline short-circuits
- **PR #2070**: Add post-worktree hook to pre-build loom-daemon binary
- **PR #2069**: Document repository-scoped GitHub token setup
- **PR #2063**: Improve builder completion phase with targeted retry
- **PR #2062**: DRY up pipe-pane log capture: strip ANSI escape sequences
- **PR #2061**: Add file-based analytics dashboard (Phase 5)
- **PR #2057**: Add shepherd pre-flight baseline health check
- **PR #2054**: Add name-based test comparison to reduce false positive regressions
- **PR #2053**: Enable Doctor to fix builder test failures
- **PR #2052**: Add atomic label state transitions
- **PR #2051**: Fix test path for relocated loom CLI wrapper
- **PR #2050**: Fix completion phase shell command parsing broken by newlines
- **PR #2042**: Document worktree deletion dangers
- **PR #2041**: Symlink node_modules from main workspace to worktrees
- **PR #2040**: Add builder completion retry phase for incomplete work
- **PR #2039**: Increase shepherd timeouts to prevent premature agent termination
- **PR #2038**: Implement input logging layer for terminal analytics
- **PR #2037**: Strip ANSI escape sequences from builder logs
- **PR #2035**: Remove auto-recovery from shepherd, add phase contracts
- **PR #2034**: Remove remaining AGENTS.md references
- **PR #2033**: Issue #1978: Auto-recovered PR
- **PR #2032**: Issue #2025: Auto-recovered PR
- **PR #2030**: Issue #1956: Auto-recovered PR
- **PR #2029**: Issue #1979: Auto-recovered PR
- **PR #1912**: Add diagnostic output on judge validation failure
- **Issue #2072** (closed): Judge: rename 'review' terminology to reduce API anchoring
- **Issue #2060** (closed): DRY up pipe-pane log capture
- **Issue #2058** (closed): Document repository-scoped GitHub token setup
- **Issue #2055** (closed): Shepherd: Improve builder completion phase
- **Issue #2049** (closed): Fix missing defaults/loom CLI wrapper
- **Issue #2048** (closed): Shepherd: Atomic label state transitions
- **Issue #2047** (closed): Shepherd: Pre-flight baseline health check
- **Issue #2046** (closed): Shepherd: Enable Doctor to fix test failures
- **Issue #2043** (closed): Shepherd: Compare specific test names
- **Issue #2036** (closed): Document: Agents must not delete worktrees directly
- **Issue #2026** (closed): Clean up remaining AGENTS.md references
- **Issue #2025** (closed): Move ./loom CLI wrapper into .loom/ folder
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
