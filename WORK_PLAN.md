# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#10059**: forge egress: wire gh resolver policy-launcher rung (#9995)

## Operator Priority

Issues the operator starred (`loom:operator-priority`); land these first.

- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate
- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh
- **#9132**: auto_update: roll on a scheduled window instead of arming a drain on every new build
- **#9758**: ETA fleet history: add the SigNoz-sourced in-sweep half (blocked on harness-ops#249)
- **#9769**: Epic: Self-hosted Gitea qualification (gitea-1), GO/NO-GO, and gated production rollout
- **#9778**: Gitea qualification evidence (gitea-1): measure sweep load, integration effort and post-GO capacity
- **#9779**: Gitea qualification: define identity, capability errors and the supported self-hosted profile
- **#9789**: Gitea qualification (gitea-1): run live capability probes and surface fatal workflow gaps first
- **#9790**: Gitea qualification (gitea-1): qualify real Actions workflows, diagnostics and delivery dependencies
- **#9791**: Gitea qualification (gitea-1): complete a supervised Loom lifecycle with minimal reusable integration
- **#9792**: Operator decision: GO or NO-GO on Gitea from gitea-1 qualification evidence
- **#9793**: Forge adapters: normalize provider context, errors and routing after Gitea GO
- **#9794**: Gitea adapter: complete issue, label, conversation and PR metadata operations
- **#9795**: Gitea landing: preserve review-thread, branch-protection and guarded-merge invariants
- **#9796**: Gitea CI adapter: complete run/check pagination, diagnostics and safe remediation
- **#9797**: Mixed-forge identity: qualify permission, trusted records and competing claims
- **#9798**: Mixed-forge fleet: isolate dispatch, state, caches, quota breakers and canonical links
- **#9799**: Loom forge integration: migrate active callers, installation and delivery to qualified profiles
- **#9800**: Post-GO only: provision a hardened Gitea AWS host in 2am using the established machine baseline
- **#9801**: Post-GO Gitea service: install the qualified build and prove backup, restore and operations
- **#9802**: Gitea production acceptance: requalify self-managed mixed fleets and cut over one opt-in repo
- **#9924**: forge_contract::InstanceOrigin::parse accepts a scheme with no host (e.g. "https://")
- **#9945**: forge-probe disposable issue titles don't follow the ${GITEA_QUAL_RUN_NS} prefix convention
- **#9970**: feat(eta): backtested land-v3 heuristic using SigNoz historical analysis to eliminate optimistic delay bias
- **#9974**: loom:operator-priority: rank stars above all work, order multiple stars by earliest star time, never evict in-flight work
- **#9978**: feat(champion): optional per-repo merge-queue mode — enqueue Judge-approved PRs to GitHub's merge queue instead of merge-pr.sh's freshness/re-date loop
- **#9979**: Codex session containers can't start Codex's bwrap sandbox: every Codex role tick does nothing yet logs SUCCESS (curator/judge/champion stalled fleet-wide)
- **#9983**: Forge egress policy: route every GitHub API call Loom makes through a mandated gateway, validated at every entry point (upstream half of 2am#1911)
- **#9985**: loom-daemon: one `gh` spawn choke point with CI scan test and client-side `invoke github` telemetry (#9983 C2)
- **#9987**: Run 2am's managed gh launcher everywhere Loom spawns gh: worker PATH, containers, outcome codes (#9983 C4)
- **#9989**: Forge egress enforcement Loom owns: guard-hook denies, role tool policy, container egress boundary + negative canary (#9983 C6)
- **#9995**: forge egress: wire gh_invocation resolver's policy-launcher rung to the C1 policy reader (follow-up to #9984)
- **#9996**: forge egress: run `forge egress doctor` after resync-installed.sh (follow-up to #9984)
- **#10000**: An agent asks a human only by a decision or a mail; operator labels stay engine-internal
- **#10001**: "Only a human can do this" is for PO-level decisions only; Curator reviews every parked item and returns what agents can do
- **#10012**: Propagate loom:operator-priority from a starred parent to its child issues and PRs, daemon-enforced and removed with the parent's star
- **#10013**: Label registry: one JSON file holding every label's properties, replacing the scattered daemon and shell label tables
- **#10019**: daemon: read loom label state from the dashboard's webhook-fed fleet state instead of polling GitHub
- **#10022**: daemon: export rate_limit_breaker trips, own/external attribution and quota gauges to SigNoz
- **#10023**: daemon: one stable host.id per machine, plus start/stop/heartbeat events in SigNoz
- **#10025**: Curator scripts on REST exhaustion: check-duplicate says 'Not authenticated', premise-check fails closed, post-comment has no GraphQL fallback
- **#10039**: Curator's per-issue gh issue view/edit fan-out exhausts the shared GraphQL pool
- **#10050**: C3 follow-up: git credential separation in provisioning + pinned upstream gh (rest of #9986)
- **#10089**: loom-daemon spends ~1,200-1,500 REST calls/hour of the shared pool, and its breaker books ~98% of it as external
- **#10116**: Attended-session agents emit no issue-tagged output, so the loom-ui issue log panel is empty for them
- **#10118**: main-red-fix issues sit in loom:triage while main stays red: the red-main lane only reorders candidates, so an unpromoted fix never enters it
- **#10120**: Attended live output: wire roles that never claim an issue (Judge, Curator, Champion, ...) into live-output-attend
- **#10125**: Attended live-output tailer: surface start diagnostics, close finished subagent runs promptly, tidy state files
- **#10137**: mail-send: resolve the ingest key and inbox URL like the daemon does, and keep flagging when they're missing
- **#10146**: mail-send: preflight that diagnoses an un-onboarded machine before sending (URL hint, key file, telemetry-key 401, Matrix leg)
- **#10150**: notify-cleared-blockers runs only from merge-pr.sh: merges made any other way never clear a block
- **#10151**: star_liveness escalates stale blocks to the operator instead of unblocking or routing to Curator
- **#10156**: Daemon re-dispatches human-gated/blocked issues forever after no-op sweeps (275 after-curator loops, ~600M tokens/24h)
- **#10163**: Merge-chain head livelocks: version-bump commits and unrelated merges re-stale its checks faster than a re-date can finish
- **#10168**: forge egress doctor reports a host with no egress policy as clean (origin: unconfigured, 0 findings, exit 0) while 2am's doctor exits 2 on the same host
- **#10169**: Star-liveness 'Operator needed' escalations should send mail, not only a GitHub comment
- **#10177**: Operator work is an issue; the daemon mails from its labels (operator-mechanical), and steps after a merge get their own issue
- **#10179**: Durable host opt-out: a stopped daemon keeps coming back (start/installer/update/agents re-provision it)
- **#10193**: ETA: test a fitted quantile model (land-m1) against heuristics — log friction features, 14d-train/48h-eval censored backtest
- **#10195**: observability: lock in long-term retention for all Loom telemetry — live SigNoz already keeps 10y; reconcile 7d/30d trial docs, aux tables, loom-ui store
- **#10196**: telemetry: make SigNoz point-in-time reconstructable — knowable-at timestamps, full fleet-state snapshots over OTLP, per-host export-coverage records, replay contract
- **#10197**: eta: point-in-time fleet-state reconstruction from forge history via a resumable throttle-safe raw event cache, with agreement check vs logged estimate features
- **#10198**: eta: queue-aware joint simulation candidate (land-q1) — simulate all in-flight items together under shared slot / Judge / merge-serialization capacity
- **#10203**: worktree.sh | tail hangs for hours: the lease renewer inherits the caller's fd 3 (pipe)
- **#10208**: eta: little-v0 — zero-parameter queue baseline (items ahead ÷ recent drain rate, plus service time) reported beside every candidate
- **#10209**: eta: recency-weight stage samples (half-life with effective-sample-size fallback) instead of a flat 60-day window
- **#10210**: eta: model stalls explicitly — quota reset, pool cooldown, breaker, holds as a 'not before T' term; stop refusing beyond_history
- **#10212**: telemetry: log each role's pick decision per tick — ranked candidates, chosen items, skip reasons — so queue position and service discipline are measurable
- **#10223**: eta: land land-2026-10-04-twin-otter in shadow — the experiment-v2 blend of stage-by-stage and direct models
- **#10229**: sweep-lease-renew.sh spends the operator's personal REST pool at ~600 calls/h and climbing; one sweep id runs ~16 renew loops
- **#10232**: eta: features needing new reads or subsystems — PR size, issue-body markers, rate-limit/pool/breaker stall signals, required checks, live coverage check — #10201 Slice C
- **#10233**: eta: promotion gate and accuracy views — late-surprise + answer-rate gate, common decidable subset, per-day win-rate CI, stability/convergence, multi-fold backtest — #10211 Slice B
- **#10235**: Claim reconciler strips loom:reviewing after 30 min by label age alone, ignoring the Judge's activity (mid-review claim loss)
- **#10238**: loom update never provisions or refreshes user-scope skills on an existing (daemonless) machine — /loom:star unfindable
- **#10298**: eta fleet refresh: daemon syncs only the issue-events listing, not main's pulls endpoint (closing refs go stale)
- **#10305**: eta backtest: derive PR-history land-case stages via stage_from_pr_labels so operator-held PRs replay as merge_hold
- **#10307**: Operator priority levels: ⭐⭐ loom:operator-high-priority (capped) above the star, inherited by every open blocker across repos, built to take ⭐⭐⭐ later

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh
- **#8742**: loom:blocked is silently stripped from operator-ruled permanent blocks (unblock probe treats 'unparseable blocker' as 'no blocker')
- **#9132**: auto_update: roll on a scheduled window instead of arming a drain on every new build
- **#9405**: guard: parse_force_ops() treats `|`, `&`, `||` inside $(cd <path> ... pwd) like `&&` — resolves cwd the shell never sets
- **#9758**: ETA fleet history: add the SigNoz-sourced in-sweep half (blocked on harness-ops#249)
- **#9779**: Gitea qualification: define identity, capability errors and the supported self-hosted profile
- **#9979**: Codex session containers can't start Codex's bwrap sandbox: every Codex role tick does nothing yet logs SUCCESS (curator/judge/champion stalled fleet-wide)
- **#10000**: An agent asks a human only by a decision or a mail; operator labels stay engine-internal
- **#10012**: Propagate loom:operator-priority from a starred parent to its child issues and PRs, daemon-enforced and removed with the parent's star
- **#10013**: Label registry: one JSON file holding every label's properties, replacing the scattered daemon and shell label tables
- **#10023**: daemon: one stable host.id per machine, plus start/stop/heartbeat events in SigNoz
- **#10025**: Curator scripts on REST exhaustion: check-duplicate says 'Not authenticated', premise-check fails closed, post-comment has no GraphQL fallback
- **#10089**: loom-daemon spends ~1,200-1,500 REST calls/hour of the shared pool, and its breaker books ~98% of it as external
- **#10118**: main-red-fix issues sit in loom:triage while main stays red: the red-main lane only reorders candidates, so an unpromoted fix never enters it
- **#10137**: mail-send: resolve the ingest key and inbox URL like the daemon does, and keep flagging when they're missing
- **#10146**: mail-send: preflight that diagnoses an un-onboarded machine before sending (URL hint, key file, telemetry-key 401, Matrix leg)
- **#10150**: notify-cleared-blockers runs only from merge-pr.sh: merges made any other way never clear a block
- **#10151**: star_liveness escalates stale blocks to the operator instead of unblocking or routing to Curator
- **#10156**: Daemon re-dispatches human-gated/blocked issues forever after no-op sweeps (275 after-curator loops, ~600M tokens/24h)
- **#10163**: Merge-chain head livelocks: version-bump commits and unrelated merges re-stale its checks faster than a re-date can finish
- **#10169**: Star-liveness 'Operator needed' escalations should send mail, not only a GitHub comment
- **#10195**: observability: lock in long-term retention for all Loom telemetry — live SigNoz already keeps 10y; reconcile 7d/30d trial docs, aux tables, loom-ui store
- **#10196**: telemetry: make SigNoz point-in-time reconstructable — knowable-at timestamps, full fleet-state snapshots over OTLP, per-host export-coverage records, replay contract
- **#10229**: sweep-lease-renew.sh spends the operator's personal REST pool at ~600 calls/h and climbing; one sweep id runs ~16 renew loops
- **#10232**: eta: features needing new reads or subsystems — PR size, issue-body markers, rate-limit/pool/breaker stall signals, required checks, live coverage check — #10201 Slice C
- **#10235**: Claim reconciler strips loom:reviewing after 30 min by label age alone, ignoring the Judge's activity (mid-review claim loss)

## In Progress

Issues currently being built (`loom:building`).

- **#9985**: loom-daemon: one `gh` spawn choke point with CI scan test and client-side `invoke github` telemetry (#9983 C2)
- **#10197**: eta: point-in-time fleet-state reconstruction from forge history via a resumable throttle-safe raw event cache, with agreement check vs logged estimate features
- **#10307**: Operator priority levels: ⭐⭐ loom:operator-high-priority (capped) above the star, inherited by every open blocker across repos, built to take ⭐⭐⭐ later

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#8314**: feat(guard): per-role tool-restriction allowlist enforced at the harness (#8256)
- **#9819**: feat(daemon): warn when a scoped-socket daemon starts alongside a live machine-level daemon (#9815)
- **#9893**: fix(guards): require && or ; for the cross-segment $(cd <path> ... pwd) close (#9405)

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#9843**: Centralize operational tunables: hyperparameters tranche 2 (env-only knobs → config block)
- **#9848**: feat(context): content-addressed retrieval cache with bounded provider adapter (#9783)
- **#9852**: test(daemon): clear ambient runtime pins so buildGate is not a function of the shell (#9360)
- **#9853**: feat(footprint): classified, revision-pinned issue footprints over the context cache (#9784)
- **#9886**: docs(forge): the forge contract — identity, outcomes, evidence (#9779)
- **#9902**: feat(collision-evidence): versioned prediction/outcome records with idempotent publication (#9786)
- **#9903**: feat(collision-shadow): prospective shadow-study capture and frozen-policy evaluation (#9787)
- **#9919**: collision-evidence: OTLP push to the configured collector (#9910)
- **#9931**: context-cache: AUGMENT_SESSION_FILE credential seam (#9930)
- **#9938**: fix(daemon): never treat a pull request as an issue in candidate selection or claim
- **#9958**: feat(fleet-config): refuse a lossy machine-tier render by default (2am#1653's ask, guard side)
- **#9966**: fix(push): pin --force-with-lease to the head the work is based on (#9487)
- **#9975**: feat(daemon): star-time ordering, forge starred/star verbs, no-eviction + star-authority docs (#9974)
- **#10032**: feat(roles): mechanical chore mail + objective as decision (#10000 slice 2)
- **#10046**: docs(curator): cut GraphQL fan-out (REST, two-pool check, cap 3)
- **#10048**: forge egress: run doctor after resync-installed.sh (#9996)
- **#10053**: ci(labels): extend drift check to the registry (#10013 slice 1 follow-up)
- **#10059**: forge egress: wire gh resolver policy-launcher rung (#9995)
- **#10061**: feat(daemon): export rate-limit breaker trips, attribution and quota gauges (#10022)
- **#10064**: docs: drop the Gitea Cloud runbook; point GITEA_QUAL_* at self-hosted gitea-1
- **#10068**: docs(credentials): reference the D1 token for loom-fleet-telemetry (#10067)
- **#10158**: feat(live-output): attend Curator and Judge runs from code they already run
- **#10175**: feat(forge-egress): doctor reports unconfigured hosts; managed hosts exit 2 (#10168)
- **#10182**: feat(merge-pr): prove restamp-only base moves never stale; export re-date pressure (#10163 PR 1)
- **#10183**: docs(auto_update): document the scheduled roll window (follow-up to #10141)
- **#10184**: fix(session-output): keep the attended start outcome, end a returned subagent's run, sweep stale tailer files (#10125)
- **#10185**: feat(merge-pr): port the post-merge worktree removal itself to Rust (#8191 slice)
- **#10205**: feat(host): durable host opt-out marker (autonomy-disabled)
- **#10228**: feat(star): create-issue.sh --parent links and stars a child (#10012 slice 3)
- **#10248**: fix(lease): keep the lease renewer from holding the caller's pipe open
- **#10259**: feat(eta): model stalls explicitly; stop refusing beyond_history (land-v4) (#10210)
- **#10261**: feat(gh): route more daemon gh spawns through GhInvocation (#10089, partial)
- **#10268**: telemetry: log each role's pick decision per tick (pick.decision)
- **#10274**: loom update: provision user-scope skills and ff-sync machine checkout (#10238)
- **#10283**: feat(telemetry): fleet.state OTLP kind with hourly anchor (slice 2 of #10196)
- **#10288**: eta: little-v0 zero-parameter queue floor baseline (#10208)
- **#10299**: eta: multi-fold backtest - union-with-refusals comparison, walk-forward daily folds, landing-instant stability (#10233 PR 2)
- **#10306**: merge queue Phase A: dormant typed controls, champion.mergeMode, capability preflight (#10255)
- **#10320**: fix(eta): stage PR-history land cases via stage_from_pr_labels (merge_hold replay)
- **#10321**: fix(eta): daemon fleet refresh syncs the pulls listing too, reader-only (#10298)
- **#10324**: feat(eta): SigNoz-sourced in-sweep half of fleet history (#9758)

## Proposed

Issues carrying `loom:curated`.

- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#8055**: experiment: fleet-wide repo-stratified model A/B — assign arms, write/remove overlays, cover role-runner ticks, stamp the arm explicitly in outcome records *(curated)*
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh *(curated)*
- **#8434**: live-verify native-ephemeral containment: canary run in-container, two-worker filesystem disjointness, post-run writable-layer credential scan *(curated)*
- **#8525**: tracing: instrument sweep phases and role attempts, including Pi/OpenCode and repair cycles *(curated)*
- **#8527**: observability: add the self-hosted ClickStack/HyperDX trial with Loom trace and log views *(curated)*
- **#8528**: observability: add the self-hosted SigNoz trial using supported Foundry deployment and Loom traces *(curated)*
- **#8529**: observability: validate both backends with the same Loom traces and publish a comparison *(curated)*
- **#8570**: Guard: refuse a build/scratch dir assignment that resolves onto a tmpfs mount (upstream PR to rjwalters/repo, split from #8512) *(curated)*
- **#8576**: observability: document managed-cloud fanout and verify indexed data in both backends *(curated)*
- **#8667**: Fleet feed: ModelLabels.tsx needs a Kimi/Moonshot label+icon mapping (marketing-site repo, follow-up to #8564/#8507) *(curated)*
- **#8698**: Live end-to-end verification of the credential egress proxy (AC5 of #8674) *(curated)*
- **#8742**: loom:blocked is silently stripped from operator-ruled permanent blocks (unblock probe treats 'unparseable blocker' as 'no blocker') *(curated)*
- **#8790**: Reaper resume-dispatch tests fail under ambient LOOM_RUNTIME override (runtime admission demands mcp) *(curated)*
- **#8801**: Credential discovery convention: agents should not need operators to re-state where keys live every session *(curated)*
- **#8812**: fleet add-worker: verify step proves the worker booted, not that it narrates (no end-to-end assertion) *(curated)*
- **#8813**: Sweep teardown does not kill its own process group -- orphaned sleep infinity holders outlive dead sweeps (cf. #7825) *(curated)*
- **#8913**: observability: live-verify ci.job.log reconstruction in SigNoz and record it in evidence.md (#8825 AC1) *(curated)*
- **#8950**: Live-verify Pi tokens_by_model against a real LOOM_RUNTIME=pi launch *(curated)*
- **#9006**: observability: live-verify the SigNoz queue-starvation alert rule fires and resolves (#8856 / PR #8935 follow-up) *(curated)*
- **#9062**: Rejection telemetry counts daemon base-conflict flags (#8922) as Judge rejections *(curated)*
- **#9065**: CI: cut PR wall time from ~8.5 min to ≤5 min (build once, dedupe nextest, shard serial suites) and stop false-stale merges *(curated)*
- **#9126**: Document the quoted-heredoc rule in comment-body-literal-path.md: an unquoted delimiter silently executes and deletes backticked prose *(curated)*
- **#9132**: auto_update: roll on a scheduled window instead of arming a drain on every new build *(curated)*
- **#9136**: main ruleset: bypass_actors 'RepositoryRole:always' makes pull_request and required_status_checks advisory *(curated)*
- **#9152**: worktree-link: an already-created worktree keeps its pnpm node_modules alias (#8944 leaves existing worktrees unfixed) *(curated)*
- **#9192**: merge-pr.sh: "Could not fetch PR #N" hides the real cause — forge_get_pr_nocache discards stderr *(curated)*
- **#9356**: ci: 'Native Port Suites' steps lack !cancelled() guards; #9118 comment overstates coverage *(curated)*
- **#9360**: buildGate fails on native dispatch workers: 4 env-sensitive unit tests (TMPDIR length, capability set, model env) *(curated)*
- **#9405**: guard: parse_force_ops() treats `|`, `&`, `||` inside $(cd <path> ... pwd) like `&&` — resolves cwd the shell never sets *(curated)*
- **#9409**: test: POOL_EXHAUSTED_SKIP_COUNT delta assertions race under parallel cargo test (flaky role_runner / runtime_preflight tests) *(curated)*
- **#9418**: Renovate: Dependency Dashboard is enabled by config:recommended (unlabelled bot issue), and renovate.json5's loom:review-requested label has no structural guard *(curated)*
- **#9487**: Doctor's --force-with-lease can succeed against a stale local tracking ref in a shared clone, silently overwriting a sibling Doctor's push *(curated)*
- **#9492**: Decide which other required CI jobs the local build gate should mirror (follow-up to #9140) *(curated)*
- **#9507**: telemetry: split per-issue effort into clean/substantive/environmental SECONDS, not event counts (#9444 criterion 4) *(curated)*
- **#9512**: loom-daemon: reaper_sweep_exited_event_carries_no_progress_classification can hang forever, wedging the whole lib suite behind its #[serial] lock *(curated)*
- **#9518**: prless-retry fleet tally can double-count its own releases on a host whose identity is UNKNOWN_HOST *(curated)*
- **#9548**: Authenticate Loom's control signals: markers and phrases count only from trusted authors (work safely on any public repo) *(curated)*
- **#9758**: ETA fleet history: add the SigNoz-sourced in-sweep half (blocked on harness-ops#249) *(curated)*
- **#9769**: Epic: Self-hosted Gitea qualification (gitea-1), GO/NO-GO, and gated production rollout *(curated)*
- **#9779**: Gitea qualification: define identity, capability errors and the supported self-hosted profile *(curated)*
- **#9783**: Augment context: persist retrieval results by issue content and source revision *(curated)*
- **#9784**: Curator footprints: classify Augment evidence and refresh on issue updates *(curated)*
- **#9842**: Centralize operational tunables: hyperparameters tranche 2 — env-only knobs onto the config block *(curated)*
- **#9927**: claim-staleness.sh stand-down message omits the bounded-fallback age floor, reading as 'overdue' when it is not *(curated)*
- **#9929**: Curator applies issue-lifecycle labels (loom:curating/loom:curated) to pull requests *(curated)*
- **#9970**: feat(eta): backtested land-v3 heuristic using SigNoz historical analysis to eliminate optimistic delay bias *(curated)*
- **#9973**: Flaky: worktree_cli::reset rescue test unexpectedly hits the live-process veto under nextest *(curated)*
- **#9974**: loom:operator-priority: rank stars above all work, order multiple stars by earliest star time, never evict in-flight work *(curated)*
- **#9978**: feat(champion): optional per-repo merge-queue mode — enqueue Judge-approved PRs to GitHub's merge queue instead of merge-pr.sh's freshness/re-date loop *(curated)*
- **#9979**: Codex session containers can't start Codex's bwrap sandbox: every Codex role tick does nothing yet logs SUCCESS (curator/judge/champion stalled fleet-wide) *(curated)*
- **#9985**: loom-daemon: one `gh` spawn choke point with CI scan test and client-side `invoke github` telemetry (#9983 C2) *(curated)*
- **#9995**: forge egress: wire gh_invocation resolver's policy-launcher rung to the C1 policy reader (follow-up to #9984) *(curated)*
- **#9996**: forge egress: run `forge egress doctor` after resync-installed.sh (follow-up to #9984) *(curated)*
- **#10000**: An agent asks a human only by a decision or a mail; operator labels stay engine-internal *(curated)*
- **#10001**: "Only a human can do this" is for PO-level decisions only; Curator reviews every parked item and returns what agents can do *(curated)*
- **#10009**: Operator-label review (Lane B of #10001): give every parked item in the wired repos a verdict and return what agents can do *(curated)*
- **#10012**: Propagate loom:operator-priority from a starred parent to its child issues and PRs, daemon-enforced and removed with the parent's star *(curated)*
- **#10013**: Label registry: one JSON file holding every label's properties, replacing the scattered daemon and shell label tables *(curated)*
- **#10019**: daemon: read loom label state from the dashboard's webhook-fed fleet state instead of polling GitHub *(curated)*
- **#10022**: daemon: export rate_limit_breaker trips, own/external attribution and quota gauges to SigNoz *(curated)*
- **#10023**: daemon: one stable host.id per machine, plus start/stop/heartbeat events in SigNoz *(curated)*
- **#10025**: Curator scripts on REST exhaustion: check-duplicate says 'Not authenticated', premise-check fails closed, post-comment has no GraphQL fallback *(curated)*
- **#10039**: Curator's per-issue gh issue view/edit fan-out exhausts the shared GraphQL pool *(curated)*
- **#10050**: C3 follow-up: git credential separation in provisioning + pinned upstream gh (rest of #9986) *(curated)*
- **#10089**: loom-daemon spends ~1,200-1,500 REST calls/hour of the shared pool, and its breaker books ~98% of it as external *(curated)*
- **#10118**: main-red-fix issues sit in loom:triage while main stays red: the red-main lane only reorders candidates, so an unpromoted fix never enters it *(curated)*
- **#10120**: Attended live output: wire roles that never claim an issue (Judge, Curator, Champion, ...) into live-output-attend *(curated)*
- **#10125**: Attended live-output tailer: surface start diagnostics, close finished subagent runs promptly, tidy state files *(curated)*
- **#10137**: mail-send: resolve the ingest key and inbox URL like the daemon does, and keep flagging when they're missing *(curated)*
- **#10146**: mail-send: preflight that diagnoses an un-onboarded machine before sending (URL hint, key file, telemetry-key 401, Matrix leg) *(curated)*
- **#10150**: notify-cleared-blockers runs only from merge-pr.sh: merges made any other way never clear a block *(curated)*
- **#10151**: star_liveness escalates stale blocks to the operator instead of unblocking or routing to Curator *(curated)*
- **#10156**: Daemon re-dispatches human-gated/blocked issues forever after no-op sweeps (275 after-curator loops, ~600M tokens/24h) *(curated)*
- **#10163**: Merge-chain head livelocks: version-bump commits and unrelated merges re-stale its checks faster than a re-date can finish *(curated)*
- **#10168**: forge egress doctor reports a host with no egress policy as clean (origin: unconfigured, 0 findings, exit 0) while 2am's doctor exits 2 on the same host *(curated)*
- **#10169**: Star-liveness 'Operator needed' escalations should send mail, not only a GitHub comment *(curated)*
- **#10177**: Operator work is an issue; the daemon mails from its labels (operator-mechanical), and steps after a merge get their own issue *(curated)*
- **#10179**: Durable host opt-out: a stopped daemon keeps coming back (start/installer/update/agents re-provision it) *(curated)*
- **#10193**: ETA: test a fitted quantile model (land-m1) against heuristics — log friction features, 14d-train/48h-eval censored backtest *(curated)*
- **#10195**: observability: lock in long-term retention for all Loom telemetry — live SigNoz already keeps 10y; reconcile 7d/30d trial docs, aux tables, loom-ui store *(curated)*
- **#10196**: telemetry: make SigNoz point-in-time reconstructable — knowable-at timestamps, full fleet-state snapshots over OTLP, per-host export-coverage records, replay contract *(curated)*
- **#10197**: eta: point-in-time fleet-state reconstruction from forge history via a resumable throttle-safe raw event cache, with agreement check vs logged estimate features *(curated)*
- **#10208**: eta: little-v0 — zero-parameter queue baseline (items ahead ÷ recent drain rate, plus service time) reported beside every candidate *(curated)*
- **#10209**: eta: recency-weight stage samples (half-life with effective-sample-size fallback) instead of a flat 60-day window *(curated)*
- **#10210**: eta: model stalls explicitly — quota reset, pool cooldown, breaker, holds as a 'not before T' term; stop refusing beyond_history *(curated)*
- **#10212**: telemetry: log each role's pick decision per tick — ranked candidates, chosen items, skip reasons — so queue position and service discipline are measurable *(curated)*
- **#10223**: eta: land land-2026-10-04-twin-otter in shadow — the experiment-v2 blend of stage-by-stage and direct models *(curated)*
- **#10229**: sweep-lease-renew.sh spends the operator's personal REST pool at ~600 calls/h and climbing; one sweep id runs ~16 renew loops *(curated)*
- **#10232**: eta: features needing new reads or subsystems — PR size, issue-body markers, rate-limit/pool/breaker stall signals, required checks, live coverage check — #10201 Slice C *(curated)*
- **#10233**: eta: promotion gate and accuracy views — late-surprise + answer-rate gate, common decidable subset, per-day win-rate CI, stability/convergence, multi-fold backtest — #10211 Slice B *(curated)*
- **#10235**: Claim reconciler strips loom:reviewing after 30 min by label age alone, ignoring the Judge's activity (mid-review claim loss) *(curated)*
- **#10238**: loom update never provisions or refreshes user-scope skills on an existing (daemonless) machine — /loom:star unfindable *(curated)*
- **#10255**: merge queue Phase A: dormant typed controls, config, and capability preflight (parent #9978) *(curated)*
- **#10257**: merge queue Phase C: combined-tree CI qualification and eligible-repo live pilot (parent #9978) *(curated)*
- **#10298**: eta fleet refresh: daemon syncs only the issue-events listing, not main's pulls endpoint (closing refs go stale) *(curated)*
- **#10305**: eta backtest: derive PR-history land-case stages via stage_from_pr_labels so operator-held PRs replay as merge_hold *(curated)*
- **#10325**: Follow-on from PR #10266: recency backtest and live shadow comparison *(curated)*

## Proposed (Architect / Hermit)

- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#8788**: Evaluate Codex private-workspace efficiency after the first production canary *(architect)*
- **#9778**: Gitea qualification evidence (gitea-1): measure sweep load, integration effort and post-GO capacity *(architect)*
- **#9790**: Gitea qualification (gitea-1): qualify real Actions workflows, diagnostics and delivery dependencies *(architect)*
- **#9791**: Gitea qualification (gitea-1): complete a supervised Loom lifecycle with minimal reusable integration *(architect)*
- **#9792**: Operator decision: GO or NO-GO on Gitea from gitea-1 qualification evidence *(architect)*
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
- **#9825**: audit-agents: Rust audit of operator-driven build and test placement, OpenCode first *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam
- **#7810**: [epic] Retire shell: 10 sequential PRs, two foundational then update + agent execution
- **#8522**: Epic: send Loom traces, logs, and metrics to ClickStack/HyperDX and SigNoz for a side-by-side trial
- **#9063**: Epic: order overlapping PRs first, then add automatic merge consolidation
- **#9429**: Epic: Fibonacci story points — a size-weighted throughput measure Loom can optimize
- **#9769**: Epic: Self-hosted Gitea qualification (gitea-1), GO/NO-GO, and gated production rollout
- **#9908**: Consolidate persistence: one journal per concern, retire duplicated stores
- **#9978**: feat(champion): optional per-repo merge-queue mode — enqueue Judge-approved PRs to GitHub's merge queue instead of merge-pr.sh's freshness/re-date loop
- **#10165**: Epic: version-locked defaults — ship scripts with the binary so a release rolls atomically

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 1 |
| Operator priority | 79 |
| Ready (`loom:issue`) | 28 |
| In Progress (`loom:building`) | 3 |
| PRs awaiting review | 3 |
| Approved PRs awaiting merge | 41 |
| Curated | 99 |
| Architect / Hermit proposals | 18 |
| Active epics | 11 |
<!-- guide:plan-body:end -->
