# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#9745**: feat(merge-pr): consolidation landing reconciliation (#9689)
- **#9966**: fix(push): pin --force-with-lease to the head the work is based on (#9487)
- **#9975**: feat(daemon): star-time ordering, forge starred/star verbs, no-eviction + star-authority docs (#9974)
- **#10074**: refactor(gh): migrate sweep_registry gh spawns onto gh_invocation facade (#9985 slice 5)
- **#10090**: fix(observability): VALUES instead of >5-term UNION ALL so landed-size.sql runs on D1; guard the limit in CI

## Operator Priority

Issues the operator starred (`loom:operator-priority`); land these first.

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh
- **#9132**: auto_update: roll on a scheduled window instead of arming a drain on every new build
- **#9769**: Epic: Self-hosted Gitea qualification (gitea-1), GO/NO-GO, and gated production rollout
- **#9777**: Forge qualification: version the operation inventory and enforce coverage accounting
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
- **#9831**: Forge inventory: wire the call-identity accounting into real forge call sites
- **#9924**: forge_contract::InstanceOrigin::parse accepts a scheme with no host (e.g. "https://")
- **#9945**: forge-probe disposable issue titles don't follow the ${GITEA_QUAL_RUN_NS} prefix convention
- **#9974**: loom:operator-priority: rank stars above all work, order multiple stars by earliest star time, never evict in-flight work
- **#9983**: Forge egress policy: route every GitHub API call Loom makes through a mandated gateway, validated at every entry point (upstream half of 2am#1911)
- **#9985**: loom-daemon: one `gh` spawn choke point with CI scan test and client-side `invoke github` telemetry (#9983 C2)
- **#9986**: credential_preflight: routing-preserving hosts.yml on observe hosts, no GitHub credential on required hosts, git credential separated (#9983 C3)
- **#9987**: Run 2am's managed gh launcher everywhere Loom spawns gh: worker PATH, containers, outcome codes (#9983 C4)
- **#9989**: Forge egress enforcement Loom owns: guard-hook denies, role tool policy, container egress boundary + negative canary (#9983 C6)
- **#9995**: forge egress: wire gh_invocation resolver's policy-launcher rung to the C1 policy reader (follow-up to #9984)
- **#9996**: forge egress: run `forge egress doctor` after resync-installed.sh (follow-up to #9984)
- **#9999**: forge egress: hermetic PolicySources seam for dispatch/spawn tests + run_bounded pipe/process-group handling (follow-up to #9984)
- **#10000**: An agent asks a human only by a decision or a mail; operator labels stay engine-internal
- **#10001**: "Only a human can do this" is for PO-level decisions only; Curator reviews every parked item and returns what agents can do
- **#10012**: Propagate loom:operator-priority from a starred parent to its child issues and PRs, daemon-enforced and removed with the parent's star
- **#10013**: Label registry: one JSON file holding every label's properties, replacing the scattered daemon and shell label tables
- **#10019**: daemon: read loom label state from the dashboard's webhook-fed fleet state instead of polling GitHub
- **#10022**: daemon: export rate_limit_breaker trips, own/external attribution and quota gauges to SigNoz
- **#10023**: daemon: one stable host.id per machine, plus start/stop/heartbeat events in SigNoz
- **#10025**: Curator scripts on REST exhaustion: check-duplicate says 'Not authenticated', premise-check fails closed, post-comment has no GraphQL fallback
- **#10026**: merge-pr.sh: repo-configured pre-merge tree checks — refuse merges whose merge tree fails cheap cross-PR guards (migration prefixes, typecheck)
- **#10027**: sweep-lease-fence: a closed PR's preserved feature/issue-N head is a permanent BRANCH_COLLISION, and leg 5 suppresses the lease check
- **#10039**: Curator's per-issue gh issue view/edit fan-out exhausts the shared GraphQL pool
- **#10050**: C3 follow-up: git credential separation in provisioning + pinned upstream gh (rest of #9986)
- **#10077**: merge sequencing chains every PR in a transitive overlap group: 32 open PRs serialized, edges between PRs that share no files
- **#10089**: loom-daemon spends ~1,200-1,500 REST calls/hour of the shared pool, and its breaker books ~98% of it as external
- **#10116**: Attended-session agents emit no issue-tagged output, so the loom-ui issue log panel is empty for them
- **#10118**: main-red-fix issues sit in loom:triage while main stays red: the red-main lane only reorders candidates, so an unpromoted fix never enters it
- **#10120**: Attended live output: wire roles that never claim an issue (Judge, Curator, Champion, ...) into live-output-attend
- **#10125**: Attended live-output tailer: surface start diagnostics, close finished subagent runs promptly, tidy state files
- **#10137**: mail-send: resolve the ingest key and inbox URL like the daemon does, and keep flagging when they're missing
- **#10138**: loom-daemon cargo tests call live GitHub via gh on the operator's token (~1,840 calls/day on 2026-10-03)
- **#10146**: mail-send: preflight that diagnoses an un-onboarded machine before sending (URL hint, key file, telemetry-key 401, Matrix leg)
- **#10150**: notify-cleared-blockers runs only from merge-pr.sh: merges made any other way never clear a block
- **#10151**: star_liveness escalates stale blocks to the operator instead of unblocking or routing to Curator
- **#10152**: Applying loom:blocked must write a park record in the body, not only a prose comment
- **#10154**: provision-skills: make /star a user-scope alias of /loom:star (symlink, or a progressive-disclosure stub)
- **#10156**: Daemon re-dispatches human-gated/blocked issues forever after no-op sweeps (275 after-curator loops, ~600M tokens/24h)

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh
- **#9405**: guard: parse_force_ops() treats `|`, `&`, `||` inside $(cd <path> ... pwd) like `&&` — resolves cwd the shell never sets
- **#9479**: git: four Rust git-fetch sinks lack the #9106 refname guard and `--` separator (defence in depth)
- **#9689**: Merge consolidation: land verified candidates and reconcile component PRs safely (9063 later increment)
- **#9777**: Forge qualification: version the operation inventory and enforce coverage accounting
- **#9779**: Gitea qualification: define identity, capability errors and the supported self-hosted profile
- **#9985**: loom-daemon: one `gh` spawn choke point with CI scan test and client-side `invoke github` telemetry (#9983 C2)
- **#9986**: credential_preflight: routing-preserving hosts.yml on observe hosts, no GitHub credential on required hosts, git credential separated (#9983 C3)
- **#10000**: An agent asks a human only by a decision or a mail; operator labels stay engine-internal
- **#10012**: Propagate loom:operator-priority from a starred parent to its child issues and PRs, daemon-enforced and removed with the parent's star
- **#10013**: Label registry: one JSON file holding every label's properties, replacing the scattered daemon and shell label tables
- **#10025**: Curator scripts on REST exhaustion: check-duplicate says 'Not authenticated', premise-check fails closed, post-comment has no GraphQL fallback
- **#10120**: Attended live output: wire roles that never claim an issue (Judge, Curator, Champion, ...) into live-output-attend

## In Progress

Issues currently being built (`loom:building`).

- **#9132**: auto_update: roll on a scheduled window instead of arming a drain on every new build
- **#10077**: merge sequencing chains every PR in a transitive overlap group: 32 open PRs serialized, edges between PRs that share no files
- **#10089**: loom-daemon spends ~1,200-1,500 REST calls/hour of the shared pool, and its breaker books ~98% of it as external
- **#10125**: Attended live-output tailer: surface start diagnostics, close finished subagent runs promptly, tidy state files
- **#10137**: mail-send: resolve the ingest key and inbox URL like the daemon does, and keep flagging when they're missing
- **#10138**: loom-daemon cargo tests call live GitHub via gh on the operator's token (~1,840 calls/day on 2026-10-03)
- **#10150**: notify-cleared-blockers runs only from merge-pr.sh: merges made any other way never clear a block
- **#10151**: star_liveness escalates stale blocks to the operator instead of unblocking or routing to Curator
- **#10152**: Applying loom:blocked must write a park record in the body, not only a prose comment
- **#10154**: provision-skills: make /star a user-scope alias of /loom:star (symlink, or a progressive-disclosure stub)
- **#10156**: Daemon re-dispatches human-gated/blocked issues forever after no-op sweeps (275 after-curator loops, ~600M tokens/24h)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#8314**: feat(guard): per-role tool-restriction allowlist enforced at the harness (#8256)
- **#9819**: feat(daemon): warn when a scoped-socket daemon starts alongside a live machine-level daemon (#9815)
- **#9832**: feat(forge): version the forge operation inventory and enforce coverage accounting
- **#9893**: fix(guards): require && or ; for the cross-segment $(cd <path> ... pwd) close (#9405)
- **#9969**: fix(harness): Codex 0.160.0 in the session image; track harness CLIs at latest
- **#10157**: feat(install): alias /star to /loom:star via user-scope symlink (#10154)
- **#10158**: feat(live-output): attend Curator and Judge runs from code they already run

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#9745**: feat(merge-pr): consolidation landing reconciliation (#9689)
- **#9843**: Centralize operational tunables: hyperparameters tranche 2 (env-only knobs → config block)
- **#9848**: feat(context): content-addressed retrieval cache with bounded provider adapter (#9783)
- **#9852**: test(daemon): clear ambient runtime pins so buildGate is not a function of the shell (#9360)
- **#9853**: feat(footprint): classified, revision-pinned issue footprints over the context cache (#9784)
- **#9886**: docs(forge): the forge contract — identity, outcomes, evidence (#9779)
- **#9897**: feat(forge): the hosted-qualification probe runner, slice 1 (#9789)
- **#9902**: feat(collision-evidence): versioned prediction/outcome records with idempotent publication (#9786)
- **#9903**: feat(collision-shadow): prospective shadow-study capture and frozen-policy evaluation (#9787)
- **#9919**: collision-evidence: OTLP push to the configured collector (#9910)
- **#9931**: context-cache: AUGMENT_SESSION_FILE credential seam (#9930)
- **#9938**: fix(daemon): never treat a pull request as an issue in candidate selection or claim
- **#9955**: mcp: daemon_status tool — the agent relay for telemetry warnings (#9950)
- **#9958**: feat(fleet-config): refuse a lossy machine-tier render by default (2am#1653's ask, guard side)
- **#9966**: fix(push): pin --force-with-lease to the head the work is based on (#9487)
- **#9975**: feat(daemon): star-time ordering, forge starred/star verbs, no-eviction + star-authority docs (#9974)
- **#10005**: docs(blocked): permanent-block marker + audit comment on unblock (#8742)
- **#10032**: feat(roles): mechanical chore mail + objective as decision (#10000 slice 2)
- **#10046**: docs(curator): cut GraphQL fan-out (REST, two-pool check, cap 3)
- **#10048**: forge egress: run doctor after resync-installed.sh (#9996)
- **#10051**: feat(credential_preflight): routing-preserving hosts.yml, no GitHub credential on required hosts (#9986)
- **#10053**: ci(labels): extend drift check to the registry (#10013 slice 1 follow-up)
- **#10056**: fix(sweep-lease-fence): closed PR head is not a collision; lease legs always run (#10027)
- **#10058**: forge egress: hermetic PolicySources seam + run_bounded drain/process-group (#9999)
- **#10059**: forge egress: wire gh resolver policy-launcher rung (#9995)
- **#10061**: feat(daemon): export rate-limit breaker trips, attribution and quota gauges (#10022)
- **#10063**: merge-pr.sh: repo-configured pre-merge tree checks (merge.treeChecks, --allow-red-tree)
- **#10064**: docs: drop the Gitea Cloud runbook; point GITEA_QUAL_* at self-hosted gitea-1
- **#10065**: feat(daemon): stable host.id per machine plus start/shutdown/heartbeat telemetry (#10023)
- **#10068**: docs(credentials): reference the D1 token for loom-fleet-telemetry (#10067)
- **#10074**: refactor(gh): migrate sweep_registry gh spawns onto gh_invocation facade (#9985 slice 5)
- **#10090**: fix(observability): VALUES instead of >5-term UNION ALL so landed-size.sql runs on D1; guard the limit in CI
- **#10093**: feat(merge-pr): port the worktree-removal identity gate to Rust (#8191 slice)
- **#10094**: refactor(labels): derive work-finder and hard-exclusion sets from the registry (#10013 slice 2a)
- **#10128**: feat(work-finder): admit unpromoted red-main fixes on a red repo, escalate if unclaimed (#10118)
- **#10141**: feat(auto_update): roll on a scheduled window instead of arming a drain on every build
- **#10147**: mail-send: resolve inbox URL and ingest key file like the daemon; flag gaps in health (#10137)
- **#10148**: feat(mail): loom-daemon mail preflight (#10146)

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
- **#9479**: git: four Rust git-fetch sinks lack the #9106 refname guard and `--` separator (defence in depth) *(curated)*
- **#9487**: Doctor's --force-with-lease can succeed against a stale local tracking ref in a shared clone, silently overwriting a sibling Doctor's push *(curated)*
- **#9492**: Decide which other required CI jobs the local build gate should mirror (follow-up to #9140) *(curated)*
- **#9507**: telemetry: split per-issue effort into clean/substantive/environmental SECONDS, not event counts (#9444 criterion 4) *(curated)*
- **#9512**: loom-daemon: reaper_sweep_exited_event_carries_no_progress_classification can hang forever, wedging the whole lib suite behind its #[serial] lock *(curated)*
- **#9548**: Authenticate Loom's control signals: markers and phrases count only from trusted authors (work safely on any public repo) *(curated)*
- **#9769**: Epic: Self-hosted Gitea qualification (gitea-1), GO/NO-GO, and gated production rollout *(curated)*
- **#9777**: Forge qualification: version the operation inventory and enforce coverage accounting *(curated)*
- **#9779**: Gitea qualification: define identity, capability errors and the supported self-hosted profile *(curated)*
- **#9783**: Augment context: persist retrieval results by issue content and source revision *(curated)*
- **#9784**: Curator footprints: classify Augment evidence and refresh on issue updates *(curated)*
- **#9842**: Centralize operational tunables: hyperparameters tranche 2 — env-only knobs onto the config block *(curated)*
- **#9927**: claim-staleness.sh stand-down message omits the bounded-fallback age floor, reading as 'overdue' when it is not *(curated)*
- **#9929**: Curator applies issue-lifecycle labels (loom:curating/loom:curated) to pull requests *(curated)*
- **#9973**: Flaky: worktree_cli::reset rescue test unexpectedly hits the live-process veto under nextest *(curated)*
- **#9974**: loom:operator-priority: rank stars above all work, order multiple stars by earliest star time, never evict in-flight work *(curated)*
- **#9985**: loom-daemon: one `gh` spawn choke point with CI scan test and client-side `invoke github` telemetry (#9983 C2) *(curated)*
- **#9986**: credential_preflight: routing-preserving hosts.yml on observe hosts, no GitHub credential on required hosts, git credential separated (#9983 C3) *(curated)*
- **#9995**: forge egress: wire gh_invocation resolver's policy-launcher rung to the C1 policy reader (follow-up to #9984) *(curated)*
- **#9996**: forge egress: run `forge egress doctor` after resync-installed.sh (follow-up to #9984) *(curated)*
- **#9999**: forge egress: hermetic PolicySources seam for dispatch/spawn tests + run_bounded pipe/process-group handling (follow-up to #9984) *(curated)*
- **#10000**: An agent asks a human only by a decision or a mail; operator labels stay engine-internal *(curated)*
- **#10001**: "Only a human can do this" is for PO-level decisions only; Curator reviews every parked item and returns what agents can do *(curated)*
- **#10009**: Operator-label review (Lane B of #10001): give every parked item in the wired repos a verdict and return what agents can do *(curated)*
- **#10012**: Propagate loom:operator-priority from a starred parent to its child issues and PRs, daemon-enforced and removed with the parent's star *(curated)*
- **#10013**: Label registry: one JSON file holding every label's properties, replacing the scattered daemon and shell label tables *(curated)*
- **#10019**: daemon: read loom label state from the dashboard's webhook-fed fleet state instead of polling GitHub *(curated)*
- **#10022**: daemon: export rate_limit_breaker trips, own/external attribution and quota gauges to SigNoz *(curated)*
- **#10023**: daemon: one stable host.id per machine, plus start/stop/heartbeat events in SigNoz *(curated)*
- **#10025**: Curator scripts on REST exhaustion: check-duplicate says 'Not authenticated', premise-check fails closed, post-comment has no GraphQL fallback *(curated)*
- **#10026**: merge-pr.sh: repo-configured pre-merge tree checks — refuse merges whose merge tree fails cheap cross-PR guards (migration prefixes, typecheck) *(curated)*
- **#10027**: sweep-lease-fence: a closed PR's preserved feature/issue-N head is a permanent BRANCH_COLLISION, and leg 5 suppresses the lease check *(curated)*
- **#10050**: C3 follow-up: git credential separation in provisioning + pinned upstream gh (rest of #9986) *(curated)*
- **#10066**: landed-size.sql fails on D1: 6-term UNION ALL exceeds D1's 5-term compound SELECT limit (LSI can't go live) *(curated)*
- **#10118**: main-red-fix issues sit in loom:triage while main stays red: the red-main lane only reorders candidates, so an unpromoted fix never enters it *(curated)*
- **#10120**: Attended live output: wire roles that never claim an issue (Judge, Curator, Champion, ...) into live-output-attend *(curated)*
- **#10125**: Attended live-output tailer: surface start diagnostics, close finished subagent runs promptly, tidy state files *(curated)*
- **#10137**: mail-send: resolve the ingest key and inbox URL like the daemon does, and keep flagging when they're missing *(curated)*
- **#10138**: loom-daemon cargo tests call live GitHub via gh on the operator's token (~1,840 calls/day on 2026-10-03) *(curated)*
- **#10146**: mail-send: preflight that diagnoses an un-onboarded machine before sending (URL hint, key file, telemetry-key 401, Matrix leg) *(curated)*
- **#10150**: notify-cleared-blockers runs only from merge-pr.sh: merges made any other way never clear a block *(curated)*
- **#10151**: star_liveness escalates stale blocks to the operator instead of unblocking or routing to Curator *(curated)*
- **#10152**: Applying loom:blocked must write a park record in the body, not only a prose comment *(curated)*
- **#10154**: provision-skills: make /star a user-scope alias of /loom:star (symlink, or a progressive-disclosure stub) *(curated)*
- **#10156**: Daemon re-dispatches human-gated/blocked issues forever after no-op sweeps (275 after-curator loops, ~600M tokens/24h) *(curated)*

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

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 5 |
| Operator priority | 60 |
| Ready (`loom:issue`) | 15 |
| In Progress (`loom:building`) | 11 |
| PRs awaiting review | 7 |
| Approved PRs awaiting merge | 38 |
| Curated | 78 |
| Architect / Hermit proposals | 18 |
| Active epics | 9 |
<!-- guide:plan-body:end -->
