# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs under a hold (`loom:blocked` or `loom:operator`) — only a human merge decision is missing.

- **#12262**: fix: track cd in subshells, brace groups and pushd in write-confinement guard
- **#12375**: chore: delete the telemetry-replay CLI, SigNoz replay queries and CI jobs
- **#12522**: fix: shed reads on failed roster load, withdraw evidence-free 404s briefly
- **#12969**: feat: age-bound Codex rollout history in live profiles
- **#12997**: feat: let several criticalFilesOnly standing classes jointly cover a PR's critical files
- **#13043**: fix: compose criticalFilesOnly standing authorizations per critical-file hit
- **#13063**: feat: report and reset stale local branches reused by worktree.sh
- **#13090**: fix: refuse to commit under a hostname-fallback git identity
- **#13097**: feat(champion): pass the kept-set hold for byte-matching resync PRs

## Very Important

`loom:very-important` (one counts per workspace); every stage takes these first.

- **#12203**: stage_dwell: every host re-lists every repo every 5 min (~8.9k billable GitHub calls/h fleet-wide)

## Important

`loom:important`; taken before unlabelled work.

- **#9081**: guard-background-subagents misses a background Bash started by a foreground subagent: headless sweep exits, Builder's make verify reaped
- **#9957**: daemon: deliberate stops (SIGTERM) are silent autonomy losses — the watchdog goes down with it, the intent marker is deleted, and bootstrap cannot recover
- **#10083**: CI path filter skips label_registry Rust tests on label-file-only PRs (how #10072 turned main red)
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue
- **#10890**: Daemon dispatches loom:issue + loom:needs-capability items; pre-flight skip then reads as insta-crash and quarantine release re-queues them
- **#11301**: check-stale-blocked: the per-repo budget probe spends ~1.4k GraphQL points/h on the writer bucket that runs dry
- **#11787**: issue.estimate coverage gap: operator-session curation sets loom:curated by hand and bypasses curation-complete (loom-ui: 58 of 62 curations since the gate have no estimate)
- **#12085**: Telemetry(doctor): structured parked-PR cause (park reason, doctor_cycles, queue gate, treating age) (ETA doctor, loom-ui#4063 #6)
- **#12217**: github-events mirror reads stale for quiet repos (1555 stale vs 1049 hits/h), so the step gather falls back to REST
- **#12447**: Epic: captain-assigned dispatch — workers ask the fleet captain for their next assignment from one fleet queue
- **#12506**: merge sequencing: Phase 1 release skips some followers whose predecessor merged at the recorded head, and fail-closed holds emit no telemetry
- **#12551**: fleet_dispatch no-progress hold follow-ups: Doctor outcome line, Judge ci_pending, CI-green release, metric attribution
- **#12653**: Emit merge_hold hold-cause + release-actor + merge-queue facts to SigNoz (unblocks merge_hold ETA prediction)
- **#12909**: ci_telemetry: 2amlogic owner discovery times out every cycle since Oct 17 (no loom-ui CI facts; 478 consecutive failures)
- **#12950**: Champion: self-release merge-risk holds at ≥90% confidence (operator direction 2026-10-20)
- **#12993**: Shadow evaluation of the new Champion (2026-10-20): 1/95 holds cleared — ranked remaining causes + fixtures
- **#13008**: Merge-risk holds are redrawn at their held head again: #12933 draws holds with no auth= digest, and only critical-file holds record one
- **#13009**: Captain champion_merge slot stays held ~20 min after a successful /v1/release (holder outlives its release until lease expiry)
- **#13026**: Champion: make per-repo merge concurrency a runtime tunable, and take hold re-reads out of the merge lane
- **#13029**: Telemetry: merge-lane waits, Champion re-read outcomes, fleet_dispatch call outcomes, held-PR gauge (gaps found 2026-10-20/21)

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#8525**: tracing: instrument sweep phases and role attempts, including Pi/OpenCode and repair cycles
- **#10890**: Daemon dispatches loom:issue + loom:needs-capability items; pre-flight skip then reads as insta-crash and quarantine release re-queues them
- **#11301**: check-stale-blocked: the per-repo budget probe spends ~1.4k GraphQL points/h on the writer bucket that runs dry
- **#12117**: Telemetry: inventory every kind; fix or delete role_tick.outcome, ci.job.log, session.output
- **#12167**: telemetry: export peer_coordination verdict and degraded duration on host.health
- **#12506**: merge sequencing: Phase 1 release skips some followers whose predecessor merged at the recorded head, and fail-closed holds emit no telemetry
- **#12653**: Emit merge_hold hold-cause + release-actor + merge-queue facts to SigNoz (unblocks merge_hold ETA prediction)
- **#12863**: Review dated guard-confinement evidence and owner-specific cases from gf180-opamp #214

## In Progress

Issues currently being built (`loom:building`).

- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#9081**: guard-background-subagents misses a background Bash started by a foreground subagent: headless sweep exits, Builder's make verify reaped
- **#9957**: daemon: deliberate stops (SIGTERM) are silent autonomy losses — the watchdog goes down with it, the intent marker is deleted, and bootstrap cannot recover
- **#10083**: CI path filter skips label_registry Rust tests on label-file-only PRs (how #10072 turned main red)
- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default)
- **#11787**: issue.estimate coverage gap: operator-session curation sets loom:curated by hand and bypasses curation-complete (loom-ui: 58 of 62 curations since the gate have no estimate)
- **#12082**: Worktree-isolation guard blocks Bash writes to paths outside the repo when the cwd is the main checkout
- **#12085**: Telemetry(doctor): structured parked-PR cause (park reason, doctor_cycles, queue gate, treating age) (ETA doctor, loom-ui#4063 #6)
- **#12094**: Retire the telemetry replay subsystem and the kinds left without a producer by #11301
- **#12362**: Telemetry: Judge runs record found-work, PR, head_sha and the real verdict; drop the synthetic loom.judge_verdict (split from #12029, no merge-pr changes)
- **#12425**: auto-update: arm pause-and-roll from the config-restart demand (#12419 child 3)
- **#12447**: Epic: captain-assigned dispatch — workers ask the fleet captain for their next assignment from one fleet queue
- **#12458**: Reap idle clean worktrees of open issues/PRs (configurable idle period)
- **#12511**: Readers: a failed roster load still routes reads to the writer; evidence-free 404 withdrawal is an hour (follow-up to #12412)
- **#12551**: fleet_dispatch no-progress hold follow-ups: Doctor outcome line, Judge ci_pending, CI-green release, metric attribution
- **#12604**: Split status_render.rs: break up ~970-line print_status_human and move inline tests out
- **#12638**: Test: lease_refresh_child_carries_the_root_owners_gh_config_dir fails on main (child sees no GH_CONFIG_DIR)
- **#12775**: Simplify merge ordering: retire automatic soft holds while preserving explicit dependencies
- **#12950**: Champion: self-release merge-risk holds at ≥90% confidence (operator direction 2026-10-20)
- **#12994**: Champion: let the union of eligible criticalFilesOnly classes cover a PR's critical files
- **#13007**: Carry the remaining queue-plan fields (urgent, operator_priority, tier, workspace_weight, shard, listing_failed, seen) over OTLP for the dashboard's SigNoz Live board
- **#13008**: Merge-risk holds are redrawn at their held head again: #12933 draws holds with no auth= digest, and only critical-file holds record one
- **#13009**: Captain champion_merge slot stays held ~20 min after a successful /v1/release (holder outlives its release until lease expiry)
- **#13026**: Champion: make per-repo merge concurrency a runtime tunable, and take hold re-reads out of the merge lane
- **#13029**: Telemetry: merge-lane waits, Champion re-read outcomes, fleet_dispatch call outcomes, held-PR gauge (gaps found 2026-10-20/21)
- **#13042**: Champion: let criticalFilesOnly standing authorizations compose per critical file
- **#13056**: worktree.sh reuses a weeks-old local branch/worktree for an issue instead of starting from origin/main
- **#13065**: Consolidate ~10 hand-rolled atomic-write helpers into fs_util::write_atomic
- **#13085**: Consolidate 20 copy-pasted git() test helpers into loom-daemon/tests/common
- **#13088**: Workers with no git identity commit as user@hostname; DCO repos then cycle Judge/Doctor before reaching the operator
- **#13096**: Champion: let a Loom resync pass the kept-set hold when every kept-set file byte-matches the upstream tag
- **#13099**: Epic revisit: a periodic role pass that expands starved epics (or escalates them) so the fleet doesn't run dry with open epics
- **#13102**: Champion: opt-in additive-migrations exception to the migrations kept-set (CREATE IF NOT EXISTS / ADD COLUMN / INSERT OR IGNORE backfill)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#12287**: merge-pr: delete the check wait, branch self-sync and mergeable pre-read (#12063 slice 2)
- **#12466**: feat: reap idle worktrees of open issues and PRs
- **#12631**: fix: release a Champion CI wait as requeued so it holds flat
- **#12637**: refactor: split status_render inline tests into per-module files
- **#12639**: test: create registered roots before registering their gh config dir
- **#12960**: feat: let Champion self-release merge-risk holds above a confidence threshold
- **#13013**: fix: ignore heartbeat steps for ended assignments in the fleet captain
- **#13028**: feat: make Champion merges per repo a runtime tunable
- **#13034**: feat: export merge-lane waits and Champion run outcomes to OTLP
- **#13103**: refactor: route five remaining atomic-write copies through fs_util::write_atomic

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#11803**: fix(daemon-stop): record operator stops instead of deleting intent; leave the watchdog armed
- **#12262**: fix: track cd in subshells, brace groups and pushd in write-confinement guard
- **#12368**: feat(telemetry): record Judge verdicts and found-work from the real write
- **#12375**: chore: delete the telemetry-replay CLI, SigNoz replay queries and CI jobs
- **#12442**: feat: restart the daemon onto the same binary for restart-required config
- **#12522**: fix: shed reads on failed roster load, withdraw evidence-free 404s briefly
- **#12969**: feat: age-bound Codex rollout history in live profiles
- **#12997**: feat: let several criticalFilesOnly standing classes jointly cover a PR's critical files
- **#13011**: fix: keep holds with no recorded auth digest at their head; export tick exclusions
- **#13012**: feat: carry queue level, workspace_weight and seen over OTLP
- **#13043**: fix: compose criticalFilesOnly standing authorizations per critical-file hit
- **#13063**: feat: report and reset stale local branches reused by worktree.sh
- **#13086**: refactor(tests): share one hermetic git helper across worktree differential tests
- **#13090**: fix: refuse to commit under a hostname-fallback git identity
- **#13097**: feat(champion): pass the kept-set hold for byte-matching resync PRs

## Proposed

Issues carrying `loom:curated`.

- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
- **#8525**: tracing: instrument sweep phases and role attempts, including Pi/OpenCode and repair cycles *(curated)*
- **#8570**: Guard: refuse a build/scratch dir assignment that resolves onto a tmpfs mount (upstream PR to rjwalters/repo, split from #8512) *(curated)*
- **#8698**: Live end-to-end verification of the credential egress proxy (AC5 of #8674) *(curated)*
- **#8913**: observability: live-verify ci.job.log reconstruction in SigNoz and record it in evidence.md (#8825 AC1) *(curated)*
- **#8950**: Live-verify Pi tokens_by_model against a real LOOM_RUNTIME=pi launch *(curated)*
- **#9006**: observability: live-verify the SigNoz queue-starvation alert rule fires and resolves (#8856 / PR #8935 follow-up) *(curated)*
- **#9081**: guard-background-subagents misses a background Bash started by a foreground subagent: headless sweep exits, Builder's make verify reaped *(curated)*
- **#9136**: main ruleset: bypass_actors 'RepositoryRole:always' makes pull_request and required_status_checks advisory *(curated)*
- **#9388**: Doctor rechecks the head SHA before pushing but not the labels before writing them *(curated)*
- **#9508**: forge polling: ADR-0021 amendment step 3 — cross-host dedup (one host polls a repo, the others reuse the answer) *(curated)*
- **#9736**: health: detect stalled release updates without a source checkout *(curated)*
- **#9957**: daemon: deliberate stops (SIGTERM) are silent autonomy losses — the watchdog goes down with it, the intent marker is deleted, and bootstrap cannot recover *(curated)*
- **#9978**: feat(champion): optional per-repo merge-queue mode — enqueue Judge-approved PRs to GitHub's merge queue instead of merge-pr.sh's freshness/re-date loop *(curated)*
- **#10083**: CI path filter skips label_registry Rust tests on label-file-only PRs (how #10072 turned main red) *(curated)*
- **#10181**: ci: backend paths filter omits defaults/forge/**, so forge_inventory tests skip on manifest/baseline-only PRs *(curated)*
- **#10257**: merge queue Phase C: combined-tree CI qualification and eligible-repo live pilot (parent #9978) *(curated)*
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day) *(curated)*
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue *(curated)*
- **#10456**: Epic: the fleet keeps its own hold queue clean — only real operator asks reach Needs attention *(curated)*
- **#10512**: Fleet reader-App pool near exhaustion: cut top billable pollers (visibility.repo cache, issue_state ETag, cross-host dedupe) — ~8k REST/h *(curated)*
- **#10698**: Fleet version floor: the daemon enforces `loom_min_version` from the fleet store, rolls itself forward, and brings each repo's installed Loom up to its own version *(curated)*
- **#10802**: General cleanup: reap agent process residue (dev servers, orphaned children) after any agent exit *(curated)*
- **#10838**: merge_pr_reconcile_differential: claim-gate differential disagrees on case-variant duplicate label (loom:buildinG) *(curated)*
- **#10941**: No-PR retry hold fires when the only remaining work is a live check or a cross-repo PR *(curated)*
- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default) *(curated)*
- **#11112**: Remove the safehouse (Matrix) integration from Loom: narration, ChatOps/Concierge, peer-claim channel (forge leases remain) *(curated)*
- **#11118**: Umbrella: claim and lease state is inferred from label age and presence — live work gets orphaned, stripped or doubled (13 bugs) *(curated)*
- **#11283**: OpenCode 2.x guarded launches: load the native-tools plugin via the 2.x default-export shape, live-verify, flip guard_verified *(curated)*
- **#11284**: Runtime readiness: per-repo, per-stage weighted model selection in fleet-gitops *(curated)*
- **#11287**: Umbrella: Z.ai GLM-5.3-Flash seats landing code in the fleet via OpenCode *(curated)*
- **#11300**: Z.ai seat telemetry via the egress proxy in transparent observe mode: per-request tokens/latency/429s to SigNoz, OpenCode headers untouched *(curated)*
- **#11301**: check-stale-blocked: the per-repo budget probe spends ~1.4k GraphQL points/h on the writer bucket that runs dry *(curated)*
- **#11595**: Pause-and-roll budget path can still kill an in-flight merge-pr.sh (follow-up to #11263) *(curated)*
- **#11703**: Config precedence: fleet-gitops values must override repo-committed .loom/config.json (general rule + enforcement) *(curated)*
- **#11712**: Daemon: export OTLP only and remove inbound control channels other than config (2am#4300) *(curated)*
- **#11787**: issue.estimate coverage gap: operator-session curation sets loom:curated by hand and bypasses curation-complete (loom-ui: 58 of 62 curations since the gate have no estimate) *(curated)*
- **#11911**: Finish retiring daemon-spawned /loom:sweep and merge the two concurrency budgets *(curated)*
- **#11928**: maintain_only dispatch aborts emit error-level telemetry for an intentional, configured refusal *(curated)*
- **#12014**: ci: approved PRs touching workflow files need several CI rounds to land as main moves (no merge queue on a user-owned repo) *(curated)*
- **#12029**: Telemetry: merge runs record PR number and observed result (merge-pr chokepoint) — Judge parts moved to #12362; held behind #12063 *(curated)*
- **#12082**: Worktree-isolation guard blocks Bash writes to paths outside the repo when the cwd is the main checkout *(curated)*
- **#12085**: Telemetry(doctor): structured parked-PR cause (park reason, doctor_cycles, queue gate, treating age) (ETA doctor, loom-ui#4063 #6) *(curated)*
- **#12094**: Retire the telemetry replay subsystem and the kinds left without a producer by #11301 *(curated)*
- **#12127**: ci: rebalance Native Port Suites legs so none runs over ~3 min (next PR critical path after #12089) *(curated)*
- **#12186**: Issue-phase claim_state from the lease: delete label-age inference (slice of #11118) *(curated)*
- **#12220**: Rate-limit probe cache follow-ups: quiet-bucket gauge gap, ambient-login docs, post-rollout measurement *(curated)*
- **#12248**: Retired hold labels: remove core registry/work_finder readers (1/3 of #11843) *(curated)*
- **#12304**: Epic: fleet discovery from one mirror read per tick — zero forge reads for discovery; delete per-host listing paths *(curated)*
- **#12311**: Discovery interface: mirror backend (one worklist read per tick, in-memory 304 cache), the config-chosen tier chain, and the step gather reading one snapshot *(curated)*
- **#12313**: `queue_blocked` reads the tick snapshot; delete its own listing and the captain queue-blocked facts on the mirror path *(curated)*
- **#12314**: Quarantine reconciliation reads the discovery snapshot; delete its `loom:blocked` listing *(curated)*
- **#12315**: Claim reconciliation's open-PR discovery listing reads the snapshot; its write-gating reads stay live *(curated)*
- **#12317**: Step gather's per-item reads: CI at Judge heads and stage-entry times from the snapshot; classify the landed closes-graph and hold-marker reads *(curated)*
- **#12318**: Claim refusals report back: `POST /v1/mirror/recheck` when the live claim read disagrees with the drawn snapshot *(curated)*
- **#12319**: Wakes: a change in a served repo ticks the step tick early (trigger attribute, one worklist read per wake), gated on mirror health *(curated)*
- **#12320**: Cleanup once every worker reads the worklist: delete the per-call `forge_mirror` discovery path, `maxStalenessSecs` and `per_call`; docs and ADR-0024; direct backend preserved *(curated)*
- **#12344**: Flow-change evidence review (#11708 slice B): verdict/merge gates and merge sequencing *(curated)*
- **#12362**: Telemetry: Judge runs record found-work, PR, head_sha and the real verdict; drop the synthetic loom.judge_verdict (split from #12029, no merge-pr changes) *(curated)*
- **#12408**: Live-verify epic child discovery against sky130-sar-adc#23 (follow-up to #12388) *(curated)*
- **#12418**: Verify epic #10698 acceptance: completed-vs-not-planned dispositions and live fleet receipts *(curated)*
- **#12425**: auto-update: arm pause-and-roll from the config-restart demand (#12419 child 3) *(curated)*
- **#12447**: Epic: captain-assigned dispatch — workers ask the fleet captain for their next assignment from one fleet queue *(curated)*
- **#12458**: Reap idle clean worktrees of open issues/PRs (configurable idle period) *(curated)*
- **#12481**: Roll-stopped role ticks export unclassified:pre-launch (#12028 follow-up) *(curated)*
- **#12511**: Readers: a failed roster load still routes reads to the writer; evidence-free 404 withdrawal is an hour (follow-up to #12412) *(curated)*
- **#12551**: fleet_dispatch no-progress hold follow-ups: Doctor outcome line, Judge ci_pending, CI-green release, metric attribution *(curated)*
- **#12604**: Split status_render.rs: break up ~970-line print_status_human and move inline tests out *(curated)*
- **#12638**: Test: lease_refresh_child_carries_the_root_owners_gh_config_dir fails on main (child sees no GH_CONFIG_DIR) *(curated)*
- **#12703**: Add a per-repo setting to turn off Champion's out-of-band acceptance-criteria hold (#6883) *(curated)*
- **#12713**: Part-of merges re-queue the issue to Builders: landed-route marker is read as a Curator 'kept' decision *(curated)*
- **#12775**: Simplify merge ordering: retire automatic soft holds while preserving explicit dependencies *(curated)*
- **#12809**: Remove the daemon's dashboard wake source (forge_events /v1/hosts/{host}/events client) *(curated)*
- **#12863**: Review dated guard-confinement evidence and owner-specific cases from gf180-opamp #214 *(curated)*
- **#12950**: Champion: self-release merge-risk holds at ≥90% confidence (operator direction 2026-10-20) *(curated)*
- **#12966**: Bound Codex session/thread history inside live profiles (the real codex-profiles growth) + uv cache *(curated)*
- **#12994**: Champion: let the union of eligible criticalFilesOnly classes cover a PR's critical files *(curated)*
- **#13007**: Carry the remaining queue-plan fields (urgent, operator_priority, tier, workspace_weight, shard, listing_failed, seen) over OTLP for the dashboard's SigNoz Live board *(curated)*
- **#13008**: Merge-risk holds are redrawn at their held head again: #12933 draws holds with no auth= digest, and only critical-file holds record one *(curated)*
- **#13009**: Captain champion_merge slot stays held ~20 min after a successful /v1/release (holder outlives its release until lease expiry) *(curated)*
- **#13025**: [ci-daily] Session and Private-Workspace Docker Fixtures (unfiltered) *(curated)*
- **#13026**: Champion: make per-repo merge concurrency a runtime tunable, and take hold re-reads out of the merge lane *(curated)*
- **#13029**: Telemetry: merge-lane waits, Champion re-read outcomes, fleet_dispatch call outcomes, held-PR gauge (gaps found 2026-10-20/21) *(curated)*
- **#13042**: Champion: let criticalFilesOnly standing authorizations compose per critical file *(curated)*
- **#13056**: worktree.sh reuses a weeks-old local branch/worktree for an issue instead of starting from origin/main *(curated)*
- **#13065**: Consolidate ~10 hand-rolled atomic-write helpers into fs_util::write_atomic *(curated)*
- **#13088**: Workers with no git identity commit as user@hostname; DCO repos then cycle Judge/Doctor before reaching the operator *(curated)*
- **#13096**: Champion: let a Loom resync pass the kept-set hold when every kept-set file byte-matches the upstream tag *(curated)*
- **#13099**: Epic revisit: a periodic role pass that expands starved epics (or escalates them) so the fleet doesn't run dry with open epics *(curated)*
- **#13102**: Champion: opt-in additive-migrations exception to the migrations kept-set (CREATE IF NOT EXISTS / ADD COLUMN / INSERT OR IGNORE backfill) *(curated)*

## Proposed (Architect / Hermit)

- **#13071**: Peer claims: move machine coordination off rendered room messages (custom event type / state events) *(architect)*
- **#12589**: Archive older WORK_PLAN and WORK_LOG content through shared Loom maintenance *(hermit)*
- **#12752**: Consolidate duplicate stats query stacks in activity/stats.rs *(hermit)*
- **#13017**: Split run_daemon (~1960-line fn) in daemon_service.rs into startup-phase modules *(hermit)*
- **#13100**: Quality-metrics capture writes input_id=0 so stats joins never match *(hermit)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#7810**: [epic] Retire shell: 10 sequential PRs, two foundational then update + agent execution
- **#9978**: feat(champion): optional per-repo merge-queue mode — enqueue Judge-approved PRs to GitHub's merge queue instead of merge-pr.sh's freshness/re-date loop
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day)
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue
- **#10456**: Epic: the fleet keeps its own hold queue clean — only real operator asks reach Needs attention
- **#10698**: Fleet version floor: the daemon enforces `loom_min_version` from the fleet store, rolls itself forward, and brings each repo's installed Loom up to its own version
- **#11287**: Umbrella: Z.ai GLM-5.3-Flash seats landing code in the fleet via OpenCode
- **#12304**: Epic: fleet discovery from one mirror read per tick — zero forge reads for discovery; delete per-host listing paths
- **#12708**: Epic: loom-tools CLI for agents, shared Rust implementation with the daemon

## Backlog Balance

| Bucket | Count |
|------|-------|
| Operator merge-risk holds | 9 |
| Very important | 1 |
| Important | 20 |
| Ready (`loom:issue`) | 8 |
| In Progress (`loom:building`) | 33 |
| PRs awaiting review | 10 |
| Approved PRs awaiting merge | 15 |
| Curated | 90 |
| Architect / Hermit proposals | 5 |
| Active epics | 10 |
<!-- guide:plan-body:end -->
