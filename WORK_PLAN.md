# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#8692**: observability: cycle-time analytics artifacts for ClickHouse + SigNoz

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#8088**: Port loom-daemon-update.sh to a daemon subcommand (1,733 lines; the binary must replace itself)
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8058**: token pool: per-model-class exhaustion state — an Opus ceiling bad-marks the whole account and starves Sonnet work
- **#8088**: Port loom-daemon-update.sh to a daemon subcommand (1,733 lines; the binary must replace itself)
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed
- **#8136**: Reconcile PR #8097's differential-corpus gaps with #8125's BotLoginNormalisation work
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8195**: Port worktree.sh to a daemon subcommand (1,812 lines; 26 fixes in 6 months, three of them data-loss classes)
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh
- **#8287**: worktree.sh reset a stale local feature/issue-N to main although origin/feature/issue-N carried the PR's commits (Doctor on #8190)
- **#8322**: Port PR #8314's per-role tool-restriction deny-spec computation out of spawn-claude.sh/spawn-codex.sh into loom-daemon (Shell Budget Ratchet blocker)
- **#8354**: Port _worktree_resolve_stale_reset_ref (#8287) to loom-daemon per shell-language-policy
- **#8410**: merge-pr.sh --auto: a server-side armed auto-merge ignores later loom:pr revocation and non-required test suites
- **#8451**: Native guard bridge: the 20s policy timeout refuses harmless commands on a saturated host, and the model cannot tell a timeout from a denial
- **#8457**: role prompts: state that shared-cargo-target-dir integration results are not verdict-bearing evidence (#8453 item 4, urgent stopgap)
- **#8458**: cargo: per-worktree CARGO_TARGET_DIR wired to worktree lifecycle, preserving the #6013/#6014 binary-reuse fast path (#8453 item 2)
- **#8460**: guard rmScope: allow removing a private build/target dir the current session created under a scratch root (#8453 item 5)
- **#8504**: Machine-readable logs use host-local time without a zone designator — write UTC with an explicit Z everywhere (daemon.log, role logs, events, status)
- **#8514**: A pending drain-and-restart roll pauses the host's dispatch for an hour or more behind long-running native sweeps, then is refused at the deadline and re-arms
- **#8515**: A release is visible before its assets are uploaded: updaters report 'no artifact for target' and an explicit --fetch hard-fails on a transient state
- **#8528**: observability: add the self-hosted SigNoz trial using supported Foundry deployment and Loom traces
- **#8542**: Journal the Curator complexity tier and fix judge_verdicts coverage in sweep.outcome (routing-evaluation prerequisite)
- **#8552**: Champion hold-rot detector: operator-held PRs go stale against main undetected until the human merge fails (3 of 6 needed rebases, one semantic)
- **#8553**: worktree.sh does not consult .loom/locks/<issue> — two sessions can collide in one worktree
- **#8564**: Kimi usage attribution: read per-session token usage from Kimi's session store / stream-json so completions carry runtime, provider and model (extends #8507)
- **#8589**: Public-surface scrub: fleet EC2 private hostnames in WORK_LOG.md + a Rust doc comment, and operator-domain emails in test fixtures, are live at HEAD
- **#8594**: Wire Pi and Codex into the usage_source seam so their completions get token numbers, not just runtime labels
- **#8599**: Surface the chosen runtime-preference tier in the launch record and role_tick.outcome
- **#8602**: Pin the chosen preference tap's modelProfile at launch (it gates but does not pin)
- **#8623**: bug: native_readiness package-cache key test fails on macOS hosts (platform mutation is a no-op)
- **#8634**: sweep-outcomes summary --group-by tap splits one tap across the #8625 profile-stamping boundary
- **#8650**: opencode runtime leaks one ~5.5 MB native-library extract into /tmp per launch and never removes it (7.6 GB / 1,382 files in 40 h on one worker)
- **#8663**: Native-runtime launches provision a fresh ~126 MB / 7,300-file tool-binding tree per session under ~/.local/state/loom/native-tools and never remove it (31–37 GB per host, ENOSPC on loom-worker-2 twice today)
- **#8668**: observability: wire Claude Code native OTLP export for interactive sessions (#8664 item 1)
- **#8671**: Move tier-3 runtime launch shape (headless flag, prompt transport, model/effort flags) into defaults/runtimes/<name>.json so a new CLI is a manifest edit, not a new spawn-*.sh
- **#8672**: Provision pooled CODEX_HOME / KIMI_CODE_HOME profiles from the default account (symlink dirs, copy files, key-merge settings, ledger) so a rotated account is not a blank install
- **#8689**: daemon: reaper resumes a sweep once more after a cap-exhausted PR block — one guaranteed no-op dispatch per blocked PR
- **#8766**: [Epic #8764] forge_events Phase 2: early-tick consumers (work-finder tick, queue-head wake, in-flight PR watch)
- **#8796**: merge-pr.sh: warn when a partial-increment trailer is backticked (the #3667 reset silently no-ops)
- **#8816**: Credential discovery convention for task agents: reference-by-name from the owner environment + explicit provisioning flow

## In Progress

Issues currently being built (`loom:building`).

_None._

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#8426**: fix(merge-pr): --auto settles checks and re-validates in-process instead of arming a server-side merge (#8410)
- **#8440**: fix(quarantine): escalate relapsed quarantines — probation + doubling TTL (vibesql#6639)
- **#8467**: fix(native-tools): distinguish policy timeout from denial, make the budget configurable
- **#8469**: role prompts: shared-cargo-target-dir integration results are not verdict-bearing evidence (#8457)
- **#8471**: feat(worktree): port the `remove` verb to `loom-daemon worktree-remove` (#8195 slice 3)
- **#8486**: feat(cargo): opt-in per-worktree CARGO_TARGET_DIR wired to the worktree lifecycle
- **#8531**: feat(guard): admit rm of the session's own private scratch dir under rmScope
- **#8538**: fix(daemon): write UTC timestamps with a trailing Z everywhere
- **#8549**: feat(dashboard): make the Charts tab readable — titles, axes, legends, tooltips and a table view (#8546)
- **#8569**: Champion: detect held-PR base staleness before merge time (#8552)
- **#8582**: feat(telemetry): journal Curator complexity tier and add --group-by complexity (#8542)
- **#8613**: chore: scrub private host identifiers and operator identities from HEAD
- **#8640**: feat(runtime-preference): report the chosen tier in the launch record and role_tick.outcome
- **#8641**: feat(usage): read per-model token usage from Codex's rollout session store
- **#8660**: feat(auto-update): supersede an overtaken pending roll and expose live roll state (#8514)
- **#8662**: fix(release): defer the Latest pointer until every platform's assets upload
- **#8677**: examples: observability gateway collector template — multi-sink fanout + runtime session tails
- **#8678**: feat(usage-attribution): read per-session Kimi token usage behind the UsageSource seam
- **#8680**: feat(daemon-update): port loom-daemon-update.sh to a daemon subcommand
- **#8682**: docs(observability): wire Claude Code native OTLP for interactive sessions
- **#8690**: fix(sweep-outcomes): flag a tap split across the #8625 profile-stamping boundary
- **#8693**: fix(daemon): pin native-harness TMPDIR into launch state, reclaim it periodically
- **#8694**: feat(accounts): provision pooled CODEX_HOME profiles from the default account (symlink dirs, copy files, key-merge settings, ledger)
- **#8695**: feat(runtime): move the tier-3 launch shape into defaults/runtimes/<name>.json (#8671)
- **#8702**: fix(worktree): refuse a worktree when the issue's claim lock is live
- **#8703**: fix(reaper): skip resume when the linked PR is parked (loom:blocked) (#8689)
- **#8705**: Share OpenCode binding trees per workspace and reap stale native launch state
- **#8799**: feat(daemon): forge event plane Phase 1 — observe-only feed consumer (ADR-0021, #8765)
- **#8808**: chore(defaults): warn on backticked partial-increment trailers (#8796)
- **#8810**: feat(merge-pr): warn when a partial-increment trailer is backticked (#8796)
- **#8819**: docs: add task-credential reference-by-name convention

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#8692**: observability: cycle-time analytics artifacts for ClickHouse + SigNoz
- **#8696**: docs(observability): verify the SigNoz trial on Linux/amd64 and gate ingestion on org registration

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
- **#8322**: Port PR #8314's per-role tool-restriction deny-spec computation out of spawn-claude.sh/spawn-codex.sh into loom-daemon (Shell Budget Ratchet blocker) *(curated)*
- **#8354**: Port _worktree_resolve_stale_reset_ref (#8287) to loom-daemon per shell-language-policy *(curated)*
- **#8370**: Concurrent sweeps exhaust host disk via per-worktree cargo target dirs; surfaces as unrelated StorageFull test failures *(curated)*
- **#8387**: spawn-codex.sh forwards a `model@effort` suffix verbatim to the Codex CLI's `-m` *(curated)*
- **#8410**: merge-pr.sh --auto: a server-side armed auto-merge ignores later loom:pr revocation and non-required test suites *(curated)*
- **#8434**: live-verify native-ephemeral containment: canary run in-container, two-worker filesystem disjointness, post-run writable-layer credential scan *(curated)*
- **#8451**: Native guard bridge: the 20s policy timeout refuses harmless commands on a saturated host, and the model cannot tell a timeout from a denial *(curated)*
- **#8457**: role prompts: state that shared-cargo-target-dir integration results are not verdict-bearing evidence (#8453 item 4, urgent stopgap) *(curated)*
- **#8458**: cargo: per-worktree CARGO_TARGET_DIR wired to worktree lifecycle, preserving the #6013/#6014 binary-reuse fast path (#8453 item 2) *(curated)*
- **#8460**: guard rmScope: allow removing a private build/target dir the current session created under a scratch root (#8453 item 5) *(curated)*
- **#8504**: Machine-readable logs use host-local time without a zone designator — write UTC with an explicit Z everywhere (daemon.log, role logs, events, status) *(curated)*
- **#8505**: Scheduled role ticks on the metered OpenCode runtime burned ~12% of the GLM-5.3 trial on launches that could never succeed (E2BIG prompt argv + toolless), relaunched every cycle *(curated)*
- **#8514**: A pending drain-and-restart roll pauses the host's dispatch for an hour or more behind long-running native sweeps, then is refused at the deadline and re-arms *(curated)*
- **#8515**: A release is visible before its assets are uploaded: updaters report 'no artifact for target' and an explicit --fetch hard-fails on a transient state *(curated)*
- **#8525**: tracing: instrument sweep phases and role attempts, including Pi/OpenCode and repair cycles *(curated)*
- **#8527**: observability: add the self-hosted ClickStack/HyperDX trial with Loom trace and log views *(curated)*
- **#8528**: observability: add the self-hosted SigNoz trial using supported Foundry deployment and Loom traces *(curated)*
- **#8529**: observability: validate both backends with the same Loom traces and publish a comparison *(curated)*
- **#8542**: Journal the Curator complexity tier and fix judge_verdicts coverage in sweep.outcome (routing-evaluation prerequisite) *(curated)*
- **#8552**: Champion hold-rot detector: operator-held PRs go stale against main undetected until the human merge fails (3 of 6 needed rebases, one semantic) *(curated)*
- **#8553**: worktree.sh does not consult .loom/locks/<issue> — two sessions can collide in one worktree *(curated)*
- **#8564**: Kimi usage attribution: read per-session token usage from Kimi's session store / stream-json so completions carry runtime, provider and model (extends #8507) *(curated)*
- **#8570**: Guard: refuse a build/scratch dir assignment that resolves onto a tmpfs mount (upstream PR to rjwalters/repo, split from #8512) *(curated)*
- **#8576**: observability: document managed-cloud fanout and verify indexed data in both backends *(curated)*
- **#8589**: Public-surface scrub: fleet EC2 private hostnames in WORK_LOG.md + a Rust doc comment, and operator-domain emails in test fixtures, are live at HEAD *(curated)*
- **#8594**: Wire Pi and Codex into the usage_source seam so their completions get token numbers, not just runtime labels *(curated)*
- **#8599**: Surface the chosen runtime-preference tier in the launch record and role_tick.outcome *(curated)*
- **#8602**: Pin the chosen preference tap's modelProfile at launch (it gates but does not pin) *(curated)*
- **#8606**: Run a live Kimi Code CLI canary with a real Moonshot/Kimi credential and record a docs/experiments receipt *(curated)*
- **#8623**: bug: native_readiness package-cache key test fails on macOS hosts (platform mutation is a no-op) *(curated)*
- **#8628**: Kimi account pool C2: AccountProvider::Kimi, kimi login lifecycle CLI, per-account KIMI_CODE_HOME, availability probe *(curated)*
- **#8634**: sweep-outcomes summary --group-by tap splits one tap across the #8625 profile-stamping boundary *(curated)*
- **#8650**: opencode runtime leaks one ~5.5 MB native-library extract into /tmp per launch and never removes it (7.6 GB / 1,382 files in 40 h on one worker) *(curated)*
- **#8663**: Native-runtime launches provision a fresh ~126 MB / 7,300-file tool-binding tree per session under ~/.local/state/loom/native-tools and never remove it (31–37 GB per host, ENOSPC on loom-worker-2 twice today) *(curated)*
- **#8665**: observability: cycle-time analytics — standing answer to "what took long to ship and where did it go slow" in SigNoz + ClickHouse *(curated)*
- **#8667**: Fleet feed: ModelLabels.tsx needs a Kimi/Moonshot label+icon mapping (marketing-site repo, follow-up to #8564/#8507) *(curated)*
- **#8668**: observability: wire Claude Code native OTLP export for interactive sessions (#8664 item 1) *(curated)*
- **#8671**: Move tier-3 runtime launch shape (headless flag, prompt transport, model/effort flags) into defaults/runtimes/<name>.json so a new CLI is a manifest edit, not a new spawn-*.sh *(curated)*
- **#8672**: Provision pooled CODEX_HOME / KIMI_CODE_HOME profiles from the default account (symlink dirs, copy files, key-merge settings, ledger) so a rotated account is not a blank install *(curated)*
- **#8689**: daemon: reaper resumes a sweep once more after a cap-exhausted PR block — one guaranteed no-op dispatch per blocked PR *(curated)*
- **#8698**: Live end-to-end verification of the credential egress proxy (AC5 of #8674) *(curated)*
- **#8699**: Per-launch usage attribution and 429 bad-marking at the credential egress proxy (follow-up to #8674) *(curated)*
- **#8711**: Ship 'quick tap' model profiles (Cerebras, Gemini Flash) as bundled presets so metered backstops are zero-config *(curated)*
- **#8726**: resync-ignore pins record no fork point: 'can this pin be lifted yet?' is archaeology, not a diff *(curated)*
- **#8730**: config: this repo pins runtimes.roles.judge = "codex" on a host with no Codex account — 157 Judge ticks skipped before spawn *(curated)*
- **#8742**: loom:blocked is silently stripped from operator-ruled permanent blocks (unblock probe treats 'unparseable blocker' as 'no blocker') *(curated)*
- **#8761**: Concierge: durable room inbox so messages sent between ticks are not lost (Phase 4 of #4196) *(curated)*
- **#8766**: [Epic #8764] forge_events Phase 2: early-tick consumers (work-finder tick, queue-head wake, in-flight PR watch) *(curated)*
- **#8786**: Route daemon Codex jobs into private clones with recoverable logs and checkpoints *(curated)*
- **#8790**: Reaper resume-dispatch tests fail under ambient LOOM_RUNTIME override (runtime admission demands mcp) *(curated)*
- **#8796**: merge-pr.sh: warn when a partial-increment trailer is backticked (the #3667 reset silently no-ops) *(curated)*
- **#8801**: Credential discovery convention: agents should not need operators to re-state where keys live every session *(curated)*
- **#8812**: fleet add-worker: verify step proves the worker booted, not that it narrates (no end-to-end assertion) *(curated)*
- **#8813**: Sweep teardown does not kill its own process group -- orphaned sleep infinity holders outlive dead sweeps (cf. #7825) *(curated)*
- **#8816**: Credential discovery convention for task agents: reference-by-name from the owner environment + explicit provisioning flow *(curated)*
- **#8818**: Account rotation for proxied Claude containers: bad-mark and swap at the egress proxy (follow-up to #8697) *(curated)*

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
| Operator merge-risk holds | 1 |
| Urgent | 3 |
| Ready (`loom:issue`) | 39 |
| In Progress (`loom:building`) | 0 |
| PRs awaiting review | 31 |
| Approved PRs awaiting merge | 2 |
| Curated | 70 |
| Architect / Hermit proposals | 4 |
| Active epics | 6 |
<!-- guide:plan-body:end -->
