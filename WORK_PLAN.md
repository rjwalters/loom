# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#7870**: feat(resync): warn and block instead of silently reverting a locally-fixed installed file
- **#7978**: fix(guards): port sky130-modexp mkdir/qsplit-continuation guard-hook fail-open fixes
- **#8016**: fix(guard): admit the exact mktemp-then-canonicalize chain in both same-command fast paths
- **#8019**: fix(guard): #7970 heredoc variable-capture masking has no $NAME read check (ask tier)
- **#8089**: fix(pricing): key the model rate card by generation, not family stem
- **#8117**: fix(merge-pr): reconcile stacked children when --auto only queues the merge
- **#8135**: feat(daemon): port claude-wrapper.sh's six retry classifiers (#8037)

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
- **#8025**: guard qsplit(): an unquoted backslash-escaped quote enters the quoted-span branch, hiding a real statement boundary
- **#8028**: [epic #7810 PR 6a] Port the artifact fetch + verification; split Phase 6 into slices
- **#8035**: guard: an unquoted heredoc body's $( ) substitution bypasses worktree write confinement (write-path analogue of #8003)
- **#8056**: telemetry: outcome journal lacks judge verdicts, doctor cycles, failure class, effort, token account — and role-runner ticks emit no record at all
- **#8060**: models: pricing table is one to two generations stale (opus 3× over, haiku 4× under, fable priced as opus); dormant workflows pin gen-4 IDs
- **#8064**: Trim narrative/reference bloat from champion-*/judge-reference/watch skills and .loom/CLAUDE.md
- **#8075**: install/hygiene: .loom-local/ overlay is not gitignored in consumer repos and not Loom-owned — quarantine stashes it, silently reverting model overrides
- **#8093**: Test 12C in test-champion-held-pr-health-pass.sh is non-discriminating: it passes with the pin line deleted AND with its own || true removed
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed
- **#8112**: Two Judges raced on one head: a PR can carry loom:pr and loom:changes-requested at once, and merge-pr.sh only reads the first
- **#8113**: Differential test's pattern literal models the retired SHELL, and nothing says so
- **#8121**: workspace add reports 'Registered' but silently skips the hot-apply when run outside the daemon's workspace directory
- **#8123**: role_runner 'tick failed' summary quotes the last stderr line (an MCP-config WARN), masking the real cause (token pool exhausted, disk full)
- **#8134**: A stub and its retained suite both mean LOOM_DAEMON_BIN, and they mean different things
- **#8136**: Reconcile PR #8097's differential-corpus gaps with #8125's BotLoginNormalisation work
- **#8140**: A retained suite's assertions about the SHELL's source text cannot survive the port, and the recipe is silent on them

## In Progress

Issues currently being built (`loom:building`).

- **#8054**: cost mode for the role-runner path: machine-level roleModels/effort defaults, honour sweep.optimization, emit --effort on role ticks
- **#8059**: telemetry: ingest transcript token usage into activity.db for dispatch-based sweeps (resource_usage/token_usage are empty)
- **#8066**: Reorder role spawn prefix so stable content precedes volatile content for cross-session cache hits
- **#8077**: Builder test runs leak into the live host: test daemons log to ~/.loom/daemon.log and reload the production user systemd manager (#7873 sweep on loom-worker-2)
- **#8086**: Port loom-daemon-watchdog.sh to a daemon subcommand (994 lines, 233 retained assertions)
- **#8103**: Repo settings: main has no required status checks, so CI cannot block a merge (and stale branches never re-run)
- **#8116**: claim_reconciliation + worktree_reaper undo in-session builders: no lease record, so loom:building is flipped back and target/ is reaped mid-build
- **#8119**: dep_recheck named-dependency item_re misses 'Blocked by #N'/'Depends on #N'/'Requires #N' checklist phrasings (false VERDICT=clear)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#8118**: test(champion): make Test 12C discriminate by executing the shipped pin line
- **#8126**: test(differential_extract_refs): document the frozen oracle pattern, run rendering invariant on every case
- **#8139**: docs(prompt-budget): trim narrative bloat from champion-reference/judge-reference/watch/.loom-CLAUDE.md
- **#8141**: docs(recipe): what to do with a retained assertion that cannot survive the port (#8140)
- **#8142**: fix(gates): derive the shell-discovery floors from the allowlist, not a constant (#8120)
- **#8143**: chore: resync installed Loom surfaces

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#7870**: feat(resync): warn and block instead of silently reverting a locally-fixed installed file
- **#7978**: fix(guards): port sky130-modexp mkdir/qsplit-continuation guard-hook fail-open fixes
- **#8016**: fix(guard): admit the exact mktemp-then-canonicalize chain in both same-command fast paths
- **#8019**: fix(guard): #7970 heredoc variable-capture masking has no $NAME read check (ask tier)
- **#8089**: fix(pricing): key the model rate card by generation, not family stem
- **#8117**: fix(merge-pr): reconcile stacked children when --auto only queues the merge
- **#8135**: feat(daemon): port claude-wrapper.sh's six retry classifiers (#8037)

## Proposed

Issues carrying `loom:curated`.

- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space *(curated)*
- **#7855**: robb-pro daemon stopped heartbeating and answering IPC for 25 min while still logging; the watchdog CONFIRMED the wedge twice and never restarted it *(curated)*
- **#7864**: Port 2AMLogic/sky130-modexp#117's resync-installed.sh local-divergence protection to defaults/ *(curated)*
- **#7945**: Port sky130-modexp's mkdir/qsplit-continuation guard-hook fail-open fixes (#116/#121) to defaults/ *(curated)*
- **#7947**: [#4196 Phase 3b] Operator-agent persona: natural-language intent over the typed daemon ChatOps surface *(curated)*
- **#7970**: guard: #7355 heredoc variable-capture masking has no $NAME read check (ask tier) *(curated)*
- **#7972**: work_finder re-claims #7893 in a loop: 14 claims, 55 label events, 4 hours, zero PRs *(curated)*
- **#7986**: Guard: same-command mktemp safe-path denies the routine `VAR=$(cd "$VAR" && pwd -P)` realpath-canonicalization reassignment *(curated)*
- **#8001**: [Part of #7708] Broadcast the pool-exhaustion hold to peers via the peer-claim room *(curated)*
- **#8005**: Generalise #7818's credential-staging guard from two hardcoded gh-config paths to the credential-bearing class (.loom/tokens/, accounts.env, claude-config/) *(curated)*
- **#8010**: Stacked-parent reconciliation is not reliably reachable after #7982's pin-and-warn downgrade *(curated)*
- **#8013**: test-guard-destructive-rm-scope.sh:338 '../ escaping the repo' assertion fails when the suite runs from a linked worktree (passes from the primary checkout) *(curated)*
- **#8015**: Add doc-comment pointer to prose-existence rule in sweep_md_stage_minus_one_doc_lint.rs *(curated)*
- **#8025**: guard qsplit(): an unquoted backslash-escaped quote enters the quoted-span branch, hiding a real statement boundary *(curated)*
- **#8026**: peer_coordination DEGRADED can be a false positive during a fleet-wide dispatch lull (advertise is dispatch-gated, not periodic) *(curated)*
- **#8028**: [epic #7810 PR 6a] Port the artifact fetch + verification; split Phase 6 into slices *(curated)*
- **#8035**: guard: an unquoted heredoc body's $( ) substitution bypasses worktree write confinement (write-path analogue of #8003) *(curated)*
- **#8037**: Port claude-wrapper.sh's six retry classifiers, with #8032's 44 assertions as the proof *(curated)*
- **#8048**: merge-pr.sh --auto exits before reconciling stacked children when the server-side merge is only queued (#8010 item 1) *(curated)*
- **#8052**: telemetry: per-role × model token consumption report + weekly-limit calibration (activity.db token tables are empty, `loom-daemon stats` crashes) *(curated)*
- **#8054**: cost mode for the role-runner path: machine-level roleModels/effort defaults, honour sweep.optimization, emit --effort on role ticks *(curated)*
- **#8055**: experiment: fleet-wide repo-stratified model A/B — assign arms, write/remove overlays, cover role-runner ticks, stamp the arm explicitly in outcome records *(curated)*
- **#8056**: telemetry: outcome journal lacks judge verdicts, doctor cycles, failure class, effort, token account — and role-runner ticks emit no record at all *(curated)*
- **#8058**: token pool: per-model-class exhaustion state — an Opus ceiling bad-marks the whole account and starves Sonnet work *(curated)*
- **#8059**: telemetry: ingest transcript token usage into activity.db for dispatch-based sweeps (resource_usage/token_usage are empty) *(curated)*
- **#8060**: models: pricing table is one to two generations stale (opus 3× over, haiku 4× under, fable priced as opus); dormant workflows pin gen-4 IDs *(curated)*
- **#8064**: Trim narrative/reference bloat from champion-*/judge-reference/watch skills and .loom/CLAUDE.md *(curated)*
- **#8066**: Reorder role spawn prefix so stable content precedes volatile content for cross-session cache hits *(curated)*
- **#8075**: install/hygiene: .loom-local/ overlay is not gitignored in consumer repos and not Loom-owned — quarantine stashes it, silently reverting model overrides *(curated)*
- **#8077**: Builder test runs leak into the live host: test daemons log to ~/.loom/daemon.log and reload the production user systemd manager (#7873 sweep on loom-worker-2) *(curated)*
- **#8086**: Port loom-daemon-watchdog.sh to a daemon subcommand (994 lines, 233 retained assertions) *(curated)*
- **#8087**: Port loom-daemon-start.sh to a daemon subcommand (1,184 lines; preserve the FLAGS-OFF contract across start) *(curated)*
- **#8088**: Port loom-daemon-update.sh to a daemon subcommand (1,733 lines; the binary must replace itself) *(curated)*
- **#8093**: Test 12C in test-champion-held-pr-health-pass.sh is non-discriminating: it passes with the pin line deleted AND with its own || true removed *(curated)*
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed *(curated)*
- **#8103**: Repo settings: main has no required status checks, so CI cannot block a merge (and stale branches never re-run) *(curated)*
- **#8112**: Two Judges raced on one head: a PR can carry loom:pr and loom:changes-requested at once, and merge-pr.sh only reads the first *(curated)*
- **#8113**: Differential test's pattern literal models the retired SHELL, and nothing says so *(curated)*
- **#8116**: claim_reconciliation + worktree_reaper undo in-session builders: no lease record, so loom:building is flipped back and target/ is reaped mid-build *(curated)*
- **#8119**: dep_recheck named-dependency item_re misses 'Blocked by #N'/'Depends on #N'/'Requires #N' checklist phrasings (false VERDICT=clear) *(curated)*
- **#8120**: Shell-lint's 400-file floor will fail as epic #7810 succeeds, and blame 'broken discovery' *(curated)*
- **#8121**: workspace add reports 'Registered' but silently skips the hot-apply when run outside the daemon's workspace directory *(curated)*
- **#8122**: Destructive-write guard denies redirects/heredocs targeting paths outside every repo when cwd is a repo with live worktrees *(curated)*
- **#8123**: role_runner 'tick failed' summary quotes the last stderr line (an MCP-config WARN), masking the real cause (token pool exhausted, disk full) *(curated)*
- **#8134**: A stub and its retained suite both mean LOOM_DAEMON_BIN, and they mean different things *(curated)*
- **#8136**: Reconcile PR #8097's differential-corpus gaps with #8125's BotLoginNormalisation work *(curated)*
- **#8138**: Port PR #8058's model-class-marker logic to loom-daemon (Shell Budget Ratchet) *(curated)*
- **#8140**: A retained suite's assertions about the SHELL's source text cannot survive the port, and the recipe is silent on them *(curated)*

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
| Operator merge-risk holds | 7 |
| Urgent | 3 |
| Ready (`loom:issue`) | 19 |
| In Progress (`loom:building`) | 8 |
| PRs awaiting review | 6 |
| Approved PRs awaiting merge | 7 |
| Curated | 49 |
| Architect / Hermit proposals | 2 |
| Active epics | 4 |
<!-- guide:plan-body:end -->
