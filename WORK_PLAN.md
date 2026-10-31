# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs under a hold (`loom:blocked` or `loom:operator`) — only a human merge decision is missing.

- **#12368**: feat(telemetry): record Judge verdicts and found-work from the real write
- **#12969**: feat: age-bound Codex rollout history in live profiles
- **#13137**: fix: accept trailing-newline issue-body hash in verdict gate
- **#13139**: fix: verdict-gate ignores the Judge's own notes and accepts --acknowledged-comment
- **#13143**: docs: state that judge wait-checks --timeout is in seconds
- **#13160**: refactor: remove worktree-return.sh and worktree.sh --return-to
- **#13293**: fix: let a bare permissions header read green on workflow blast radius
- **#13297**: fix: resolve repo explicitly and surface gh errors in suspect-critical-diff
- **#13304**: fix: pass --repo to forge verdict-equivalent from guard and Champion hold
- **#13305**: feat: export unreadable PR base branch as a closed hold.transition trigger
- **#13325**: feat: hand operator-released critical-file PRs to the merge queue
- **#13328**: test: pin resync waiver to hold roles and skills copies
- **#13339**: fix: supply canonical guard to ci-daily flake-shell; portable sed in waiver suite
- **#13340**: fix: trust github-actions[bot] as a promotion author for CI trackers
- **#13344**: ci: move macOS runners from macos-14 to macos-15
- **#13360**: fix(install): build daemon and probe pnpm before reinstall uninstall

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
- **#12318**: Claim refusals report back: `POST /v1/mirror/recheck` when the live claim read disagrees with the drawn snapshot
- **#12604**: Split status_render.rs: break up ~970-line print_status_human and move inline tests out
- **#12950**: Champion: self-release merge-risk holds at ≥90% confidence (operator direction 2026-10-20)
- **#13118**: Remove worktree-return.sh and --return-to: cd in a child process cannot return the caller
- **#13134**: post-verdict stale-read gate counts the reviewing Judge's own comments and spec-neutral same-account rulings as newer human feedback
- **#13136**: Judge role: the issue-body hash for post-verdict is easy to compute wrong (trailing newline), giving a spurious 'issue body changed' refusal
- **#13149**: Export forge verdict-equivalent failure reasons as SigNoz span attributes
- **#13150**: Pass --repo through verdict-staleness-guard.sh and Champion to forge verdict-equivalent
- **#13289**: Follow-on from PR #13288: verify completion.narrated tokens in SigNoz on a fleet host
- **#13291**: Champion: a read-only permissions block in a workflow should not read red on blast radius
- **#13296**: suspect-critical-diff fails closed with "could not read the PR's file list" on script-only package.json edits
- **#13306**: write-scope: NOT_IN_INSTALLATION still refused for new repos on 0.19.1227 despite #13153 (installation is on All repositories)
- **#13313**: Resync waiver: .agents/skills mapping and judge/champion roles stay unreachable under the #12945 gate
- **#13321**: champion: on merge-queue repos, hand a released critical-file PR to the queue instead of leaving the merge to the operator
- **#13324**: [ci-daily] Flake Detection (shell suites, ubuntu-latest, parallelism 8)
- **#13330**: sweep: park-for-operator silently fails when loom:operator label doesn't exist on the forge
- **#13331**: Cleared-blocker notice doesn't unpark: issue stays loom:blocked for hours after its sole blocker merges
- **#13332**: merge-pr stacked reconcile retargets a non-feature/issue-N child to main without stripping squashed parent commits (left CONFLICTING)
- **#13337**: Workflow-filed trackers never enter the fleet: the author gate refuses github-actions[bot] and ci-daily.yml rewrites the body nightly
- **#13343**: CI: macos-check fails during macOS 14 runner brownout; image retires 2026-11-02
- **#13345**: Ratification lane: wire the lane into the tick and post per-key RATIFY markers as comments
- **#13346**: Consolidate seven duplicated feature/issue-N branch parsers in loom-daemon
- **#13350**: simplify: remove the per-dispatch runtime/modelProfile override (#11493): zero uses in 7 days
- **#13356**: install --confirm-reinstall: daemon/toolchain failure after the uninstall leaves the target partially uninstalled
- **#13357**: merge queue: enqueue intermittently refused with "No merge queue found for branch" while the ruleset is active; the handoff rolls back and later succeeds
- **#13358**: merge queue: a failed_checks drop whose only failed jobs were runner cancellations is classified as a check failure and routed to Doctor
- **#13359**: champion: critical-file hold re-armed on a tree-equivalent Doctor rebase when the forge compare is unreadable (equivalence=unread); retry or local fallback before re-arming

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#12631**: fix: release a Champion CI wait as requeued so it holds flat
- **#12637**: refactor: split status_render inline tests into per-module files
- **#12639**: test: create registered roots before registering their gh config dir
- **#13338**: feat: unpark Curator-parked issues once their blockers resolve
- **#13341**: feat: warn on missing hold labels and hint sync-labels on failed park
- **#13348**: fix: re-mint the writer token when a miss refresh still lacks the repo
- **#13349**: feat: post per-key ratification markers and run the lane from the champion tick
- **#13351**: refactor: route feature/issue-N branch parsing through worktree_ops::naming
- **#13355**: refactor: remove unused per-dispatch runtime/modelProfile override
- **#13363**: fix: retry merge-queue enqueue once on QueueUnavailable and key stall notice on reason
- **#13364**: feat: fall back to local patch comparison when the forge compare is unreadable
- **#13365**: feat: name cancelled jobs and declining guard in merge-queue drop comments

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#12287**: merge-pr: delete the check wait, branch self-sync and mergeable pre-read (#12063 slice 2)
- **#12368**: feat(telemetry): record Judge verdicts and found-work from the real write
- **#12375**: chore: delete the telemetry-replay CLI, SigNoz replay queries and CI jobs
- **#12960**: feat: let Champion self-release merge-risk holds above a confidence threshold
- **#12969**: feat: age-bound Codex rollout history in live profiles
- **#13137**: fix: accept trailing-newline issue-body hash in verdict gate
- **#13139**: fix: verdict-gate ignores the Judge's own notes and accepts --acknowledged-comment
- **#13143**: docs: state that judge wait-checks --timeout is in seconds
- **#13160**: refactor: remove worktree-return.sh and worktree.sh --return-to
- **#13293**: fix: let a bare permissions header read green on workflow blast radius
- **#13297**: fix: resolve repo explicitly and surface gh errors in suspect-critical-diff
- **#13304**: fix: pass --repo to forge verdict-equivalent from guard and Champion hold
- **#13305**: feat: export unreadable PR base branch as a closed hold.transition trigger
- **#13325**: feat: hand operator-released critical-file PRs to the merge queue
- **#13328**: test: pin resync waiver to hold roles and skills copies
- **#13339**: fix: supply canonical guard to ci-daily flake-shell; portable sed in waiver suite
- **#13340**: fix: trust github-actions[bot] as a promotion author for CI trackers
- **#13344**: ci: move macOS runners from macos-14 to macos-15
- **#13360**: fix(install): build daemon and probe pnpm before reinstall uninstall

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
- **#12727**: Daemon-root checkout (~/loom-daemon) is never fast-forwarded on release-roll hosts (worker-1 2,764 / worker-2 5,135 commits behind) *(curated)*
- **#12809**: Remove the daemon's dashboard wake source (forge_events /v1/hosts/{host}/events client) *(curated)*
- **#12950**: Champion: self-release merge-risk holds at ≥90% confidence (operator direction 2026-10-20) *(curated)*
- **#12966**: Bound Codex session/thread history inside live profiles (the real codex-profiles growth) + uv cache *(curated)*
- **#12978**: Codex shared-seat dispatch: live receipts and startup measurements (split from #10796) *(curated)*
- **#13025**: [ci-daily] Session and Private-Workspace Docker Fixtures (unfiltered) *(curated)*
- **#13134**: post-verdict stale-read gate counts the reviewing Judge's own comments and spec-neutral same-account rulings as newer human feedback *(curated)*
- **#13136**: Judge role: the issue-body hash for post-verdict is easy to compute wrong (trailing newline), giving a spurious 'issue body changed' refusal *(curated)*
- **#13141**: judge.md: wait-checks --timeout examples don't say the unit is seconds *(curated)*
- **#13149**: Export forge verdict-equivalent failure reasons as SigNoz span attributes *(curated)*
- **#13150**: Pass --repo through verdict-staleness-guard.sh and Champion to forge verdict-equivalent *(curated)*
- **#13155**: [ci-daily] Flake Detection (shell suites, macos-latest, parallelism 1) *(curated)*
- **#13169**: tracing: put the work item's keys (pr_number, role, head/base SHA) on every step and tick span at open *(curated)*
- **#13170**: agent gh front: one span per GitHub write (operation, repo, number, head SHA) *(curated)*
- **#13171**: tracing: daemon 'invoke github' calls outside a step have no sweep_id or role *(curated)*
- **#13173**: docs: tracing.md and ci-observability.md still describe the story join removed in #11984 *(curated)*
- **#13175**: role_tick telemetry: loom.actions.prs_merged overcounts ~10x (whole-transcript rescans + counts merge attempts) *(curated)*
- **#13176**: Builder: 16/26 sweeps on fleet-1 end no-outcome-line (exit 0, no outcome) — main throughput limit *(curated)*
- **#13209**: [ci-daily] Supply Chain *(curated)*
- **#13289**: Follow-on from PR #13288: verify completion.narrated tokens in SigNoz on a fleet host *(curated)*
- **#13291**: Champion: a read-only permissions block in a workflow should not read red on blast radius *(curated)*
- **#13296**: suspect-critical-diff fails closed with "could not read the PR's file list" on script-only package.json edits *(curated)*
- **#13313**: Resync waiver: .agents/skills mapping and judge/champion roles stay unreachable under the #12945 gate *(curated)*
- **#13319**: accounts check --live leaves the shared Codex profile root at 0700 — fights chezmoi-managed mode on the operator Mac *(curated)*
- **#13321**: champion: on merge-queue repos, hand a released critical-file PR to the queue instead of leaving the merge to the operator *(curated)*
- **#13323**: [ci-daily] Flake Detection (shell suites, ubuntu-latest, parallelism 1) *(curated)*
- **#13330**: sweep: park-for-operator silently fails when loom:operator label doesn't exist on the forge *(curated)*
- **#13331**: Cleared-blocker notice doesn't unpark: issue stays loom:blocked for hours after its sole blocker merges *(curated)*
- **#13332**: merge-pr stacked reconcile retargets a non-feature/issue-N child to main without stripping squashed parent commits (left CONFLICTING) *(curated)*
- **#13337**: Workflow-filed trackers never enter the fleet: the author gate refuses github-actions[bot] and ci-daily.yml rewrites the body nightly *(curated)*
- **#13343**: CI: macos-check fails during macOS 14 runner brownout; image retires 2026-11-02 *(curated)*
- **#13345**: Ratification lane: wire the lane into the tick and post per-key RATIFY markers as comments *(curated)*
- **#13350**: simplify: remove the per-dispatch runtime/modelProfile override (#11493): zero uses in 7 days *(curated)*
- **#13356**: install --confirm-reinstall: daemon/toolchain failure after the uninstall leaves the target partially uninstalled *(curated)*
- **#13357**: merge queue: enqueue intermittently refused with "No merge queue found for branch" while the ruleset is active; the handoff rolls back and later succeeds *(curated)*
- **#13358**: merge queue: a failed_checks drop whose only failed jobs were runner cancellations is classified as a check failure and routed to Doctor *(curated)*
- **#13359**: champion: critical-file hold re-armed on a tree-equivalent Doctor rebase when the forge compare is unreadable (equivalence=unread); retry or local fallback before re-arming *(curated)*

## Proposed (Architect / Hermit)

- **#13071**: Peer claims: move machine coordination off rendered room messages (custom event type / state events) *(architect)*
- **#12752**: Consolidate duplicate stats query stacks in activity/stats.rs *(hermit)*
- **#13017**: Split run_daemon (~1960-line fn) in daemon_service.rs into startup-phase modules *(hermit)*
- **#13118**: Remove worktree-return.sh and --return-to: cd in a child process cannot return the caller *(hermit)*
- **#13261**: Consolidate ~23 duplicated private git subprocess wrappers in loom-daemon *(hermit)*
- **#13354**: Remove unused role-manifest fields: defaultInterval, defaultIntervalPrompt, autonomousRecommended *(hermit)*
- **#13362**: Drop the fleet.json-absent legacy fallback and retire fleet_store/yaml.rs (#10705 follow-up) *(hermit)*

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
| Operator merge-risk holds | 16 |
| Very important | 2 |
| Important | 13 |
| Ready (`loom:issue`) | 11 |
| In Progress (`loom:building`) | 28 |
| PRs awaiting review | 12 |
| Approved PRs awaiting merge | 19 |
| Curated | 91 |
| Architect / Hermit proposals | 7 |
| Active epics | 10 |
<!-- guide:plan-body:end -->
