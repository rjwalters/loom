# Changelog

All notable changes to Loom will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.1] - 2026-05-04

### Summary

Patch fix for `install-loom.sh`: PR creation now passes `--head` so it doesn't fail in shells where gh's origin auto-detection is degraded, and the rollback path cleans up orphan remote branches when the install fails after push. Caught and fixed during the v0.7.0 rollout to vibesql and kicad-tools.

### Fixed

- `scripts/install/create-pr.sh` — pass `--head "$BRANCH_NAME"` to `gh pr create` so it skips origin auto-detection (which can fail with "could not resolve remote 'origin'" in shells where gh's host detection is degraded, even when `-R` already pins the target repo) (#3245)
- `scripts/install-loom.sh` — `cleanup_on_error` now deletes the remote branch when the install fails after the push step, restricted to `feature/loom-install-v*` so unrelated branches are never touched (#3245)
- Three new regression tests in `scripts/test-installer.sh` covering the `--head` flag, the remote-branch cleanup call, and the prefix-anchored regex (#3245)

## [0.7.0] - 2026-05-03

### Summary

Multi-account Claude OAuth token rotation: Loom can now spread agent spawns across multiple Pro/Max accounts and recover automatically when a single account hits its weekly limit. Includes a `loom-tokens` CLI for pool management and a hardened `spawn-claude.sh` wrapper. Plus a batch of `install-loom.sh` hardening fixes from real-world install failures.

### Added

- `loom-tokens bootstrap` CLI — materialize numbered `ACCOUNT_EMAIL_N` / `ACCOUNT_KEY_N` / `ACCOUNT_TOKEN_FILE_N` triples from `.env` into per-account `.loom/tokens/<name>.token` files (mode 0600) with `index.json` manifest containing sha256 fingerprints — no secret material in the manifest (#3239)
- `loom-tokens check` CLI — probe each account via Anthropic Messages API, parse rate-limit headers (suffix-match for rename resilience), write atomic JSON `.ranking` for the spawn-time selector (#3241)
- `loom-tokens pin` / `unpin` / `unblock` CLI — operator controls for allowlist management and bad-token recovery (#3243)
- `.loom/scripts/spawn-claude.sh` wrapper with 3-tier token selection (ranking → allowlist → random), `CLAUDE_CODE_OAUTH_TOKEN` injection, `mkdir`-lock guarded bad-token tracking, and exit-code-first error classification (fixes lean-genius bug where exit-0 with rate-limit substring in stdout was misclassified as RECOVERABLE) (#3242)
- `.loom/scripts/probe-tokens.sh` cron-friendly periodic-probe wrapper
- `agent_spawn.py` integration: injects selected `CLAUDE_CODE_OAUTH_TOKEN` when `.loom/tokens/` is configured; falls back to existing keychain auth when not (#3240)
- Auto-unpin guardrail: when all allowlisted accounts hit the consecutive-failure threshold (default 5), `spawn-claude.sh` clears `.allowlist` to prevent pin-induced lockout
- Empty-pool guard: `spawn-claude.sh` refuses to silently auto-clear `.bad_tokens` (operators must explicitly `unblock` or `unpin`)
- `loom-forge --version` flag (#3214)

### Fixed

- `install-loom.sh` no longer omits `.loom/scripts/lib/`, which was breaking `merge-pr.sh` and ~30 other scripts (regression of #2392; hardened with three layers of defense including post-copy assertion and recursive parity check) (#3225)
- `install-loom.sh` post-install "unstaged changes" warning now snapshots pre-install state and only flags installer-introduced changes; replaces destructive `git restore` recommendation with inspect-first guidance (#3222)
- `install-loom.sh` non-TTY stdin no longer crashes the install and rolls back completed work — fixes the `curl | bash` install path (#3223)
- `install-loom.sh` detects pre-existing overlapping branch rulesets via `~DEFAULT_BRANCH` enumeration; offers Skip / Replace / Update on conflict instead of silently creating duplicates (#3224)
- `install-loom.sh` pre-flight upgrade message no longer leaks the literal `{{LOOM_VERSION}}` placeholder; uses layered version detection from `install-metadata.json` with placeholder rejection (#3221)
- `loom-daemon init` no longer flags merge-preserved customizations (`.claude/`, `.codex/`, `.github/`) as "Verification failures" with misleading `--force` advice (#3226)

### Changed

- `defaults/scripts/cli/loom-scale.sh` now invokes `claude-wrapper.sh` instead of `claude` directly (#3240)
- Dependency bumps: production (#3211) and dev (#3212) groups

## [0.6.5] - 2026-04-22

### Summary

Fix for shell shepherd builders hitting thinking stalls when tmux server inherits CLAUDECODE from a parent Claude Code session.

### Fixed

- Prevent CLAUDECODE environment variable leaking through tmux server inheritance by using explicit empty set instead of `-u` (unset) in `agent_spawn.py` and `spawn-shell-shepherd.sh` (#3208)
- Add diagnostic logging in `claude-wrapper.sh` for skip-permissions mode and CLAUDECODE presence (#3208)

## [0.6.4] - 2026-04-22

### Summary

Bug fix for broken `loom-forge` fallback in duplicate detection, plus documentation updates mandating `merge-pr.sh` for all PR merges.

### Fixed

- `check-duplicate.sh` now validates `loom-forge` works before using it, falling back to `gh` when the editable install is broken (#3206)

### Changed

- Documentation (CLAUDE.md, champion.md, builder.md) now mandates `.loom/scripts/merge-pr.sh` instead of `gh pr merge` (#3207)

## [0.6.3] - 2026-04-21

### Summary

Hermit role improvements to reduce false positives in stateless ceremony detection, dead code removal, and installation infrastructure cleanup.

### Changed

- Hermit stub theater now defaults to "finish the feature" over removal

### Fixed

- Exclude dispatch-table classes from stateless ceremony heuristic (#3199, #3200)
- Skip classes with 10+ methods in stateless ceremony check (#3200)

### Removed

- Duplicate `health_check.py` module — parallel drift of `daemon_diagnostic.py`, -1034 LOC (#3202)
- Stale `.codex/` configuration directory and all installer references
- Broken Lines of Code badge (ghloc branch was pruned; workflow will regenerate it)

## [0.5.0] - 2026-04-19

### Summary

Major feature release: Loom is now forge-agnostic with full Gitea support. A new ForgeClient abstraction layer enables Loom to orchestrate development workflows against both GitHub and Gitea, with automatic forge detection from git remote URLs.

### Added

- ForgeClient protocol with 21 methods for forge-agnostic operations (#3132)
- GitHubForge implementation wrapping existing GitHub code behind ForgeClient (#3133)
- GiteaForge implementation with full Gitea REST API v1 support (#3144)
- Forge detection and selection from git remote URL, config, or environment (#3135)
- CachedForgeClient replacing `gh-cached` with forge-neutral caching (#3149)
- Gitea CI status integration with client-side aggregation and Actions API fallback (#3148)
- Gitea branch protection and repository settings support (#3145)
- `loom-forge` CLI entry point for forge-agnostic shell script dispatch (#3146)
- `loom-auto-merge` CLI with poll-and-merge fallback for Gitea (#3170)
- Forge-agnostic dispatch for raw-API shell scripts: `merge-pr.sh`, `check-ci-status.sh`, `test-plan-metrics.sh` (#3147)
- Shared `forge-helpers.sh` bash library with 14 dispatch functions (#3147)
- `forge-detect.sh` helper for shell-level forge detection (#3145)
- End-to-end integration tests with Docker Gitea instance (#3156)
- GitHub Actions CI workflow for Gitea integration tests (#3156)
- Pagination, HTTP 429 retry with backoff, and expanded body-search patterns in GiteaForge (#3158)
- "Built with Loom" badge for downstream projects (#3137)
- Forge authentication documentation covering both GitHub and Gitea (#3157)

### Changed

- Parameterized `github_parser.rs` to support configurable forge URL patterns (#3134)
- Installation scripts (`validate-target.sh`, `create-pr.sh`, `sync-labels.sh`) now detect and support both forges (#3157)
- `check_github_remote` Tauri command now recognizes Gitea hosts (#3159)
- `reset_github_labels` dispatches through `loom-forge` CLI instead of `gh` directly (#3160)
- `shepherd/labels.py` migrated from `gh` CLI calls to ForgeClient abstraction (#3168)
- CLAUDE.md templates updated with multi-forge context and mandatory shepherd lifecycle section
- `defaults/config.json` now includes `forge` configuration section (#3157)

### Renamed

- `github_parser.rs` → `forge_parser.rs` (#3161)
- `ParsedGitHubEvent` → `ParsedForgeEvent` (#3161)
- `check_github_remote` → `check_forge_remote` (#3161)
- `PromptGitHubEvent` → `PromptForgeEvent` and related types (#3169)
- `github_events.rs` → `forge_events.rs` (#3169)

### Removed

- Dead Tauri commands: `check_label_exists`, `create_github_label`, `update_github_label` (#3160)
- Expired `loom:in-progress` migration code (#3160)

### Fixed

- Idle support role tmux sessions no longer consume memory indefinitely (#3136)

## [0.4.1] - 2026-04-14

### Summary

Stability and reliability release focused on daemon resilience, stall detection improvements, and auth/session robustness.

### Added

- Auto-decay failure counters when main branch advances (#3124)
- 3 missing daemon startup cleanup steps (#3129)
- Auto-detect CI presence and avoid false `ci_failing` warnings (#3121)
- Parse issue dependencies to avoid scheduling issues with unmet prerequisites (#3119)
- Comprehensive failure counter reset mechanism (#3118)
- Pre-PR validation gate and stale branch detection to builder (#3130)
- Configurable issue failure threshold with dependency block exemptions (#3113)

### Fixed

- Daemon accounts for actionable PRs in health status and work detection (#3128)
- Eliminate auth cache lock contention across all agent spawns (#3122)
- Implement per-phase stall detection timeouts to prevent killing active shepherds (#3126)
- Increase stall detection thresholds for initial agent planning (#3115)
- Detect completed support roles by checking Claude process, not just tmux session (#3127)
- Detect merge-conflicted approved PRs and dispatch Doctor to resolve them (#3125)
- Clean up orphaned sessions and stale labels after daemon crash (#3123)
- Catch non-critical errors in daemon loop instead of crashing (#3120)
- Handle missing `.claude.json.lock` in session cleanup (#3117)
- Don't install external issue labeling workflow by default (#3116)
- Stagger agent spawns to avoid auth cache thundering herd (#3114)
- Use `hex::encode` for sha2 0.11 compatibility

### Dependencies

- Bump the dev-dependencies group with 6 updates (#3093)
- Bump tokio in the all-dependencies group (#3092)

## [0.2.3] - 2026-02-15

### Summary

Reliability and stability release focused on shepherd pipeline hardening, per-agent config isolation, and analytics integration.

### Added

- Shepherd reflection phase for post-run analysis and upstream issue filing (#2275)
- Per-agent `CLAUDE_CONFIG_DIR` isolation for concurrent session stability (#2285, #2313)
- Analytics pipeline UI integration (Phases 3-5) (#2373)
- Pre-implementation reproducibility check in builder phase (#2363)
- Dedicated timeout and stuck recovery for doctor test-fix phase (#2347)
- MCP server failure detection and recovery in shepherd pipeline (#2310)
- Dual-mode GitHub API layer with REST fallback (#2255)
- Pre-flight auth check in claude-wrapper.sh (#2332)
- `--skip-builder` and `--pr` flags for shepherd (#2314)
- Installer integration test suite for install/reinstall/uninstall paths (#2272)
- Kill orphaned claude processes during terminal/session lifecycle (#2273)
- Wrapper retry state reporting for shepherd observability (#2311)

### Fixed

- Detect already-merged PRs in shepherd validation to prevent unnecessary builder runs (#2384)
- Replace broken `st_mtime`-`st_ctime` duration gate with output volume check (#2382)
- Remove `mcp.json` from shared config symlinks to fix MCP init failures (#2380)
- Duration gate for MCP failure detection to prevent false positives (#2377)
- Label cleanup handler for shepherd partial failures (#2372)
- Broaden shepherd builder recovery to use PR existence as primary signal (#2371)
- Detect `loom:changes-requested` label in judge unexpected-result branch (#2364)
- Set PYTHONPATH in worktree subprocesses to resolve imports correctly (#2361)
- Push doctor test-fix commits to remote before re-verification (#2351)
- Shepherd recovers from builder non-zero exit when PR already created (#2346)
- Check for existing judge approval before retrying on MCP/exit failures (#2340)
- Ensure agents skip Claude Code onboarding wizard (#2336)
- Clone macOS Keychain credentials for per-agent config dir isolation (#2323)
- Baseline comparison parses biome/clippy errors and detects cross-tool mismatches (#2331)
- Regression guard for doctor test-fix loop (#2317)
- Prevent theme picker from blocking agents in isolated config dirs (#2302)
- TTY fallback in claude-wrapper to avoid hanging when no terminal available (#2297)
- Prevent unsafe worktree removal during merge and agent destroy (#2251)
- Enforce `merge-pr.sh` over `gh pr merge` to prevent worktree errors (#2306)
- Ad-hoc sign Rust test binaries on macOS to prevent `_dyld_start` hangs (#2304)

### Changed

- Replace husky/lint-staged with plain `.githooks/` directory (#2305)
- Split loom-daemon into lib + binary to prevent test hang (#2337)

## [0.2.0] - 2026-01-24

### Summary

This release introduces the **Three-Layer Architecture** with the new `/loom` daemon as the centerpiece. Loom has evolved from a manual orchestration tool to a fully autonomous development system capable of generating its own work, scaling shepherds, and maintaining continuous operation.

### Architecture

#### Three-Layer Orchestration Model

Loom now operates across four distinct layers:

- **Layer 3: Human Observer** - Oversight, proposal approval, and strategic direction
- **Layer 2: Loom Daemon (`/loom`)** - System-wide orchestration and work generation
- **Layer 1: Shepherds (`/shepherd <issue>`)** - Per-issue lifecycle orchestration
- **Layer 0: Workers** - Single task execution (Builder, Judge, Curator, Doctor, etc.)

#### Role Restructuring

- Renamed `loom.md` to `shepherd.md` (now Layer 1)
- Created new `loom.md` for Layer 2 daemon role
- Updated all command references and documentation

### Added

#### Fully Autonomous Daemon (`/loom`)

- Continuous loop with configurable polling interval (default 30 seconds)
- Auto-spawns shepherds when `loom:issue` issues are available
- Auto-triggers Architect/Hermit when issue backlog falls below threshold
- Auto-ensures Guide and Champion roles keep running
- State persistence in `.loom/daemon-state.json` for crash recovery
- Graceful shutdown via `.loom/stop-daemon` signal file

#### Status Observation (`/loom status`)

- Read-only Layer 3 observation interface
- Shell script helper: `.loom/scripts/loom-status.sh`
- JSON output option for scripting and automation
- Shows shepherd assignments, issue counts, and daemon health

#### Cleanup Mechanisms

- Task artifact archival (`./scripts/archive-logs.sh`)
- Safe worktree cleanup (`./scripts/safe-worktree-cleanup.sh`)
- Event-driven cleanup (`./scripts/daemon-cleanup.sh`)

#### Resilience Features

- Stuck agent detection and recovery system
- Circuit breaker pattern for daemon IPC resilience
- Automatic recovery from `daemon-state.json` on restart

#### Observability

- Agent effectiveness metrics tracking
- LLM resource usage tracking (tokens and cost)
- Test outcome tracking in activity database
- Prompts linked to GitHub issues, PRs, and commits

#### Builder Enhancements

- Parallel claiming workflow for faster issue claiming
- Pre-implementation review section in role guidelines
- Worktree merge graceful handling

#### Other Features

- Auto-configuration of missing terminals in force mode
- Graceful shutdown signal script for Loom agents
- Dependency unblocking in Guide role
- Auto-unblock for dependent issues when Champion merges PR
- Extend command to claim system for long-running work
- CLI fallback mode for `/loom` orchestrator when MCP unavailable

### Changed

- Documentation updated to reflect three-layer architecture
- Clarified daemon execution model: background process vs interactive mode
- `/loom --force-merge` now runs Judge phase instead of skipping

### Fixed

- Worktree merge handling in Loom orchestrator
- Force-merge mode properly runs Judge phase

### Configuration

#### Daemon Configuration Parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `ISSUE_THRESHOLD` | 3 | Trigger Architect/Hermit when `loom:issue` count below this |
| `MAX_PROPOSALS` | 5 | Maximum pending proposal issues |
| `MAX_SHEPHERDS` | 3 | Maximum concurrent shepherd processes |
| `ISSUES_PER_SHEPHERD` | 2 | Scale factor: target = ready_issues / ISSUES_PER_SHEPHERD |
| `POLL_INTERVAL` | 60 | Seconds between daemon loop iterations |

### Migration Notes

Existing v0.1.x installations can upgrade cleanly:

1. The installation script automatically injects the correct version
2. Role references are backward-compatible
3. Existing workflows continue to work unchanged

### Related Issues

- #1040 - Implement fully autonomous daemon loop for /loom
- #1039 - Add /loom status command for Layer 3 observation
- #1038 - Clarify daemon execution model
- #1034 - Add cleanup mechanisms for task artifacts and worktrees
- #1031 - Update documentation to three-layer architecture
- #1029 - Add stuck agent detection and recovery system
- #1030 - Add circuit breaker pattern for daemon IPC resilience
- #1028 - Add basic agent effectiveness metrics
- #1020 - Link prompts to GitHub issues and PRs
- #1018 - Add LLM resource usage tracking
- #1016 - Update /loom to run continuously with parallel subagents
- #1008 - Create Layer 2 loom.md daemon role
- #1005-#1007 - Rename loom.md to shepherd.md
- #1004 - Add parallel claiming workflow to Builder
- #1003 - Handle worktree merge gracefully
- #1002 - Add agent status reporting script
- #1001 - Add auto-configuration of missing terminals
- #1000 - Add Pre-Implementation Review to Builder
- #998 - Add graceful shutdown signal script
- #997 - Add dependency unblocking to Guide
- #988 - Add auto-unblock for dependent issues
- #987 - Add extend command to claim system
- #986 - Fix /loom --force-merge Judge phase
- #984 - Add CLI fallback mode to /loom

## [0.1.0] - 2025-12-01

### Added

- Initial release of Loom
- Multi-terminal GUI with Tauri + xterm.js
- Role-based terminal configuration
- GitHub label-based workflow coordination
- Worker roles: Builder, Judge, Curator, Doctor, Champion, Architect, Hermit, Guide
- Git worktree isolation for concurrent work
- Manual Orchestration Mode (MOM) with Claude Code
- Tauri App Mode for automated orchestration
- MCP servers for programmatic control (loom-terminals, loom-ui, loom-logs)
- Installation script for target repositories
- Quickstart templates for webapp, desktop, and API projects

[Unreleased]: https://github.com/rjwalters/loom/compare/v0.2.3...HEAD
[0.2.3]: https://github.com/rjwalters/loom/compare/v0.2.0...v0.2.3
[0.2.0]: https://github.com/rjwalters/loom/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/rjwalters/loom/releases/tag/v0.1.0
