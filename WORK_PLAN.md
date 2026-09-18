# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#8016**: fix(guard): admit the exact mktemp-then-canonicalize chain in both same-command fast paths

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space
- **#8035**: guard: an unquoted heredoc body's $( ) substitution bypasses worktree write confinement (write-path analogue of #8003)

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space
- **#8013**: test-guard-destructive-rm-scope.sh:338 '../ escaping the repo' assertion fails when the suite runs from a linked worktree (passes from the primary checkout)
- **#8035**: guard: an unquoted heredoc body's $( ) substitution bypasses worktree write confinement (write-path analogue of #8003)
- **#8056**: telemetry: outcome journal lacks judge verdicts, doctor cycles, failure class, effort, token account — and role-runner ticks emit no record at all
- **#8075**: install/hygiene: .loom-local/ overlay is not gitignored in consumer repos and not Loom-owned — quarantine stashes it, silently reverting model overrides
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed
- **#8112**: Two Judges raced on one head: a PR can carry loom:pr and loom:changes-requested at once, and merge-pr.sh only reads the first
- **#8136**: Reconcile PR #8097's differential-corpus gaps with #8125's BotLoginNormalisation work
- **#8138**: Port PR #8058's model-class-marker logic to loom-daemon (Shell Budget Ratchet)
- **#8150**: docs(config_resolver): drop the stale "tier 1 is not yet shipped" framing from two doc comments
- **#8154**: shell-budget ratchet: message says 'say why in the commit' but nothing honours it — no override path, three approved safety PRs blocked
- **#8166**: guard qsplit(): the closing-quote scan matches an escaped quote, ending a double-quoted span early
- **#8168**: test: watchdog case 20 proves "the probe did not run" with a 2s wall-clock budget, which flakes under host load

## In Progress

Issues currently being built (`loom:building`).

- **#8025**: guard qsplit(): an unquoted backslash-escaped quote enters the quoted-span branch, hiding a real statement boundary
- **#8028**: [epic #7810 PR 6a] Port the artifact fetch + verification; split Phase 6 into slices
- **#8059**: telemetry: ingest transcript token usage into activity.db for dispatch-based sweeps (resource_usage/token_usage are empty)
- **#8060**: models: pricing table is one to two generations stale (opus 3× over, haiku 4× under, fable priced as opus); dormant workflows pin gen-4 IDs
- **#8077**: Builder test runs leak into the live host: test daemons log to ~/.loom/daemon.log and reload the production user systemd manager (#7873 sweep on loom-worker-2)
- **#8086**: Port loom-daemon-watchdog.sh to a daemon subcommand (994 lines, 233 retained assertions)
- **#8116**: claim_reconciliation + worktree_reaper undo in-session builders: no lease record, so loom:building is flipped back and target/ is reaped mid-build
- **#8119**: dep_recheck named-dependency item_re misses 'Blocked by #N'/'Depends on #N'/'Requires #N' checklist phrasings (false VERDICT=clear)
- **#8121**: workspace add reports 'Registered' but silently skips the hot-apply when run outside the daemon's workspace directory
- **#8123**: role_runner 'tick failed' summary quotes the last stderr line (an MCP-config WARN), masking the real cause (token pool exhausted, disk full)
- **#8134**: A stub and its retained suite both mean LOOM_DAEMON_BIN, and they mean different things
- **#8146**: Token selection: prefer the account that last warmed this (repo, role) prompt cache (65% vs 1.4% hit rate)
- **#8147**: CLAUDE.md's **Loom Version** stamp invalidates every session's cached prefix on each bump

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#8078**: fix(install): ignore .loom-local/ in consumer repos and treat it as Loom-owned
- **#8155**: chore: resync installed Loom surfaces
- **#8167**: fix(scripts): give a stub's own implementation its own knob, LOOM_DAEMON_SELF_BIN
- **#8169**: fix(shell-budget): make "say why in the commit" real — declared floor growth (#8154)
- **#8171**: fix(guard): treat an unquoted backslash-escaped quote as literal, not a span opener

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#8016**: fix(guard): admit the exact mktemp-then-canonicalize chain in both same-command fast paths
- **#8158**: test(role-runner): extract the #8066 prompt-cache guard; tighten the ledger (-7752, zero raises)

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
- **#8025**: guard qsplit(): an unquoted backslash-escaped quote enters the quoted-span branch, hiding a real statement boundary *(curated)*
- **#8026**: peer_coordination DEGRADED can be a false positive during a fleet-wide dispatch lull (advertise is dispatch-gated, not periodic) *(curated)*
- **#8028**: [epic #7810 PR 6a] Port the artifact fetch + verification; split Phase 6 into slices *(curated)*
- **#8035**: guard: an unquoted heredoc body's $( ) substitution bypasses worktree write confinement (write-path analogue of #8003) *(curated)*
- **#8052**: telemetry: per-role × model token consumption report + weekly-limit calibration (activity.db token tables are empty, `loom-daemon stats` crashes) *(curated)*
- **#8055**: experiment: fleet-wide repo-stratified model A/B — assign arms, write/remove overlays, cover role-runner ticks, stamp the arm explicitly in outcome records *(curated)*
- **#8056**: telemetry: outcome journal lacks judge verdicts, doctor cycles, failure class, effort, token account — and role-runner ticks emit no record at all *(curated)*
- **#8058**: token pool: per-model-class exhaustion state — an Opus ceiling bad-marks the whole account and starves Sonnet work *(curated)*
- **#8059**: telemetry: ingest transcript token usage into activity.db for dispatch-based sweeps (resource_usage/token_usage are empty) *(curated)*
- **#8060**: models: pricing table is one to two generations stale (opus 3× over, haiku 4× under, fable priced as opus); dormant workflows pin gen-4 IDs *(curated)*
- **#8075**: install/hygiene: .loom-local/ overlay is not gitignored in consumer repos and not Loom-owned — quarantine stashes it, silently reverting model overrides *(curated)*
- **#8077**: Builder test runs leak into the live host: test daemons log to ~/.loom/daemon.log and reload the production user systemd manager (#7873 sweep on loom-worker-2) *(curated)*
- **#8086**: Port loom-daemon-watchdog.sh to a daemon subcommand (994 lines, 233 retained assertions) *(curated)*
- **#8087**: Port loom-daemon-start.sh to a daemon subcommand (1,184 lines; preserve the FLAGS-OFF contract across start) *(curated)*
- **#8088**: Port loom-daemon-update.sh to a daemon subcommand (1,733 lines; the binary must replace itself) *(curated)*
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed *(curated)*
- **#8103**: Repo settings: main has no required status checks, so CI cannot block a merge (and stale branches never re-run) *(curated)*
- **#8112**: Two Judges raced on one head: a PR can carry loom:pr and loom:changes-requested at once, and merge-pr.sh only reads the first *(curated)*
- **#8116**: claim_reconciliation + worktree_reaper undo in-session builders: no lease record, so loom:building is flipped back and target/ is reaped mid-build *(curated)*
- **#8119**: dep_recheck named-dependency item_re misses 'Blocked by #N'/'Depends on #N'/'Requires #N' checklist phrasings (false VERDICT=clear) *(curated)*
- **#8121**: workspace add reports 'Registered' but silently skips the hot-apply when run outside the daemon's workspace directory *(curated)*
- **#8122**: Destructive-write guard denies redirects/heredocs targeting paths outside every repo when cwd is a repo with live worktrees *(curated)*
- **#8123**: role_runner 'tick failed' summary quotes the last stderr line (an MCP-config WARN), masking the real cause (token pool exhausted, disk full) *(curated)*
- **#8134**: A stub and its retained suite both mean LOOM_DAEMON_BIN, and they mean different things *(curated)*
- **#8136**: Reconcile PR #8097's differential-corpus gaps with #8125's BotLoginNormalisation work *(curated)*
- **#8138**: Port PR #8058's model-class-marker logic to loom-daemon (Shell Budget Ratchet) *(curated)*
- **#8146**: Token selection: prefer the account that last warmed this (repo, role) prompt cache (65% vs 1.4% hit rate) *(curated)*
- **#8147**: CLAUDE.md's **Loom Version** stamp invalidates every session's cached prefix on each bump *(curated)*
- **#8150**: docs(config_resolver): drop the stale "tier 1 is not yet shipped" framing from two doc comments *(curated)*
- **#8154**: shell-budget ratchet: message says 'say why in the commit' but nothing honours it — no override path, three approved safety PRs blocked *(curated)*
- **#8156**: guard: quoted-delimiter heredoc capture fed to eval/sh -c is still a silent ALLOW (the <<'EOF' sibling of #7970) *(curated)*
- **#8160**: sweep: issue-side existing-PR probe routes a human-authored, unlabeled draft PR to Judge *(curated)*
- **#8166**: guard qsplit(): the closing-quote scan matches an escaped quote, ending a double-quoted span early *(curated)*
- **#8168**: test: watchdog case 20 proves "the probe did not run" with a 2s wall-clock budget, which flakes under host load *(curated)*

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
| In Progress (`loom:building`) | 13 |
| PRs awaiting review | 5 |
| Approved PRs awaiting merge | 2 |
| Curated | 44 |
| Architect / Hermit proposals | 2 |
| Active epics | 4 |
<!-- guide:plan-body:end -->
