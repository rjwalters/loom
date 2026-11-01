# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs under a hold (`loom:blocked` or `loom:operator`) — only a human merge decision is missing.

- **#12368**: feat(telemetry): record Judge verdicts and found-work from the real write
- **#12969**: feat: age-bound Codex rollout history in live profiles
- **#13297**: fix: resolve repo explicitly and surface gh errors in suspect-critical-diff
- **#13336**: fix: rebase children of non-feature/issue-N parents before retargeting
- **#13348**: fix: re-mint the writer token when a miss refresh still lacks the repo
- **#13363**: fix: retry merge-queue enqueue once on QueueUnavailable and key stall notice on reason
- **#13364**: feat: fall back to local patch comparison when the forge compare is unreadable
- **#13413**: feat: add champion-hold self-release-calibration verb

## Very Important

`loom:very-important` (one counts per workspace); every stage takes these first.

_None._

## Important

`loom:important`; taken before unlabelled work.

- **#10257**: merge queue Phase C: combined-tree CI qualification and eligible-repo live pilot (parent #9978)
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue
- **#12909**: ci_telemetry: 2amlogic owner discovery times out every cycle since Oct 17 (no loom-ui CI facts; 478 consecutive failures)
- **#12993**: Shadow evaluation of the new Champion (2026-10-20): 1/95 holds cleared — ranked remaining causes + fixtures

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#8525**: tracing: instrument sweep phases and role attempts, including Pi/OpenCode and repair cycles
- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default)
- **#12117**: Telemetry: inventory every kind; fix or delete role_tick.outcome, ci.job.log, session.output
- **#12167**: telemetry: export peer_coordination verdict and degraded duration on host.health
- **#12638**: Test: lease_refresh_child_carries_the_root_owners_gh_config_dir fails on main (child sees no GH_CONFIG_DIR)

## In Progress

Issues currently being built (`loom:building`).

- **#12094**: Retire the telemetry replay subsystem and the kinds left without a producer by #11301
- **#12604**: Split status_render.rs: break up ~970-line print_status_human and move inline tests out
- **#12952**: Champion: calibration check for merge-risk self-releases vs reverts (7-day)
- **#13170**: agent gh front: one span per GitHub write (operation, repo, number, head SHA)
- **#13175**: role_tick telemetry: loom.actions.prs_merged overcounts ~10x (whole-transcript rescans + counts merge attempts)
- **#13296**: suspect-critical-diff fails closed with "could not read the PR's file list" on script-only package.json edits
- **#13306**: write-scope: NOT_IN_INSTALLATION still refused for new repos on 0.19.1227 despite #13153 (installation is on All repositories)
- **#13319**: accounts check --live leaves the shared Codex profile root at 0700 — fights chezmoi-managed mode on the operator Mac
- **#13332**: merge-pr stacked reconcile retargets a non-feature/issue-N child to main without stripping squashed parent commits (left CONFLICTING)
- **#13345**: Ratification lane: wire the lane into the tick and post per-key RATIFY markers as comments
- **#13357**: merge queue: enqueue intermittently refused with "No merge queue found for branch" while the ruleset is active; the handoff rolls back and later succeeds
- **#13359**: champion: critical-file hold re-armed on a tree-equivalent Doctor rebase when the forge compare is unreadable (equivalence=unread); retry or local fallback before re-arming
- **#13382**: Resync waiver (#13096) cannot cover generated .agents/skills/*/SKILL.md copies, so every fleet resync PR is held for a human merge
- **#13384**: Remove self-referential tests (Test 4/5) from test-guide-operator-attention-fold.sh
- **#13396**: test flake: safehouse run_sink_reconciliation_narrates_normally_once_the_dedup_file_is_no_longer_fresh fails with ETXTBSY on Rust Unit Tests
- **#13421**: Consolidate duplicate find_repo_root gitdir walks in daemon_start/daemon_update onto repo_root.rs
- **#13456**: Consolidate duplicated test git helpers into test_git_helpers (loom-daemon)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#12631**: fix: release a Champion CI wait as requeued so it holds flat
- **#12637**: refactor: split status_render inline tests into per-module files
- **#12639**: test: create registered roots before registering their gh config dir
- **#13349**: feat: post per-key ratification markers and run the lane from the champion tick
- **#13398**: test: write fake forge gh script atomically to avoid ETXTBSY

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#12287**: merge-pr: delete the check wait, branch self-sync and mergeable pre-read (#12063 slice 2)
- **#12368**: feat(telemetry): record Judge verdicts and found-work from the real write
- **#12375**: chore: delete the telemetry-replay CLI, SigNoz replay queries and CI jobs
- **#12969**: feat: age-bound Codex rollout history in live profiles
- **#13297**: fix: resolve repo explicitly and surface gh errors in suspect-critical-diff
- **#13336**: fix: rebase children of non-feature/issue-N parents before retargeting
- **#13348**: fix: re-mint the writer token when a miss refresh still lacks the repo
- **#13363**: fix: retry merge-queue enqueue once on QueueUnavailable and key stall notice on reason
- **#13364**: feat: fall back to local patch comparison when the forge compare is unreadable
- **#13413**: feat: add champion-hold self-release-calibration verb
- **#13450**: fix: stop accounts read-only actions from chmod-ing the shared Codex profile root
- **#13451**: fix: window-filter role-tick transcript scan and count only confirmed merges
- **#13455**: feat: span traced write passthroughs in the agent gh front
- **#13457**: test: pin resync waiver notice to name the offending file
- **#13458**: test: run guide.md's real held filter instead of self-referential reconstructions
- **#13460**: refactor: share the gitdir pointer parse between daemon_start and daemon_update
- **#13461**: test: use shared test_git_helpers in fleet_sync tests

## Proposed

Issues carrying `loom:curated`.

- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
- **#8525**: tracing: instrument sweep phases and role attempts, including Pi/OpenCode and repair cycles *(curated)*
- **#8698**: Live end-to-end verification of the credential egress proxy (AC5 of #8674) *(curated)*
- **#9136**: main ruleset: bypass_actors 'RepositoryRole:always' makes pull_request and required_status_checks advisory *(curated)*
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
- **#11595**: Pause-and-roll budget path can still kill an in-flight merge-pr.sh (follow-up to #11263) *(curated)*
- **#11703**: Config precedence: fleet-gitops values must override repo-committed .loom/config.json (general rule + enforcement) *(curated)*
- **#11911**: Finish retiring daemon-spawned /loom:sweep and merge the two concurrency budgets *(curated)*
- **#11926**: peer-claim coordination is DEGRADED on loom-worker-4 (#6157 Layer 3) *(curated)*
- **#12029**: Telemetry: merge runs record PR number and observed result (merge-pr chokepoint) — Judge parts moved to #12362; held behind #12063 *(curated)*
- **#12094**: Retire the telemetry replay subsystem and the kinds left without a producer by #11301 *(curated)*
- **#12248**: Retired hold labels: remove core registry/work_finder readers (1/3 of #11843) *(curated)*
- **#12304**: Epic: fleet discovery from one mirror read per tick — zero forge reads for discovery; delete per-host listing paths *(curated)*
- **#12320**: Cleanup once every worker reads the worklist: delete the per-call `forge_mirror` discovery path, `maxStalenessSecs` and `per_call`; docs and ADR-0024; direct backend preserved *(curated)*
- **#12362**: Telemetry: Judge runs record found-work, PR, head_sha and the real verdict; drop the synthetic loom.judge_verdict (split from #12029, no merge-pr changes) *(curated)*
- **#12408**: Live-verify epic child discovery against sky130-sar-adc#23 (follow-up to #12388) *(curated)*
- **#12418**: Verify epic #10698 acceptance: completed-vs-not-planned dispositions and live fleet receipts *(curated)*
- **#12604**: Split status_render.rs: break up ~970-line print_status_human and move inline tests out *(curated)*
- **#12626**: docs: /repo:release Phase 1.5 CHANGELOG completeness gate has no seam for generated-notes policy *(curated)*
- **#12638**: Test: lease_refresh_child_carries_the_root_owners_gh_config_dir fails on main (child sees no GH_CONFIG_DIR) *(curated)*
- **#12703**: Add a per-repo setting to turn off Champion's out-of-band acceptance-criteria hold (#6883) *(curated)*
- **#12713**: Part-of merges re-queue the issue to Builders: landed-route marker is read as a Curator 'kept' decision *(curated)*
- **#12909**: ci_telemetry: 2amlogic owner discovery times out every cycle since Oct 17 (no loom-ui CI facts; 478 consecutive failures) *(curated)*
- **#12952**: Champion: calibration check for merge-risk self-releases vs reverts (7-day) *(curated)*
- **#12966**: Bound Codex session/thread history inside live profiles (the real codex-profiles growth) + uv cache *(curated)*
- **#12978**: Codex shared-seat dispatch: live receipts and startup measurements (split from #10796) *(curated)*
- **#13025**: [ci-daily] Session and Private-Workspace Docker Fixtures (unfiltered) *(curated)*
- **#13155**: [ci-daily] Flake Detection (shell suites, macos-latest, parallelism 1) *(curated)*
- **#13170**: agent gh front: one span per GitHub write (operation, repo, number, head SHA) *(curated)*
- **#13171**: tracing: daemon 'invoke github' calls outside a step have no sweep_id or role *(curated)*
- **#13175**: role_tick telemetry: loom.actions.prs_merged overcounts ~10x (whole-transcript rescans + counts merge attempts) *(curated)*
- **#13176**: Builder: 16/26 sweeps on fleet-1 end no-outcome-line (exit 0, no outcome) — main throughput limit *(curated)*
- **#13296**: suspect-critical-diff fails closed with "could not read the PR's file list" on script-only package.json edits *(curated)*
- **#13308**: Re-evaluate #13280 (daemon lands its own resync PRs through the merge queue) after one week: on or after 2026-11-05 *(curated)*
- **#13317**: Audit Champion merge-risk self-releases two weeks after #12960 is live *(curated)*
- **#13319**: accounts check --live leaves the shared Codex profile root at 0700 — fights chezmoi-managed mode on the operator Mac *(curated)*
- **#13332**: merge-pr stacked reconcile retargets a non-feature/issue-N child to main without stripping squashed parent commits (left CONFLICTING) *(curated)*
- **#13345**: Ratification lane: wire the lane into the tick and post per-key RATIFY markers as comments *(curated)*
- **#13357**: merge queue: enqueue intermittently refused with "No merge queue found for branch" while the ruleset is active; the handoff rolls back and later succeeds *(curated)*
- **#13359**: champion: critical-file hold re-armed on a tree-equivalent Doctor rebase when the forge compare is unreadable (equivalence=unread); retry or local fallback before re-arming *(curated)*
- **#13378**: Scope trust for github-actions[bot] to ci-daily tracker authorship instead of repo-wide trustedCommenters (alternative to #13340) *(curated)*
- **#13396**: test flake: safehouse run_sink_reconciliation_narrates_normally_once_the_dedup_file_is_no_longer_fresh fails with ETXTBSY on Rust Unit Tests *(curated)*

## Proposed (Architect / Hermit)

- **#13071**: Peer claims: move machine coordination off rendered room messages (custom event type / state events) *(architect)*
- **#12752**: Consolidate duplicate stats query stacks in activity/stats.rs *(hermit)*
- **#13017**: Split run_daemon (~1960-line fn) in daemon_service.rs into startup-phase modules *(hermit)*
- **#13261**: Consolidate ~23 duplicated private git subprocess wrappers in loom-daemon *(hermit)*
- **#13362**: Drop the fleet.json-absent legacy fallback and retire fleet_store/yaml.rs (#10705 follow-up) *(hermit)*
- **#13376**: Consolidate ~14 duplicated env-truthy helpers in loom-daemon into one *(hermit)*
- **#13384**: Remove self-referential tests (Test 4/5) from test-guide-operator-attention-fold.sh *(hermit)*
- **#13400**: Remove write-only role-runner skip-counter totals and unused *_count() wrappers *(hermit)*
- **#13421**: Consolidate duplicate find_repo_root gitdir walks in daemon_start/daemon_update onto repo_root.rs *(hermit)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
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
| Operator merge-risk holds | 8 |
| Very important | 0 |
| Important | 4 |
| Ready (`loom:issue`) | 5 |
| In Progress (`loom:building`) | 17 |
| PRs awaiting review | 5 |
| Approved PRs awaiting merge | 17 |
| Curated | 58 |
| Architect / Hermit proposals | 9 |
| Active epics | 9 |
<!-- guide:plan-body:end -->
