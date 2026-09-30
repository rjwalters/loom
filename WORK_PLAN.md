# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#9514**: feat(merge-pr): record-rework — the first writer for #9444's rework-event markers

## Operator Priority

Issues the operator starred (`loom:operator-priority`); land these first.

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8195**: Port worktree.sh to a daemon subcommand (1,812 lines; 26 fixes in 6 months, three of them data-loss classes)
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh
- **#8589**: Public-surface scrub: fleet EC2 private hostnames in WORK_LOG.md + a Rust doc comment, and operator-domain emails in test fixtures, are live at HEAD
- **#9017**: Queue snapshot: name the workspace_halted hold cause (red main, gate, token pool, drain, breaker)
- **#9043**: Decide whether worktree.sh's stale-reset liveness veto should use the widened (#7466) any-open-fd signal
- **#9085**: CI: add a slow, thorough daily run to backstop the fast per-commit gate
- **#9089**: ci-telemetry: step spans, per-job queue wait, and shard/suite/test spans so CI sharding is observable in SigNoz
- **#9096**: Champion: a killed merge-pr.sh --auto call leaves no forge-visible outcome (follow-up to #9091 item 2)
- **#9111**: worktree.sh --json: the "preserve existing work" path exits 0 with empty stdout, emitting no JSON document
- **#9198**: config: enable the CI telemetry poller (owners 2amlogic + rjwalters) and declare fleet.captain = loom-worker-1
- **#9287**: install/upgrade never verifies the repo's merge configuration — a ruleset can make merge-pr.sh structurally unable to merge, silently
- **#9576**: verdict-staleness-guard.sh drops Judge verdicts on tree-identical re-date commits — the daemon path's #9124 tree-unchanged exemption was never ported

## In Progress

Issues currently being built (`loom:building`).

- **#9304**: Guard telemetry: rm-scope-unresolved-var denies resolvable scratch-cleanup shapes (57 events)
- **#9434**: [Epic #9429] story-points: estimate-vs-actual calibration loop
- **#9548**: Authenticate Loom's control signals: markers and phrases count only from trusted authors (work safely on any public repo)
- **#9572**: daemon: Issue dispatch_sweep takes 30–70s — ~20 serial gh calls under the registry mutex on a tokio worker; retry not idempotent

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#9271**: docs(worktree-safety): decide the cwd-only-vs-any-open-fd tradeoff for the stale-worktree reset veto (#9043)
- **#9396**: docs(champion): pin the merge settle budget under the caller's timeout and report a killed merge
- **#9506**: feat(telemetry): first writer for the rework-event marker protocol (#9444)
- **#9581**: fix: share the tree-identical test between both verdict-invalidation paths

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#9514**: feat(merge-pr): record-rework — the first writer for #9444's rework-event markers
- **#9578**: feat(eta): eta backfill + leak-free eta backtest harness (#9325)

## Proposed

Issues carrying `loom:curated`.

- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#7855**: robb-pro daemon stopped heartbeating and answering IPC for 25 min while still logging; the watchdog CONFIRMED the wedge twice and never restarted it *(curated)*
- **#8052**: telemetry: per-role × model token consumption report + weekly-limit calibration (activity.db token tables are empty, `loom-daemon stats` crashes) *(curated)*
- **#8055**: experiment: fleet-wide repo-stratified model A/B — assign arms, write/remove overlays, cover role-runner ticks, stamp the arm explicitly in outcome records *(curated)*
- **#8058**: token pool: per-model-class exhaustion state — an Opus ceiling bad-marks the whole account and starves Sonnet work *(curated)*
- **#8103**: Repo settings: main has no required status checks, so CI cannot block a merge (and stale branches never re-run) *(curated)*
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
- **#8195**: Port worktree.sh to a daemon subcommand (1,812 lines; 26 fixes in 6 months, three of them data-loss classes) *(curated)*
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh *(curated)*
- **#8257**: dashboard: add an ephemeral_compute record type (running-now + elastic-spend views, leak detection, hostless ingest) with a D1 migration *(curated)*
- **#8370**: Concurrent sweeps exhaust host disk via per-worktree cargo target dirs; surfaces as unrelated StorageFull test failures *(curated)*
- **#8387**: spawn-codex.sh forwards a `model@effort` suffix verbatim to the Codex CLI's `-m` *(curated)*
- **#8434**: live-verify native-ephemeral containment: canary run in-container, two-worker filesystem disjointness, post-run writable-layer credential scan *(curated)*
- **#8505**: Scheduled role ticks on the metered OpenCode runtime burned ~12% of the GLM-5.3 trial on launches that could never succeed (E2BIG prompt argv + toolless), relaunched every cycle *(curated)*
- **#8525**: tracing: instrument sweep phases and role attempts, including Pi/OpenCode and repair cycles *(curated)*
- **#8527**: observability: add the self-hosted ClickStack/HyperDX trial with Loom trace and log views *(curated)*
- **#8528**: observability: add the self-hosted SigNoz trial using supported Foundry deployment and Loom traces *(curated)*
- **#8529**: observability: validate both backends with the same Loom traces and publish a comparison *(curated)*
- **#8570**: Guard: refuse a build/scratch dir assignment that resolves onto a tmpfs mount (upstream PR to rjwalters/repo, split from #8512) *(curated)*
- **#8576**: observability: document managed-cloud fanout and verify indexed data in both backends *(curated)*
- **#8589**: Public-surface scrub: fleet EC2 private hostnames in WORK_LOG.md + a Rust doc comment, and operator-domain emails in test fixtures, are live at HEAD *(curated)*
- **#8606**: Run a live Kimi Code CLI canary with a real Moonshot/Kimi credential and record a docs/experiments receipt *(curated)*
- **#8628**: Kimi account pool C2: AccountProvider::Kimi, kimi login lifecycle CLI, per-account KIMI_CODE_HOME, availability probe *(curated)*
- **#8667**: Fleet feed: ModelLabels.tsx needs a Kimi/Moonshot label+icon mapping (marketing-site repo, follow-up to #8564/#8507) *(curated)*
- **#8698**: Live end-to-end verification of the credential egress proxy (AC5 of #8674) *(curated)*
- **#8726**: resync-ignore pins record no fork point: 'can this pin be lifted yet?' is archaeology, not a diff *(curated)*
- **#8730**: config: this repo pins runtimes.roles.judge = "codex" on a host with no Codex account — 157 Judge ticks skipped before spawn *(curated)*
- **#8742**: loom:blocked is silently stripped from operator-ruled permanent blocks (unblock probe treats 'unparseable blocker' as 'no blocker') *(curated)*
- **#8761**: Concierge: durable room inbox so messages sent between ticks are not lost (Phase 4 of #4196) *(curated)*
- **#8790**: Reaper resume-dispatch tests fail under ambient LOOM_RUNTIME override (runtime admission demands mcp) *(curated)*
- **#8801**: Credential discovery convention: agents should not need operators to re-state where keys live every session *(curated)*
- **#8812**: fleet add-worker: verify step proves the worker booted, not that it narrates (no end-to-end assertion) *(curated)*
- **#8813**: Sweep teardown does not kill its own process group -- orphaned sleep infinity holders outlive dead sweeps (cf. #7825) *(curated)*
- **#8879**: bot-PR review routing is Dependabot-only: Renovate repos silently lose Judge review *(curated)*
- **#8902**: Fleet captain: alert when the declared captain stops reporting host.health (lease, not failover) *(curated)*
- **#8913**: observability: live-verify ci.job.log reconstruction in SigNoz and record it in evidence.md (#8825 AC1) *(curated)*
- **#8917**: ci-telemetry: the local journal has no rotation and is read whole on every backfill — phase-2 log capture makes it a GB/day, whole-file-read problem *(curated)*
- **#8950**: Live-verify Pi tokens_by_model against a real LOOM_RUNTIME=pi launch *(curated)*
- **#9006**: observability: live-verify the SigNoz queue-starvation alert rule fires and resolves (#8856 / PR #8935 follow-up) *(curated)*
- **#9017**: Queue snapshot: name the workspace_halted hold cause (red main, gate, token pool, drain, breaker) *(curated)*
- **#9043**: Decide whether worktree.sh's stale-reset liveness veto should use the widened (#7466) any-open-fd signal *(curated)*
- **#9045**: Doctor: Priority 1 conflict query doesn't exclude loom:operator-only/-decision, only loom:operator *(curated)*
- **#9062**: Rejection telemetry counts daemon base-conflict flags (#8922) as Judge rejections *(curated)*
- **#9063**: feat(champion): dynamic mega-PR batching and merge-train consolidation under high PR congestion *(curated)*
- **#9065**: CI: cut PR wall time from ~8.5 min to ≤5 min (build once, dedupe nextest, shard serial suites) and stop false-stale merges *(curated)*
- **#9079**: provenance: D33 record in lease/verdict comments, prompt hash, and in-session trailers (follow-up to #9027) *(curated)*
- **#9085**: CI: add a slow, thorough daily run to backstop the fast per-commit gate *(curated)*
- **#9089**: ci-telemetry: step spans, per-job queue wait, and shard/suite/test spans so CI sharding is observable in SigNoz *(curated)*
- **#9096**: Champion: a killed merge-pr.sh --auto call leaves no forge-visible outcome (follow-up to #9091 item 2) *(curated)*
- **#9111**: worktree.sh --json: the "preserve existing work" path exits 0 with empty stdout, emitting no JSON document *(curated)*
- **#9126**: Document the quoted-heredoc rule in comment-body-literal-path.md: an unquoted delimiter silently executes and deletes backticked prose *(curated)*
- **#9136**: main ruleset: bypass_actors 'RepositoryRole:always' makes pull_request and required_status_checks advisory *(curated)*
- **#9152**: worktree-link: an already-created worktree keeps its pnpm node_modules alias (#8944 leaves existing worktrees unfixed) *(curated)*
- **#9198**: config: enable the CI telemetry poller (owners 2amlogic + rjwalters) and declare fleet.captain = loom-worker-1 *(curated)*
- **#9201**: ci-telemetry: drive capture from workflow_run/workflow_job webhooks (forge_events feed); demote the repo sweep to a slow correction floor *(curated)*
- **#9287**: install/upgrade never verifies the repo's merge configuration — a ruleset can make merge-pr.sh structurally unable to merge, silently *(curated)*
- **#9304**: Guard telemetry: rm-scope-unresolved-var denies resolvable scratch-cleanup shapes (57 events) *(curated)*
- **#9323**: Guard: extract_rm_targets() misses a loop/conditional-body rm written as a one-liner (`; do rm -rf …`) *(curated)*
- **#9325**: ETA phase 2: backfill + leak-free backtest harness (eta backfill, eta backtest) *(curated)*
- **#9548**: Authenticate Loom's control signals: markers and phrases count only from trusted authors (work safely on any public repo) *(curated)*
- **#9572**: daemon: Issue dispatch_sweep takes 30–70s — ~20 serial gh calls under the registry mutex on a tokio worker; retry not idempotent *(curated)*
- **#9576**: verdict-staleness-guard.sh drops Judge verdicts on tree-identical re-date commits — the daemon path's #9124 tree-unchanged exemption was never ported *(curated)*

## Proposed (Architect / Hermit)

- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#8788**: Evaluate Codex private-workspace efficiency after the first production canary *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam
- **#7810**: [epic] Retire shell: 10 sequential PRs, two foundational then update + agent execution
- **#8522**: Epic: send Loom traces, logs, and metrics to ClickStack/HyperDX and SigNoz for a side-by-side trial
- **#8764**: Forge event plane: push GitHub events to daemons via operator Webhook Worker feed (ADR-0021, lifts ADR-0014 Lever C)
- **#9429**: Epic: Fibonacci story points — a size-weighted throughput measure Loom can optimize

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 1 |
| Operator priority | 3 |
| Ready (`loom:issue`) | 14 |
| In Progress (`loom:building`) | 4 |
| PRs awaiting review | 4 |
| Approved PRs awaiting merge | 2 |
| Curated | 62 |
| Architect / Hermit proposals | 3 |
| Active epics | 7 |
<!-- guide:plan-body:end -->
