# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#8559**: Adopt Renovate dependency security policy (14-day quarantine)
- **#9119**: fix(role-tool-policy): one wildcard rule — fail closed on a glob-shaped capability (#8943)

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh
- **#8838**: rework #8486 (issue #8458): bring the per-worktree CARGO_TARGET_DIR portable-shell delta to <= 0 (declaration commit ineffective - portable pool has no declare-exit)
- **#8841**: resync-installed.sh materialized .loom/docs/private-session-dispatch.md as a real file instead of a symlink, breaking Docs/Defaults Parity Check on main

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8058**: token pool: per-model-class exhaustion state — an Opus ceiling bad-marks the whole account and starves Sonnet work
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed
- **#8136**: Reconcile PR #8097's differential-corpus gaps with #8125's BotLoginNormalisation work
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh
- **#8287**: worktree.sh reset a stale local feature/issue-N to main although origin/feature/issue-N carried the PR's commits (Doctor on #8190)
- **#8354**: Port _worktree_resolve_stale_reset_ref (#8287) to loom-daemon per shell-language-policy
- **#8458**: cargo: per-worktree CARGO_TARGET_DIR wired to worktree lifecycle, preserving the #6013/#6014 binary-reuse fast path (#8453 item 2)
- **#8460**: guard rmScope: allow removing a private build/target dir the current session created under a scratch root (#8453 item 5)
- **#8589**: Public-surface scrub: fleet EC2 private hostnames in WORK_LOG.md + a Rust doc comment, and operator-domain emails in test fixtures, are live at HEAD
- **#8787**: Admit Codex mutable roles using verified private-clone containment
- **#8838**: rework #8486 (issue #8458): bring the per-worktree CARGO_TARGET_DIR portable-shell delta to <= 0 (declaration commit ineffective - portable pool has no declare-exit)
- **#8841**: resync-installed.sh materialized .loom/docs/private-session-dispatch.md as a real file instead of a symlink, breaking Docs/Defaults Parity Check on main
- **#8875**: install over a pre-#4187 install duplicates 11 workflow labels, so sync-labels.sh --check can never converge
- **#8884**: Install-time session-mode flag: empty terminals + loom.sh start refusal
- **#8925**: Blocked PRs have no unblock path: the unblock sweep lists issues only, and a park recorded in a comment leaves no parseable dependency

## In Progress

Issues currently being built (`loom:building`).

- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8195**: Port worktree.sh to a daemon subcommand (1,812 lines; 26 fixes in 6 months, three of them data-loss classes)
- **#8923**: Investigate and reduce PR latency: the review queue is 4h deep, but approved-awaiting-merge is 68h and changes-requested is 90h
- **#9123**: install.sh --full fails its own completeness check: .loom/credentials.md.example is recorded in install-metadata.json but never copied (--quick hides it)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#8531**: feat(guard): admit rm of the session's own private scratch dir under rmScope
- **#8613**: chore: scrub private host identifiers and operator identities from HEAD
- **#8893**: feat(install): add --mode session install-time flag
- **#9046**: fix(security): remove leaked token-pool copy from main; ignore sibling token dirs
- **#9080**: feat(dashboard): Live tab — perpetually-updating status board (#9077)
- **#9099**: feat(dashboard): Live board cards never move, and show label transitions (#9094)
- **#9137**: feat(daemon): measure PR queue latency by segment and surface silent queues pre-wave

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#8559**: Adopt Renovate dependency security policy (14-day quarantine)
- **#8680**: feat(daemon-update): port loom-daemon-update.sh to a daemon subcommand
- **#8853**: fix(dispatch): admit a renewed lease into the claim-episode comparison
- **#9118**: ci: move spawn-claude off the critical path; #9093 review nits
- **#9119**: fix(role-tool-policy): one wildcard rule — fail closed on a glob-shaped capability (#8943)

## Proposed

Issues carrying `loom:curated`.

- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#7855**: robb-pro daemon stopped heartbeating and answering IPC for 25 min while still logging; the watchdog CONFIRMED the wedge twice and never restarted it *(curated)*
- **#8052**: telemetry: per-role × model token consumption report + weekly-limit calibration (activity.db token tables are empty, `loom-daemon stats` crashes) *(curated)*
- **#8055**: experiment: fleet-wide repo-stratified model A/B — assign arms, write/remove overlays, cover role-runner ticks, stamp the arm explicitly in outcome records *(curated)*
- **#8058**: token pool: per-model-class exhaustion state — an Opus ceiling bad-marks the whole account and starves Sonnet work *(curated)*
- **#8088**: Port loom-daemon-update.sh to a daemon subcommand (1,733 lines; the binary must replace itself) *(curated)*
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed *(curated)*
- **#8103**: Repo settings: main has no required status checks, so CI cannot block a merge (and stale branches never re-run) *(curated)*
- **#8136**: Reconcile PR #8097's differential-corpus gaps with #8125's BotLoginNormalisation work *(curated)*
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
- **#8195**: Port worktree.sh to a daemon subcommand (1,812 lines; 26 fixes in 6 months, three of them data-loss classes) *(curated)*
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh *(curated)*
- **#8257**: dashboard: add an ephemeral_compute record type (running-now + elastic-spend views, leak detection, hostless ingest) with a D1 migration *(curated)*
- **#8287**: worktree.sh reset a stale local feature/issue-N to main although origin/feature/issue-N carried the PR's commits (Doctor on #8190) *(curated)*
- **#8354**: Port _worktree_resolve_stale_reset_ref (#8287) to loom-daemon per shell-language-policy *(curated)*
- **#8370**: Concurrent sweeps exhaust host disk via per-worktree cargo target dirs; surfaces as unrelated StorageFull test failures *(curated)*
- **#8387**: spawn-codex.sh forwards a `model@effort` suffix verbatim to the Codex CLI's `-m` *(curated)*
- **#8434**: live-verify native-ephemeral containment: canary run in-container, two-worker filesystem disjointness, post-run writable-layer credential scan *(curated)*
- **#8458**: cargo: per-worktree CARGO_TARGET_DIR wired to worktree lifecycle, preserving the #6013/#6014 binary-reuse fast path (#8453 item 2) *(curated)*
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
- **#8699**: Per-launch usage attribution and 429 bad-marking at the credential egress proxy (follow-up to #8674) *(curated)*
- **#8726**: resync-ignore pins record no fork point: 'can this pin be lifted yet?' is archaeology, not a diff *(curated)*
- **#8730**: config: this repo pins runtimes.roles.judge = "codex" on a host with no Codex account — 157 Judge ticks skipped before spawn *(curated)*
- **#8742**: loom:blocked is silently stripped from operator-ruled permanent blocks (unblock probe treats 'unparseable blocker' as 'no blocker') *(curated)*
- **#8761**: Concierge: durable room inbox so messages sent between ticks are not lost (Phase 4 of #4196) *(curated)*
- **#8787**: Admit Codex mutable roles using verified private-clone containment *(curated)*
- **#8790**: Reaper resume-dispatch tests fail under ambient LOOM_RUNTIME override (runtime admission demands mcp) *(curated)*
- **#8801**: Credential discovery convention: agents should not need operators to re-state where keys live every session *(curated)*
- **#8812**: fleet add-worker: verify step proves the worker booted, not that it narrates (no end-to-end assertion) *(curated)*
- **#8813**: Sweep teardown does not kill its own process group -- orphaned sleep infinity holders outlive dead sweeps (cf. #7825) *(curated)*
- **#8818**: Account rotation for proxied Claude containers: bad-mark and swap at the egress proxy (follow-up to #8697) *(curated)*
- **#8838**: rework #8486 (issue #8458): bring the per-worktree CARGO_TARGET_DIR portable-shell delta to <= 0 (declaration commit ineffective - portable pool has no declare-exit) *(curated)*
- **#8840**: Investigate daemon dispatch superseding a freshly renewed in-session sweep lease *(curated)*
- **#8841**: resync-installed.sh materialized .loom/docs/private-session-dispatch.md as a real file instead of a symlink, breaking Docs/Defaults Parity Check on main *(curated)*
- **#8875**: install over a pre-#4187 install duplicates 11 workflow labels, so sync-labels.sh --check can never converge *(curated)*
- **#8879**: bot-PR review routing is Dependabot-only: Renovate repos silently lose Judge review *(curated)*
- **#8884**: Install-time session-mode flag: empty terminals + loom.sh start refusal *(curated)*
- **#8902**: Fleet captain: alert when the declared captain stops reporting host.health (lease, not failover) *(curated)*
- **#8913**: observability: live-verify ci.job.log reconstruction in SigNoz and record it in evidence.md (#8825 AC1) *(curated)*
- **#8917**: ci-telemetry: the local journal has no rotation and is read whole on every backfill — phase-2 log capture makes it a GB/day, whole-file-read problem *(curated)*
- **#8923**: Investigate and reduce PR latency: the review queue is 4h deep, but approved-awaiting-merge is 68h and changes-requested is 90h *(curated)*
- **#8925**: Blocked PRs have no unblock path: the unblock sweep lists issues only, and a park recorded in a comment leaves no parseable dependency *(curated)*
- **#8943**: Unify the two wildcard rules in the per-role tool restriction: spawn-claude's substring test fails open where spawn-codex's exact match fails closed *(curated)*
- **#8944**: worktree node_modules symlinks the primary clone, so pnpm's purge prompt invites deleting it *(curated)*
- **#9123**: install.sh --full fails its own completeness check: .loom/credentials.md.example is recorded in install-metadata.json but never copied (--quick hides it) *(curated)*

## Proposed (Architect / Hermit)

- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#8787**: Admit Codex mutable roles using verified private-clone containment *(architect)*
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
| Operator merge-risk holds | 2 |
| Urgent | 3 |
| Ready (`loom:issue`) | 16 |
| In Progress (`loom:building`) | 4 |
| PRs awaiting review | 7 |
| Approved PRs awaiting merge | 5 |
| Curated | 57 |
| Architect / Hermit proposals | 4 |
| Active epics | 6 |
<!-- guide:plan-body:end -->
