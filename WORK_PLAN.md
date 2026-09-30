# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#8559**: Adopt Renovate dependency security policy (14-day quarantine)
- **#8853**: fix(dispatch): admit a renewed lease into the claim-episode comparison
- **#9080**: feat(dashboard): Live tab — perpetually-updating status board (#9077)
- **#9118**: ci: move spawn-claude off the critical path; #9093 review nits
- **#9119**: fix(role-tool-policy): one wildcard rule — fail closed on a glob-shaped capability (#8943)
- **#9143**: fix(merge-pr): raise the merge-pr daemon floor to its newest fail-closed verb
- **#9147**: feat(worktree): port the sparse-checkout family to `loom-daemon worktree-sparse` (#8195 slice 10)
- **#9148**: security: refuse credential-shaped content on commit, push and in CI (#9133)
- **#9159**: chore(deps): bump actions/github-script from 7.1.0 to 9.0.0
- **#9163**: test(usage): close the read idioms the pi/codex source scans missed
- **#9175**: check-duplicate: match cross-reference repo case-insensitively
- **#9236**: feat(accounts): live Codex rate limits via app-server (accounts check --live) + snapshot fixes

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
- **#8460**: guard rmScope: allow removing a private build/target dir the current session created under a scratch root (#8453 item 5)
- **#8589**: Public-surface scrub: fleet EC2 private hostnames in WORK_LOG.md + a Rust doc comment, and operator-domain emails in test fixtures, are live at HEAD
- **#8841**: resync-installed.sh materialized .loom/docs/private-session-dispatch.md as a real file instead of a symlink, breaking Docs/Defaults Parity Check on main
- **#8875**: install over a pre-#4187 install duplicates 11 workflow labels, so sync-labels.sh --check can never converge
- **#8884**: Install-time session-mode flag: empty terminals + loom.sh start refusal
- **#9017**: Queue snapshot: name the workspace_halted hold cause (red main, gate, token pool, drain, breaker)
- **#9043**: Decide whether worktree.sh's stale-reset liveness veto should use the widened (#7466) any-open-fd signal
- **#9051**: observability: host memory/swap/pressure context at role-attempt span boundaries — separate deferred-for-memory from killed from timed-out
- **#9198**: config: enable the CI telemetry poller (owners 2amlogic + rjwalters) and declare fleet.captain = loom-worker-1
- **#9251**: forge-call accounting: per-caller 200/304/pool counts on loom-daemon status (ADR-0021 amendment step 0)
- **#9252**: forge_listing ETag cache: key by resolved owner/repo (not cwd) and persist across daemon restarts
- **#9253**: pipeline_snapshot: stop firing 9 GraphQL lists per repo root; use the ETag REST listing and bound fan-out
- **#9304**: Guard telemetry: rm-scope-unresolved-var denies resolvable scratch-cleanup shapes (57 events)

## In Progress

Issues currently being built (`loom:building`).

- **#9289**: feat: ETA as a Loom primitive — versioned start/finish/land estimates, logged to SigNoz and scored against outcomes
- **#9337**: feat(telemetry): attribute CI re-run trigger (new_commit vs stale_main_bump vs flaky_retry) on story.ci.run spans

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#8531**: feat(guard): admit rm of the session's own private scratch dir under rmScope
- **#8613**: chore: scrub private host identifiers and operator identities from HEAD
- **#8893**: feat(install): add --mode session install-time flag
- **#9142**: fix(resync): create the dogfood .loom/docs symlink for a brand-new defaults doc
- **#9155**: chore(deps): bump sigstore/cosign-installer from 3.9.1 to 4.1.2
- **#9156**: chore(deps): bump the all-dependencies group with 2 updates
- **#9158**: chore(deps-dev): bump the all-dependencies group in /dashboard with 3 updates
- **#9206**: feat(curator): backlog rightsizing and sibling consolidation gate (#9026)
- **#9218**: fix(merge-pr): recognize feature/harness-ops-<N> stacked-parent branches
- **#9250**: docs(adr-0021): amend — the forge event feed buys down forge calls, not only latency
- **#9256**: fix(daemon): serve pipeline_snapshot counts from cached REST, bound fan-out (#9253)
- **#9261**: feat(daemon): forge-call accounting + identity-keyed, disk-backed listing cache (#9251, #9252)
- **#9271**: docs(worktree-safety): decide the cwd-only-vs-any-open-fd tradeoff for the stale-worktree reset veto (#9043)
- **#9276**: feat(merge-pr): port the worktree remove-vs-preserve decision to Rust (#8191 slice)
- **#9307**: chore(deps): bump the all-dependencies group across 1 directory with 3 updates
- **#9351**: train: L1 (#9080, #9119, #9143, #9163, #9175, #9236)
- **#9352**: train: L2 (#8559, #9118)

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#8559**: Adopt Renovate dependency security policy (14-day quarantine)
- **#8853**: fix(dispatch): admit a renewed lease into the claim-episode comparison
- **#9080**: feat(dashboard): Live tab — perpetually-updating status board (#9077)
- **#9118**: ci: move spawn-claude off the critical path; #9093 review nits
- **#9119**: fix(role-tool-policy): one wildcard rule — fail closed on a glob-shaped capability (#8943)
- **#9143**: fix(merge-pr): raise the merge-pr daemon floor to its newest fail-closed verb
- **#9147**: feat(worktree): port the sparse-checkout family to `loom-daemon worktree-sparse` (#8195 slice 10)
- **#9148**: security: refuse credential-shaped content on commit, push and in CI (#9133)
- **#9159**: chore(deps): bump actions/github-script from 7.1.0 to 9.0.0
- **#9163**: test(usage): close the read idioms the pi/codex source scans missed
- **#9175**: check-duplicate: match cross-reference repo case-insensitively
- **#9236**: feat(accounts): live Codex rate limits via app-server (accounts check --live) + snapshot fixes
- **#9317**: fix(guard): resolve NAME=$(cd <path> && pwd)/$(realpath <path>) in force-op cwd capture
- **#9348**: feat(ci-telemetry): attribute each CI run's trigger on loom.ci.run (#9337)

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
- **#8460**: guard rmScope: allow removing a private build/target dir the current session created under a scratch root (#8453 item 5) *(curated)*
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
- **#8840**: Investigate daemon dispatch superseding a freshly renewed in-session sweep lease *(curated)*
- **#8841**: resync-installed.sh materialized .loom/docs/private-session-dispatch.md as a real file instead of a symlink, breaking Docs/Defaults Parity Check on main *(curated)*
- **#8875**: install over a pre-#4187 install duplicates 11 workflow labels, so sync-labels.sh --check can never converge *(curated)*
- **#8879**: bot-PR review routing is Dependabot-only: Renovate repos silently lose Judge review *(curated)*
- **#8884**: Install-time session-mode flag: empty terminals + loom.sh start refusal *(curated)*
- **#8902**: Fleet captain: alert when the declared captain stops reporting host.health (lease, not failover) *(curated)*
- **#8913**: observability: live-verify ci.job.log reconstruction in SigNoz and record it in evidence.md (#8825 AC1) *(curated)*
- **#8917**: ci-telemetry: the local journal has no rotation and is read whole on every backfill — phase-2 log capture makes it a GB/day, whole-file-read problem *(curated)*
- **#8943**: Unify the two wildcard rules in the per-role tool restriction: spawn-claude's substring test fails open where spawn-codex's exact match fails closed *(curated)*
- **#8950**: Live-verify Pi tokens_by_model against a real LOOM_RUNTIME=pi launch *(curated)*
- **#8959**: Harden the pi_usage / codex_usage source-scan tests against missed read idioms *(curated)*
- **#8967**: merge-pr.sh: the 'requires-daemon: merge-pr >= 0.19.172' floor is stale; fail-closed loom-pr-guard needs >= 0.19.375 *(curated)*
- **#9006**: observability: live-verify the SigNoz queue-starvation alert rule fires and resolves (#8856 / PR #8935 follow-up) *(curated)*
- **#9017**: Queue snapshot: name the workspace_halted hold cause (red main, gate, token pool, drain, breaker) *(curated)*
- **#9043**: Decide whether worktree.sh's stale-reset liveness veto should use the widened (#7466) any-open-fd signal *(curated)*
- **#9045**: Doctor: Priority 1 conflict query doesn't exclude loom:operator-only/-decision, only loom:operator *(curated)*
- **#9051**: observability: host memory/swap/pressure context at role-attempt span boundaries — separate deferred-for-memory from killed from timed-out *(curated)*
- **#9062**: Rejection telemetry counts daemon base-conflict flags (#8922) as Judge rejections *(curated)*
- **#9063**: feat(champion): dynamic mega-PR batching and merge-train consolidation under high PR congestion *(curated)*
- **#9065**: CI: cut PR wall time from ~8.5 min to ≤5 min (build once, dedupe nextest, shard serial suites) and stop false-stale merges *(curated)*
- **#9067**: flake: private_workspace_docker adapter_chain_pushes_private_branch fixture SIGTERMed (exit 143) in Codex Adapter Smoke *(curated)*
- **#9077**: dashboard: add a Live tab — perpetually-updating status board *(curated)*
- **#9198**: config: enable the CI telemetry poller (owners 2amlogic + rjwalters) and declare fleet.captain = loom-worker-1 *(curated)*
- **#9233**: accounts probe: live Codex rate limits via app-server (in-container for session-managed accounts) + #8963 snapshot fixes *(curated)*
- **#9251**: forge-call accounting: per-caller 200/304/pool counts on loom-daemon status (ADR-0021 amendment step 0) *(curated)*
- **#9252**: forge_listing ETag cache: key by resolved owner/repo (not cwd) and persist across daemon restarts *(curated)*
- **#9253**: pipeline_snapshot: stop firing 9 GraphQL lists per repo root; use the ETag REST listing and bound fan-out *(curated)*
- **#9304**: Guard telemetry: rm-scope-unresolved-var denies resolvable scratch-cleanup shapes (57 events) *(curated)*
- **#9312**: Guard telemetry: force-op:detached blocks own-branch worktree resync via -C "$VAR" (31 events) *(curated)*
- **#9337**: feat(telemetry): attribute CI re-run trigger (new_commit vs stale_main_bump vs flaky_retry) on story.ci.run spans *(curated)*

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

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 12 |
| Operator priority | 3 |
| Ready (`loom:issue`) | 17 |
| In Progress (`loom:building`) | 2 |
| PRs awaiting review | 17 |
| Approved PRs awaiting merge | 14 |
| Curated | 64 |
| Architect / Hermit proposals | 3 |
| Active epics | 6 |
<!-- guide:plan-body:end -->
