# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#9733**: docs(adr): combined-PR consolidation contract (#9687)
- **#9775**: observability(signoz): execute cycle-time-extract.sql against the pinned ClickHouse

## Operator Priority

Issues the operator starred (`loom:operator-priority`); land these first.

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh
- **#8528**: observability: add the self-hosted SigNoz trial using supported Foundry deployment and Loom traces
- **#9548**: Authenticate Loom's control signals: markers and phrases count only from trusted authors (work safely on any public repo)
- **#9687**: Merge consolidation: specify eligibility, failure recovery and component ownership (9063 next-stage design)

## In Progress

Issues currently being built (`loom:building`).

- **#9764**: observability: stream issue-scoped agent output to OTLP during active runs
- **#9772**: loom-daemon: one comment chokepoint (forge comment) that always appends the dashboard link

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#9636**: fix(security): Loom writes only to repos it manages (#9548)

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#9733**: docs(adr): combined-PR consolidation contract (#9687)
- **#9775**: observability(signoz): execute cycle-time-extract.sql against the pinned ClickHouse
- **#9780**: feat(config): hot-apply the lease TTL, add hyperparams --validate, link the docs

## Proposed

Issues carrying `loom:curated`.

- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#7855**: robb-pro daemon stopped heartbeating and answering IPC for 25 min while still logging; the watchdog CONFIRMED the wedge twice and never restarted it *(curated)*
- **#8052**: telemetry: per-role × model token consumption report + weekly-limit calibration (activity.db token tables are empty, `loom-daemon stats` crashes) *(curated)*
- **#8055**: experiment: fleet-wide repo-stratified model A/B — assign arms, write/remove overlays, cover role-runner ticks, stamp the arm explicitly in outcome records *(curated)*
- **#8058**: token pool: per-model-class exhaustion state — an Opus ceiling bad-marks the whole account and starves Sonnet work *(curated)*
- **#8103**: Repo settings: main has no required status checks, so CI cannot block a merge (and stale branches never re-run) *(curated)*
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
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
- **#9062**: Rejection telemetry counts daemon base-conflict flags (#8922) as Judge rejections *(curated)*
- **#9065**: CI: cut PR wall time from ~8.5 min to ≤5 min (build once, dedupe nextest, shard serial suites) and stop false-stale merges *(curated)*
- **#9079**: provenance: D33 record in lease/verdict comments, prompt hash, and in-session trailers (follow-up to #9027) *(curated)*
- **#9126**: Document the quoted-heredoc rule in comment-body-literal-path.md: an unquoted delimiter silently executes and deletes backticked prose *(curated)*
- **#9136**: main ruleset: bypass_actors 'RepositoryRole:always' makes pull_request and required_status_checks advisory *(curated)*
- **#9152**: worktree-link: an already-created worktree keeps its pnpm node_modules alias (#8944 leaves existing worktrees unfixed) *(curated)*
- **#9304**: Guard telemetry: rm-scope-unresolved-var denies resolvable scratch-cleanup shapes (57 events) *(curated)*
- **#9323**: Guard: extract_rm_targets() misses a loop/conditional-body rm written as a one-liner (`; do rm -rf …`) *(curated)*
- **#9356**: ci: 'Native Port Suites' steps lack !cancelled() guards; #9118 comment overstates coverage *(curated)*
- **#9357**: Champion critical-file hold: bare .sql pattern false-positives on reference queries (defaults/observability/) *(curated)*
- **#9360**: buildGate fails on native dispatch workers: 4 env-sensitive unit tests (TMPDIR length, capability set, model env) *(curated)*
- **#9362**: docs: state what Loom optimizes for — and that it is the wrong tool under time pressure (hackathon fit note) *(curated)*
- **#9373**: Guard: for-loop rm-scope resolver proves header appears in text, not that it executed/still binds — 10 reproduced bypasses *(curated)*
- **#9548**: Authenticate Loom's control signals: markers and phrases count only from trusted authors (work safely on any public repo) *(curated)*
- **#9601**: Guard: rm-scope denies the composed `for w in <literals>` → `d="<literal>/$w/…"` → `rm -rf "$d"` cleanup shape (5/205 real denies, all 5 would allow) *(curated)*
- **#9687**: Merge consolidation: specify eligibility, failure recovery and component ownership (9063 next-stage design) *(curated)*
- **#9764**: observability: stream issue-scoped agent output to OTLP during active runs *(curated)*
- **#9772**: loom-daemon: one comment chokepoint (forge comment) that always appends the dashboard link *(curated)*

## Proposed (Architect / Hermit)

- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#8788**: Evaluate Codex private-workspace efficiency after the first production canary *(architect)*
- **#9777**: Forge qualification: version the operation inventory and enforce coverage accounting *(architect)*
- **#9778**: Gitea hosted evidence: measure sweep load, integration effort and post-GO AWS capacity *(architect)*
- **#9779**: Gitea qualification: define identity, capability errors and the supported hosted profile *(architect)*
- **#9783**: Augment context: persist retrieval results by issue content and source revision *(architect)*
- **#9784**: Curator footprints: classify Augment evidence and refresh on issue updates *(architect)*
- **#9785**: Historical Augment replay: predict from old issues and code, then measure actual PR overlap *(architect)*
- **#9786**: Collision evidence: publish versioned predictions and attributed outcomes in SigNoz *(architect)*
- **#9787**: Collision shadow study: calibrate risk against Curator baseline before dispatch enforcement *(architect)*
- **#9788**: Gitea qualification: provision the hosted sandbox and external credential references *(architect)*
- **#9789**: Gitea hosted trial: run live capability probes and surface fatal workflow gaps first *(architect)*
- **#9790**: Gitea hosted trial: qualify real Actions workflows, diagnostics and delivery dependencies *(architect)*
- **#9791**: Gitea hosted trial: complete a supervised Loom lifecycle with minimal reusable integration *(architect)*
- **#9792**: Operator decision: GO or NO-GO on Gitea from hosted qualification evidence *(architect)*
- **#9793**: Forge adapters: normalize provider context, errors and routing after Gitea GO *(architect)*
- **#9794**: Gitea adapter: complete issue, label, conversation and PR metadata operations *(architect)*
- **#9795**: Gitea landing: preserve review-thread, branch-protection and guarded-merge invariants *(architect)*
- **#9796**: Gitea CI adapter: complete run/check pagination, diagnostics and safe remediation *(architect)*
- **#9797**: Mixed-forge identity: qualify permission, trusted records and competing claims *(architect)*
- **#9798**: Mixed-forge fleet: isolate dispatch, state, caches, quota breakers and canonical links *(architect)*
- **#9799**: Loom forge integration: migrate active callers, installation and delivery to qualified profiles *(architect)*
- **#9800**: Post-GO only: provision a hardened Gitea AWS host in 2am using the established machine baseline *(architect)*
- **#9801**: Post-GO Gitea service: install the qualified build and prove backup, restore and operations *(architect)*
- **#9802**: Gitea production acceptance: requalify self-managed mixed fleets and cut over one opt-in repo *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam
- **#7810**: [epic] Retire shell: 10 sequential PRs, two foundational then update + agent execution
- **#8522**: Epic: send Loom traces, logs, and metrics to ClickStack/HyperDX and SigNoz for a side-by-side trial
- **#8764**: Forge event plane: push GitHub events to daemons via operator Webhook Worker feed (ADR-0021, lifts ADR-0014 Lever C)
- **#9063**: Epic: order overlapping PRs first, then add automatic merge consolidation
- **#9429**: Epic: Fibonacci story points — a size-weighted throughput measure Loom can optimize
- **#9769**: Epic: Hosted-first Gitea qualification, GO/NO-GO, and gated 2am AWS rollout

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 2 |
| Operator priority | 3 |
| Ready (`loom:issue`) | 6 |
| In Progress (`loom:building`) | 2 |
| PRs awaiting review | 1 |
| Approved PRs awaiting merge | 3 |
| Curated | 55 |
| Architect / Hermit proposals | 26 |
| Active epics | 9 |
<!-- guide:plan-body:end -->
