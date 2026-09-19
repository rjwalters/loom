# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#8016**: fix(guard): admit the exact mktemp-then-canonicalize chain in both same-command fast paths
- **#8207**: feat(pricing): deliver the model rate card as a resync-updatable defaults/pricing.json asset
- **#8220**: feat(tokens): prefer the account that last warmed this (repo, role) prompt cache (#8146)
- **#8227**: fix(release-fetch): refuse a published-but-unfetchable .sig instead of downgrading to checksum-only
- **#8258**: fix(provision-hooks): quote ${CLAUDE_PROJECT_DIR} in project-level hook entries
- **#8279**: fix(worktree): reconcile the dirty-marker filter with its shell twin (#8195)
- **#8329**: test(sweep-registry): make the mid-build watchdog's git fixture hermetic (#8170)
- **#8337**: feat(dashboard): ingest ephemeral_compute records with an explicit redaction policy

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh
- **#8322**: Port PR #8314's per-role tool-restriction deny-spec computation out of spawn-claude.sh/spawn-codex.sh into loom-daemon (Shell Budget Ratchet blocker)

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8013**: test-guard-destructive-rm-scope.sh:338 '../ escaping the repo' assertion fails when the suite runs from a linked worktree (passes from the primary checkout)
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed
- **#8136**: Reconcile PR #8097's differential-corpus gaps with #8125's BotLoginNormalisation work
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8195**: Port worktree.sh to a daemon subcommand (1,812 lines; 26 fixes in 6 months, three of them data-loss classes)
- **#8221**: Guard: same-command mktemp fast paths count only bare `NAME=` assignments — `export`/`declare`/`read`/`printf -v` rebinding slips the ambiguity rule
- **#8248**: A green ratchet check goes stale when the baseline tightens under an in-flight PR — this is what broke main
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh
- **#8286**: loom-daemon health: codesign_identity preflight runs in the CLI process, so over non-interactive ssh it always reads DEGRADED without saying why
- **#8297**: tokens: claude-monitor ranking.json exposes per-class utilization that monitor.rs ignores
- **#8322**: Port PR #8314's per-role tool-restriction deny-spec computation out of spawn-claude.sh/spawn-codex.sh into loom-daemon (Shell Budget Ratchet blocker)
- **#8323**: dep-recheck-fingerprint.sh: multi-line DEPS/BLOCKERS/REFS output breaks the documented eval invocation with 2+ entries
- **#8326**: build-gate.sh runs `cargo test`, the shared-process runner `.config/nextest.toml` exists to avoid
- **#8328**: Two loom-daemon unit tests fail on a live-daemon host by reading inherited LOOM_* env (escalate::decide, registry_refresh)
- **#8330**: shell-budget: ratchet `comparable()`, not just `portable()` — `settled` reclassification can buy portable growth behind a floor declaration
- **#8333**: bug(lease): sweep-lease-renew.sh's cmd_start extra_args[@] hits the same Bash 3.2 empty-array bug as #8281

## In Progress

Issues currently being built (`loom:building`).

- **#8063**: limit calibration: surface $-equivalent per weekly-limit-point in loom-daemon health, warn on step change
- **#8253**: dep-recheck-fingerprint: an already-MERGED PR's transient UNKNOWN mergeability still moves CONCLUSION_HASH (churns curator dep-recheck comments)
- **#8263**: gh api call sites pass an unsupported --repo flag, silently disabling five LOOM_REPO-host probes
- **#8267**: Agent work is lost by default: a subagent can report success with its entire deliverable uncommitted, and the completion notification looks identical
- **#8268**: Subagents spend 30-40% of runtime re-verifying work the coordinator is already verifying, because neither can see the other
- **#8277**: Thread the in-flight model into LOOM_TERMINAL_RESULT so #8058's class-scoped health marks have a producer
- **#8284**: merge-pr.sh version guard runs main's checker, so a PR that changes the version-bearing set (e.g. #8190) can never pass it — evaluate the PR head's checker when the diff touches it

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#8016**: fix(guard): admit the exact mktemp-then-canonicalize chain in both same-command fast paths
- **#8207**: feat(pricing): deliver the model rate card as a resync-updatable defaults/pricing.json asset
- **#8220**: feat(tokens): prefer the account that last warmed this (repo, role) prompt cache (#8146)
- **#8227**: fix(release-fetch): refuse a published-but-unfetchable .sig instead of downgrading to checksum-only
- **#8258**: fix(provision-hooks): quote ${CLAUDE_PROJECT_DIR} in project-level hook entries
- **#8279**: fix(worktree): reconcile the dirty-marker filter with its shell twin (#8195)
- **#8329**: test(sweep-registry): make the mid-build watchdog's git fixture hermetic (#8170)
- **#8337**: feat(dashboard): ingest ephemeral_compute records with an explicit redaction policy

## Proposed

Issues carrying `loom:curated`.

- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space *(curated)*
- **#7855**: robb-pro daemon stopped heartbeating and answering IPC for 25 min while still logging; the watchdog CONFIRMED the wedge twice and never restarted it *(curated)*
- **#7947**: [#4196 Phase 3b] Operator-agent persona: natural-language intent over the typed daemon ChatOps surface *(curated)*
- **#7972**: work_finder re-claims #7893 in a loop: 14 claims, 55 label events, 4 hours, zero PRs *(curated)*
- **#7986**: Guard: same-command mktemp safe-path denies the routine `VAR=$(cd "$VAR" && pwd -P)` realpath-canonicalization reassignment *(curated)*
- **#8001**: [Part of #7708] Broadcast the pool-exhaustion hold to peers via the peer-claim room *(curated)*
- **#8005**: Generalise #7818's credential-staging guard from two hardcoded gh-config paths to the credential-bearing class (.loom/tokens/, accounts.env, claude-config/) *(curated)*
- **#8013**: test-guard-destructive-rm-scope.sh:338 '../ escaping the repo' assertion fails when the suite runs from a linked worktree (passes from the primary checkout) *(curated)*
- **#8026**: peer_coordination DEGRADED can be a false positive during a fleet-wide dispatch lull (advertise is dispatch-gated, not periodic) *(curated)*
- **#8052**: telemetry: per-role × model token consumption report + weekly-limit calibration (activity.db token tables are empty, `loom-daemon stats` crashes) *(curated)*
- **#8055**: experiment: fleet-wide repo-stratified model A/B — assign arms, write/remove overlays, cover role-runner ticks, stamp the arm explicitly in outcome records *(curated)*
- **#8058**: token pool: per-model-class exhaustion state — an Opus ceiling bad-marks the whole account and starves Sonnet work *(curated)*
- **#8063**: limit calibration: surface $-equivalent per weekly-limit-point in loom-daemon health, warn on step change *(curated)*
- **#8087**: Port loom-daemon-start.sh to a daemon subcommand (1,184 lines; preserve the FLAGS-OFF contract across start) *(curated)*
- **#8088**: Port loom-daemon-update.sh to a daemon subcommand (1,733 lines; the binary must replace itself) *(curated)*
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed *(curated)*
- **#8103**: Repo settings: main has no required status checks, so CI cannot block a merge (and stale branches never re-run) *(curated)*
- **#8136**: Reconcile PR #8097's differential-corpus gaps with #8125's BotLoginNormalisation work *(curated)*
- **#8146**: Token selection: prefer the account that last warmed this (repo, role) prompt cache (65% vs 1.4% hit rate) *(curated)*
- **#8170**: sweep_registry::watchdog midbuild_* tests fail under the full parallel suite, pass 90/90 in isolation *(curated)*
- **#8177**: pricing: deliver the model rate card as a resync-updatable defaults/pricing.json asset (ask 2 of #8060) *(curated)*
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
- **#8195**: Port worktree.sh to a daemon subcommand (1,812 lines; 26 fixes in 6 months, three of them data-loss classes) *(curated)*
- **#8197**: release-fetch: a .sig download failure is indistinguishable from an unsigned release, silently downgrading to checksum-only *(curated)*
- **#8221**: Guard: same-command mktemp fast paths count only bare `NAME=` assignments — `export`/`declare`/`read`/`printf -v` rebinding slips the ambiguity rule *(curated)*
- **#8248**: A green ratchet check goes stale when the baseline tightens under an in-flight PR — this is what broke main *(curated)*
- **#8253**: dep-recheck-fingerprint: an already-MERGED PR's transient UNKNOWN mergeability still moves CONCLUSION_HASH (churns curator dep-recheck comments) *(curated)*
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh *(curated)*
- **#8257**: dashboard: add an ephemeral_compute record type (running-now + elastic-spend views, leak detection, hostless ingest) with a D1 migration *(curated)*
- **#8263**: gh api call sites pass an unsupported --repo flag, silently disabling five LOOM_REPO-host probes *(curated)*
- **#8267**: Agent work is lost by default: a subagent can report success with its entire deliverable uncommitted, and the completion notification looks identical *(curated)*
- **#8268**: Subagents spend 30-40% of runtime re-verifying work the coordinator is already verifying, because neither can see the other *(curated)*
- **#8277**: Thread the in-flight model into LOOM_TERMINAL_RESULT so #8058's class-scoped health marks have a producer *(curated)*
- **#8284**: merge-pr.sh version guard runs main's checker, so a PR that changes the version-bearing set (e.g. #8190) can never pass it — evaluate the PR head's checker when the diff touches it *(curated)*
- **#8285**: merge-pr.sh fails closed on a daemon predating a required subcommand — name the remediation (artifact roll) and declare the minimum daemon version per script *(curated)*
- **#8286**: loom-daemon health: codesign_identity preflight runs in the CLI process, so over non-interactive ssh it always reads DEGRADED without saying why *(curated)*
- **#8297**: tokens: claude-monitor ranking.json exposes per-class utilization that monitor.rs ignores *(curated)*
- **#8304**: dashboard: ephemeral_compute ingest + D1 schema + redaction policy (Phase 1 of #8257) *(curated)*
- **#8309**: Curator: state that an autonomous filing is not operator approval (#8269 point 3) *(curated)*
- **#8322**: Port PR #8314's per-role tool-restriction deny-spec computation out of spawn-claude.sh/spawn-codex.sh into loom-daemon (Shell Budget Ratchet blocker) *(curated)*
- **#8323**: dep-recheck-fingerprint.sh: multi-line DEPS/BLOCKERS/REFS output breaks the documented eval invocation with 2+ entries *(curated)*
- **#8326**: build-gate.sh runs `cargo test`, the shared-process runner `.config/nextest.toml` exists to avoid *(curated)*
- **#8328**: Two loom-daemon unit tests fail on a live-daemon host by reading inherited LOOM_* env (escalate::decide, registry_refresh) *(curated)*
- **#8330**: shell-budget: ratchet `comparable()`, not just `portable()` — `settled` reclassification can buy portable growth behind a floor declaration *(curated)*
- **#8333**: bug(lease): sweep-lease-renew.sh's cmd_start extra_args[@] hits the same Bash 3.2 empty-array bug as #8281 *(curated)*

## Proposed (Architect / Hermit)

- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam
- **#7810**: [epic] Retire shell: 10 sequential PRs, two foundational then update + agent execution

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 8 |
| Urgent | 3 |
| Ready (`loom:issue`) | 17 |
| In Progress (`loom:building`) | 7 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 8 |
| Curated | 46 |
| Architect / Hermit proposals | 2 |
| Active epics | 4 |
<!-- guide:plan-body:end -->
