# Changelog

All notable changes to Loom will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.8.0] - 2026-05-24

### Summary

`/loom:sweep` matures from MVP to a full orchestration surface: parallel wave dispatch (`--builders-per-wave N`), natural-language selector interpretation, and `--dry-run` previews. Together these are Phase 1 of a phased plan to deprecate `loom-daemon` + the shepherd model in favor of in-session sweep-driven orchestration (architect's proposal on #3317 — ~10:1 LOC simplification opportunity). v0.8 ships sweep alongside the daemon (daemon stays default) so users can validate sweep against real workloads before Phase 2 lands in v0.9.

Also in this release: a long-overdue dogfood fix that makes `loom-*` subagents discoverable in installed repos and in this checkout itself (#3305 umbrella → #3313 / #3314 / #3315), Gitea hardening for basic-auth instances and merge-mergeability races, and a clarified label-ownership taxonomy in `.github/labels.yml` + `CLAUDE.md`.

### Added

- `/loom:sweep --builders-per-wave N` — in-session parallel wave dispatch for the sweep skill. Each wave dispatches up to N `loom-builder` subagents directly from the orchestrator session (one level deep, sidestepping the two-level-deep race documented in #3289). Defaults to N=1 (sequential MVP behavior); N=2 recommended, N=3 validated, N≥4 warns. Silent clamp when N > candidate count. Prominent "CRITICAL: One level deep — never spawn `/shepherd` as a subagent" guardrail in the skill text (#3316 / PR #3320)
- `/loom:sweep` natural-language selector interpretation — dual-mode contract: regex fast-path for `^#?\d+$` tokens (preserves explicit `#N #M #K` MVP behavior bit-for-bit); natural-language description for anything else, translated to `gh issue list` flags (`--label`, `--author`, `--search`, `--state`) by the orchestrator. Deliberately non-formal: no grammar, no parser. Documents 4 edge cases (zero matches, >100 cap warning, file-touch queries → clarification, ambiguous time windows → clarification) (#3318 / PR #3321)
- `/loom:sweep --dry-run` — read-only preview that prints the candidate list, wave layout (respects `--builders-per-wave`), and total wave count without spawning agents, editing labels, creating worktrees, or merging PRs. Concrete acceptance: pre/post snapshots of candidate labels, open-PR list, and `.loom/worktrees/` count are unchanged (#3319 / PR #3322)
- Installer copies `defaults/.claude/agents/loom-*.md` into target `.claude/agents/` so native subagent dispatch (`Agent(subagent_type="loom-builder", ...)`) works in fresh installs. Adds parallel `.claude/agents/` validation to `loom-daemon init` (≥5 subagent files required) mirroring the existing `.claude/commands/loom/` check (#3310 / PR #3314)
- Installer dogfood mode — auto-detected when `TARGET_PATH == LOOM_ROOT` or forced with `--dogfood` / `--no-dogfood`. Creates `.claude/agents -> ../defaults/.claude/agents` symlink in the loom repo's own checkout instead of copying (zero drift from source of truth). `.gitignore` gains `.claude/agents`. Refuses to replace local-only agent files (#3311 / PR #3315)
- Gitea basic-auth support for token-less self-hosted instances — `loom-forge` and helpers now accept `GITEA_USER` / `GITEA_PASS` env vars as a fallback when `GITEA_TOKEN` is absent (#3303)
- `loom-tools/src/loom_tools/common/gitea.py:merge_pull_request` polls `mergeable` for up to 10s (treating both `null` and `false` as "still computing" since Gitea returns `false` as the initial async-computation value, not `null`) then attempts the merge POST and lets the response be the source of truth (#3306 / PR #3309)

### Changed

- `defaults/.claude/README.md` rewrites the subagent-invocation pattern to show `subagent_type="loom-<role>"` (native dispatch) as the primary pattern; the legacy `general-purpose` + slash-command-in-prompt pattern is preserved and clearly marked as a fallback. Together with the installer copy/symlink work, this closes the gap where the docs documented one pattern but the lookup path didn't support it (#3312 / PR #3313)
- `.github/labels.yml` — every lifecycle label description gains an `Applied by:` clause naming the agent or role that owns the transition. Prevents the "intake should be `loom:curating` or `loom:triage`?" confusion that caused a mislabel mid-session. Covers `loom:triage`, `loom:issue`, `loom:building`, `loom:curating`, `loom:curated`, `loom:treating`, `loom:reviewing`, `loom:review-requested`, `loom:changes-requested`, `loom:pr`, `loom:architect`, `loom:hermit`, `loom:auditor` (#3307 / PR #3308)
- `CLAUDE.md` "Issue Lifecycle" diagram (lines 127-134) — fixes two pre-existing bugs: missing `loom:triage` intake state and incorrect attribution of `loom:issue` to the Curator (humans, or Champion in `--merge` mode, apply that label). Now shows the full chain: `loom:triage → loom:curating → loom:curated → loom:issue → loom:building → (closed)` (#3307 / PR #3308)
- 5 sites in `loom-tools/tests/integration/test_gitea_e2e.py` migrated from `EntityType.ISSUE` / `EntityType.PULL_REQUEST` attribute access to literal strings `"issue"` / `"pr"`. `EntityType` is `Literal["issue", "pr"]`, not an Enum — the tests were written against an Enum shape that never existed. Took the Gitea Integration Tests workflow from 0/31 → 31/31 passing combined with the bootstrap fixes (#3306 / PR #3309)
- `tests/integration/setup-gitea.sh` — detects the Gitea container by `gitea/gitea` image name (works on GHA's hash-named service containers as well as local-compose); admin user creation runs as `git` user (Gitea refuses to run as root) (#3303)
- `loom-tools/src/loom_tools/common/gitea.py:_request` — non-404 4xx responses now log response body at warning level (was debug) — surfaces 405/409/422 failure modes that were previously swallowed (#3306 / PR #3309)

### Fixed

- Installer refuses to install when the target's `.gitignore` would shadow `lib/*.sh` (regression guard for the recurring "installer skipped lib/" class of bugs) (#3287 / PR #3304)
- `loom-daemon` `db.rs` — `query_map` call sites converted from `usize` to `i64` after rusqlite 0.39.0 dropped the `ToSql` impl for `usize` (Dependabot bump #3295 had broken main's Rust CI; bundled into this PR as a side benefit) (#3287 / PR #3304)
- `scripts/install-loom.sh` — pass `--head` to `gh pr create` and clean up orphan remote branches on install failure (#3245, backport from v0.7.1)

### Dependencies

- Dependabot: all-dependencies group bumped, including rusqlite 0.37.0 → 0.39.0 (#3295)
- `actions/upload-artifact` 4 → 7 (#3293)
- `actions/setup-node` 4 → 6 (#3292)
- `dorny/paths-filter` 3 → 4 (#3291)
- `zod` bumped via audit fix (#3296)
- `@tauri-apps/cli` bumped via audit fix (#3294)

### Strategic context

This release is **Phase 1 of the phased daemon-deprecation plan** described in architect proposal #3317. Sweep is now functional enough for users to drive full Curator → Builder → Judge → Doctor → Merge lifecycles from a single Claude session. Phase 2 (soft-deprecate the daemon via `LOOM_USE_SWEEP=1` env flag) and Phase 3 (remove daemon in v1.0) depend on closing two prerequisites: `/schedule` integration for periodic background roles (#3323) and `.loom/sweep-history.json` for cross-session state (#3324). v0.8 ships sweep alongside the daemon (daemon stays default) so we can gather real-world validation before committing further.

## [0.7.2] - 2026-05-14

### Summary

Hardening release driven by a high-throughput parallel-shepherding session that surfaced latent bugs in merge tooling, installer hooks, and role workflow gates. Headline fixes: auto-merge no longer collides with the host worktree (#3284), installer hooks use `${CLAUDE_PROJECT_DIR}` so they survive cwd changes (#3277), and Champion closes issues via GraphQL `closingIssuesReferences` instead of a brittle regex (#3276). Plus a new `worktree.sh --sparse`/`--full` cone-mode flag, curator/builder decomposition guardrails, security patches for 9 transitive npm vulnerabilities, and dependency bumps.

### Added

- `defaults/scripts/worktree.sh` — `--sparse <paths...>` and `--full` flags for cone-mode checkout, with per-worktree config, always-included safety set (`.claude/`, `.loom/`, `.githooks/`, `scripts/`), and `LOOM_WORKTREE_ALWAYS_INCLUDE` env var. JSON output gains `sparse` and `cone` fields (#3278)
- `loom:epic` and `loom:epic-phase` labels in the install bundle (`defaults/.github/labels.yml`), plus idempotent preflight in the `epic` skill (#3273)
- Re-curation playbook section in `curator.md` with decision table for revisiting already-`loom:curated` issues (#3275)

### Changed

- Installer writes hook commands with `${CLAUDE_PROJECT_DIR}/` prefix; `merge_hook_commands` strips legacy bare-relative entries to prevent duplicate-hook accumulation on upgrade (#3277, resolves #3251)
- Champion closes referenced issues via `gh`'s GraphQL `closingIssuesReferences` instead of `grep -Eo "(Closes|Fixes|Resolves) #[0-9]+"`. Gitea backend uses a word-boundary regex that excludes `Updates`/`See`/`References`/`Discloses` (#3276)
- `merge-pr.sh --auto` path is now worktree-safe — enables auto-merge via GraphQL `enablePullRequestAutoMerge` (no local checkout), inherits the sync-path `Base branch was modified` retry loop, and falls through to shared cleanup after confirming merge (#3284)
- Curator must not pre-curate decomposed sub-issues; sub-issues land in `loom:triage` for a dedicated curator pass (#3272)
- Builder-complexity decomposition no longer self-adds `loom:issue` to sub-issues — preserves the curator + human gate (#3282, resolves #3253)
- `loom:curated` label description revised to reflect additive (not "awaiting approval") semantics (#3275)
- `scripts/install-loom.sh` builds `loom-daemon` via direct `cargo build` instead of `pnpm daemon:build`, decoupling the daemon build from pnpm install-state (#3271)
- Dependency bumps: tokio 1.52.1 → 1.52.3, tauri 2.10.3 → 2.11.1, tauri-plugin-dialog 2.7.0 → 2.7.1, tauri-plugin-opener 2.5.3 → 2.5.4, tower-http 0.6.8 → 0.6.10 (#3264); pnpm/action-setup 4→6 (#3262); codeql-action 3→4 (#3259); action-gh-release 2→3 (#3261); setup-python 5→6 (#3260); checkout 4→6 (#3258); dev-dependencies group — biome, playwright, tailwindcss, vitest, vite, postcss (#3268); production-dependencies (#3248)

### Fixed

- `defaults/scripts/worktree.sh` submodule init uses `--init --recursive` (handles nested), 300s timeout (`LOOM_SUBMODULE_TIMEOUT`), and preserves stderr (#3274)
- `defaults/optional/github-workflows/label-external-issues.yml` — removed broken `push:` trigger, dead `validate` guard job, and redundant `if: github.event_name == 'issues'` predicate (#3269)
- Removed ineffective `/clear` "Context Clearing (Cost Optimization)" instruction from 8 role files (architect, auditor, champion, champion-common, curator, guide, hermit, judge) — `/clear` is a CLI construct that agents emit as plain text, never executed (#3270)
- `pnpm.overrides` patches 9 transitive vulnerabilities (2 high `fast-uri`, 5 moderate `hono`/`ip-address`, 2 low `hono`/`qs`); `pnpm audit --audit-level moderate` now exits 0 (#3283)

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
