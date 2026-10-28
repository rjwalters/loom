# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs under a hold (`loom:blocked` or `loom:operator`) — only a human merge decision is missing.

- **#12287**: merge-pr: delete the check wait, branch self-sync and mergeable pre-read (#12063 slice 2)
- **#12368**: feat(telemetry): record Judge verdicts and found-work from the real write
- **#12375**: chore: delete the telemetry-replay CLI, SigNoz replay queries and CI jobs
- **#12960**: feat: let Champion self-release merge-risk holds above a confidence threshold
- **#12969**: feat: age-bound Codex rollout history in live profiles
- **#13113**: feat(merge-pr): port the stale-mergeable recheck loop to Rust (#8191 slice)
- **#13137**: fix: accept trailing-newline issue-body hash in verdict gate
- **#13139**: fix: verdict-gate ignores the Judge's own notes and accepts --acknowledged-comment
- **#13143**: docs: state that judge wait-checks --timeout is in seconds
- **#13144**: fix: add loom marker to judge-ci-red flake note template
- **#13147**: feat: add --repo to forge verdict-equivalent and name forge-read failures
- **#13160**: refactor: remove worktree-return.sh and worktree.sh --return-to
- **#13188**: fix: hold ratification requests under ratification/** in the champion ratification hold
- **#13194**: fix: map installed roles and skills to defaults counterparts in the resync waiver
- **#13208**: build(deps): Bump the github-actions group across 1 directory with 6 updates

## Very Important

`loom:very-important` (one counts per workspace); every stage takes these first.

- **#12203**: stage_dwell: every host re-lists every repo every 5 min (~8.9k billable GitHub calls/h fleet-wide)
- **#12653**: Emit merge_hold hold-cause + release-actor + merge-queue facts to SigNoz (unblocks merge_hold ETA prediction)

## Important

`loom:important`; taken before unlabelled work.

- **#10257**: merge queue Phase C: combined-tree CI qualification and eligible-repo live pilot (parent #9978)
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue
- **#10890**: Daemon dispatches loom:issue + loom:needs-capability items; pre-flight skip then reads as insta-crash and quarantine release re-queues them
- **#11301**: check-stale-blocked: the per-repo budget probe spends ~1.4k GraphQL points/h on the writer bucket that runs dry
- **#12085**: Telemetry(doctor): structured parked-PR cause (park reason, doctor_cycles, queue gate, treating age) (ETA doctor, loom-ui#4063 #6)
- **#12217**: github-events mirror reads stale for quiet repos (1555 stale vs 1049 hits/h), so the step gather falls back to REST
- **#12323**: merge queue: ship queue execution for the 2AMLogic/loom-ui pilot (Phase C, #10257)
- **#12447**: Epic: captain-assigned dispatch — workers ask the fleet captain for their next assignment from one fleet queue
- **#12506**: merge sequencing: Phase 1 release skips some followers whose predecessor merged at the recorded head, and fail-closed holds emit no telemetry
- **#12551**: fleet_dispatch no-progress hold follow-ups: Doctor outcome line, Judge ci_pending, CI-green release, metric attribution
- **#12909**: ci_telemetry: 2amlogic owner discovery times out every cycle since Oct 17 (no loom-ui CI facts; 478 consecutive failures)
- **#12950**: Champion: self-release merge-risk holds at ≥90% confidence (operator direction 2026-10-20)
- **#12993**: Shadow evaluation of the new Champion (2026-10-20): 1/95 holds cleared — ranked remaining causes + fixtures

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8525**: tracing: instrument sweep phases and role attempts, including Pi/OpenCode and repair cycles
- **#10890**: Daemon dispatches loom:issue + loom:needs-capability items; pre-flight skip then reads as insta-crash and quarantine release re-queues them
- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default)
- **#11301**: check-stale-blocked: the per-repo budget probe spends ~1.4k GraphQL points/h on the writer bucket that runs dry
- **#12117**: Telemetry: inventory every kind; fix or delete role_tick.outcome, ci.job.log, session.output
- **#12167**: telemetry: export peer_coordination verdict and degraded duration on host.health
- **#12447**: Epic: captain-assigned dispatch — workers ask the fleet captain for their next assignment from one fleet queue
- **#12506**: merge sequencing: Phase 1 release skips some followers whose predecessor merged at the recorded head, and fail-closed holds emit no telemetry
- **#12551**: fleet_dispatch no-progress hold follow-ups: Doctor outcome line, Judge ci_pending, CI-green release, metric attribution
- **#12638**: Test: lease_refresh_child_carries_the_root_owners_gh_config_dir fails on main (child sees no GH_CONFIG_DIR)
- **#12653**: Emit merge_hold hold-cause + release-actor + merge-queue facts to SigNoz (unblocks merge_hold ETA prediction)

## In Progress

Issues currently being built (`loom:building`).

- **#12094**: Retire the telemetry replay subsystem and the kinds left without a producer by #11301
- **#12362**: Telemetry: Judge runs record found-work, PR, head_sha and the real verdict; drop the synthetic loom.judge_verdict (split from #12029, no merge-pr changes)
- **#12604**: Split status_render.rs: break up ~970-line print_status_human and move inline tests out
- **#12950**: Champion: self-release merge-risk holds at ≥90% confidence (operator direction 2026-10-20)
- **#13118**: Remove worktree-return.sh and --return-to: cd in a child process cannot return the caller
- **#13134**: post-verdict stale-read gate counts the reviewing Judge's own comments and spec-neutral same-account rulings as newer human feedback
- **#13136**: Judge role: the issue-body hash for post-verdict is easy to compute wrong (trailing newline), giving a spurious 'issue body changed' refusal
- **#13141**: judge.md: wait-checks --timeout examples don't say the unit is seconds
- **#13146**: verdict-equivalent: 'could not read PR's base branch' re-arms human-released holds after every Doctor merge (5 PRs, 10-23/24); failure reason not in telemetry
- **#13244**: Work finder draws gap-to-T1 tracker issues as work: lease-comment loops (one tracker hit GitHub's 2,500-comment cap)
- **#13245**: Bound lease/outcome comment volume per issue (GitHub 2,500-comment cap)
- **#13246**: Auto-unpin failure counter is never reset on a successful spawn (not strictly consecutive)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#12631**: fix: release a Champion CI wait as requeued so it holds flat
- **#12637**: refactor: split status_render inline tests into per-module files
- **#12639**: test: create registered roots before registering their gh config dir
- **#13249**: fix: bound lease and builder-outcome comment growth per issue
- **#13250**: docs: correct auto-unpin counter wording to match actual reset points

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#12287**: merge-pr: delete the check wait, branch self-sync and mergeable pre-read (#12063 slice 2)
- **#12368**: feat(telemetry): record Judge verdicts and found-work from the real write
- **#12375**: chore: delete the telemetry-replay CLI, SigNoz replay queries and CI jobs
- **#12960**: feat: let Champion self-release merge-risk holds above a confidence threshold
- **#12969**: feat: age-bound Codex rollout history in live profiles
- **#13113**: feat(merge-pr): port the stale-mergeable recheck loop to Rust (#8191 slice)
- **#13137**: fix: accept trailing-newline issue-body hash in verdict gate
- **#13139**: fix: verdict-gate ignores the Judge's own notes and accepts --acknowledged-comment
- **#13143**: docs: state that judge wait-checks --timeout is in seconds
- **#13144**: fix: add loom marker to judge-ci-red flake note template
- **#13147**: feat: add --repo to forge verdict-equivalent and name forge-read failures
- **#13160**: refactor: remove worktree-return.sh and worktree.sh --return-to
- **#13188**: fix: hold ratification requests under ratification/** in the champion ratification hold
- **#13194**: fix: map installed roles and skills to defaults counterparts in the resync waiver
- **#13208**: build(deps): Bump the github-actions group across 1 directory with 6 updates
- **#13247**: feat: pass <!-- loom:tracker --> issues silently in the step classifier

## Proposed

Issues carrying `loom:curated`.

- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
- **#8525**: tracing: instrument sweep phases and role attempts, including Pi/OpenCode and repair cycles *(curated)*
- **#8698**: Live end-to-end verification of the credential egress proxy (AC5 of #8674) *(curated)*
- **#8913**: observability: live-verify ci.job.log reconstruction in SigNoz and record it in evidence.md (#8825 AC1) *(curated)*
- **#8950**: Live-verify Pi tokens_by_model against a real LOOM_RUNTIME=pi launch *(curated)*
- **#9006**: observability: live-verify the SigNoz queue-starvation alert rule fires and resolves (#8856 / PR #8935 follow-up) *(curated)*
- **#9136**: main ruleset: bypass_actors 'RepositoryRole:always' makes pull_request and required_status_checks advisory *(curated)*
- **#9388**: Doctor rechecks the head SHA before pushing but not the labels before writing them *(curated)*
- **#9978**: feat(champion): optional per-repo merge-queue mode — enqueue Judge-approved PRs to GitHub's merge queue instead of merge-pr.sh's freshness/re-date loop *(curated)*
- **#10257**: merge queue Phase C: combined-tree CI qualification and eligible-repo live pilot (parent #9978) *(curated)*
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day) *(curated)*
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue *(curated)*
- **#10456**: Epic: the fleet keeps its own hold queue clean — only real operator asks reach Needs attention *(curated)*
- **#10512**: Fleet reader-App pool near exhaustion: cut top billable pollers (visibility.repo cache, issue_state ETag, cross-host dedupe) — ~8k REST/h *(curated)*
- **#10698**: Fleet version floor: the daemon enforces `loom_min_version` from the fleet store, rolls itself forward, and brings each repo's installed Loom up to its own version *(curated)*
- **#10755**: gh telemetry: record 304 Not Modified as success (not error), stop writing it to the failures log *(curated)*
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
- **#11911**: Finish retiring daemon-spawned /loom:sweep and merge the two concurrency budgets *(curated)*
- **#11928**: maintain_only dispatch aborts emit error-level telemetry for an intentional, configured refusal *(curated)*
- **#12014**: ci: approved PRs touching workflow files need several CI rounds to land as main moves (no merge queue on a user-owned repo) *(curated)*
- **#12029**: Telemetry: merge runs record PR number and observed result (merge-pr chokepoint) — Judge parts moved to #12362; held behind #12063 *(curated)*
- **#12085**: Telemetry(doctor): structured parked-PR cause (park reason, doctor_cycles, queue gate, treating age) (ETA doctor, loom-ui#4063 #6) *(curated)*
- **#12094**: Retire the telemetry replay subsystem and the kinds left without a producer by #11301 *(curated)*
- **#12127**: ci: rebalance Native Port Suites legs so none runs over ~3 min (next PR critical path after #12089) *(curated)*
- **#12220**: Rate-limit probe cache follow-ups: quiet-bucket gauge gap, ambient-login docs, post-rollout measurement *(curated)*
- **#12248**: Retired hold labels: remove core registry/work_finder readers (1/3 of #11843) *(curated)*
- **#12304**: Epic: fleet discovery from one mirror read per tick — zero forge reads for discovery; delete per-host listing paths *(curated)*
- **#12313**: `queue_blocked` reads the tick snapshot; delete its own listing and the captain queue-blocked facts on the mirror path *(curated)*
- **#12314**: Quarantine reconciliation reads the discovery snapshot; delete its `loom:blocked` listing *(curated)*
- **#12315**: Claim reconciliation's open-PR discovery listing reads the snapshot; its write-gating reads stay live *(curated)*
- **#12318**: Claim refusals report back: `POST /v1/mirror/recheck` when the live claim read disagrees with the drawn snapshot *(curated)*
- **#12319**: Wakes: a change in a served repo ticks the step tick early (trigger attribute, one worklist read per wake), gated on mirror health *(curated)*
- **#12320**: Cleanup once every worker reads the worklist: delete the per-call `forge_mirror` discovery path, `maxStalenessSecs` and `per_call`; docs and ADR-0024; direct backend preserved *(curated)*
- **#12362**: Telemetry: Judge runs record found-work, PR, head_sha and the real verdict; drop the synthetic loom.judge_verdict (split from #12029, no merge-pr changes) *(curated)*
- **#12408**: Live-verify epic child discovery against sky130-sar-adc#23 (follow-up to #12388) *(curated)*
- **#12418**: Verify epic #10698 acceptance: completed-vs-not-planned dispositions and live fleet receipts *(curated)*
- **#12447**: Epic: captain-assigned dispatch — workers ask the fleet captain for their next assignment from one fleet queue *(curated)*
- **#12481**: Roll-stopped role ticks export unclassified:pre-launch (#12028 follow-up) *(curated)*
- **#12551**: fleet_dispatch no-progress hold follow-ups: Doctor outcome line, Judge ci_pending, CI-green release, metric attribution *(curated)*
- **#12604**: Split status_render.rs: break up ~970-line print_status_human and move inline tests out *(curated)*
- **#12638**: Test: lease_refresh_child_carries_the_root_owners_gh_config_dir fails on main (child sees no GH_CONFIG_DIR) *(curated)*
- **#12703**: Add a per-repo setting to turn off Champion's out-of-band acceptance-criteria hold (#6883) *(curated)*
- **#12713**: Part-of merges re-queue the issue to Builders: landed-route marker is read as a Curator 'kept' decision *(curated)*
- **#12809**: Remove the daemon's dashboard wake source (forge_events /v1/hosts/{host}/events client) *(curated)*
- **#12950**: Champion: self-release merge-risk holds at ≥90% confidence (operator direction 2026-10-20) *(curated)*
- **#12966**: Bound Codex session/thread history inside live profiles (the real codex-profiles growth) + uv cache *(curated)*
- **#12978**: Codex shared-seat dispatch: live receipts and startup measurements (split from #10796) *(curated)*
- **#13025**: [ci-daily] Session and Private-Workspace Docker Fixtures (unfiltered) *(curated)*
- **#13134**: post-verdict stale-read gate counts the reviewing Judge's own comments and spec-neutral same-account rulings as newer human feedback *(curated)*
- **#13136**: Judge role: the issue-body hash for post-verdict is easy to compute wrong (trailing newline), giving a spurious 'issue body changed' refusal *(curated)*
- **#13140**: judge-ci-red flake note has no Loom marker, so it trips post-verdict.sh's stale-read gate on the Judge's own verdict *(curated)*
- **#13141**: judge.md: wait-checks --timeout examples don't say the unit is seconds *(curated)*
- **#13146**: verdict-equivalent: 'could not read PR's base branch' re-arms human-released holds after every Doctor merge (5 PRs, 10-23/24); failure reason not in telemetry *(curated)*
- **#13155**: [ci-daily] Flake Detection (shell suites, macos-latest, parallelism 1) *(curated)*
- **#13166**: write-scope: a repo created after daemon start is still refused 'not-in-installation' 19h later on 0.19.1221 (the #13153 refresh does not clear it) *(curated)*
- **#13169**: tracing: put the work item's keys (pr_number, role, head/base SHA) on every step and tick span at open *(curated)*
- **#13170**: agent gh front: one span per GitHub write (operation, repo, number, head SHA) *(curated)*
- **#13171**: tracing: daemon 'invoke github' calls outside a step have no sweep_id or role *(curated)*
- **#13173**: docs: tracing.md and ci-observability.md still describe the story join removed in #11984 *(curated)*
- **#13175**: role_tick telemetry: loom.actions.prs_merged overcounts ~10x (whole-transcript rescans + counts merge attempts) *(curated)*
- **#13176**: Builder: 16/26 sweeps on fleet-1 end no-outcome-line (exit 0, no outcome) — main throughput limit *(curated)*
- **#13185**: Champion ratification hold misses ratification/requests/** — a key-review request PR auto-merged with no key reviews *(curated)*
- **#13193**: Champion resync waiver never eligible: resync_upstream_path lacks .agents/skills and .loom/roles *(curated)*
- **#13209**: [ci-daily] Supply Chain *(curated)*
- **#13238**: [ci-daily] Flake Detection (Rust) *(curated)*
- **#13244**: Work finder draws gap-to-T1 tracker issues as work: lease-comment loops (one tracker hit GitHub's 2,500-comment cap) *(curated)*
- **#13245**: Bound lease/outcome comment volume per issue (GitHub 2,500-comment cap) *(curated)*
- **#13246**: Auto-unpin failure counter is never reset on a successful spawn (not strictly consecutive) *(curated)*

## Proposed (Architect / Hermit)

- **#13071**: Peer claims: move machine coordination off rendered room messages (custom event type / state events) *(architect)*
- **#12752**: Consolidate duplicate stats query stacks in activity/stats.rs *(hermit)*
- **#13017**: Split run_daemon (~1960-line fn) in daemon_service.rs into startup-phase modules *(hermit)*
- **#13118**: Remove worktree-return.sh and --return-to: cd in a child process cannot return the caller *(hermit)*

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
| Operator merge-risk holds | 15 |
| Very important | 2 |
| Important | 13 |
| Ready (`loom:issue`) | 12 |
| In Progress (`loom:building`) | 12 |
| PRs awaiting review | 5 |
| Approved PRs awaiting merge | 16 |
| Curated | 81 |
| Architect / Hermit proposals | 4 |
| Active epics | 10 |
<!-- guide:plan-body:end -->
