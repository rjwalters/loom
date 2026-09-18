# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#8016**: fix(guard): admit the exact mktemp-then-canonicalize chain in both same-command fast paths

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#8077**: Builder test runs leak into the live host: test daemons log to ~/.loom/daemon.log and reload the production user systemd manager (#7873 sweep on loom-worker-2)
- **#8086**: Port loom-daemon-watchdog.sh to a daemon subcommand (994 lines, 233 retained assertions)
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space
- **#8013**: test-guard-destructive-rm-scope.sh:338 '../ escaping the repo' assertion fails when the suite runs from a linked worktree (passes from the primary checkout)
- **#8077**: Builder test runs leak into the live host: test daemons log to ~/.loom/daemon.log and reload the production user systemd manager (#7873 sweep on loom-worker-2)
- **#8086**: Port loom-daemon-watchdog.sh to a daemon subcommand (994 lines, 233 retained assertions)
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed
- **#8112**: Two Judges raced on one head: a PR can carry loom:pr and loom:changes-requested at once, and merge-pr.sh only reads the first
- **#8136**: Reconcile PR #8097's differential-corpus gaps with #8125's BotLoginNormalisation work
- **#8138**: Port PR #8058's model-class-marker logic to loom-daemon (Shell Budget Ratchet)
- **#8147**: CLAUDE.md's **Loom Version** stamp invalidates every session's cached prefix on each bump
- **#8173**: test-loom-daemon-start.sh's #6568 control cases fail when the suite runs inside a Loom agent session (si_run does not strip LOOM_SWEEP_*/LOOM_TERMINAL_ID/LOOM_ROLE)
- **#8176**: Shell suites can silently test a stale or foreign loom-daemon when CARGO_TARGET_DIR is shared (false T15-style regressions)
- **#8177**: pricing: deliver the model rate card as a resync-updatable defaults/pricing.json asset (ask 2 of #8060)
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)

## In Progress

Issues currently being built (`loom:building`).

- **#8035**: guard: an unquoted heredoc body's $( ) substitution bypasses worktree write confinement (write-path analogue of #8003)
- **#8056**: telemetry: outcome journal lacks judge verdicts, doctor cycles, failure class, effort, token account — and role-runner ticks emit no record at all
- **#8116**: claim_reconciliation + worktree_reaper undo in-session builders: no lease record, so loom:building is flipped back and target/ is reaped mid-build
- **#8121**: workspace add reports 'Registered' but silently skips the hot-apply when run outside the daemon's workspace directory
- **#8146**: Token selection: prefer the account that last warmed this (repo, role) prompt cache (65% vs 1.4% hit rate)
- **#8156**: guard: quoted-delimiter heredoc capture fed to eval/sh -c is still a silent ALLOW (the <<'EOF' sibling of #7970)
- **#8166**: guard qsplit(): the closing-quote scan matches an escaped quote, ending a double-quoted span early
- **#8170**: sweep_registry::watchdog midbuild_* tests fail under the full parallel suite, pass 90/90 in isolation
- **#8193**: Builder: in-session Task-tool builders publish no loom:lease, so claim_reconciliation reclaims them mid-build
- **#8195**: Port worktree.sh to a daemon subcommand (1,812 lines; 26 fixes in 6 months, three of them data-loss classes)
- **#8197**: release-fetch: a .sig download failure is indistinguishable from an unsigned release, silently downgrading to checksum-only

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#8179**: fix(tests): stop sweep test runs leaking into the live host (#8077)
- **#8184**: feat(watchdog): retire loom-daemon-watchdog.sh (2436 lines) — the epic goes net-negative
- **#8190**: fix(cache): stop stamping the running version into CLAUDE.md (#8147)
- **#8199**: feat(merge-pr): port the closing-reference analysis to Rust (#8191 slice 1)
- **#8205**: fix(tests): pin the freshest repo build and snapshot it in require-daemon-bin
- **#8207**: feat(pricing): deliver the model rate card as a resync-updatable defaults/pricing.json asset
- **#8209**: fix(guard): gate quoted-delimiter heredoc capture masking on the #7970 re-parse check (#8156)
- **#8212**: fix(guard): qsplit() closing-quote scan honours backslash parity (#8166)

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#8016**: fix(guard): admit the exact mktemp-then-canonicalize chain in both same-command fast paths
- **#8078**: fix(install): ignore .loom-local/ in consumer repos and treat it as Loom-owned

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
- **#8015**: Add doc-comment pointer to prose-existence rule in sweep_md_stage_minus_one_doc_lint.rs *(curated)*
- **#8026**: peer_coordination DEGRADED can be a false positive during a fleet-wide dispatch lull (advertise is dispatch-gated, not periodic) *(curated)*
- **#8035**: guard: an unquoted heredoc body's $( ) substitution bypasses worktree write confinement (write-path analogue of #8003) *(curated)*
- **#8052**: telemetry: per-role × model token consumption report + weekly-limit calibration (activity.db token tables are empty, `loom-daemon stats` crashes) *(curated)*
- **#8055**: experiment: fleet-wide repo-stratified model A/B — assign arms, write/remove overlays, cover role-runner ticks, stamp the arm explicitly in outcome records *(curated)*
- **#8056**: telemetry: outcome journal lacks judge verdicts, doctor cycles, failure class, effort, token account — and role-runner ticks emit no record at all *(curated)*
- **#8058**: token pool: per-model-class exhaustion state — an Opus ceiling bad-marks the whole account and starves Sonnet work *(curated)*
- **#8062**: add 'loom-daemon usage report' command: token/cost breakdown by role, model, repo, day *(curated)*
- **#8075**: install/hygiene: .loom-local/ overlay is not gitignored in consumer repos and not Loom-owned — quarantine stashes it, silently reverting model overrides *(curated)*
- **#8077**: Builder test runs leak into the live host: test daemons log to ~/.loom/daemon.log and reload the production user systemd manager (#7873 sweep on loom-worker-2) *(curated)*
- **#8086**: Port loom-daemon-watchdog.sh to a daemon subcommand (994 lines, 233 retained assertions) *(curated)*
- **#8087**: Port loom-daemon-start.sh to a daemon subcommand (1,184 lines; preserve the FLAGS-OFF contract across start) *(curated)*
- **#8088**: Port loom-daemon-update.sh to a daemon subcommand (1,733 lines; the binary must replace itself) *(curated)*
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed *(curated)*
- **#8103**: Repo settings: main has no required status checks, so CI cannot block a merge (and stale branches never re-run) *(curated)*
- **#8112**: Two Judges raced on one head: a PR can carry loom:pr and loom:changes-requested at once, and merge-pr.sh only reads the first *(curated)*
- **#8116**: claim_reconciliation + worktree_reaper undo in-session builders: no lease record, so loom:building is flipped back and target/ is reaped mid-build *(curated)*
- **#8121**: workspace add reports 'Registered' but silently skips the hot-apply when run outside the daemon's workspace directory *(curated)*
- **#8122**: Destructive-write guard denies redirects/heredocs targeting paths outside every repo when cwd is a repo with live worktrees *(curated)*
- **#8136**: Reconcile PR #8097's differential-corpus gaps with #8125's BotLoginNormalisation work *(curated)*
- **#8138**: Port PR #8058's model-class-marker logic to loom-daemon (Shell Budget Ratchet) *(curated)*
- **#8146**: Token selection: prefer the account that last warmed this (repo, role) prompt cache (65% vs 1.4% hit rate) *(curated)*
- **#8147**: CLAUDE.md's **Loom Version** stamp invalidates every session's cached prefix on each bump *(curated)*
- **#8156**: guard: quoted-delimiter heredoc capture fed to eval/sh -c is still a silent ALLOW (the <<'EOF' sibling of #7970) *(curated)*
- **#8166**: guard qsplit(): the closing-quote scan matches an escaped quote, ending a double-quoted span early *(curated)*
- **#8170**: sweep_registry::watchdog midbuild_* tests fail under the full parallel suite, pass 90/90 in isolation *(curated)*
- **#8173**: test-loom-daemon-start.sh's #6568 control cases fail when the suite runs inside a Loom agent session (si_run does not strip LOOM_SWEEP_*/LOOM_TERMINAL_ID/LOOM_ROLE) *(curated)*
- **#8176**: Shell suites can silently test a stale or foreign loom-daemon when CARGO_TARGET_DIR is shared (false T15-style regressions) *(curated)*
- **#8177**: pricing: deliver the model rate card as a resync-updatable defaults/pricing.json asset (ask 2 of #8060) *(curated)*
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
- **#8193**: Builder: in-session Task-tool builders publish no loom:lease, so claim_reconciliation reclaims them mid-build *(curated)*
- **#8195**: Port worktree.sh to a daemon subcommand (1,812 lines; 26 fixes in 6 months, three of them data-loss classes) *(curated)*
- **#8197**: release-fetch: a .sig download failure is indistinguishable from an unsigned release, silently downgrading to checksum-only *(curated)*

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
| Operator merge-risk holds | 1 |
| Urgent | 3 |
| Ready (`loom:issue`) | 14 |
| In Progress (`loom:building`) | 11 |
| PRs awaiting review | 8 |
| Approved PRs awaiting merge | 2 |
| Curated | 42 |
| Architect / Hermit proposals | 2 |
| Active epics | 4 |
<!-- guide:plan-body:end -->
