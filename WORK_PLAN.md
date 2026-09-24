# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#8227**: fix(release-fetch): refuse a published-but-unfetchable .sig instead of downgrading to checksum-only
- **#8386**: fix(merge-pr): name the daemon roll in the refusal, and declare every script's daemon version floor
- **#8425**: feat(premise-gate): refuse a self-approved design reversal before Curator enriches it
- **#8426**: fix(merge-pr): --auto settles checks and re-validates in-process instead of arming a server-side merge (#8410)

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh
- **#8322**: Port PR #8314's per-role tool-restriction deny-spec computation out of spawn-claude.sh/spawn-codex.sh into loom-daemon (Shell Budget Ratchet blocker)
- **#8401**: native harness API-key account pool: rotate a fleet of Z.ai coding-plan subscriptions through OpenCode/Pi with per-account exhaustion state (the API-key analogue of the Claude token pool)

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed
- **#8136**: Reconcile PR #8097's differential-corpus gaps with #8125's BotLoginNormalisation work
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh
- **#8287**: worktree.sh reset a stale local feature/issue-N to main although origin/feature/issue-N carried the PR's commits (Doctor on #8190)
- **#8322**: Port PR #8314's per-role tool-restriction deny-spec computation out of spawn-claude.sh/spawn-codex.sh into loom-daemon (Shell Budget Ratchet blocker)
- **#8354**: Port _worktree_resolve_stale_reset_ref (#8287) to loom-daemon per shell-language-policy
- **#8401**: native harness API-key account pool: rotate a fleet of Z.ai coding-plan subscriptions through OpenCode/Pi with per-account exhaustion state (the API-key analogue of the Claude token pool)
- **#8443**: Codex-runtime role ticks never record TOKEN_EXHAUSTED into account health, so #8408's pre-spawn codex gate has no holds to read
- **#8444**: Codex pool gate (#8408): zero-provisioned and unreadable-state skips are reported as self-healing PoolExhausted; wording and coverage polish
- **#8448**: Guarded tool binding is never loaded on OpenCode 2.x: role workers start toolless, do nothing, and exit 0 (live canary FAILED safely)
- **#8451**: Native guard bridge: the 20s policy timeout refuses harmless commands on a saturated host, and the model cannot tell a timeout from a denial

## In Progress

Issues currently being built (`loom:building`).

- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8195**: Port worktree.sh to a daemon subcommand (1,812 lines; 26 fixes in 6 months, three of them data-loss classes)
- **#8407**: Codex per-subscription availability probe: expose quota/rate-limit state per account into provider-aware ranking (the `tokens check` analogue)
- **#8413**: Worktree reaper hard-reset an in-flight builder's worktree mid-compile (idle-detection misfire)
- **#8439**: daemon: reaper no_progress predicate charges productive exit-0 partial-increment sweeps to the insta-crash quarantine tally

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#8227**: fix(release-fetch): refuse a published-but-unfetchable .sig instead of downgrading to checksum-only
- **#8386**: fix(merge-pr): name the daemon roll in the refusal, and declare every script's daemon version floor
- **#8425**: feat(premise-gate): refuse a self-approved design reversal before Curator enriches it
- **#8426**: fix(merge-pr): --auto settles checks and re-validates in-process instead of arming a server-side merge (#8410)

## Proposed

Issues carrying `loom:curated`.

- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#7855**: robb-pro daemon stopped heartbeating and answering IPC for 25 min while still logging; the watchdog CONFIRMED the wedge twice and never restarted it *(curated)*
- **#7947**: [#4196 Phase 3b] Operator-agent persona: natural-language intent over the typed daemon ChatOps surface *(curated)*
- **#7972**: work_finder re-claims #7893 in a loop: 14 claims, 55 label events, 4 hours, zero PRs *(curated)*
- **#8001**: [Part of #7708] Broadcast the pool-exhaustion hold to peers via the peer-claim room *(curated)*
- **#8005**: Generalise #7818's credential-staging guard from two hardcoded gh-config paths to the credential-bearing class (.loom/tokens/, accounts.env, claude-config/) *(curated)*
- **#8026**: peer_coordination DEGRADED can be a false positive during a fleet-wide dispatch lull (advertise is dispatch-gated, not periodic) *(curated)*
- **#8052**: telemetry: per-role × model token consumption report + weekly-limit calibration (activity.db token tables are empty, `loom-daemon stats` crashes) *(curated)*
- **#8055**: experiment: fleet-wide repo-stratified model A/B — assign arms, write/remove overlays, cover role-runner ticks, stamp the arm explicitly in outcome records *(curated)*
- **#8058**: token pool: per-model-class exhaustion state — an Opus ceiling bad-marks the whole account and starves Sonnet work *(curated)*
- **#8087**: Port loom-daemon-start.sh to a daemon subcommand (1,184 lines; preserve the FLAGS-OFF contract across start) *(curated)*
- **#8088**: Port loom-daemon-update.sh to a daemon subcommand (1,733 lines; the binary must replace itself) *(curated)*
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed *(curated)*
- **#8103**: Repo settings: main has no required status checks, so CI cannot block a merge (and stale branches never re-run) *(curated)*
- **#8136**: Reconcile PR #8097's differential-corpus gaps with #8125's BotLoginNormalisation work *(curated)*
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
- **#8195**: Port worktree.sh to a daemon subcommand (1,812 lines; 26 fixes in 6 months, three of them data-loss classes) *(curated)*
- **#8197**: release-fetch: a .sig download failure is indistinguishable from an unsigned release, silently downgrading to checksum-only *(curated)*
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh *(curated)*
- **#8257**: dashboard: add an ephemeral_compute record type (running-now + elastic-spend views, leak detection, hostless ingest) with a D1 migration *(curated)*
- **#8285**: merge-pr.sh fails closed on a daemon predating a required subcommand — name the remediation (artifact roll) and declare the minimum daemon version per script *(curated)*
- **#8287**: worktree.sh reset a stale local feature/issue-N to main although origin/feature/issue-N carried the PR's commits (Doctor on #8190) *(curated)*
- **#8322**: Port PR #8314's per-role tool-restriction deny-spec computation out of spawn-claude.sh/spawn-codex.sh into loom-daemon (Shell Budget Ratchet blocker) *(curated)*
- **#8354**: Port _worktree_resolve_stale_reset_ref (#8287) to loom-daemon per shell-language-policy *(curated)*
- **#8370**: Concurrent sweeps exhaust host disk via per-worktree cargo target dirs; surfaces as unrelated StorageFull test failures *(curated)*
- **#8387**: spawn-codex.sh forwards a `model@effort` suffix verbatim to the Codex CLI's `-m` *(curated)*
- **#8396**: Design and wire a premise-check gate before Curator for autonomously-filed/design-reversing issues (re-file of #8310, corrected citation) *(curated)*
- **#8401**: native harness API-key account pool: rotate a fleet of Z.ai coding-plan subscriptions through OpenCode/Pi with per-account exhaustion state (the API-key analogue of the Claude token pool) *(curated)*
- **#8407**: Codex per-subscription availability probe: expose quota/rate-limit state per account into provider-aware ranking (the `tokens check` analogue) *(curated)*
- **#8410**: merge-pr.sh --auto: a server-side armed auto-merge ignores later loom:pr revocation and non-required test suites *(curated)*
- **#8413**: Worktree reaper hard-reset an in-flight builder's worktree mid-compile (idle-detection misfire) *(curated)*
- **#8434**: live-verify native-ephemeral containment: canary run in-container, two-worker filesystem disjointness, post-run writable-layer credential scan *(curated)*
- **#8436**: Ordered runtime preference with fall-through: prefer Claude/Codex accounts, backstop on a metered OpenCode endpoint when their pools are exhausted *(curated)*
- **#8439**: daemon: reaper no_progress predicate charges productive exit-0 partial-increment sweeps to the insta-crash quarantine tally *(curated)*
- **#8443**: Codex-runtime role ticks never record TOKEN_EXHAUSTED into account health, so #8408's pre-spawn codex gate has no holds to read *(curated)*
- **#8444**: Codex pool gate (#8408): zero-provisioned and unreadable-state skips are reported as self-healing PoolExhausted; wording and coverage polish *(curated)*
- **#8448**: Guarded tool binding is never loaded on OpenCode 2.x: role workers start toolless, do nothing, and exit 0 (live canary FAILED safely) *(curated)*
- **#8451**: Native guard bridge: the 20s policy timeout refuses harmless commands on a saturated host, and the model cannot tell a timeout from a denial *(curated)*
- **#8454**: native containment: no regression test pins `Selection.credential_sources`, and file-path credentials are not mounted *(curated)*

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
| Operator merge-risk holds | 4 |
| Urgent | 3 |
| Ready (`loom:issue`) | 12 |
| In Progress (`loom:building`) | 5 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 4 |
| Curated | 39 |
| Architect / Hermit proposals | 2 |
| Active epics | 4 |
<!-- guide:plan-body:end -->
