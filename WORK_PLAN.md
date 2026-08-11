# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#5999**: fix(uninstall): preserve live .loom/worktrees/* by default in --local mode
- **#5995**: chore: resync installed Loom surfaces
- **#5986**: fix(guard): add fourth dispatcher probe for --body @path capability (#5974)
- **#5904**: fix(daemon): treat stale worktree registrations as already-removed in loom-daemon clean
- **#5899**: chore: resync installed Loom surfaces
- **#5684**: fix: correct BSD sed -i separate-suffix write-target resolution
- **#5683**: fix(guard): count only unescaped/unquoted pipes in read-only fast path (#5673)
- **#5681**: fix(guard-loom-workflow): mask unquoted-delimiter cat-heredoc bodies captured into text-data flags
- **#5636**: fix(tokens): propagate .ranking exhausted/blocked exclusions to the allowlist and random tiers
- **#5619**: tokens: record (provider, upstream account id) in the pool storage layer
- **#5569**: fix(fleet): idle-shutdown guard asks daemon eligibility instead of vetoing on bare process presence
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#6010**: Releases lag VERSION by 13 patches, so the signed-artifact --fetch path cannot reach current
- **#5895**: loom-daemon clean fails on stale worktree registrations — no git worktree prune before worktree remove, and --dry-run cannot see it
- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#6021**: Auditor Capability Request: worktree-isolation guard blocks local docker worker-image-smoke validation
- **#6010**: Releases lag VERSION by 13 patches, so the signed-artifact --fetch path cannot reach current
- **#5974**: v0.18.0 drops the `--body @path` hard deny with guard-destructive-generic.sh, but judge.md still documents it as enforced
- **#5895**: loom-daemon clean fails on stale worktree registrations — no git worktree prune before worktree remove, and --dry-run cannot see it
- **#5673**: Guard read-only fast path (#5274) still denies sql-ddl when the grep pattern argument itself contains an escaped/quoted pipe
- **#5672**: Guard false positive: loom:gh-pr-merge-redirect denies gh pr comment bodies that merely quote/discuss 'gh pr merge' in prose
- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer

## In Progress

Issues currently being built (`loom:building`).

- **#6031**: Installed files fail consumer repo linters: biome parse error in judge-fanout-workflow.js, unformatted JSON metadata
- **#6030**: Pool spawns children on auth-dead tokens (401 Invalid bearer token): classify distinctly from exhaustion and exclude at selection
- **#6007**: Drain and the work finder livelock — a busy host can never roll onto a new binary
- **#5973**: Reinstall silently removes existing .loom/worktrees/issue-* worktrees
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#6044**: fix(installer): ship nested Biome configs to exclude Loom-managed paths
- **#6029**: docs(release): document cadence + surface --fetch source-gap in loom-daemon-update.sh

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#6026**: fix(guard): scope worktree-write-confinement to allow read-only-role dist/ staging
- **#6018**: fix(install): stop the reinstall sweep from deleting repo-owned files under .loom/
- **#5999**: fix(uninstall): preserve live .loom/worktrees/* by default in --local mode
- **#5995**: chore: resync installed Loom surfaces
- **#5986**: fix(guard): add fourth dispatcher probe for --body @path capability (#5974)
- **#5904**: fix(daemon): treat stale worktree registrations as already-removed in loom-daemon clean
- **#5899**: chore: resync installed Loom surfaces
- **#5684**: fix: correct BSD sed -i separate-suffix write-target resolution
- **#5683**: fix(guard): count only unescaped/unquoted pipes in read-only fast path (#5673)
- **#5681**: fix(guard-loom-workflow): mask unquoted-delimiter cat-heredoc bodies captured into text-data flags
- **#5636**: fix(tokens): propagate .ranking exhausted/blocked exclusions to the allowlist and random tiers
- **#5619**: tokens: record (provider, upstream account id) in the pool storage layer
- **#5569**: fix(fleet): idle-shutdown guard asks daemon eligibility instead of vetoing on bare process presence
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#6031**: Installed files fail consumer repo linters: biome parse error in judge-fanout-workflow.js, unformatted JSON metadata *(curated)*
- **#6030**: Pool spawns children on auth-dead tokens (401 Invalid bearer token): classify distinctly from exhaustion and exclude at selection *(curated)*
- **#6010**: Releases lag VERSION by 13 patches, so the signed-artifact --fetch path cannot reach current *(curated)*
- **#6007**: Drain and the work finder livelock — a busy host can never roll onto a new binary *(curated)*
- **#5974**: v0.18.0 drops the `--body @path` hard deny with guard-destructive-generic.sh, but judge.md still documents it as enforced *(curated)*
- **#5973**: Reinstall silently removes existing .loom/worktrees/issue-* worktrees *(curated)*
- **#5897**: sweep skill: presumed-dead Builder Task can survive a session roll — verify task liveness before re-dispatch (duplicate-builder hazard) *(curated)*
- **#5895**: loom-daemon clean fails on stale worktree registrations — no git worktree prune before worktree remove, and --dry-run cannot see it *(curated)*
- **#5729**: loom-daemon is DOWN on robb-studio and watchdog recovery is exhausted *(curated)*
- **#5674**: Guard false positive: worktree-write-confinement denies cp/mv writes to /tmp or fully in-repo tmp-then-rename, unrelated to the main checkout *(curated)*
- **#5660**: Vendored guard-destructive-generic.sh has drifted ~2,200 lines ahead of its upstream, and the single-marker capability probe makes partial reconciliation unsafe *(curated)*
- **#5629**: Role-spawn token selection (mode=random) hands out accounts marked in tokens-exhausted; monthly-spend-limit errors retried as RECOVERABLE *(curated)*
- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer *(curated)*
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision *(curated)*
- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*

## Proposed (Architect / Hermit)

- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 12 |
| Urgent | 3 |
| Ready (`loom:issue`) | 7 |
| In Progress (`loom:building`) | 5 |
| PRs awaiting review | 2 |
| Approved PRs awaiting merge | 14 |
| Curated | 18 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

