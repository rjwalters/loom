# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#8179**: fix(tests): stop sweep test runs leaking into the live host (#8077)
- **#8227**: fix(release-fetch): refuse a published-but-unfetchable .sig instead of downgrading to checksum-only

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8086**: Port loom-daemon-watchdog.sh to a daemon subcommand (994 lines, 233 retained assertions)
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#7986**: Guard: same-command mktemp safe-path denies the routine `VAR=$(cd "$VAR" && pwd -P)` realpath-canonicalization reassignment
- **#8013**: test-guard-destructive-rm-scope.sh:338 '../ escaping the repo' assertion fails when the suite runs from a linked worktree (passes from the primary checkout)
- **#8086**: Port loom-daemon-watchdog.sh to a daemon subcommand (994 lines, 233 retained assertions)
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed
- **#8136**: Reconcile PR #8097's differential-corpus gaps with #8125's BotLoginNormalisation work
- **#8170**: sweep_registry::watchdog midbuild_* tests fail under the full parallel suite, pass 90/90 in isolation
- **#8173**: test-loom-daemon-start.sh's #6568 control cases fail when the suite runs inside a Loom agent session (si_run does not strip LOOM_SWEEP_*/LOOM_TERMINAL_ID/LOOM_ROLE)
- **#8177**: pricing: deliver the model rate card as a resync-updatable defaults/pricing.json asset (ask 2 of #8060)
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8195**: Port worktree.sh to a daemon subcommand (1,812 lines; 26 fixes in 6 months, three of them data-loss classes)
- **#8221**: Guard: same-command mktemp fast paths count only bare `NAME=` assignments — `export`/`declare`/`read`/`printf -v` rebinding slips the ambiguity rule

## In Progress

Issues currently being built (`loom:building`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space
- **#8146**: Token selection: prefer the account that last warmed this (repo, role) prompt cache (65% vs 1.4% hit rate)
- **#8211**: guard mask_ws(): a quote inside an unquoted backtick substitution masks away a live statement boundary
- **#8217**: guard: an unquoted heredoc body's $( ) substitution bypasses rm-scope (extract_rm_targets analogue of #8035)
- **#8222**: telemetry: source judge_verdicts and doctor_cycles from the forge label timeline, not the sampled phase history
- **#8224**: health: extend #8163's root-scaled IPC budget to cli::status and the dashboard's serve.rs FETCH_TIMEOUT

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#8016**: fix(guard): admit the exact mktemp-then-canonicalize chain in both same-command fast paths
- **#8207**: feat(pricing): deliver the model rate card as a resync-updatable defaults/pricing.json asset
- **#8220**: feat(tokens): prefer the account that last warmed this (repo, role) prompt cache (#8146)

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#8179**: fix(tests): stop sweep test runs leaking into the live host (#8077)
- **#8227**: fix(release-fetch): refuse a published-but-unfetchable .sig instead of downgrading to checksum-only
- **#8236**: feat(shell-budget): report fix-weighted progress, not just lines (#8233)

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
- **#8056**: telemetry: outcome journal lacks judge verdicts, doctor cycles, failure class, effort, token account — and role-runner ticks emit no record at all *(curated)*
- **#8058**: token pool: per-model-class exhaustion state — an Opus ceiling bad-marks the whole account and starves Sonnet work *(curated)*
- **#8077**: Builder test runs leak into the live host: test daemons log to ~/.loom/daemon.log and reload the production user systemd manager (#7873 sweep on loom-worker-2) *(curated)*
- **#8086**: Port loom-daemon-watchdog.sh to a daemon subcommand (994 lines, 233 retained assertions) *(curated)*
- **#8087**: Port loom-daemon-start.sh to a daemon subcommand (1,184 lines; preserve the FLAGS-OFF contract across start) *(curated)*
- **#8088**: Port loom-daemon-update.sh to a daemon subcommand (1,733 lines; the binary must replace itself) *(curated)*
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed *(curated)*
- **#8103**: Repo settings: main has no required status checks, so CI cannot block a merge (and stale branches never re-run) *(curated)*
- **#8136**: Reconcile PR #8097's differential-corpus gaps with #8125's BotLoginNormalisation work *(curated)*
- **#8146**: Token selection: prefer the account that last warmed this (repo, role) prompt cache (65% vs 1.4% hit rate) *(curated)*
- **#8170**: sweep_registry::watchdog midbuild_* tests fail under the full parallel suite, pass 90/90 in isolation *(curated)*
- **#8173**: test-loom-daemon-start.sh's #6568 control cases fail when the suite runs inside a Loom agent session (si_run does not strip LOOM_SWEEP_*/LOOM_TERMINAL_ID/LOOM_ROLE) *(curated)*
- **#8177**: pricing: deliver the model rate card as a resync-updatable defaults/pricing.json asset (ask 2 of #8060) *(curated)*
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
- **#8195**: Port worktree.sh to a daemon subcommand (1,812 lines; 26 fixes in 6 months, three of them data-loss classes) *(curated)*
- **#8197**: release-fetch: a .sig download failure is indistinguishable from an unsigned release, silently downgrading to checksum-only *(curated)*
- **#8211**: guard mask_ws(): a quote inside an unquoted backtick substitution masks away a live statement boundary *(curated)*
- **#8217**: guard: an unquoted heredoc body's $( ) substitution bypasses rm-scope (extract_rm_targets analogue of #8035) *(curated)*
- **#8221**: Guard: same-command mktemp fast paths count only bare `NAME=` assignments — `export`/`declare`/`read`/`printf -v` rebinding slips the ambiguity rule *(curated)*
- **#8222**: telemetry: source judge_verdicts and doctor_cycles from the forge label timeline, not the sampled phase history *(curated)*
- **#8224**: health: extend #8163's root-scaled IPC budget to cli::status and the dashboard's serve.rs FETCH_TIMEOUT *(curated)*
- **#8233**: shell-budget reports lines, not fragility — add a fix-weighted progress figure *(curated)*

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
| Operator merge-risk holds | 2 |
| Urgent | 3 |
| Ready (`loom:issue`) | 11 |
| In Progress (`loom:building`) | 7 |
| PRs awaiting review | 3 |
| Approved PRs awaiting merge | 3 |
| Curated | 34 |
| Architect / Hermit proposals | 2 |
| Active epics | 4 |
<!-- guide:plan-body:end -->
