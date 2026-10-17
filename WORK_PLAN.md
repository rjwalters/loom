# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs under a hold (`loom:blocked` or `loom:operator`) — only a human merge decision is missing.

- **#10346**: fix(dispatch): retain attributed own claims before leaseless yield
- **#11803**: fix(daemon-stop): record operator stops instead of deleting intent; leave the watchdog armed
- **#12000**: C18: finish retiring the old dispatch paths, one host budget
- **#12154**: fix(resync): make forge label create/refresh opt-in via --sync-labels
- **#12262**: fix: track cd in subshells, brace groups and pushd in write-confinement guard
- **#12268**: feat: derive operator presence and in-session sweep facts in the daemon
- **#12287**: merge-pr: delete the check wait, branch self-sync and mergeable pre-read (#12063 slice 2)
- **#12321**: refactor: answer the gh pr merge guard from the shared command lexer
- **#12354**: feat: report auto-update progress and degrade health when checks or convergence stall
- **#12368**: feat(telemetry): record Judge verdicts and found-work from the real write
- **#12375**: chore: delete the telemetry-replay CLI, SigNoz replay queries and CI jobs
- **#12381**: ci: make the macOS flake-shell leg of CI Daily advisory
- **#12402**: ci: check macOS compile and run the release otlp_collector step on every backend PR
- **#12412**: feat: never use the writer App for reads; shed a read no reader can serve
- **#12428**: feat: show a read-only host update state in status and tick records
- **#12442**: feat: restart the daemon onto the same binary for restart-required config
- **#12464**: feat(daemon): prune unregistered idle codex profile dirs; document uv cache bound
- **#12466**: feat: reap idle worktrees of open issues and PRs

## Very Important

`loom:very-important` (one counts per workspace); every stage takes these first.

- **#12203**: stage_dwell: every host re-lists every repo every 5 min (~8.9k billable GitHub calls/h fleet-wide)

## Important

`loom:important`; taken before unlabelled work.

- **#9049**: Security: no secrets under any repo/worktree — move all credential state to ~/.loom (21 Claude OAuth tokens leaked, 2nd incident)
- **#9081**: guard-background-subagents misses a background Bash started by a foreground subagent: headless sweep exits, Builder's make verify reaped
- **#9957**: daemon: deliberate stops (SIGTERM) are silent autonomy losses — the watchdog goes down with it, the intent marker is deleted, and bootstrap cannot recover
- **#10083**: CI path filter skips label_registry Rust tests on label-file-only PRs (how #10072 turned main red)
- **#10337**: Dispatcher yields to its own leaseless loom:building label and orphans the issue (3 starred issues 2026-10-05 00:40Z)
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue
- **#10890**: Daemon dispatches loom:issue + loom:needs-capability items; pre-flight skip then reads as insta-crash and quarantine release re-queues them
- **#11301**: check-stale-blocked: the per-repo budget probe spends ~1.4k GraphQL points/h on the writer bucket that runs dry
- **#11787**: issue.estimate coverage gap: operator-session curation sets loom:curated by hand and bypasses curation-complete (loom-ui: 58 of 62 curations since the gate have no estimate)
- **#12217**: github-events mirror reads stale for quiet repos (1555 stale vs 1049 hits/h), so the step gather falls back to REST
- **#12447**: Epic: captain-assigned dispatch — workers ask the fleet captain for their next assignment from one fleet queue
- **#12450**: Fleet dispatch Phase 1b: worker client — claim via the captain before the forge pre-claim, fail-open with hysteresis

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#9081**: guard-background-subagents misses a background Bash started by a foreground subagent: headless sweep exits, Builder's make verify reaped
- **#10083**: CI path filter skips label_registry Rust tests on label-file-only PRs (how #10072 turned main red)
- **#10890**: Daemon dispatches loom:issue + loom:needs-capability items; pre-flight skip then reads as insta-crash and quarantine release re-queues them
- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default)
- **#11112**: Remove the safehouse (Matrix) integration from Loom: narration, ChatOps/Concierge, peer-claim channel (forge leases remain)
- **#11301**: check-stale-blocked: the per-repo budget probe spends ~1.4k GraphQL points/h on the writer bucket that runs dry
- **#11787**: issue.estimate coverage gap: operator-session curation sets loom:curated by hand and bypasses curation-complete (loom-ui: 58 of 62 curations since the gate have no estimate)
- **#12085**: Telemetry(doctor): structured parked-PR cause (park reason, doctor_cycles, queue gate, treating age) (ETA doctor, loom-ui#4063 #6)
- **#12093**: Simplify: retire the dispatch-inert tier:* axis (Level::of ignores it; no consumer) — it only adds confusion beside the real priority labels
- **#12117**: Telemetry: inventory every kind; fix or delete role_tick.outcome, ci.job.log, session.output
- **#12167**: telemetry: export peer_coordination verdict and degraded duration on host.health
- **#12475**: Judge re-review ignores prior changes-requested verdicts posted as comments (review-reconciliation reads formal reviews only)

## In Progress

Issues currently being built (`loom:building`).

- **#9736**: health: detect stalled release updates without a source checkout
- **#11332**: A real fleet commit is authored as the test-fixture identity Loom CI <ci@loom.test>
- **#11983**: Provenance out of Loom, stage 3: remove provenance-only docs, fixtures and CI checks
- **#12051**: pass_shard: run the stale-blocked release pass and fleet.state listings on one host per repo
- **#12082**: Worktree-isolation guard blocks Bash writes to paths outside the repo when the cwd is the main checkout
- **#12094**: Retire the telemetry replay subsystem and the kinds left without a producer by #11301
- **#12121**: Telemetry: daemon derives operator.presence and in-session sweep.* from observed sessions
- **#12198**: ci-daily: macOS shell flake leg never completes green — decide fix, advisory or remove
- **#12236**: guard: shared command-analysis lexer + corpus harness, first move: merge-redirect check (slice 1 of #11113)
- **#12362**: Telemetry: Judge runs record found-work, PR, head_sha and the real verdict; drop the synthetic loom.judge_verdict (split from #12029, no merge-pr changes)
- **#12394**: The writer App is never used for reads: a read with no reader available is shed, not sent to the writer
- **#12396**: CI misses release-only build failures: check the macOS target and the release's test step on every PR
- **#12416**: main CI red: every_role_queue_query_excludes_frozen flags champion-common.md:283 (#10935)
- **#12420**: status/gauges: expose host state H0-H7, target and its source, and per-workspace W state
- **#12425**: auto-update: arm pause-and-roll from the config-restart demand (#12419 child 3)
- **#12447**: Epic: captain-assigned dispatch — workers ask the fleet captain for their next assignment from one fleet queue
- **#12450**: Fleet dispatch Phase 1b: worker client — claim via the captain before the forge pre-claim, fail-open with hysteresis
- **#12454**: [ci-daily] Supply Chain
- **#12456**: [ci-daily] Session and Private-Workspace Docker Fixtures (unfiltered)
- **#12458**: Reap idle clean worktrees of open issues/PRs (configurable idle period)
- **#12460**: Bound shared tool caches (uv) and prune non-live codex profiles

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#12467**: feat(fleet-dispatch): worker client claims through the captain first (#12450)

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#10346**: fix(dispatch): retain attributed own claims before leaseless yield
- **#11803**: fix(daemon-stop): record operator stops instead of deleting intent; leave the watchdog armed
- **#12000**: C18: finish retiring the old dispatch paths, one host budget
- **#12154**: fix(resync): make forge label create/refresh opt-in via --sync-labels
- **#12262**: fix: track cd in subshells, brace groups and pushd in write-confinement guard
- **#12268**: feat: derive operator presence and in-session sweep facts in the daemon
- **#12287**: merge-pr: delete the check wait, branch self-sync and mergeable pre-read (#12063 slice 2)
- **#12321**: refactor: answer the gh pr merge guard from the shared command lexer
- **#12354**: feat: report auto-update progress and degrade health when checks or convergence stall
- **#12368**: feat(telemetry): record Judge verdicts and found-work from the real write
- **#12375**: chore: delete the telemetry-replay CLI, SigNoz replay queries and CI jobs
- **#12381**: ci: make the macOS flake-shell leg of CI Daily advisory
- **#12402**: ci: check macOS compile and run the release otlp_collector step on every backend PR
- **#12412**: feat: never use the writer App for reads; shed a read no reader can serve
- **#12428**: feat: show a read-only host update state in status and tick records
- **#12442**: feat: restart the daemon onto the same binary for restart-required config
- **#12464**: feat(daemon): prune unregistered idle codex profile dirs; document uv cache bound
- **#12466**: feat: reap idle worktrees of open issues and PRs

## Proposed

Issues carrying `loom:curated`.

- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
- **#8525**: tracing: instrument sweep phases and role attempts, including Pi/OpenCode and repair cycles *(curated)*
- **#8698**: Live end-to-end verification of the credential egress proxy (AC5 of #8674) *(curated)*
- **#8913**: observability: live-verify ci.job.log reconstruction in SigNoz and record it in evidence.md (#8825 AC1) *(curated)*
- **#8950**: Live-verify Pi tokens_by_model against a real LOOM_RUNTIME=pi launch *(curated)*
- **#9006**: observability: live-verify the SigNoz queue-starvation alert rule fires and resolves (#8856 / PR #8935 follow-up) *(curated)*
- **#9049**: Security: no secrets under any repo/worktree — move all credential state to ~/.loom (21 Claude OAuth tokens leaked, 2nd incident) *(curated)*
- **#9081**: guard-background-subagents misses a background Bash started by a foreground subagent: headless sweep exits, Builder's make verify reaped *(curated)*
- **#9136**: main ruleset: bypass_actors 'RepositoryRole:always' makes pull_request and required_status_checks advisory *(curated)*
- **#9255**: forge_events: event-gated per-workspace polling under a bounded staleness cap (ADR-0021 amendment step 2) *(curated)*
- **#9388**: Doctor rechecks the head SHA before pushing but not the labels before writing them *(curated)*
- **#9508**: forge polling: ADR-0021 amendment step 3 — cross-host dedup (one host polls a repo, the others reuse the answer) *(curated)*
- **#9736**: health: detect stalled release updates without a source checkout *(curated)*
- **#9908**: Consolidate persistence: one journal per concern, retire duplicated stores *(curated)*
- **#9957**: daemon: deliberate stops (SIGTERM) are silent autonomy losses — the watchdog goes down with it, the intent marker is deleted, and bootstrap cannot recover *(curated)*
- **#9978**: feat(champion): optional per-repo merge-queue mode — enqueue Judge-approved PRs to GitHub's merge queue instead of merge-pr.sh's freshness/re-date loop *(curated)*
- **#10083**: CI path filter skips label_registry Rust tests on label-file-only PRs (how #10072 turned main red) *(curated)*
- **#10150**: notify-cleared-blockers runs only from merge-pr.sh: merges made any other way never clear a block *(curated)*
- **#10181**: ci: backend paths filter omits defaults/forge/**, so forge_inventory tests skip on manifest/baseline-only PRs *(curated)*
- **#10196**: telemetry: make SigNoz point-in-time reconstructable — knowable-at timestamps, full fleet-state snapshots over OTLP, per-host export-coverage records, replay contract *(curated)*
- **#10257**: merge queue Phase C: combined-tree CI qualification and eligible-repo live pilot (parent #9978) *(curated)*
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day) *(curated)*
- **#10335**: Per-repo opt-out for the PreToolUse guard hooks that install/upgrade respects *(curated)*
- **#10337**: Dispatcher yields to its own leaseless loom:building label and orphans the issue (3 starred issues 2026-10-05 00:40Z) *(curated)*
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue *(curated)*
- **#10456**: Epic: the fleet keeps its own hold queue clean — only real operator asks reach Needs attention *(curated)*
- **#10512**: Fleet reader-App pool near exhaustion: cut top billable pollers (visibility.repo cache, issue_state ETag, cross-host dedupe) — ~8k REST/h *(curated)*
- **#10698**: Fleet version floor: the daemon enforces `loom_min_version` from the fleet store, rolls itself forward, and brings each repo's installed Loom up to its own version *(curated)*
- **#10770**: resync-installed.sh writes forge labels by default; make forge writes opt-in or list them in --dry-run *(curated)*
- **#10802**: General cleanup: reap agent process residue (dev servers, orphaned children) after any agent exit *(curated)*
- **#10827**: Promotion author gate: don't auto-promote issues authored by untrusted identities (signed decision markers dropped) *(curated)*
- **#10838**: merge_pr_reconcile_differential: claim-gate differential disagrees on case-variant duplicate label (loom:buildinG) *(curated)*
- **#10916**: Fleet singleton output watchdog: alert when any one-host fleet job stops producing output (not just when its host goes silent) *(curated)*
- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default) *(curated)*
- **#11112**: Remove the safehouse (Matrix) integration from Loom: narration, ChatOps/Concierge, peer-claim channel (forge leases remain) *(curated)*
- **#11118**: Umbrella: claim and lease state is inferred from label age and presence — live work gets orphaned, stripped or doubled (13 bugs) *(curated)*
- **#11240**: Verify secret-scanning and push-protection settings for #9049 with authoritative live evidence *(curated)*
- **#11283**: OpenCode 2.x guarded launches: load the native-tools plugin via the 2.x default-export shape, live-verify, flip guard_verified *(curated)*
- **#11284**: Z.ai seats: per-dispatch runtime/modelProfile and keep free seats busy (remaining scope after #11412/#11303/fleet-gitops#196) *(curated)*
- **#11287**: Umbrella: Z.ai GLM-5.3-Flash seats landing code in the fleet via OpenCode *(curated)*
- **#11300**: Z.ai seat telemetry via the egress proxy in transparent observe mode: per-request tokens/latency/429s to SigNoz, OpenCode headers untouched *(curated)*
- **#11332**: A real fleet commit is authored as the test-fixture identity Loom CI <ci@loom.test> *(curated)*
- **#11441**: Fleet policy lives in fleet-gitops only: move runtimes/autonomous policy out of committed .loom/config.json (tier 2 silently shadows the fleet tier) *(curated)*
- **#11595**: Pause-and-roll budget path can still kill an in-flight merge-pr.sh (follow-up to #11263) *(curated)*
- **#11703**: Config precedence: fleet-gitops values must override repo-committed .loom/config.json (general rule + enforcement) *(curated)*
- **#11712**: Daemon: export OTLP only and remove inbound control channels other than config (2am#4300) *(curated)*
- **#11761**: Keep-it-simple beyond Curator: Judge and Champion gate on unjustified complexity; Curator checks 'already covered' first (follow-up to #11662) *(curated)*
- **#11787**: issue.estimate coverage gap: operator-session curation sets loom:curated by hand and bypasses curation-complete (loom-ui: 58 of 62 curations since the gate have no estimate) *(curated)*
- **#11983**: Provenance out of Loom, stage 3: remove provenance-only docs, fixtures and CI checks *(curated)*
- **#12014**: ci: approved PRs touching workflow files need several CI rounds to land as main moves (no merge queue on a user-owned repo) *(curated)*
- **#12019**: [ci-daily] Flake Detection (shell suites, macos-latest, parallelism 1) *(curated)*
- **#12029**: Telemetry: merge runs record PR number and observed result (merge-pr chokepoint) — Judge parts moved to #12362; held behind #12063 *(curated)*
- **#12043**: macOS bash 3.2: 7 shell suites fail on bash-4+ semantics (empty arrays under set -u, ${x,,}, \001 array sentinel, heredoc apostrophe in $(), mapfile, source <()) *(curated)*
- **#12082**: Worktree-isolation guard blocks Bash writes to paths outside the repo when the cwd is the main checkout *(curated)*
- **#12086**: Telemetry(coverage): every lease-writing host exports pick.decision + fleet.state (ETA ready_wait over-prediction, loom-ui#4063 #7) *(curated)*
- **#12094**: Retire the telemetry replay subsystem and the kinds left without a producer by #11301 *(curated)*
- **#12121**: Telemetry: daemon derives operator.presence and in-session sweep.* from observed sessions *(curated)*
- **#12127**: ci: rebalance Native Port Suites legs so none runs over ~3 min (next PR critical path after #12089) *(curated)*
- **#12198**: ci-daily: macOS shell flake leg never completes green — decide fix, advisory or remove *(curated)*
- **#12220**: Rate-limit probe cache follow-ups: quiet-bucket gauge gap, ambient-login docs, post-rollout measurement *(curated)*
- **#12236**: guard: shared command-analysis lexer + corpus harness, first move: merge-redirect check (slice 1 of #11113) *(curated)*
- **#12248**: Retired hold labels: remove core registry/work_finder readers (1/3 of #11843) *(curated)*
- **#12304**: Epic: fleet discovery from one mirror read per tick — zero forge reads for discovery; delete per-host listing paths *(curated)*
- **#12311**: Discovery interface: mirror backend (one worklist read per tick, in-memory 304 cache), the config-chosen tier chain, and the step gather reading one snapshot *(curated)*
- **#12312**: Direct backend: standalone Loom discovery (GitHub or Gitea, `gh` only, the default), forge-neutral, one snapshot per tick, ETag cache and gatherConcurrency; CI parity tests for both forges *(curated)*
- **#12313**: `queue_blocked` reads the tick snapshot; delete its own listing and the captain queue-blocked facts on the mirror path *(curated)*
- **#12314**: Quarantine reconciliation reads the discovery snapshot; delete its `loom:blocked` listing *(curated)*
- **#12315**: Claim reconciliation's open-PR discovery listing reads the snapshot; its write-gating reads stay live *(curated)*
- **#12317**: Step gather's per-item reads: CI at Judge heads and stage-entry times from the snapshot; classify the landed closes-graph and hold-marker reads *(curated)*
- **#12318**: Claim refusals report back: `POST /v1/mirror/recheck` when the live claim read disagrees with the drawn snapshot *(curated)*
- **#12319**: Wakes: a change in a served repo ticks the step tick early (trigger attribute, one worklist read per wake), gated on mirror health *(curated)*
- **#12320**: Cleanup once every worker reads the worklist: delete the per-call `forge_mirror` discovery path, `maxStalenessSecs` and `per_call`; docs and ADR-0024; direct backend preserved *(curated)*
- **#12322**: Roster pass: emit a telemetry record per register / set-priority / skip:not-cloned change *(curated)*
- **#12344**: Flow-change evidence review (#11708 slice B): verdict/merge gates and merge sequencing *(curated)*
- **#12357**: Verify live OTLP delivery of host.status gauges (follow-up to #12309) *(curated)*
- **#12362**: Telemetry: Judge runs record found-work, PR, head_sha and the real verdict; drop the synthetic loom.judge_verdict (split from #12029, no merge-pr changes) *(curated)*
- **#12396**: CI misses release-only build failures: check the macOS target and the release's test step on every PR *(curated)*
- **#12408**: Live-verify epic child discovery against sky130-sar-adc#23 (follow-up to #12388) *(curated)*
- **#12416**: main CI red: every_role_queue_query_excludes_frozen flags champion-common.md:283 (#10935) *(curated)*
- **#12418**: Verify epic #10698 acceptance: completed-vs-not-planned dispositions and live fleet receipts *(curated)*
- **#12420**: status/gauges: expose host state H0-H7, target and its source, and per-workspace W state *(curated)*
- **#12425**: auto-update: arm pause-and-roll from the config-restart demand (#12419 child 3) *(curated)*
- **#12447**: Epic: captain-assigned dispatch — workers ask the fleet captain for their next assignment from one fleet queue *(curated)*
- **#12454**: [ci-daily] Supply Chain *(curated)*
- **#12456**: [ci-daily] Session and Private-Workspace Docker Fixtures (unfiltered) *(curated)*
- **#12458**: Reap idle clean worktrees of open issues/PRs (configurable idle period) *(curated)*
- **#12460**: Bound shared tool caches (uv) and prune non-live codex profiles *(curated)*
- **#12475**: Judge re-review ignores prior changes-requested verdicts posted as comments (review-reconciliation reads formal reviews only) *(curated)*

## Proposed (Architect / Hermit)

- **#12093**: Simplify: retire the dispatch-inert tier:* axis (Level::of ignores it; no consumer) — it only adds confusion beside the real priority labels *(hermit)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#7810**: [epic] Retire shell: 10 sequential PRs, two foundational then update + agent execution
- **#9908**: Consolidate persistence: one journal per concern, retire duplicated stores
- **#9978**: feat(champion): optional per-repo merge-queue mode — enqueue Judge-approved PRs to GitHub's merge queue instead of merge-pr.sh's freshness/re-date loop
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day)
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue
- **#10456**: Epic: the fleet keeps its own hold queue clean — only real operator asks reach Needs attention
- **#10698**: Fleet version floor: the daemon enforces `loom_min_version` from the fleet store, rolls itself forward, and brings each repo's installed Loom up to its own version
- **#10916**: Fleet singleton output watchdog: alert when any one-host fleet job stops producing output (not just when its host goes silent)
- **#11287**: Umbrella: Z.ai GLM-5.3-Flash seats landing code in the fleet via OpenCode
- **#12304**: Epic: fleet discovery from one mirror read per tick — zero forge reads for discovery; delete per-host listing paths

## Backlog Balance

| Bucket | Count |
|------|-------|
| Operator merge-risk holds | 18 |
| Very important | 1 |
| Important | 12 |
| Ready (`loom:issue`) | 12 |
| In Progress (`loom:building`) | 21 |
| PRs awaiting review | 1 |
| Approved PRs awaiting merge | 18 |
| Curated | 88 |
| Architect / Hermit proposals | 1 |
| Active epics | 11 |
<!-- guide:plan-body:end -->
