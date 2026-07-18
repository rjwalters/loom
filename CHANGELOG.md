# Changelog

All notable changes to Loom will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.10.10] - 2026-07-18

### Summary

Patch release hardening installer idempotency across reinstall/upgrade paths and refining the destructive-command guards. The `--quick` reinstall and config-merge paths now round-trip byte-stably (`.gitignore`, `.loom/config.json`, install metadata, the committed `CLAUDE.md` pointer), so repeated installs no longer strand edits or clobber consumer keys. Guards gain an opt-in repo-scoped `rm` mode with an ephemeral allowlist and a dedicated Loom-workflow guard module, while cloud-delete ASK prompts narrow to mutating verbs. Broken docs links in the vendored templates are corrected.

### Added

- **Opt-in `guards.rmScope=repo` mode with ephemeral allowlist** — per-project setting scoping `rm` guarding to the repo; ships off by default (byte-for-byte compatible). (#3617)
- **`cloudCli` toggle for cloud-delete guarding** — narrows the cloud ASK to mutating verbs and adds a per-project toggle; `ec2-terminate` downgraded from block to ask. (#3595)

### Fixed

- **Installer preserves an existing tuned guard hook on the quick-install path** — `install.sh` no longer unconditionally overwrites a consumer's customized hook on `--quick` reinstall, mirroring `install-loom.sh`'s preserve-unless-`--clean` semantics. (#3626)
- **`.loom/config.json` merge is idempotent from first install** and preserves consumer keys on reinstall. (#3621, #3602)
- **`--quick` reinstall no longer strands uncommitted `.gitignore` edits**; the install/uninstall round-trip is byte-idempotent. (#3589, #3591)
- **`--quick` reinstall preserves the staged split on pop and reconciles `install-metadata.json`.** (#3618)
- **Committed root `CLAUDE.md` pointer is self-sufficient.** (#3620)
- **Legacy `.gitignore` migration is byte-stable** and orphan markers normalized. (#3594)
- **`verify-install` scopes its manifest to Loom-owned files** and region-hashes `CLAUDE.md`. (#3603)
- **Reinstall stash guard scoped to Loom-owned paths.** (#3601)
- **Skill-router table gated on a route match and deduplicated per session**; machine-generated turns skipped, the methodology topic fallback anchored. (#3616, #3615)
- **Broken `WORKFLOWS.md` link corrected in the `CONFIGURATION.md` copies**; the vendored `.claude/README.md` migration link is now absolute. (#3614, #3613)

### Changed

- **Loom-workflow guards extracted into `guard-loom-workflow.sh`.** (#3607)
- **Role docs gain a partial-increment carve-out** so family/epic PRs don't auto-close their parent issue. (#3599)

## [0.10.9] - 2026-07-16

### Summary

Patch release hardening the destructive-command lifecycle guard against prose false-positives.

### Fixed

- **Lifecycle guard resolves the command word past `env NAME=value` assignments** so leading environment assignments no longer mask the guarded command. (#3587)
- **Lifecycle and cloud-delete patterns are segment-parsed** to stop prose from triggering false-positive denials. (#3585)

## [0.10.8] - 2026-07-15

### Summary

Patch release redefining `/sweep all` as an aggressive build-everything sentinel and fixing label creation on Quick installs.

### Changed

- **`/sweep all` redefined as an aggressive build-everything sentinel** — resolves the entire open backlog and drives each item toward a merged PR. (#3580)

### Fixed

- **Quick installs can create labels** — `sync-labels.sh` is now shipped in the install payload. (#3583)
- **`.claude/skills` ignored for co-install hygiene.** (#3579)

## [0.10.7] - 2026-07-15

### Summary

Patch release retiring the in-repo `/loom:release` skill in favor of the shared `/repo:release` command, expanding `/loom:sweep` with the `all` actionable-backlog sentinel and a resource-gated automatic wave-size default, and clearing a batch of worktree, hook, guard, and installer correctness bugs.

### Added

- **`/loom:sweep all` actionable-backlog sentinel** — resolves the open backlog and drives each issue through the lifecycle. (#3569)
- **Resource-gated automatic wave-size default for `/loom:sweep`** — an omitted `--builders-per-wave` resolves to a backend- and disk-aware wave size. (#3567)
- **Per-project opt-out for SQL DDL/DML guard blocking.** (#3562)
- **Content-gated cleanup of the retired `release.md` stray** on install and daemon init. (#3575, #3577)

### Fixed

- **Guard false-positive denials reduced** by replacing unanchored substring matching. (#3564)
- **Worktree default-branch detection** instead of hardcoding `origin/main`. (#3561)
- **`.loom-managed` sentinel written on all worktree re-invocation paths.** (#3560)
- **`worktree.sh --json` stdout kept pure via an fd-swap contract.** (#3556)
- **PreToolUse guard decisions include the required `hookEventName`.** (#3559)
- **`--quick` reinstall reconciles the index and scopes uninstall staging.** (#3557)
- **`merge-pr.sh` stops passing the gh-cached `--no-cache` flag to plain `gh`.** (#3555)
- **Dogfood: `.claude/commands` materialized as a real copy, not a symlink into `defaults/`.** (#3570)

### Changed

- **Retired `/loom:release` in favor of the repo's `/repo:release`.** (#3571)
- **Added the `/loom:help` command** describing the installed Loom commands. (#3558)
- **Recorded the LOOM-EXTENSION-POINT release-seams audit finding.** (#3574)
- **Dependency maintenance** — bump the all-dependencies group (2 updates). (#3544)

## [0.10.6] - 2026-07-11

### Summary

Patch release delivering the configurable-worktree-root feature end to end plus worktree ergonomics for JS monorepos. `LOOM_WORKTREE_ROOT` lets operators relocate worktrees outside the repo (#3538), with the daemon's terminal-destroy GC (#3539, #3542) and the Python `LoomPaths`/CLI surface (#3541) both honoring the override; nested-workspace `node_modules` symlinking (#3534) rounds out the worktree work. Release tooling, installer, and label fixes clear real breakage, and a transitive-dependency security bump keeps the Security Scan green.

### Added

- **Opt-in configurable worktree root (`LOOM_WORKTREE_ROOT`)** — operators can relocate Loom-managed worktrees outside `.loom/worktrees/` via environment variable. (#3538)
- **Nested workspace `node_modules` symlinking + configurable gitignored artifacts** — worktree creation now symlinks nested workspace `node_modules` directories and supports a configurable list of gitignored artifacts to carry into new worktrees. (#3534)

### Fixed

- **Daemon terminal-destroy GC honors the worktree-root override** — `destroy_terminal` resolves the override-aware worktree root instead of assuming `.loom/worktrees/` (#3539), guards removal with the `.loom-managed` sentinel, and preserves the caller's environment (#3542).
- **`LoomPaths` and GC/CLI sites honor `LOOM_WORKTREE_ROOT`** — the Python tooling (`loom-clean`, orphan recovery, daemon cleanup) resolves the same override-aware root as the daemon. (#3541)
- **`version.sh do_tag` stages `CHANGELOG.md`** — the changelog promotion now ships in the tagged release commit instead of being left unstaged; this release is the first cut with the fix in place. (#3535)
- **Installer preserves consumer CLAUDE.md content on single legacy signature** — the upgrade path no longer discards a consumer's custom CLAUDE.md content when exactly one legacy Loom signature is present. (#3533)
- **`loom:operator-only` label description fits GitHub's 100-char cap** — label sync no longer fails on the over-length description. (#3532)

### Changed

- **Prometheus comparison note** — documents adopt/reject/spike verdicts from a Prometheus-orchestration comparison review. (#3531)

### Security

- **Bump `crossbeam-epoch` 0.9.18 → 0.9.20 (RUSTSEC-2026-0204)** — clears the `cargo audit` denial for the invalid pointer dereference in the `fmt::Pointer` impl that was failing the Security Scan. (#3543)

## [0.10.5] - 2026-06-30

### Summary

Patch release hardening the install path and the `/loom:release` tooling. Two installer correctness fixes (the `loom-shepherd` sentinel that made every install report failure, and non-portable in-place `sed` edits) plus a transitive-dependency security bump clear real breakage; the release skill gains two more version-tool backends and a pre-bump drift gate; routine dependency maintenance lands alongside.

### Added

- **`/loom:release` cargo-set-version + cargo-workspace fallbacks** — the version-tool detector now supports `cargo set-version` (cargo-edit) and a no-external-tool `cargo-workspace` direct-edit fallback for `[workspace.package]` repos. (#3510)
- **`/loom:release` Phase 2a.5 drift gate** — a pre-bump consistency check that fails fast when the manifest set has drifted, preventing a mis-delta'd version file from shipping in a tagged release. (#3508 / PR #3511)

### Fixed

- **Installer no longer gates on the removed `loom-shepherd` binary** — `setup-python-tools.sh` repoints its install sentinel from the deleted `loom-shepherd` to `loom-status`, so `--check`, the idempotency fast-path, and post-install verification stop reporting failure on a successful editable install. (#3520 / PR #3521)
- **Portable in-place `sed` edits; installer CI on ubuntu** — replaces non-portable `sed -i` usage and runs the installer integration tests on ubuntu. (#3516)

### Changed

- **`/loom:release` seam-contract clarification** — records the `pre-changelog-style` seam's phase scope (Phase 1.5 AND Phase 4) and documents the augment-vs-replace prose-prefix convention for seam overrides. Purely additive; no seam renames. (#3509)
- **Builder absolute-path discipline + `check-main-clean.sh` backstop** — documents the capture-the-worktree-path-once rule and adds a post-builder main-clean backstop to catch worktree contamination. (#3514)
- **Dependency maintenance** — bump `actions/checkout` 6 → 7 across all workflows (#3517) and the Cargo `all-dependencies` group: `tower-http` 0.6 → 0.7 plus `log`, `env_logger`, and `uuid` lockfile updates (#3523).

### Security

- **Bump `anyhow` 1.0.102 → 1.0.103 (RUSTSEC-2026-0190)** — clears the `cargo audit` "unsound" denial for `Error::downcast_mut()` that was failing the Security Scan. (#3522)

## [0.10.4] - 2026-06-16

### Summary

Patch release tying up two `/loom:release` follow-ups from v0.10.3 and seeding the first concrete step on the topics-injection-vs-procedural-overrides design space (#3503). Two fixes restore correctness — Phase 1.5's phantom `MISSING entries: (?, ...)` line on bash 3.2, and `install.sh --quick`'s dropped metadata/skill-routes/CLAUDE.md substitution — plus one additive seam mechanism (Option B) so projects can layer procedural overrides on top of the default `/loom:release` skill without forking it.

### Fixed

- **Phase 1.5 phantom MISSING entry on bash 3.2** — adds a single-line `[ -n "$_t" ] || continue` defensive guard at array population in `defaults/.claude/commands/loom/release.md`. Eliminates the bash-3.2 timing-sensitive `< <(...)` process-substitution boundary failure that surfaced `MISSING entries: (?, N commits)` during the v0.10.3 release flow. Real gaps are still detected (verified on synthesized CHANGELOG with `## [0.10.2]` block removed). (#3501 / PR #3504)
- **`install.sh --quick` emits metadata, skill-routes, and CLAUDE.md substitution** — the fast install path now produces the three artifacts the upgrade detector / skill-router / template substituter expect: `install-metadata.json` (consumed by `scripts/install-loom.sh:763` and the uninstaller), `config/skill-routes.json`, and CLAUDE.md `{{LOOM_VERSION}}` / `{{LOOM_COMMIT}}` substitution (via a new `prepare_loom_metadata_env` helper that exports the vars before `loom-daemon init`). Shared `finalize_quick_install` helper invoked from both the reinstall and fresh-install branches. `verify_install` now also warns on surviving `{{...}}` placeholders or the literal `Loom Version: unknown` line so the regression class trips immediately next time. (#3502 / PR #3505)

### Added

- **`/loom:release` skill seams (Option B)** — annotates `defaults/.claude/commands/loom/release.md` with five named `<!-- LOOM-EXTENSION-POINT: <name> -->` HTML-comment markers at well-chosen phase boundaries that project-side topics files can target for procedural overrides: `pre-changelog-style` (Gap 3: CHANGELOG style override), `pre-push` (Gap 4: irreversibility prompt), `post-push` (Gap 1: multi-workflow trigger gate), `pre-github-release` (Gap 2: ordering enforcement), and `post-summary` (project-specific follow-ups). Adds a new "Operator extension points" doc section listing every seam, a new "scripts/version.sh interface" section documenting the subcommands the skill dispatches (`bump`, `set`, `list`, `check`, `--tag`) so projects with pre-existing forks know what to support, and an example topics file at `defaults/hooks/example-context/topics/release.md`. Markers are HTML comments so they render invisibly in the prose. No procedural content of the skill was changed. Options A (phase-extension files) and C (`@override`/`@inject` directives) from #3503's proposal sketch remain deferred. (#3503 / PR #3506)

## [0.10.3] - 2026-06-16

### Summary

Two quality-of-life improvements to `/loom:release` that compound the v0.10.2 generalization work — the skill now catches missed CHANGELOG entries before they accumulate, and it dispatches to the host repo's preferred version-bumping tool instead of always invoking the bundled `scripts/version.sh`. Both changes are skill-prose only; no binary or wire-protocol changes.

### Added

- **Phase 1.5 CHANGELOG completeness gate** — release skill now checks the last N (default 5) tags against `^## \[X.Y.Z\]` headers in `CHANGELOG.md` before cutting the next release. Detected gaps surface a three-way `[b]ackfill / [c]ontinue / [a]bort` prompt; `--yes` non-interactive mode prints a stderr warning and continues without blocking. No-op when `CHANGELOG.md` is absent (Phase 4 bootstrap still handles young repos). Catches the "we shipped v0.10.0 and v0.10.1 without their CHANGELOG blocks" failure mode the v0.10.2 backfill uncovered. (#3497 / PR #3499)
- **Phase 2a version-tool detection** — release skill now probes the host repo for an existing version-bumping tool before invoking the bundled `scripts/version.sh`. Detection order (first match wins): `./scripts/version.sh` → `cargo-release` → `bumpversion`/`bump2version` → `poetry` → `npm`. The detected tool is surfaced to the operator before bumping; the no-tool case asks for explicit operator direction rather than silently falling through. Bump command syntax dispatches per tool. Loom's own release flow is unchanged since `./scripts/version.sh` is first in the order. (#3498 / PR #3500)

## [0.10.2] - 2026-06-15

### Summary

A small patch release with one critical fix and one downstream-quality improvement. **#3492 is the urgency driver**: reinstalling Loom over a repo previously installed with a pre-#3450 version silently deleted consumer-authored files under `.claude/` (the on-disk manifest was over-broad, and the reinstall/prune path trusted it). The fix intersects deletion candidates against Loom's *current* `defaults/` ownership boundary before deleting, so any path Loom no longer ships is preserved with a warning. Anyone on 0.10.0/0.10.1 should upgrade promptly.

The other notable change generalizes the `/loom:release` skill so downstream repos don't have to fork it on install, and adds an installer migration prompt that handles customized `release.md` files.

### Fixed

- **Reinstall preserves consumer-authored files** — both the install-time stale-file sweep (`scripts/install-loom.sh`) and the uninstall-time hard-delete loop (`scripts/uninstall-loom.sh`) now intersect each deletion candidate against the current Loom ownership boundary (`defaults/` enumeration + `defaults/.loom-internal.list`). Any path Loom's *current* defaults do not ship is preserved with a `preserving <path> (not owned by current Loom defaults/; likely consumer-authored, captured by pre-#3450 manifest)` warning. Resolves the destructive-loss class that #3450 only fixed at manifest *generation* time. Shared `_emit_loom_ownership_set` helper in `scripts/install/manifest.sh` keeps both call sites in sync. Also fixes a latent `find` → `find -L` bug so symlinked role files at `defaults/roles/*.md` are correctly included in the ownership set. (#3492 / PR #3494)

### Added

- **`./scripts/version.sh list` subcommand** — emits the canonical `VERSION_FILES` array one path per line so the release skill (and any other tooling) can discover the version-bearing file set without hardcoding. (#3495 / PR #3496)
- **`scripts/install-loom.sh` migration prompt for customized `release.md`** — detects when the on-disk `defaults/.claude/commands/loom/release.md` diverges from the canonical shipped version and prompts `[y/N/d=show diff]` (default N = preserve). `--yes` preserves silently; `--force` replaces silently. Snapshots the customized file *before* `loom-daemon init` overwrites it. Composes with the #3492 ownership-boundary work. (#3495 / PR #3496)

### Changed

- **Generalized `/loom:release` skill** — `defaults/.claude/commands/loom/release.md` is now project-agnostic: uses `{{workspace}}` for project name (no more hardcoded "Loom"), discovers version-bearing files via `./scripts/version.sh list` (resolves the prior 7/5/5/5 prose inconsistency), degrades gracefully when `.github/workflows/` is absent (clean-main gate instead of empty CI output), detects `.github/workflows/release.yml` before claiming the GitHub Release triggers a build, drops Loom-specific semver examples (`ForgeClient` / MCP / daemon references) in favor of generic categories, and adds a CHANGELOG.md bootstrap path for young repos. Loom's own release flow continues to work via dogfooding; downstream consumers (e.g. Anvil) no longer need to fork the skill. (#3495 / PR #3496)
- **`release.md` now ships to consumer installs** — removed from `defaults/.loom-internal.list` so every downstream install gets a working release skill out of the box. (#3495 / PR #3496)

### Build

- Bump the cargo `all-dependencies` group with 2 updates. (Dependabot, PR #3493)

### Tests

- 9 new installer tests (Tests 53–54 for the ownership-boundary intersection, Section 11 / Tests 55–63 for the four operator-flag combinations of the customized-`release.md` migration prompt). 109/109 installer tests pass.

## [0.10.1] - 2026-06-13

### Summary

A patch release dominated by the **model-selection plumbing** (#3477 / #3481 / #3482) and a cluster of install/upgrade hardening. Model selection lands as a first-class orchestration concern: a fixed precedence chain (explicit dispatch param → workspace `roleConfig.model` → role `suggestedModel` → session default), deterministic escalation on Judge rejection (`sweep.escalation` ladder), and per-model observability across spawn, daemon, checkpoint, and metrics surfaces. Several install regressions hit by operators upgrading from v0.7.x are fixed, plus Gitea support and unstable-state fallbacks in `merge-pr.sh`.

### Added

- **Model-selection Phase 1 (#3477 / PR #3479)** — fixed precedence chain across `/loom:sweep`, the Rust `loom-daemon`, `spawn-claude.sh`, and `claude-wrapper.sh`. Subagent role workers receive an explicit `model` param via the Task tool when any tier above session default resolves. Aliases (`sonnet`/`opus`/`haiku`) and pinned IDs (e.g. `claude-sonnet-4-6`) are both valid at every tier. Static per-role `suggestedModel` mapping ships in `.loom/roles/*.json` (Builder/Judge/Architect = opus; Curator/Doctor/Hermit/Champion/Guide = sonnet).
- **Model-selection Phase 2 (#3481 / PR #3484)** — deterministic escalation on Judge rejection. When the Judge requests changes and `/loom:sweep` dispatches a Doctor, the Doctor's model escalates one rung up the `sweep.escalation` ladder (default `["sonnet", "opus"]`) instead of resolving through tier 3/4. Tier-1/tier-2 pins always win. Composes with the single Doctor→Judge cycle cap. Mode C inherits the same rule.
- **Model-selection Phase 3a (#3482 / PR #3485)** — per-model observability. Spawn-claude logs the resolved model, the daemon records it on each sweep, `sweep-checkpoint.sh write` accepts `--model <resolved>`, and the metrics pipeline groups by model. Absent fields read cleanly on legacy checkpoints.
- **Gitea support in `merge-pr.sh --auto` UNSTABLE-fallback** (#3488 / PR #3489) — the immediate-merge fallback path is now forge-agnostic.

### Fixed

- **`loom-daemon init` regression guards** — adds a docs subdir regression test and embeds commit / build time in `--version` output for downstream debugging. (#3470 / PR #3472)
- **`loom-clean` silently skipped stale branches** — three compounding bugs caused stale local branches for closed issues to remain. (#3473)
- **Install upgrade from v0.7.1 → v0.10.0** — hybrid CLAUDE.md placeholders (some `{{INSTALL_DATE}}`, some already-substituted) confused the upgrade-time docs sync. Also missing docs sync source path. (#3476 / PR #3478)
- **`.github/` install sweep inverted to allowlist** — the previous carve-out was a denylist that missed consumer files added under conventions Loom doesn't know about (custom workflows, `dependabot.yml`, project-specific actions). Inverted to an allowlist of just the Loom-shipped paths so consumer additions survive. (#3480 / PR #3483)
- **`merge-pr.sh` immediate-merge fallback on UNSTABLE** — when failing required checks are non-required (e.g. optional CI jobs marked failure), the script now transparently merges instead of blocking on the auto-merge queue. (#3486 / PR #3487)
- **`loom-daemon init` copies `defaults/.loom/bin/`** — the workspace bootstrapping step missed `defaults/.loom/bin/` so workspace-local helper binaries were absent. (#3490)
- **Release workflow installs both Apple targets explicitly** (#3491) — `aarch64-apple-darwin` + `x86_64-apple-darwin` so both binaries always ship.

### Build

- Bump `actions/checkout` from 4 to 6. (Dependabot, PR #3474)
- Bump the cargo `all-dependencies` group with 3 updates. (Dependabot, PR #3475)

## [0.10.0] - 2026-06-05

### Summary

The major architectural milestone signaled by the 0.9.1 deprecation cycle: **deletion of the shepherd brain (`loom-tools/src/loom_tools/shepherd/`), the Python daemon brain (`loom-tools/src/loom_tools/daemon_v2/`), and the `/shepherd` slash command** (epic #3372), paired with the **Rust `loom-daemon` rebuild** (epic #3449). The replacement architecture is two-surface: `/loom:sweep` for in-session subagent dispatch (Tier 1) and the Rust `loom-daemon` binary for multi-account MCP-level dispatch (Tier 2). Six MCP tools (`dispatch_sweep`, `list_sweeps`, `get_sweep_status`, `cancel_sweep`, `tail_sweep_log`, `subscribe_to_events`, `publish_event`, `tail_event_bus`) cover dispatch, monitoring, pub/sub eventing, and cancellation. A frozen 6-topic event taxonomy ships for v0.10.0; new topics require a follow-up issue.

`/loom:sweep` grows a **Stage -1 backend detection** that probes for the daemon and a multi-account token pool — strict AND — and delegates to the daemon when both are present, falling back to in-process subagent dispatch otherwise. Mode C (`--prs`) and `--no-daemon` short-circuit to subagent. Solo-token operators see no behavior change.

Around the deletion: `defaults/scripts/spawn-loop.sh` (the minimal multi-account orchestrator from v0.9.1) is soft-deprecated with a stderr warning per invocation; deletion is queued for v0.11.0. A new `/loom:bump` skill provides a project-agnostic version-bump + tag flow for consumer projects.

Several install/upgrade fixes land in the same release: stale-file sweep on upgrade, consumer `CLAUDE.md`/`.gitignore`/`.github/` preservation, Loom-internal skill exclusion from consumer installs.

**v1.0.0 is intentionally unscheduled.** Loom remains pre-1.0 while the architecture settles.

### Added

- **`loom-daemon` Phase A — `Sweep` resource + `DispatchSweep` MCP tool** (#3452 / PR #3459) — in-memory sweep registry (HashMap keyed by issue number), `mcp__loom__dispatch_sweep` enqueues a sweep that fork+execs `claude -p "/loom:sweep N"` via `spawn-claude.sh` (token rotation at process-spawn boundary), `mcp__loom__list_sweeps` enumerates the registry, reaper task sweeps dead PIDs every 30s.
- **`loom-daemon` Phase B — pub/sub event bus** (#3453 / PR #3460) — tokio `broadcast::channel<Event>` (capacity 1024). Sweep children publish via `Request::PublishEvent { topic, payload }`. Six frozen topics: `sweep.issue.{N}.phase`, `sweep.issue.{N}.blocker`, `sweep.issue.{N}.exited`, `sweep.issue.{N}.crashed`, `sweep.global.dispatch`, `sweep.global.completed`. Pass-through overflow on slow subscribers (synthetic `topic_lag` event), no silent drops.
- **`mcp-loom` Phase C — sweep monitoring + subscription MCP tools** (#3455 / PR #3463) — `get_sweep_status`, `tail_sweep_log`, `subscribe_to_events`, `publish_event`, `cancel_sweep` (SIGTERM → grace → SIGKILL), `tail_event_bus`. `.loom/docs/daemon-reference.md` rewritten to document the wire protocol, registry semantics, and reaper task.
- **`/loom:sweep` Phase D — backend detection (Stage -1)** (#3454 / PR #3462) — strict-AND probe for daemon reachability (500ms Ping timeout) and multi-account token pool (`.loom/tokens/*.token` count ≥ 2 OR `.env` `ACCOUNT_KEY_*` count ≥ 2). Both succeed → delegate to daemon via `dispatch_sweep`, exit sub-2-second. Either fails → fall through to in-process subagent dispatch (existing Mode A/B/C lifecycle, no behavior change). `--no-daemon` forces subagent unconditionally; Mode C always uses subagent.
- **`/loom:bump` skill** (#3468 / PR #3469) — generic version-bump + tag flow for consumer projects. Independent of the `/loom:release` skill (which handles CHANGELOG / GitHub Release creation in addition to the bump).
- **Curator: verify Affected Files against `origin/main`** (#3418 / PR #3447) — curated "Affected Files" lists are now checked against the actual repo state to catch references to deleted, moved, or renamed files before they reach the Builder.
- **Stale-file sweep on installer upgrade** (#3431) — `scripts/install-loom.sh` now removes files from the previous install's manifest that the current `defaults/` no longer ships. Paired with the carve-outs documented under v0.10.1's `.github/` allowlist fix.
- **`rust-toolchain.toml` pinning Rust 1.96.0** (#3427) — fixes `cfg_select!` breakage and adds rustfmt/clippy components (#3446).

### Changed

- **Two-surface orchestration architecture** — `/loom:sweep` (Tier 1, in-session) and `loom-daemon` (Tier 2, MCP-level multi-account dispatch). Periodic support roles (Champion, Curator, Judge, Auditor, Guide) run on GitHub Actions cron under `.github/workflows/loom-*.yml` (shipped in v0.9.1, opt-in by default).
- **Architecture narrative rewrites + ADR-0009** (#3435) — `CLAUDE.md`, `defaults/CLAUDE.md`, and `.loom/docs/` rewritten around the post-deletion architecture. ADR-0009 records the shepherd/daemon-Python-brain deprecation decision.
- **`v0.10.0` daemon-rebuild migration guide** (#3457 / PR #3466) — `docs/migration/v0.10.0-shepherd-deprecation.md` documents the deletion narrative, per-CLI breaking changes, and the replacement surfaces.

### Deprecated

- **`defaults/scripts/spawn-loop.sh`** (#3456 / PR #3465) — soft-deprecated as of Phase E of #3449. Emits a stderr warning on every `start` / `status` / `stop` invocation. Suppress with `LOOM_SUPPRESS_DEPRECATION=1`. Deletion queued for v0.11.0. The replacement is `mcp__loom__dispatch_sweep` against `loom-daemon`.

### Removed

- **`loom-tools/src/loom_tools/shepherd/` (shepherd brain + `/shepherd` slash command + milestone writers)** (#3433, BREAKING) — the per-issue orchestrator deleted. Replacement: `/loom:sweep <issue>`.
- **`loom-tools/src/loom_tools/daemon_v2/` (Python daemon brain + producer shell scripts)** (#3432, BREAKING) — the work-generation and pool-management brain deleted. Replacement: `mcp__loom__dispatch_sweep` (operator-driven enqueue) + GitHub Actions cron workflows (support roles).
- **`daemon-state.json` fallback paths in 9 ported CLIs** (#3434) — the v0.9.1 ports of `loom-status`, `loom-backlog`, `loom-stuck-detection`, etc. carried fallback reads from `daemon-state.json` during the deprecation window. All trimmed; the spawn-loop + forge are now the only data sources.
- **Dead shepherd-progress logic from `_is_claim_abandoned()`** (#3440).
- **Dead `DEFAULT_HEARTBEAT_STALE_THRESHOLD` constant** (#3441 / PR #3448).

### Fixed

- **Preserve consumer-owned `CLAUDE.md`, `.gitignore`, `.github/` on reinstall** (#3450 / PR #3461) — narrow carve-outs in the install-time stale-file sweep so consumer-authored files at these roots survive upgrades. (Generation-time fix; the *consumption-time* fix lands in v0.10.2.)
- **Exclude Loom-internal skills from consumer install** (#3464 / PR #3467) — `defaults/.loom-internal.list` enumerates skills meant for the Loom-source repo only (architect-patterns, hermit-patterns, etc.) so consumer installs don't get cluttered with them.
- **`scripts/install-loom.sh`: reject unknown flags** (#3429) — previously silently ignored. Now fails fast with a clear error.
- **`scripts/install-loom.sh`: `rm -rf` worktree dir after `git worktree remove`** (#3426) — Git's worktree remove leaves a stub directory under some conditions; the cleanup path now follows up with the directory removal.
- **`validate_phase`: replace stale `loom-shepherd.sh` references with `/loom:sweep`** (#3439).
- **CI: add `rust-toolchain.toml` to backend paths-filter** (#3444) so toolchain changes trigger the relevant Rust jobs.

### Tests

- **Installer upgrade-path tests for the stale-file sweep (Tests 42-44)** (#3442).
- **Installer flag-rejection tests for the unknown-flag guard (Tests 45-47)** (#3443).

## [0.9.1] - 2026-06-04

### Summary

A bridge release toward v0.10.0. Loom 0.9.1 ships the **soft-deprecation infrastructure** for the upcoming **shepherd removal** (epic #3372): the `loom-shepherd` CLI, the `/shepherd` Claude Code slash command, and the Python `daemon_v2/` brain now emit a one-shot stderr warning pointing at their replacements. A comprehensive `docs/migration/v0.10.0-shepherd-deprecation.md` guide lands ahead of the deletions. **Nothing is removed yet** — every deprecated component continues to function identically, so downstream consumers (notably sphere) get a full release cycle to migrate.

**Daemon mode is preserved going forward.** The Python daemon brain (`loom_tools/daemon_v2/`) is on the v0.10.0 chopping block, but the shell-level daemon surface (`./.loom/scripts/daemon.sh` + tmux session runner + multi-account token rotation) survives — re-implemented around the spawn loop and GitHub Actions cron in v0.10.0. The architectural reason: Claude Code subagents inherit the parent's `CLAUDE_CODE_OAUTH_TOKEN`, so multi-account rotation only works at process-spawn boundaries. The daemon's tmux-launched-separate-sessions surface is the long-running counterpart to `/loom:sweep`'s subagent dispatch — both are first-class execution surfaces post-v0.10.0.

Around the deprecation framing, this release ships the **replacement substrate** — `./.loom/scripts/spawn-loop.sh` as a minimal multi-account orchestrator (#3374), scheduled support-role workflows under `.github/workflows/loom-*.yml` (#3375), and `/loom:sweep` matures with checkpoint/resume (#3373), Mode C PR-set lifecycle (#3417), `loom:operator-only` skip semantics (#3362), and existing-PR pre-flight routing (#3361). Eight of the nine `loom-*` operator CLIs are ported from `daemon-state.json` to spawn-loop + forge data sources (the ninth, `loom-daemon-cleanup`, is renamed to `loom-cleanup`). Several worktree/merge correctness fixes round it out.

### Added

- **Spawn-loop orchestrator** — `./.loom/scripts/spawn-loop.sh` is a minimal alternative to the full daemon for multi-account `/loom:sweep` launching. Polls `loom:issue`, atomically claims ready issues via `.loom/locks/issue-<N>/`, and detaches `claude -p "/loom:sweep N"` per issue with its own OAuth token via `spawn-claude.sh`. No work generation, no support-role triggers, no pool-slot bookkeeping. State at `.loom/spawn-loop-state.json`, logs at `.loom/logs/spawn-loop.log`. Crashed children with surviving checkpoints (#3373) are re-queued on the next tick. Opt-in via `LOOM_USE_SPAWN_LOOP=1`. (#3374 / PR #3385)
- **Scheduled support-role GitHub Actions workflows** — `.github/workflows/loom-*.yml` provide a daemon-free way to run the periodic support roles (Champion, Curator, Judge, Auditor, Guide) on cron schedules. Each workflow checks out the repo, installs the Claude CLI, and runs `claude -p "/<role>" --dangerously-skip-permissions` for one tick. **Disabled by default** — every workflow ships with its `schedule:` block commented out so forks don't burn Actions minutes accidentally. Activation requires a `CLAUDE_API_KEY` repo secret and uncommenting the `- cron:` lines. (Phase 2a of #3372, #3375 / PR #3386)
- **Soft-deprecation infrastructure** — `loom-shepherd`, the `/shepherd` slash command, and the Python `daemon_v2` brain now emit a one-shot stderr `⚠️  DEPRECATED` block on invocation pointing at replacements. **No behavior change**: every component continues to function identically. Two helpers ship the warning text: Python (`loom_tools.common.deprecation.warn_deprecated`) and shell (`.loom/scripts/lib/deprecation.sh`, safe to source). Suppress with `LOOM_SUPPRESS_DEPRECATION=1` (Python + shell entry points only — the markdown `/shepherd` skill warning always renders by design). The `./.loom/scripts/daemon.sh` warning that shipped under this PR will be withdrawn in v0.10.0, since the shell-level daemon surface is preserved. (#3376 / PR #3387)
- **`/loom:sweep` checkpoint/resume** — per-issue phase checkpoint at `.loom/sweep-checkpoint/issue-<N>.json` so a killed-and-relaunched sweep can pick up where it left off. Schema: `{phase, task_id, timestamp, pr_number?}` with `curator-done` / `builder-done` / `judge-done` / `doctor-done` / `merge-done` states. Helper script `.loom/scripts/sweep-checkpoint.sh {write|read|phase|exists|delete|list}` with atomic writes. Stale-checkpoint cleanup on entry. No mid-builder recovery — kill during Builder resumes at builder start, worktree preserved by `worktree.sh` idempotency. (Phase 0 of #3372, #3373 / PR #3383)
- **`/loom:sweep` Mode C — PR-set lifecycle** — drive Judge / Doctor → Judge / Merge from an open-PR set without re-running Curator or Builder. Two trigger forms: explicit `--prs` flag (`/sweep --prs 100 101`), or NL phrase ("all open `loom:pr`"). Size-1 waves; reuses the issue-keyed checkpoint helper via `closingIssuesReferences`. (#3384 / PR #3417)
- **`loom:operator-only` label** — pre-flight skip in `/loom:sweep` for issues requiring human action outside automation (credential rotations, infra ops, manual deploys, hardware access). Champion `--merge` mode also refuses to auto-promote them. (#3360 / PR #3362)
- **Existing-PR detection in `/loom:sweep` pre-flight** — probes `closedByPullRequestsReferences` and routes single open linked PRs to Judge (or Merge if already `loom:pr`) instead of dispatching a duplicate Builder. Multi-PR ambiguity is logged and skipped (human-attention case). (#3359 / PR #3361)
- **Phase 4 sphere-downstream migration banners** — added soft-deprecation banners to `defaults/.claude/commands/loom/shepherd.md` and the deprecated subagent files, plus a "Migration: deprecations targeted for v0.10.0" section in both `CLAUDE.md` and `defaults/CLAUDE.md` so the next downstream install surfaces the migration narrative without operator intervention. (#3382 / PR #3404)

### Refactored

- **Eight `loom-*` CLIs ported from `daemon-state.json` to spawn-loop + forge data sources** (Phase 3.1.x of #3372). Each port preserves CLI output shape and continues to honor `daemon-state.json` as a fallback during the 0.9.x deprecation window:
  - `loom-status` → `spawn-loop-state.json` + forge with daemon-state fallback (#3390 / PR #3407)
  - `loom-backlog` → `issue-failures.json` only (#3391 / PR #3408)
  - `loom-stuck-detection` → `spawn-loop-state.json` heartbeats (#3392 / PR #3411)
  - `loom-completions` → spawn-loop output-file paths (#3393 / PR #3410)
  - `loom-agent-metrics` → `activity.db` only (dropped `daemon-state.json` fallback) (#3394 / PR #3409)
  - `loom-orphan-recovery` → spawn-loop tasks + forge cross-check (#3395 / PR #3414)
  - `loom-health-monitor` → forge + spawn-loop inputs (#3397 / PR #3413)
  - `loom-clean` → spawn-loop claim-set (`.loom/locks/issue-<N>/`) (#3398 / PR #3415)

### Renamed

- `loom-daemon-cleanup` → `loom-cleanup` (CLI rename, drops session-rotation events that were specific to the Python daemon brain). (#3396 / PR #3412)

### Fixed

- **Worktree creation serialization** — `./.loom/scripts/worktree.sh` now serializes concurrent invocations for the same issue and cleans up partial state on failure, preventing the duplicate-worktree / locked-branch corruption pattern observed under parallel `/loom:sweep` waves. (#3380 / PR #3416)
- **`merge-pr.sh` fall-through to immediate merge when PR is CLEAN** — `--auto` queues via GitHub's server-side auto-merge, but on PRs already in `CLEAN` state the script now transparently falls back to an immediate merge instead of blocking on a queue that has nothing to wait for. (#3371 / PR #3379)
- **`/loom:sweep` Mode B unknown-label guard uses live repo label set** — instead of consulting only `.github/labels.yml` (which is the Loom-managed subset and misses labels added via the GitHub UI or Dependabot), the orchestrator queries `gh label list` once per sweep invocation as the source of truth, falling back to the YAML only on `gh` failure. (#3370)
- **`merge-pr.sh --worktree-path` override + porcelain warn-only fallback** — surface worktree-detection failures without aborting the merge. (#3364 / PR #3367)
- **Doctor isolates external-fork PRs in `pr-<N>` worktrees** — prevents external-fork doctor cycles from contaminating the issue's main worktree. (#3363)
- **`loom-daemon` post-init retires daemon-brain `.gitignore` patterns** — removes stale ignore entries that referenced now-defunct daemon-brain artifacts. (#3406)

### Changed (docs)

- **Documentation sweep retiring `/shepherd`, `loom-daemon`, `daemon-state.json`, and `.loom/progress/` references** across `CLAUDE.md`, `defaults/CLAUDE.md`, `.loom/docs/`, `docs/guides/`, and READMEs in favor of `/loom:sweep`, the spawn-loop, and the GitHub Actions cron workflows. `.loom/docs/daemon-reference.md` is now a deprecated-stub pointing at the spawn-loop replacements. Adds **`docs/migration/v0.10.0-shepherd-deprecation.md`** — the authoritative migration guide for the v0.10.0 release. Historical ADRs and CHANGELOG entries are intentionally preserved. (Phase 3.6 of #3372, #3403 / PR #3419)
- **Phase 3.6.5 reconciliation** — rewrites the migration narrative across CLAUDE.md, defaults/CLAUDE.md, the migration guide, and seven other docs to reflect that **daemon mode (`./.loom/scripts/daemon.sh` + tmux + multi-account token rotation) is preserved**. The shepherd surface and the Python daemon brain are still removed in v0.10.0; the shell-level daemon launcher is re-implemented around the spawn loop. Renames the migration guide to `v0.10.0-shepherd-deprecation.md`. Deletes the stale `## [1.0.0] - unreleased` CHANGELOG placeholder (v1.0.0 is now unscheduled). The architectural framing the rewrite captures: Loom supports **both** subagent dispatch (one Claude Code session, shared OAuth token, fast iteration) and tmux-launched separate Claude Code sessions (multi-process, rotated tokens, multi-day runtime), because token rotation only works at process-spawn boundaries. (Phase 3.6.5 of #3372, #3420 / PR #3421)
- **`docs/migration/daemon-state-consumers.md`** — inventory of all `daemon-state.json` and `.loom/progress/` consumers across the codebase, with category tags and Phase 3 PR sequencing recommendations. (Phase 2c of #3372, #3377 / PR #3389)
- **`docs/design/architect-hermit-cadence.md`** — design doc covering the architect/hermit work-generation cadence after the Python brain is removed (Phase 2d follow-up, #3381). (#3388)
- **Curator/architect playbook entries for multi-phase sweeps** — added to `.loom/roles/curator.md` and `.loom/roles/architect.md`. (#3365 / PR #3366)

### Build

- Dependabot bump: 3 dependencies updated. (#3368)

## [0.9.0] - 2026-05-28

### Summary

The Tauri desktop GUI is removed. Loom is now **CLI + daemon only** — ~77k LOC and the entire JS toolchain go away in a single coherent change (#3353). Around that headline change, this release hardens Loom for **Claude Code 2.1+** (the namespaced `/loom:<role>` resolver is now the only form Loom emits, the per-agent `CLAUDE_CONFIG_DIR` actually sees the project's commands, and the bypass-permissions modal gets auto-accepted post-Tauri-removal) and ships an **opt-in post-build quality gate** that catches "no commits / no real changes / build broken" before a PR opens. Plus a warn-only host-sleep readiness check that prevents long sweeps from dying overnight when the laptop suspends.

### Breaking changes

- Removed the Tauri desktop application surface entirely. Loom is now CLI + daemon only. Removed: `src-tauri/`, `src/`, the entire JS toolchain (`tsconfig*.json`, `vite.config.ts`, `vitest.config.ts`, `playwright.config.ts`, `biome.json`, `tailwind.config.js`, `postcss.config.js`, `pnpm-lock.yaml`, `pnpm-workspace.yaml`, `index.html`), GUI dev scripts (`scripts/dev-app*.sh`), `e2e/`, and the `e2e.yml` workflow. Decoupled mcp-loom log paths from Tauri, dropped `src-tauri` from the Cargo workspace members, rewrote `.github/workflows/release.yml` to publish `loom-daemon` binaries instead of DMGs, and trimmed `scripts/version.sh` from 7 to 5 version-bearing files (dropped `src-tauri/tauri.conf.json` and `src-tauri/Cargo.toml`). Existing DMG release artifacts remain available in prior GitHub releases as historical artifacts. The project name "Loom" is unaffected — only the desktop-app surface goes. Closes #3330 (PR #3353)

### Added

- Post-builder quality gate (orchestrator-side, deterministic) that runs after the builder agent exits but **before `gh pr create`**. Three checks: has-commits (`git rev-list --count origin/main..HEAD > 0`), has-real-changes (configurable file globs), build-passes (configurable command, bounded timeout). On any failure: releases the `loom:building` claim back to `loom:issue`, logs an `error` milestone (`reason=build_failed_post_builder`), and returns FAILED so the shepherd does not advance to Judge. Opt-in via `.loom/config.json` `buildGate` block — repos without that config see zero behavior change. 24 new tests across 6 test classes cover the FAILED-path including claim-release verification. Closes #3347 (PR #3355)
- Host-sleep readiness check at startup of `/sweep`, `/loom`, and multi-issue `/shepherd`. New `defaults/scripts/check-host-sleep.sh` advisory helper — non-blocking (always exits 0), platform-aware (macOS parses `pmset -g`; Linux greps `systemd-inhibit --list`). Honest framing on macOS: recommends `sudo pmset -c sleep 0` or the sleep-manager "allow system sleep when display is off" toggle as the reliable fix, and explicitly warns that `caffeinate -dimsu` does NOT reliably defeat macOS Maintenance Sleep on Apple Silicon. Filed after an overnight sweep lost ~33 minutes of curator work to a Maintenance Sleep cycle. Closes #3350 (PR #3357)
- BypassPermissions modal auto-accept in the CLI/daemon spawn path. Restores functionality that disappeared with the Tauri removal (the equivalent JS-side logic in `src/lib/agent-launcher.ts` was deleted by #3353 without a Python-side replacement). Uses bounded polling on `tmux capture-pane` (15s timeout) against `BYPASS_PROMPT_MARKERS` rather than a hardcoded sleep; uses the module's `_tmux()` helper rather than a detached `subprocess.Popen`; gated behind `LOOM_AUTO_ACCEPT_BYPASS` env var (default ON, `=0` is a true no-op). External contribution from @jperla, polished via maintainer-takeover doctor cycle. 6 new tests in `TestAutoAcceptBypassPrompt` cover detection, polling, timeout, env-var disable, marker variants, and `capture-pane` error handling (PR #3348)

### Fixed

- Spawn paths now emit the namespaced `/loom:<role>` form everywhere. After #3176 moved commands into `.claude/commands/loom/*.md`, Claude Code 2.1+ requires `/loom:<role>` for subdirectory commands, but three spawn sites still emitted the bare `/<role>` form and would fail with `Unknown command: /<role>`. Patched the three sites the original report identified (`loom-tools/src/loom_tools/agent_spawn.py`, `loom-tools/src/loom_tools/agent_monitor.py`, `defaults/scripts/agent-wait-bg.sh`) plus 7 additional sites the same audit caught (`daemon.py`, `shepherds.py`, `skill-routes.json`, `enable-skill-routing.sh`, `.loom/bin/loom`, `loom-help.sh`, `loom-send.sh`, `defaults/.claude/README.md`), plus three detection-side regexes in `agent_monitor.py`, `agent-wait-bg.sh`, and `shepherd/phases/base.py` that accept both bare and namespaced echoes (so existing sessions don't break). Regression test added. Closes #3345 (PR #3352)
- Per-agent `CLAUDE_CONFIG_DIR` now links the project's `.claude/commands` and `.claude/agents` trees. After #3345 (now #3352) emitted the right command form, shepherds still failed with `Unknown command: /loom:shepherd` because their isolated config dir at `.loom/claude-config/shepherd-N` had no view of the project's slash-command tree. Both `setup_agent_config_dir` (Python, `loom-tools/src/loom_tools/common/claude_config.py`) and the Rust mirror in `loom-daemon/src/terminal.rs` now create absolute-path symlinks for `commands` and `agents`, refresh stale links (e.g. moved worktrees), and preserve operator-placed real directories. `validate_agent_config_dir` rejects bare config dirs so they auto-heal on next spawn. 10 new Python tests + 6 new Rust tests. **This unblocks fully autonomous `/loom` mode on Claude Code 2.1+** — without it, only `--no-auto-build` worked. Closes #3346 (PR #3356)

### Changed (docs)

- `defaults/.claude/commands/loom/builder.md` adds a `### Build-time performance` subsection near "Run quality checks". When a builder adds code called from the project's build pipeline (`pnpm build` / `cargo build` / equivalent), time it before pushing: downstream deploy scripts often wrap the build in `timeout` (e.g., lean-genius caps at 20 minutes in `scripts/deploy/sync-and-deploy.sh:570`). Sanity-check magnitude claims in the issue body against the actual repo state. Cautionary tale: lean-genius PR #20849 added a "recently updated" sort spawning ~2435 git log subprocesses against an issue body that said "~300 items" — 8× error, blew past the 20m cap, killed the production deploy. Closes #3343 (PR #3351)
- `defaults/.claude/commands/loom/judge.md` adds a complementary `### Performance` subsection — **"Build-time perf is load-bearing, not advisory"**. Three numbered checks (re-derive N from repo state, ~25% headroom threshold for blocking, local-build-passes ≠ deploy-passes). Same incident as #3351 but the Judge-side framing: a "several minutes added" non-blocking review note can translate directly into a failed production deploy. Closes #3344 (PR #3354)

## [0.8.1] - 2026-05-26

### Summary

Installer and worktree hardening. Headline: Loom no longer removes worktrees it didn't create — `merge-pr.sh` and `agent-destroy.sh` refuse to clean up any worktree lacking a `.loom-managed` sentinel file (written by `worktree.sh` at creation time), and `LOOM_PRESERVE_WORKTREE=1` is a session-wide opt-out. This fixes Anton's regression where Loom would delete editor-provisioned worktrees mid-session (#3334).

Around that core fix, five installer-side improvements ship: legacy root `CLAUDE.md` upgrade path (#3325), actionable `gitignore`-guard errors with file/line/pattern + suggested fix (#3326), source-state and target-state guards refusing installs from non-`main` branches or stale targets (#3327), active-session detection refusing to install on top of a live Loom (#3331), and docs-only markers on installer-generated PRs so target CI can opt to skip them (#3333). `loom-clean` gains an `--aggressive` mode for cleaning up vestigial locked worktrees (#3332), built on the same sentinel ownership model.

### Fixed

- Worktree cleanup honors a `.loom-managed` sentinel and `LOOM_PRESERVE_WORKTREE=1` opt-out. `worktree.sh` and the Tauri-app worktree creators write the sentinel; `merge-pr.sh` and `agent-destroy.sh` refuse to remove worktrees lacking it. CLAUDE.md documents the worktree ownership model explicitly (Loom owns under `.loom/worktrees/` with sentinel; everything else is user-owned). Closes Anton's regression where merge-time worktree cleanup was deleting editor-provisioned worktrees (#3334 / PR #3335)
- Installer upgrade path now detects legacy root `CLAUDE.md` files (markerless installer-managed content) via a curated `LEGACY_LOOM_SIGNATURES` heuristic in `loom-daemon/src/init/scaffolding.rs` and replaces them with the modern marker block instead of appending, eliminating duplicate Loom content and stale `{{LOOM_VERSION}}` / `{{INSTALL_DATE}}` placeholder leaks. Defense-in-depth `assert_no_placeholders()` runs immediately before every root-CLAUDE.md write (#3325 / PR #3337)

### Added

- `loom-clean --aggressive` for removing vestigial locked agent worktrees. Implements an 8-step decision tree (open-PR → active-shepherd → `.loom-managed` sentinel → uncommitted → reachability from `origin/main` → mtime → fallback) with strict fail-closed semantics. Enumerates `git worktree list --porcelain` to catch orphans not in `gh` issue results. Locked worktrees are unlocked before removal; an unreachable HEAD fallback requires `--force` and logs the SHA for `git reflog` recovery (#3332 / PR #3340)
- Installer-generated PRs now carry passive docs-only markers (`chore(loom):` title prefix, `loom-install: true` body line, `Skip-CI-Hint: docs-only` commit trailer). New `--skip-target-ci` flag is the opt-in equivalent that prepends `[skip ci]` to title and commit subject. Existing `PR_TITLE` / `PR_BODY` / `COMMIT_MSG` env-var overrides remain composable. Ships new `defaults/.loom/docs/ci-integration.md` with `paths-ignore` examples for target repos (#3333 / PR #3341)
- Installer detects an active Loom session in the target before mutating anything. New `scripts/install/check-active-session.sh` helper checks three indicators (live `daemon-loop.pid`, `daemon-state.json` with `"running": true` within 5 minutes, recently-active `.loom/worktrees/issue-N` dirs). `--allow-active-session` is the explicit override; `--force` deliberately does *not* imply it (#3331 / PR #3339)
- Installer refuses to run from a non-`main` branch of the Loom source checkout (with optional `--allow-non-main-source`) and from a target with uncommitted changes or stale state (with optional `--allow-stale-target`). Dogfood-exempt when `TARGET_PATH == LOOM_ROOT`. Tagged-release detached-HEAD exemption handles `git describe --exact-match` matches (#3327 / PR #3338)
- Installer's gitignore-guard error now uses `git check-ignore -v` to surface the offending `<file>:<line>:<pattern>` and branches the suggested fix on pattern shape — unanchored single-segment dirs get an anchor suggestion (`lib/` → `/lib/`), `.loom*`-shaped patterns get a deletion suggestion, anything else gets generic narrow-or-remove guidance (#3326 / PR #3336)

### Changed

- `Cargo.lock` dependency group bumped (2 updates, #3328)
- Dev-dependency group bumped: `vitest` 4.1.6→4.1.7, `vite` 8.0.13→8.0.14, `@vitest/coverage-v8` 4.1.6→4.1.7, `@vitest/ui` 4.1.6→4.1.7, `postcss` 8.5.14→8.5.15 (#3329)
- Dogfood install of root `CLAUDE.md` now resolves `{{LOOM_VERSION}}` / `{{INSTALL_DATE}}` placeholders correctly (precursor fix in `54b0a335`, generalized by #3325's structural fix above)

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
