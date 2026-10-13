# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#9938**: fix(daemon): never treat a pull request as an issue in candidate selection or claim
- **#10005**: docs(blocked): permanent-block marker + audit comment on unblock (#8742)
- **#10065**: feat(daemon): stable host.id per machine plus start/shutdown/heartbeat telemetry (#10023)
- **#10180**: feat(daemon): cleared-blocker re-check on any issue/PR close (#10150)
- **#10359**: feat(guards): guards.enabled master opt-out + guard false-positive fixes (#10335)
- **#11231**: feat(watchdog): worktree + agent tool-process liveness signals (#9533)
- **#11233**: feat(role-runner): deterministic per-repo lane rule for judge and doctor (#10630)
- **#11235**: Pull Docker Hub test images through mirror.gcr.io with a bounded Hub fallback (#11232)
- **#11238**: Reap agent process residue after any agent exit (#10802)
- **#11244**: Every-tick resync follow-ups from #10998 (#11003)
- **#11246**: feat(comment-trust): promotion author gate (#10827)
- **#11261**: fix(auto_update): stop unsupervised re-downloads; typed below-floor not-rolling reason (#11042)
- **#11277**: fix(park-record,stale-blocked): ignore park markers quoted in code; vet release writes under root credential (Part of #10837)
- **#11305**: Retire $LOOM_HYPERPARAMS vector tier and hyperparams digest (#11107)
- **#11321**: feat(doctor): add forge doctor-handback, a verified add-first hand-back verb
- **#11419**: feat(fleet-sync): restart-only fleet config change arms a same-binary pause-and-roll
- **#11558**: feat(checkout_ff): fast-forward past dirt identical to origin (#11553)
- **#11628**: feat: loom-daemon telemetry-join (#11528)
- **#11704**: CI is the test gate: Builders run the fast tier only; Judge checks CI first (#11690)
- **#11715**: feat(presence): fleet-controlled operator presence wiring + daemon-less OTLP flush (#11713)
- **#11717**: refactor: remove the safehouse integration and peer-claim channel (#11112 final slice)

## Very Important

`loom:very-important` (one counts per workspace); every stage takes these first.

_None._

## Important

`loom:important`; taken before unlabelled work.

- **#9533**: review-stall-watchdog kills healthy sweeps: sweep-log mtime is not a liveness signal
- **#10196**: telemetry: make SigNoz point-in-time reconstructable — knowable-at timestamps, full fleet-state snapshots over OTLP, per-host export-coverage records, replay contract
- **#10337**: Dispatcher yields to its own leaseless loom:building label and orphans the issue (3 starred issues 2026-10-05 00:40Z)
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue
- **#10916**: Fleet singleton output watchdog: alert when any one-host fleet job stops producing output (not just when its host goes silent)
- **#11094**: Work-finder RAM admission charges 2 GB per sweep, but a loom-daemon rustc peaks at ~13 GB: concurrent loom builds OOM 30 GB workers
- **#11179**: Dashboard session pane: links should open the dashboard view, not GitHub
- **#11283**: OpenCode 2.x guarded launches: load the native-tools plugin via the 2.x default-export shape, live-verify, flip guard_verified
- **#11284**: Z.ai seats: per-dispatch runtime/modelProfile and keep free seats busy (remaining scope after #11412/#11303/fleet-gitops#196)
- **#11285**: Sweeps: run each phase on its role's runtime (GLM Builder/Doctor via a phase-worker subcommand; Judge/Curator never GLM)
- **#11286**: Z.ai api-keys pool fidelity: real exhaustion fixtures, reset-aligned cooldowns, per-account gauges, burn sampler misses guarded OpenCode stores
- **#11287**: Umbrella: Z.ai GLM-5.3-Flash seats landing code in the fleet via OpenCode
- **#11300**: Z.ai seat telemetry via the egress proxy in transparent observe mode: per-request tokens/latency/429s to SigNoz, OpenCode headers untouched
- **#11360**: Mixed sweep usage: verify phase launch records and per-tap journal accounting
- **#11463**: telemetry-replay: live SigNoz logs have no created_at, so every replay query fails (knowable-at column never existed on the live store)
- **#11688**: Build/runtime failure on main: Security Scan fails — quinn-proto 0.11.16 has 5 RUSTSEC-2026 advisories

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8570**: Guard: refuse a build/scratch dir assignment that resolves onto a tmpfs mount (upstream PR to rjwalters/repo, split from #8512)
- **#10337**: Dispatcher yields to its own leaseless loom:building label and orphans the issue (3 starred issues 2026-10-05 00:40Z)
- **#10837**: Release pass (#10556) never released loom-ui#1695: all three park-record blockers closed ~22 h ago, still loom:blocked
- **#11086**: sweep-lease-renew.sh stop <arg> kills any PID, so an issue number passed by mistake signals an unrelated process
- **#11094**: Work-finder RAM admission charges 2 GB per sweep, but a loom-daemon rustc peaks at ~13 GB: concurrent loom builds OOM 30 GB workers
- **#11194**: Judge worktrees: align supported checkout guidance and report legacy directories without reclaiming them
- **#11232**: CI fails on Docker Hub's anonymous pull rate limit (Codex smoke, ClickHouse-backed OTLP tests): pull through a mirror with fallback and retry
- **#11528**: loom-daemon telemetry join: one command that brings a host into SigNoz and proves its records land under the right identity
- **#11556**: Fact export slice C: spill to an append-only tail instead of drop-oldest queue overflow
- **#11688**: Build/runtime failure on main: Security Scan fails — quinn-proto 0.11.16 has 5 RUSTSEC-2026 advisories

## In Progress

Issues currently being built (`loom:building`).

- **#8387**: spawn-codex.sh forwards a `model@effort` suffix verbatim to the Codex CLI's `-m`
- **#11300**: Z.ai seat telemetry via the egress proxy in transparent observe mode: per-request tokens/latency/429s to SigNoz, OpenCode headers untouched
- **#11711**: Audit every network input to loom-daemon: config, forge, wake-only webhook, or removed (2am#4300)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#11589**: chore(deps): lock file maintenance
- **#11725**: docs: audit every network input to loom-daemon (#11711)

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#9938**: fix(daemon): never treat a pull request as an issue in candidate selection or claim
- **#10005**: docs(blocked): permanent-block marker + audit comment on unblock (#8742)
- **#10065**: feat(daemon): stable host.id per machine plus start/shutdown/heartbeat telemetry (#10023)
- **#10180**: feat(daemon): cleared-blocker re-check on any issue/PR close (#10150)
- **#10359**: feat(guards): guards.enabled master opt-out + guard false-positive fixes (#10335)
- **#11063**: feat(fleet-alert): wire the singleton output watchdog to a real OutputSource (#10916)
- **#11231**: feat(watchdog): worktree + agent tool-process liveness signals (#9533)
- **#11233**: feat(role-runner): deterministic per-repo lane rule for judge and doctor (#10630)
- **#11235**: Pull Docker Hub test images through mirror.gcr.io with a bounded Hub fallback (#11232)
- **#11238**: Reap agent process residue after any agent exit (#10802)
- **#11244**: Every-tick resync follow-ups from #10998 (#11003)
- **#11246**: feat(comment-trust): promotion author gate (#10827)
- **#11261**: fix(auto_update): stop unsupervised re-downloads; typed below-floor not-rolling reason (#11042)
- **#11277**: fix(park-record,stale-blocked): ignore park markers quoted in code; vet release writes under root credential (Part of #10837)
- **#11305**: Retire $LOOM_HYPERPARAMS vector tier and hyperparams digest (#11107)
- **#11321**: feat(doctor): add forge doctor-handback, a verified add-first hand-back verb
- **#11410**: fix(stale_blocked): memoise the budget probe per credential (#11301)
- **#11419**: feat(fleet-sync): restart-only fleet config change arms a same-binary pause-and-roll
- **#11558**: feat(checkout_ff): fast-forward past dirt identical to origin (#11553)
- **#11628**: feat: loom-daemon telemetry-join (#11528)
- **#11692**: Seed the workspace draw from the UTC minute and host id (#11683)
- **#11704**: CI is the test gate: Builders run the fast tier only; Judge checks CI first (#11690)
- **#11715**: feat(presence): fleet-controlled operator presence wiring + daemon-less OTLP flush (#11713)
- **#11717**: refactor: remove the safehouse integration and peer-claim channel (#11112 final slice)

## Proposed

Issues carrying `loom:curated`.

- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
- **#8372**: Canary the uncommitted-work Stop guard in opted-in consumer workspaces with measured block outcomes *(curated)*
- **#8387**: spawn-codex.sh forwards a `model@effort` suffix verbatim to the Codex CLI's `-m` *(curated)*
- **#8434**: live-verify native-ephemeral containment: canary run in-container, two-worker filesystem disjointness, post-run writable-layer credential scan *(curated)*
- **#8525**: tracing: instrument sweep phases and role attempts, including Pi/OpenCode and repair cycles *(curated)*
- **#8570**: Guard: refuse a build/scratch dir assignment that resolves onto a tmpfs mount (upstream PR to rjwalters/repo, split from #8512) *(curated)*
- **#8698**: Live end-to-end verification of the credential egress proxy (AC5 of #8674) *(curated)*
- **#8742**: loom:blocked is silently stripped from operator-ruled permanent blocks (unblock probe treats 'unparseable blocker' as 'no blocker') *(curated)*
- **#8913**: observability: live-verify ci.job.log reconstruction in SigNoz and record it in evidence.md (#8825 AC1) *(curated)*
- **#8950**: Live-verify Pi tokens_by_model against a real LOOM_RUNTIME=pi launch *(curated)*
- **#9006**: observability: live-verify the SigNoz queue-starvation alert rule fires and resolves (#8856 / PR #8935 follow-up) *(curated)*
- **#9049**: Security: no secrets under any repo/worktree — move all credential state to ~/.loom (21 Claude OAuth tokens leaked, 2nd incident) *(curated)*
- **#9136**: main ruleset: bypass_actors 'RepositoryRole:always' makes pull_request and required_status_checks advisory *(curated)*
- **#9255**: forge_events: event-gated per-workspace polling under a bounded staleness cap (ADR-0021 amendment step 2) *(curated)*
- **#9388**: Doctor rechecks the head SHA before pushing but not the labels before writing them *(curated)*
- **#9533**: review-stall-watchdog kills healthy sweeps: sweep-log mtime is not a liveness signal *(curated)*
- **#9548**: Authenticate Loom's control signals: markers and phrases count only from trusted authors (work safely on any public repo) *(curated)*
- **#9735**: auto_update: roll back candidates that fail startup and quarantine failed artifacts *(curated)*
- **#9874**: fix(guard): ignore remote rm targets inside ssh/docker exec command strings in LOOM_RM_SCOPE=repo check *(curated)*
- **#9908**: Consolidate persistence: one journal per concern, retire duplicated stores *(curated)*
- **#9929**: Curator applies issue-lifecycle labels (loom:curating/loom:curated) to pull requests *(curated)*
- **#9978**: feat(champion): optional per-repo merge-queue mode — enqueue Judge-approved PRs to GitHub's merge queue instead of merge-pr.sh's freshness/re-date loop *(curated)*
- **#10001**: "Only a human can do this" is for PO-level decisions only; Curator reviews every parked item and returns what agents can do *(curated)*
- **#10009**: Operator-label review (Lane B of #10001): give every parked item in the wired repos a verdict and return what agents can do *(curated)*
- **#10023**: daemon: one stable host.id per machine, plus start/stop/heartbeat events in SigNoz *(curated)*
- **#10150**: notify-cleared-blockers runs only from merge-pr.sh: merges made any other way never clear a block *(curated)*
- **#10196**: telemetry: make SigNoz point-in-time reconstructable — knowable-at timestamps, full fleet-state snapshots over OTLP, per-host export-coverage records, replay contract *(curated)*
- **#10229**: sweep-lease-renew.sh spends the operator's personal REST pool at ~600 calls/h and climbing; one sweep id runs ~16 renew loops *(curated)*
- **#10257**: merge queue Phase C: combined-tree CI qualification and eligible-repo live pilot (parent #9978) *(curated)*
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day) *(curated)*
- **#10335**: Per-repo opt-out for the PreToolUse guard hooks that install/upgrade respects *(curated)*
- **#10337**: Dispatcher yields to its own leaseless loom:building label and orphans the issue (3 starred issues 2026-10-05 00:40Z) *(curated)*
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue *(curated)*
- **#10456**: Epic: the fleet keeps its own hold queue clean — only real operator asks reach Needs attention *(curated)*
- **#10470**: security(update): enforce independent release approval and artifact assurance for managed fleets *(curated)*
- **#10512**: Fleet reader-App pool near exhaustion: cut top billable pollers (visibility.repo cache, issue_state ETag, cross-host dedupe) — ~8k REST/h *(curated)*
- **#10630**: Judge/Doctor lanes per repo scale with that repo's own debt (deterministic; replaces the marginal-value allocator) *(curated)*
- **#10698**: Fleet version floor: the daemon enforces `loom_min_version` from the fleet store, rolls itself forward, and brings each repo's installed Loom up to its own version *(curated)*
- **#10720**: Restart-only fleet config change triggers pause and restart *(curated)*
- **#10721**: Gauges and status for host H0-H7 and workspace W states *(curated)*
- **#10802**: General cleanup: reap agent process residue (dev servers, orphaned children) after any agent exit *(curated)*
- **#10825**: CI: path-filter the image-smoke jobs on main pushes (docker/** changes only); run them unconditionally in ci-daily *(curated)*
- **#10827**: Promotion author gate: don't auto-promote issues authored by untrusted identities (signed decision markers dropped) *(curated)*
- **#10837**: Release pass (#10556) never released loom-ui#1695: all three park-record blockers closed ~22 h ago, still loom:blocked *(curated)*
- **#10913**: forge_events: hold read caches (gh-cached, pipeline_snapshot) under the poll-gating health signal (#9255 slice 2) *(curated)*
- **#10916**: Fleet singleton output watchdog: alert when any one-host fleet job stops producing output (not just when its host goes silent) *(curated)*
- **#11003**: Every-tick resync: follow-ups from #10998 (breaker re-check before fallback/claim, per-owner budget, host-level credential alert, empty and archived repos) *(curated)*
- **#11042**: Floor-loop follow-ups: unsupervised hosts re-download every tick; typed not-rolling reason; status enabled semantics *(curated)*
- **#11086**: sweep-lease-renew.sh stop <arg> kills any PID, so an issue number passed by mistake signals an unrelated process *(curated)*
- **#11094**: Work-finder RAM admission charges 2 GB per sweep, but a loom-daemon rustc peaks at ~13 GB: concurrent loom builds OOM 30 GB workers *(curated)*
- **#11103**: Priority model: loom:important / loom:very-important for everyone; weighted workspace pick, then level → oldest → number *(curated)*
- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default) *(curated)*
- **#11112**: Remove the safehouse (Matrix) integration from Loom: narration, ChatOps/Concierge, peer-claim channel (forge leases remain) *(curated)*
- **#11189**: Epic: fleet workers stop filling their disks — bound per-sweep build output and reclaim what the reaper misses (loom-worker-1, 2026-10-09) *(curated)*
- **#11194**: Judge worktrees: align supported checkout guidance and report legacy directories without reclaiming them *(curated)*
- **#11278**: Priority model rollout: wire priority_pick into dispatch, remove old readers, prompts, label migration (follow-up to #11103) *(curated)*
- **#11283**: OpenCode 2.x guarded launches: load the native-tools plugin via the 2.x default-export shape, live-verify, flip guard_verified *(curated)*
- **#11284**: Z.ai seats: per-dispatch runtime/modelProfile and keep free seats busy (remaining scope after #11412/#11303/fleet-gitops#196) *(curated)*
- **#11285**: Sweeps: run each phase on its role's runtime (GLM Builder/Doctor via a phase-worker subcommand; Judge/Curator never GLM) *(curated)*
- **#11286**: Z.ai api-keys pool fidelity: real exhaustion fixtures, reset-aligned cooldowns, per-account gauges, burn sampler misses guarded OpenCode stores *(curated)*
- **#11287**: Umbrella: Z.ai GLM-5.3-Flash seats landing code in the fleet via OpenCode *(curated)*
- **#11300**: Z.ai seat telemetry via the egress proxy in transparent observe mode: per-request tokens/latency/429s to SigNoz, OpenCode headers untouched *(curated)*
- **#11360**: Mixed sweep usage: verify phase launch records and per-tap journal accounting *(curated)*
- **#11441**: Fleet policy lives in fleet-gitops only: move runtimes/autonomous policy out of committed .loom/config.json (tier 2 silently shadows the fleet tier) *(curated)*
- **#11463**: telemetry-replay: live SigNoz logs have no created_at, so every replay query fails (knowable-at column never existed on the live store) *(curated)*
- **#11528**: loom-daemon telemetry join: one command that brings a host into SigNoz and proves its records land under the right identity *(curated)*
- **#11553**: fleet_sync checkout_ff: fast-forward through dirty tracked files identical to origin (reuse #11543's predicate) *(curated)*
- **#11556**: Fact export slice C: spill to an append-only tail instead of drop-oldest queue overflow *(curated)*
- **#11565**: fleet.state repos[].main_ci is 'unknown' for every repo on every host *(curated)*
- **#11639**: Verify and recover captain HTTPS fact losses: live drop counter +2277 in two hours *(curated)*
- **#11683**: Workspace draw seed = sha256(UTC minute ‖ host_id): reproducible from the timestamp, decorrelated across hosts *(curated)*
- **#11690**: Host-side test burden to a bare minimum: CI is the test gate, Builders don't run the suite (operator direction 2026-10-13) *(curated)*
- **#11711**: Audit every network input to loom-daemon: config, forge, wake-only webhook, or removed (2am#4300) *(curated)*
- **#11713**: Make operator.presence opt-in a clean one-step for operator hosts (hook wiring + journal export) *(curated)*

## Proposed (Architect / Hermit)

- **#9825**: Epic: audit actual agent build/test placement from evidence *(architect)*
- **#11347**: [Epic #9908] Phase 2c: Execution journal shadow appends from checkpoint CLI *(architect)*
- **#11689**: IPC server: bound client frame size (unbounded lines() reader, parse error echoes whole payload) *(architect)*
- **#11695**: Dedupe assert_eq/assert_contains helpers copied into 175 shell test suites *(hermit)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam
- **#7810**: [epic] Retire shell: 10 sequential PRs, two foundational then update + agent execution
- **#9825**: Epic: audit actual agent build/test placement from evidence
- **#9908**: Consolidate persistence: one journal per concern, retire duplicated stores
- **#9978**: feat(champion): optional per-repo merge-queue mode — enqueue Judge-approved PRs to GitHub's merge queue instead of merge-pr.sh's freshness/re-date loop
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day)
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue
- **#10456**: Epic: the fleet keeps its own hold queue clean — only real operator asks reach Needs attention
- **#10698**: Fleet version floor: the daemon enforces `loom_min_version` from the fleet store, rolls itself forward, and brings each repo's installed Loom up to its own version
- **#10721**: Gauges and status for host H0-H7 and workspace W states
- **#10916**: Fleet singleton output watchdog: alert when any one-host fleet job stops producing output (not just when its host goes silent)
- **#11189**: Epic: fleet workers stop filling their disks — bound per-sweep build output and reclaim what the reaper misses (loom-worker-1, 2026-10-09)
- **#11287**: Umbrella: Z.ai GLM-5.3-Flash seats landing code in the fleet via OpenCode

## Backlog Balance

| Bucket | Count |
|------|-------|
| Operator merge-risk holds | 21 |
| Very important | 0 |
| Important | 16 |
| Ready (`loom:issue`) | 11 |
| In Progress (`loom:building`) | 3 |
| PRs awaiting review | 2 |
| Approved PRs awaiting merge | 24 |
| Curated | 74 |
| Architect / Hermit proposals | 4 |
| Active epics | 15 |
<!-- guide:plan-body:end -->
