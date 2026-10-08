# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#9843**: Centralize operational tunables: hyperparameters tranche 2 (env-only knobs → config block)
- **#9848**: feat(context): content-addressed retrieval cache with bounded provider adapter (#9783)
- **#9902**: feat(collision-evidence): versioned prediction/outcome records with idempotent publication (#9786)
- **#9903**: feat(collision-shadow): prospective shadow-study capture and frozen-policy evaluation (#9787)
- **#9919**: collision-evidence: OTLP push to the configured collector (#9910)
- **#9931**: context-cache: AUGMENT_SESSION_FILE credential seam (#9930)
- **#10032**: feat(roles): mechanical chore mail + objective as decision (#10000 slice 2)
- **#10274**: loom update: provision user-scope skills and ff-sync machine checkout (#10238)
- **#10339**: chore(deps): update clickhouse/clickhouse-keeper docker tag to v25.12.11
- **#10359**: feat(guards): guards.enabled master opt-out + guard false-positive fixes (#10335)
- **#10362**: chore(deps): update clickhouse/clickhouse-keeper docker tag to v26
- **#10602**: feat(stale_blocked): hand undocumented loom:blocked to Curator via loom:blocked-unnamed (#10558)
- **#10882**: feat(watchdog): opt-in bounded supervised recovery for a wedged daemon (#7855)
- **#10889**: feat(forge): forge-probe slice 2 — coordination profile rows, scoped cleanup, injected page faults (#9789)
- **#10906**: ETA: liveness means emitted, authority heartbeat, silent-authority alert (#10898)

## Operator Priority

Issues the operator starred (`loom:operator-priority`); land these first.

- **#10156**: Daemon re-dispatches human-gated/blocked issues forever after no-op sweeps (275 after-curator loops, ~600M tokens/24h)
- **#10337**: Dispatcher yields to its own leaseless loom:building label and orphans the issue (3 starred issues 2026-10-05 00:40Z)
- **#10753**: Champion approval: 17% of curated issues wait a day or more — stop merges starving promotion, revisit tier caps, send 'needs revision' to Curator
- **#10897**: ETA single authority covers only the authority host's managed repos — coverage fell from ~30 repos to 2 (incident 10-07)
- **#10898**: ETA: critical 'no ETAs emitted' alert + liveness that means emitted + authority heartbeat (incident 10-07 went unnoticed ~31h)
- **#10916**: Fleet singleton output watchdog: alert when any one-host fleet job stops producing output (not just when its host goes silent)
- **#10928**: ETA: eta.snapshot 200-row cap drops ~70% of items now that one authority covers the fleet (rows_truncated=459)
- **#10929**: ETA: per-future-stage predictions + per-stage true transitions, joined by estimate_id, with stage-level error attribution
- **#10949**: ETA: promote twin-otter-b to primary land heuristic (gated on live non-refusal), retire the failing IPCW shadows (quick-tern, swift-tern, bold-lark)
- **#10958**: ETA features: model holds as human response time (hold type, release-latency history, operator activity at as_of) — the largest error source
- **#10959**: ETA features: fleet capacity and health at as_of (live workers, token pool, rate-limit breaker, main red, CI queue, recent delivery rate)
- **#10960**: ETA features: PR size and scope from logged file lists (diff stat, docs/tests-only, critical-file, churn) plus story points
- **#10973**: Silent ETA outage #2: ETA authority disk full (0/386 GB) stalled the work finder and Ready ETAs with no alert — disk alerts, daemon self-protection, stale-work-finder alert

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh
- **#8742**: loom:blocked is silently stripped from operator-ruled permanent blocks (unblock probe treats 'unparseable blocker' as 'no blocker')
- **#9132**: auto_update: roll on a scheduled window instead of arming a drain on every new build
- **#9243**: Forge polling exhausts the GraphQL quota while REST sits idle
- **#9255**: forge_events: event-gated per-workspace polling under a bounded staleness cap (ADR-0021 amendment step 2)
- **#9405**: guard: parse_force_ops() treats `|`, `&`, `||` inside $(cd <path> ... pwd) like `&&` — resolves cwd the shell never sets
- **#9487**: Doctor's --force-with-lease can succeed against a stale local tracking ref in a shared clone, silently overwriting a sibling Doctor's push
- **#9507**: telemetry: split per-issue effort into clean/substantive/environmental SECONDS, not event counts (#9444 criterion 4)
- **#9784**: Curator footprints: classify Augment evidence and refresh on issue updates
- **#9789**: Gitea qualification (gitea-1): run live capability probes and surface fatal workflow gaps first
- **#9929**: Curator applies issue-lifecycle labels (loom:curating/loom:curated) to pull requests
- **#9970**: feat(eta): backtested land-v3 heuristic using SigNoz historical analysis to eliminate optimistic delay bias
- **#9974**: loom:operator-priority: rank stars above all work, order multiple stars by earliest star time, never evict in-flight work
- **#9989**: Forge egress enforcement Loom owns: guard-hook denies, role tool policy, container egress boundary + negative canary (#9983 C6)
- **#10000**: An agent asks a human only by a decision or a mail; operator labels stay engine-internal
- **#10023**: daemon: one stable host.id per machine, plus start/stop/heartbeat events in SigNoz
- **#10146**: mail-send: preflight that diagnoses an un-onboarded machine before sending (URL hint, key file, telemetry-key 401, Matrix leg)
- **#10150**: notify-cleared-blockers runs only from merge-pr.sh: merges made any other way never clear a block
- **#10156**: Daemon re-dispatches human-gated/blocked issues forever after no-op sweeps (275 after-curator loops, ~600M tokens/24h)
- **#10169**: Star-liveness 'Operator needed' escalations should send mail, not only a GitHub comment
- **#10196**: telemetry: make SigNoz point-in-time reconstructable — knowable-at timestamps, full fleet-state snapshots over OTLP, per-host export-coverage records, replay contract
- **#10223**: eta: land land-2026-10-04-twin-otter in shadow — the experiment-v2 blend of stage-by-stage and direct models
- **#10229**: sweep-lease-renew.sh spends the operator's personal REST pool at ~600 calls/h and climbing; one sweep id runs ~16 renew loops
- **#10235**: Claim reconciler strips loom:reviewing after 30 min by label age alone, ignoring the Judge's activity (mid-review claim loss)
- **#10257**: merge queue Phase C: combined-tree CI qualification and eligible-repo live pilot (parent #9978)
- **#10337**: Dispatcher yields to its own leaseless loom:building label and orphans the issue (3 starred issues 2026-10-05 00:40Z)
- **#10357**: Operational work lane: approved issues whose deliverable is forge state (labels, comments, closes), not a PR
- **#10424**: eta doctor: account for provisioned repositories before their first snapshot
- **#10539**: merge-pr.sh: workflow-file PRs 403 without gh 'workflow' scope (blocks loom-ui#2022)
- **#10607**: Fleet workers: agent gh calls are invisible (no shim, no agent_gh_front export) — attribute served+passthrough agent calls in SigNoz (~93% writer-App shadow)
- **#10630**: Per-repo demand-driven balance across builder / judge / doctor / champion / curator (generalize build back-off)
- **#10642**: Sweeps on 2AMLogic/2am die before any phase signal and retry without cap: 278/day unclassified:no-phase-signal, ~359M input tokens, up to attempt 35
- **#10737**: ETA friction source: map observed CI history to PR heads for fit and serving
- **#10825**: CI: path-filter the image-smoke jobs on main pushes (docker/** changes only); run them unconditionally in ci-daily
- **#10827**: Comment trust: signed operator-decision markers (verified against fleet-store keys) and an author gate on promotion
- **#10837**: Release pass (#10556) never released loom-ui#1695: all three park-record blockers closed ~22 h ago, still loom:blocked
- **#10845**: CI: the cfg(not(otlp)) targeted step should fail when tests-run != tests-derived; fix stale OTLP-family doc comments
- **#10846**: CI: move the loom-worker :buildcache write into a main-only job (no packages:write on PR tokens)
- **#10875**: Champion re-arms a released merge-risk hold after a tree-identical re-date commit
- **#10897**: ETA single authority covers only the authority host's managed repos — coverage fell from ~30 repos to 2 (incident 10-07)
- **#10916**: Fleet singleton output watchdog: alert when any one-host fleet job stops producing output (not just when its host goes silent)
- **#10919**: ETA: per-heuristic adaptation-time replay feeds shadow_stats::AdaptationTimes into promotion and retirement (Part of #10528; unblocks #10525)
- **#10925**: host.health: the explicit ETA authority's armed eta-fleet-refresh / eta-nightly-folds trip loom-ui's 'singleton armed on a non-captain host' flag
- **#10960**: ETA features: PR size and scope from logged file lists (diff stat, docs/tests-only, critical-file, churn) plus story points

## In Progress

Issues currently being built (`loom:building`).

- **#9935**: builder role_attempt spans are zero-duration by construction — SigNoz's builder stage distribution is unusable
- **#10718**: Per-workspace resync (W0-W2) with first-host-wins claim, only from H0
- **#10753**: Champion approval: 17% of curated issues wait a day or more — stop merges starving promotion, revisit tier caps, send 'needs revision' to Curator
- **#10831**: Pause-and-roll PR 2: pause side (H3/H4), unify all roll triggers on trigger_pause_roll, remove drain_roll/roll_stall (#10715)
- **#10832**: Pause-and-roll PR 3: resume side (H5): health probation, resume from manifest, block legacy recovery, rollback-safety test (#10715)
- **#10869**: Fast-forward each host's main checkout on the sync timer so resynced loom installs take effect (#10698)
- **#10903**: ETA authority estimates every ready row it lists, including rows its own planner cannot dispatch (Part of #10897)
- **#10921**: ETA: planner-in-the-loop queue position for the PR stages in hazard_sim (Part of #10528)
- **#10928**: ETA: eta.snapshot 200-row cap drops ~70% of items now that one authority covers the fleet (rows_truncated=459)
- **#10929**: ETA: per-future-stage predictions + per-stage true transitions, joined by estimate_id, with stage-level error attribution
- **#10933**: ETA: outcome-coverage accounting, missing-outcome alert, gap backfill, censoring-aware headline scores
- **#10949**: ETA: promote twin-otter-b to primary land heuristic (gated on live non-refusal), retire the failing IPCW shadows (quick-tern, swift-tern, bold-lark)
- **#10958**: ETA features: model holds as human response time (hold type, release-latency history, operator activity at as_of) — the largest error source
- **#10959**: ETA features: fleet capacity and health at as_of (live workers, token pool, rate-limit breaker, main red, CI queue, recent delivery rate)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#10972**: feat(eta): estimate ready rows the authority's own planner cannot dispatch (#10903)
- **#10976**: feat(observability): otlp exporter headers from an owner-only file (#10961)

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#9843**: Centralize operational tunables: hyperparameters tranche 2 (env-only knobs → config block)
- **#9848**: feat(context): content-addressed retrieval cache with bounded provider adapter (#9783)
- **#9902**: feat(collision-evidence): versioned prediction/outcome records with idempotent publication (#9786)
- **#9903**: feat(collision-shadow): prospective shadow-study capture and frozen-policy evaluation (#9787)
- **#9919**: collision-evidence: OTLP push to the configured collector (#9910)
- **#9931**: context-cache: AUGMENT_SESSION_FILE credential seam (#9930)
- **#10032**: feat(roles): mechanical chore mail + objective as decision (#10000 slice 2)
- **#10274**: loom update: provision user-scope skills and ff-sync machine checkout (#10238)
- **#10339**: chore(deps): update clickhouse/clickhouse-keeper docker tag to v25.12.11
- **#10355**: chore(deps): update rust to v1.98.1
- **#10359**: feat(guards): guards.enabled master opt-out + guard false-positive fixes (#10335)
- **#10361**: feat: loom:ops label and ops-lane definition (first slice of #10357)
- **#10362**: chore(deps): update clickhouse/clickhouse-keeper docker tag to v26
- **#10410**: test+docs(lease-renew): define release signal, pin cached-read failure (Part of #10229)
- **#10602**: feat(stale_blocked): hand undocumented loom:blocked to Curator via loom:blocked-unnamed (#10558)
- **#10857**: CI: path-filter the image jobs on main pushes; ci-daily runs them unfiltered (#10825)
- **#10882**: feat(watchdog): opt-in bounded supervised recovery for a wedged daemon (#7855)
- **#10889**: feat(forge): forge-probe slice 2 — coordination profile rows, scoped cleanup, injected page faults (#9789)
- **#10906**: ETA: liveness means emitted, authority heartbeat, silent-authority alert (#10898)
- **#10968**: feat(merge-pr): port the already-merged / closed terminal-state gate to Rust (#8191 slice)

## Proposed

Issues carrying `loom:curated`.

- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#7855**: robb-pro daemon stopped heartbeating and answering IPC for 25 min while still logging; the watchdog CONFIRMED the wedge twice and never restarted it *(curated)*
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
- **#8698**: Live end-to-end verification of the credential egress proxy (AC5 of #8674) *(curated)*
- **#8726**: resync-ignore pins record no fork point: 'can this pin be lifted yet?' is archaeology, not a diff *(curated)*
- **#8742**: loom:blocked is silently stripped from operator-ruled permanent blocks (unblock probe treats 'unparseable blocker' as 'no blocker') *(curated)*
- **#8790**: Reaper resume-dispatch tests fail under ambient LOOM_RUNTIME override (runtime admission demands mcp) *(curated)*
- **#8812**: fleet add-worker: verify step proves the worker booted, not that it narrates (no end-to-end assertion) *(curated)*
- **#8813**: Sweep teardown does not kill its own process group -- orphaned sleep infinity holders outlive dead sweeps (cf. #7825) *(curated)*
- **#8849**: Read llm-monitor's new data dir (~/.llm-monitor / LOOM_LLM_MONITOR_DIR), keeping ~/.claude-monitor fallback *(curated)*
- **#8858**: private-control bundle: CONTROL_VERSION bump is unenforced convention, not checked by CI *(curated)*
- **#8913**: observability: live-verify ci.job.log reconstruction in SigNoz and record it in evidence.md (#8825 AC1) *(curated)*
- **#8950**: Live-verify Pi tokens_by_model against a real LOOM_RUNTIME=pi launch *(curated)*
- **#9006**: observability: live-verify the SigNoz queue-starvation alert rule fires and resolves (#8856 / PR #8935 follow-up) *(curated)*
- **#9049**: Security: no secrets under any repo/worktree — move all credential state to ~/.loom (21 Claude OAuth tokens leaked, 2nd incident) *(curated)*
- **#9062**: Rejection telemetry counts daemon base-conflict flags (#8922) as Judge rejections *(curated)*
- **#9064**: fleet add-worker verify always fails on a work-finder-off worker: loom-daemon status exits 5 (EXIT_AUTONOMY_MISMATCH), read as 'not ready' *(curated)*
- **#9065**: CI: cut PR wall time from ~8.5 min to ≤5 min (build once, dedupe nextest, shard serial suites) and stop false-stale merges *(curated)*
- **#9126**: Document the quoted-heredoc rule in comment-body-literal-path.md: an unquoted delimiter silently executes and deletes backticked prose *(curated)*
- **#9131**: status: a drain-paused host reports "the limiter is work availability" while queue reports HALTED *(curated)*
- **#9132**: auto_update: roll on a scheduled window instead of arming a drain on every new build *(curated)*
- **#9136**: main ruleset: bypass_actors 'RepositoryRole:always' makes pull_request and required_status_checks advisory *(curated)*
- **#9152**: worktree-link: an already-created worktree keeps its pnpm node_modules alias (#8944 leaves existing worktrees unfixed) *(curated)*
- **#9192**: merge-pr.sh: "Could not fetch PR #N" hides the real cause — forge_get_pr_nocache discards stderr *(curated)*
- **#9243**: Forge polling exhausts the GraphQL quota while REST sits idle *(curated)*
- **#9255**: forge_events: event-gated per-workspace polling under a bounded staleness cap (ADR-0021 amendment step 2) *(curated)*
- **#9356**: ci: 'Native Port Suites' steps lack !cancelled() guards; #9118 comment overstates coverage *(curated)*
- **#9405**: guard: parse_force_ops() treats `|`, `&`, `||` inside $(cd <path> ... pwd) like `&&` — resolves cwd the shell never sets *(curated)*
- **#9409**: test: POOL_EXHAUSTED_SKIP_COUNT delta assertions race under parallel cargo test (flaky role_runner / runtime_preflight tests) *(curated)*
- **#9418**: Renovate: Dependency Dashboard is enabled by config:recommended (unlabelled bot issue), and renovate.json5's loom:review-requested label has no structural guard *(curated)*
- **#9487**: Doctor's --force-with-lease can succeed against a stale local tracking ref in a shared clone, silently overwriting a sibling Doctor's push *(curated)*
- **#9492**: Decide which other required CI jobs the local build gate should mirror (follow-up to #9140) *(curated)*
- **#9507**: telemetry: split per-issue effort into clean/substantive/environmental SECONDS, not event counts (#9444 criterion 4) *(curated)*
- **#9512**: loom-daemon: reaper_sweep_exited_event_carries_no_progress_classification can hang forever, wedging the whole lib suite behind its #[serial] lock *(curated)*
- **#9518**: prless-retry fleet tally can double-count its own releases on a host whose identity is UNKNOWN_HOST *(curated)*
- **#9548**: Authenticate Loom's control signals: markers and phrases count only from trusted authors (work safely on any public repo) *(curated)*
- **#9714**: create-issue.sh falls back to REST only on rate-limit errors, not on other GraphQL failures *(curated)*
- **#9748**: stale-checks: scope Role Prompt Prefix freshness to the files its checker reads *(curated)*
- **#9758**: ETA fleet history: add the SigNoz-sourced in-sweep half (blocked on harness-ops#249) *(curated)*
- **#9769**: Epic: Self-hosted Gitea qualification (gitea-1), GO/NO-GO, and gated production rollout *(curated)*
- **#9783**: Augment context: persist retrieval results by issue content and source revision *(curated)*
- **#9784**: Curator footprints: classify Augment evidence and refresh on issue updates *(curated)*
- **#9789**: Gitea qualification (gitea-1): run live capability probes and surface fatal workflow gaps first *(curated)*
- **#9790**: Gitea qualification (gitea-1): qualify real Actions workflows, diagnostics and delivery dependencies *(curated)*
- **#9842**: Centralize operational tunables: hyperparameters tranche 2 — env-only knobs onto the config block *(curated)*
- **#9927**: claim-staleness.sh stand-down message omits the bounded-fallback age floor, reading as 'overdue' when it is not *(curated)*
- **#9929**: Curator applies issue-lifecycle labels (loom:curating/loom:curated) to pull requests *(curated)*
- **#9935**: builder role_attempt spans are zero-duration by construction — SigNoz's builder stage distribution is unusable *(curated)*
- **#9970**: feat(eta): backtested land-v3 heuristic using SigNoz historical analysis to eliminate optimistic delay bias *(curated)*
- **#9973**: Flaky: worktree_cli::reset rescue test unexpectedly hits the live-process veto under nextest *(curated)*
- **#9974**: loom:operator-priority: rank stars above all work, order multiple stars by earliest star time, never evict in-flight work *(curated)*
- **#9978**: feat(champion): optional per-repo merge-queue mode — enqueue Judge-approved PRs to GitHub's merge queue instead of merge-pr.sh's freshness/re-date loop *(curated)*
- **#9979**: Codex session containers can't start Codex's bwrap sandbox: every Codex role tick does nothing yet logs SUCCESS (curator/judge/champion stalled fleet-wide) *(curated)*
- **#9989**: Forge egress enforcement Loom owns: guard-hook denies, role tool policy, container egress boundary + negative canary (#9983 C6) *(curated)*
- **#10000**: An agent asks a human only by a decision or a mail; operator labels stay engine-internal *(curated)*
- **#10001**: "Only a human can do this" is for PO-level decisions only; Curator reviews every parked item and returns what agents can do *(curated)*
- **#10009**: Operator-label review (Lane B of #10001): give every parked item in the wired repos a verdict and return what agents can do *(curated)*
- **#10023**: daemon: one stable host.id per machine, plus start/stop/heartbeat events in SigNoz *(curated)*
- **#10036**: Private Codex sessions for any fleet repo: doctor/builder isolation without one-account-per-repo binding *(curated)*
- **#10146**: mail-send: preflight that diagnoses an un-onboarded machine before sending (URL hint, key file, telemetry-key 401, Matrix leg) *(curated)*
- **#10150**: notify-cleared-blockers runs only from merge-pr.sh: merges made any other way never clear a block *(curated)*
- **#10156**: Daemon re-dispatches human-gated/blocked issues forever after no-op sweeps (275 after-curator loops, ~600M tokens/24h) *(curated)*
- **#10169**: Star-liveness 'Operator needed' escalations should send mail, not only a GitHub comment *(curated)*
- **#10177**: Operator work is an issue; the daemon mails from its labels (operator-mechanical), and steps after a merge get their own issue *(curated)*
- **#10193**: ETA: test a fitted quantile model (land-m1) against heuristics — log friction features, 14d-train/48h-eval censored backtest *(curated)*
- **#10196**: telemetry: make SigNoz point-in-time reconstructable — knowable-at timestamps, full fleet-state snapshots over OTLP, per-host export-coverage records, replay contract *(curated)*
- **#10197**: eta: point-in-time fleet-state reconstruction from forge history via a resumable throttle-safe raw event cache, with agreement check vs logged estimate features *(curated)*
- **#10209**: eta: recency-weight stage samples (half-life with effective-sample-size fallback) instead of a flat 60-day window *(curated)*
- **#10212**: telemetry: log each role's pick decision per tick — ranked candidates, chosen items, skip reasons — so queue position and service discipline are measurable *(curated)*
- **#10223**: eta: land land-2026-10-04-twin-otter in shadow — the experiment-v2 blend of stage-by-stage and direct models *(curated)*
- **#10229**: sweep-lease-renew.sh spends the operator's personal REST pool at ~600 calls/h and climbing; one sweep id runs ~16 renew loops *(curated)*
- **#10232**: eta: features needing new reads or subsystems — PR size, issue-body markers, rate-limit/pool/breaker stall signals, required checks, live coverage check — #10201 Slice C *(curated)*
- **#10235**: Claim reconciler strips loom:reviewing after 30 min by label age alone, ignoring the Judge's activity (mid-review claim loss) *(curated)*
- **#10238**: loom update never provisions or refreshes user-scope skills on an existing (daemonless) machine — /loom:star unfindable *(curated)*
- **#10257**: merge queue Phase C: combined-tree CI qualification and eligible-repo live pilot (parent #9978) *(curated)*
- **#10287**: eta: Q2 and the SigNoz README still say the 3-quantile pinball loss decides promotion (gate now uses pinball4) *(curated)*
- **#10325**: Follow-on from PR #10266: recency backtest and live shadow comparison *(curated)*
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day) *(curated)*
- **#10335**: Per-repo opt-out for the PreToolUse guard hooks that install/upgrade respects *(curated)*
- **#10337**: Dispatcher yields to its own leaseless loom:building label and orphans the issue (3 starred issues 2026-10-05 00:40Z) *(curated)*
- **#10357**: Operational work lane: approved issues whose deliverable is forge state (labels, comments, closes), not a PR *(curated)*
- **#10404**: eta fit publication follow-ups from PR #10403 review: 304 path skips captain re-check, refusal state refetches every cycle, captain doc contradiction *(curated)*
- **#10414**: Workers stuck on 0.19.701 since ~01:30Z (12 releases behind) and their fleet-refresh task went silent at the same moment — no auto-update telemetry or version-lag alert *(curated)*
- **#10424**: eta doctor: account for provisioned repositories before their first snapshot *(curated)*
- **#10456**: Epic: the fleet keeps its own hold queue clean — only real operator asks reach Needs attention *(curated)*
- **#10470**: security(update): enforce independent release approval and artifact assurance for managed fleets *(curated)*
- **#10508**: ETA: priority-aware land model — linked-issue star, priority level, repo fleet_priority, dispatch-order position as inputs (new shadow heuristic after twin-otter-b) *(curated)*
- **#10512**: Fleet reader-App pool near exhaustion: cut top billable pollers (visibility.repo cache, issue_state ETag, cross-host dedupe) — ~8k REST/h *(curated)*
- **#10524**: ETA: conformalized survival calibration wrapper (IPCW split-conformal, history-aware) — coverage guarantee under censoring for any base heuristic *(curated)*
- **#10525**: ETA: shadow-model budget, tiers (baseline/candidate/retired) and nightly-fold auto-retirement proposals — run many shadows safely *(curated)*
- **#10528**: ETA: adapt rapidly to system changes (umbrella) — latent-regime residual adjustment, adaptive recency, drift-widened intervals, planner-in-the-loop simulation; adaptation time (t_p50/t_cov/t_alarm) as a gated metric *(curated)*
- **#10539**: merge-pr.sh: workflow-file PRs 403 without gh 'workflow' scope (blocks loom-ui#2022) *(curated)*
- **#10550**: ETA: log file lists and CI runs for the #10521 friction predictors, fit them under a new schema and ship a shadow heuristic with backtest *(curated)*
- **#10558**: Undocumented loom:blocked (no blocker named) is handed to Curator each tick: name the blocker or release it *(curated)*
- **#10607**: Fleet workers: agent gh calls are invisible (no shim, no agent_gh_front export) — attribute served+passthrough agent calls in SigNoz (~93% writer-App shadow) *(curated)*
- **#10630**: Per-repo demand-driven balance across builder / judge / doctor / champion / curator (generalize build back-off) *(curated)*
- **#10642**: Sweeps on 2AMLogic/2am die before any phase signal and retry without cap: 278/day unclassified:no-phase-signal, ~359M input tokens, up to attempt 35 *(curated)*
- **#10672**: ETA: let the captain publish the fit to a dedicated store repo (fleet.etaFitRepo), separate from fleet.repo *(curated)*
- **#10698**: Fleet version floor: the daemon enforces `loom_min_version` from the fleet store, rolls itself forward, and brings each repo's installed Loom up to its own version *(curated)*
- **#10715**: Pause-and-roll: floor-driven roll pauses in-flight work, restarts, resumes (D2) *(curated)*
- **#10719**: Dispatch holds for W3/W4 and repo_ahead_target (D1) *(curated)*
- **#10720**: Restart-only fleet config change triggers pause and restart *(curated)*
- **#10721**: Gauges and status for host H0-H7 and workspace W states *(curated)*
- **#10737**: ETA friction source: map observed CI history to PR heads for fit and serving *(curated)*
- **#10748**: ETA regime layer: drift check false-alarms on a calm synthetic stream; post-shift t_cov is borderline vs the 12 h target *(curated)*
- **#10752**: telemetry: the hold-cleanup and approval passes leave no record in SigNoz — export what each pass did, per item, with its caller *(curated)*
- **#10753**: Champion approval: 17% of curated issues wait a day or more — stop merges starving promotion, revisit tier caps, send 'needs revision' to Curator *(curated)*
- **#10756**: ETA SigNoz timeline: export the daemon stage journal's label.* rows over OTLP, or remove the gated LabelSet path *(curated)*
- **#10802**: General cleanup: reap agent process residue (dev servers, orphaned children) after any agent exit *(curated)*
- **#10815**: Balance slice 2: wire the per-repo allocator into role admission (judge/doctor lanes, build back-off) behind autonomous.balance.enabled *(curated)*
- **#10827**: Comment trust: signed operator-decision markers (verified against fleet-store keys) and an author gate on promotion *(curated)*
- **#10837**: Release pass (#10556) never released loom-ui#1695: all three park-record blockers closed ~22 h ago, still loom:blocked *(curated)*
- **#10845**: CI: the cfg(not(otlp)) targeted step should fail when tests-run != tests-derived; fix stale OTLP-family doc comments *(curated)*
- **#10846**: CI: move the loom-worker :buildcache write into a main-only job (no packages:write on PR tokens) *(curated)*
- **#10870**: MCP pre-flight smoke test ignores the server's .mcp.json env block *(curated)*
- **#10874**: eta.snapshot contract: re-vendor loom-ui's consumer fixture (now a file, adds alternates[].as_of) *(curated)*
- **#10875**: Champion re-arms a released merge-risk hold after a tree-identical re-date commit *(curated)*
- **#10880**: auto-update persistence: follow-ups from #10877 (floor roll in a consumed window, rollback refetch loop, hardening) *(curated)*
- **#10885**: Remove roll windows; floor-driven rolls act on the next tick (no chase-latest when a floor is set) *(curated)*
- **#10896**: ETA: after ~2 weeks of nightly-fold data, test whether story points improve ETA prediction (paired shadow experiment) *(curated)*
- **#10897**: ETA single authority covers only the authority host's managed repos — coverage fell from ~30 repos to 2 (incident 10-07) *(curated)*
- **#10898**: ETA: critical 'no ETAs emitted' alert + liveness that means emitted + authority heartbeat (incident 10-07 went unnoticed ~31h) *(curated)*
- **#10903**: ETA authority estimates every ready row it lists, including rows its own planner cannot dispatch (Part of #10897) *(curated)*
- **#10913**: forge_events: hold read caches (gh-cached, pipeline_snapshot) under the poll-gating health signal (#9255 slice 2) *(curated)*
- **#10916**: Fleet singleton output watchdog: alert when any one-host fleet job stops producing output (not just when its host goes silent) *(curated)*
- **#10919**: ETA: per-heuristic adaptation-time replay feeds shadow_stats::AdaptationTimes into promotion and retirement (Part of #10528; unblocks #10525) *(curated)*
- **#10921**: ETA: planner-in-the-loop queue position for the PR stages in hazard_sim (Part of #10528) *(curated)*
- **#10924**: Fleet singleton output watchdog, slice 2: wire fleet_outputs into fleet_alert + SigNoz rule *(curated)*
- **#10925**: host.health: the explicit ETA authority's armed eta-fleet-refresh / eta-nightly-folds trip loom-ui's 'singleton armed on a non-captain host' flag *(curated)*
- **#10926**: eta doctor and docs gaps after #10918: env hard stop on the authority reads OK; stale fit/trigger docs; undocumented double-run rollout window *(curated)*
- **#10928**: ETA: eta.snapshot 200-row cap drops ~70% of items now that one authority covers the fleet (rows_truncated=459) *(curated)*
- **#10929**: ETA: per-future-stage predictions + per-stage true transitions, joined by estimate_id, with stage-level error attribution *(curated)*
- **#10930**: ETA: log per-estimate input vector + 'loom-daemon eta explain' replay/diff, split error into input vs model error *(curated)*
- **#10931**: ETA: classify each large miss by cause (stage overrun, exogenous event, capacity shortfall, rework, scope change, model) — eta.miss + error share by cause *(curated)*
- **#10932**: ETA: nightly residual slice report (significant-bias segments) + daily worst-misses digest *(curated)*
- **#10933**: ETA: outcome-coverage accounting, missing-outcome alert, gap backfill, censoring-aware headline scores *(curated)*
- **#10949**: ETA: promote twin-otter-b to primary land heuristic (gated on live non-refusal), retire the failing IPCW shadows (quick-tern, swift-tern, bold-lark) *(curated)*
- **#10958**: ETA features: model holds as human response time (hold type, release-latency history, operator activity at as_of) — the largest error source *(curated)*
- **#10959**: ETA features: fleet capacity and health at as_of (live workers, token pool, rate-limit breaker, main red, CI queue, recent delivery rate) *(curated)*
- **#10960**: ETA features: PR size and scope from logged file lists (diff stat, docs/tests-only, critical-file, churn) plus story points *(curated)*

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
- **#10036**: Private Codex sessions for any fleet repo: doctor/builder isolation without one-account-per-repo binding
- **#10165**: Epic: version-locked defaults — ship scripts with the binary so a release rolls atomically
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day)
- **#10456**: Epic: the fleet keeps its own hold queue clean — only real operator asks reach Needs attention

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 15 |
| Operator priority | 13 |
| Ready (`loom:issue`) | 46 |
| In Progress (`loom:building`) | 14 |
| PRs awaiting review | 2 |
| Approved PRs awaiting merge | 20 |
| Curated | 149 |
| Architect / Hermit proposals | 18 |
| Active epics | 14 |
<!-- guide:plan-body:end -->
