# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#10173**: feat: daemon-enforced hold for no-op dispatch loops (#10156)
- **#10180**: feat(daemon): cleared-blocker re-check on any issue/PR close (#10150)
- **#10339**: chore(deps): update clickhouse/clickhouse-keeper docker tag to v25.12.11
- **#10359**: feat(guards): guards.enabled master opt-out + guard false-positive fixes (#10335)
- **#11130**: Bound CI backfill admission to exporter capacity and expose burst pressure
- **#11165**: fix(ci_telemetry): stream the dedupe ledger and bound its seen set
- **#11212**: test: isolate parallel-cargo-test shared state (skip counters, bad-mark covering, lease re-acquire) (#9409)
- **#11214**: fix(classify-error): classify real Claude Code 401 and usage messages (#11205)
- **#11228**: fix(guard): rm-scope ignores python heredoc payload (#10422)
- **#11229**: docs: drop retired 2am decision-record citations from defaults/
- **#11231**: feat(watchdog): worktree + agent tool-process liveness signals (#9533)
- **#11233**: feat(role-runner): deterministic per-repo lane rule for judge and doctor (#10630)
- **#11235**: Pull Docker Hub test images through mirror.gcr.io with a bounded Hub fallback (#11232)
- **#11236**: fix(daemon): reconstruct keeps the claim of a dead leader whose group is alive (#11076)
- **#11238**: Reap agent process residue after any agent exit (#10802)
- **#11244**: Every-tick resync follow-ups from #10998 (#11003)
- **#11246**: feat(comment-trust): promotion author gate (#10827)
- **#11258**: fix(guard): sed -e/-f option arguments are not write targets (#11074)
- **#11261**: fix(auto_update): stop unsupervised re-downloads; typed below-floor not-rolling reason (#11042)
- **#11277**: fix(park-record,stale-blocked): ignore park markers quoted in code; vet release writes under root credential (Part of #10837)
- **#11282**: fix(daemon-update): don't predict a --fetch hard-fail when the release is being installed (#11070)
- **#11292**: feat(labels): prune undeclared loom:* labels; retire urgent, heavy, operator-objective (#11105)
- **#11302**: fix(fleet-sync): hold a checkout with resync_pending regardless of versions
- **#11303**: feat(worker): worker run --role builder|doctor (Slice 1 of #11285)
- **#11305**: Retire $LOOM_HYPERPARAMS vector tier and hyperparams digest (#11107)
- **#11314**: refactor: remove safehouse ChatOps (slice 3 of #11112)
- **#11316**: feat(runtime): Z.ai GLM first for Builder/Doctor, Judge excluded from zai-* (slice 1 of #11284)
- **#11318**: fix(egress-proxy): deliver observe records from the worker launcher via trace journals (#11300 slice 1)
- **#11322**: Rate-limit version bumps to one per RELEASE_MIN_INTERVAL (daily catch-up, forced hotfix path)

## Operator Priority

Issues the operator starred (`loom:operator-priority`); land these first.

- **#8256**: security: per-role tool allowlist — decision in a loom-daemon subcommand, thin guard/spawn enforcement (read-only roles can't reach ssh/aws/gh secret/~/.ssh)
- **#9533**: review-stall-watchdog kills healthy sweeps: sweep-log mtime is not a liveness signal
- **#10156**: Daemon re-dispatches human-gated/blocked issues forever after no-op sweeps (275 after-curator loops, ~600M tokens/24h)
- **#10196**: telemetry: make SigNoz point-in-time reconstructable — knowable-at timestamps, full fleet-state snapshots over OTLP, per-host export-coverage records, replay contract
- **#10337**: Dispatcher yields to its own leaseless loom:building label and orphans the issue (3 starred issues 2026-10-05 00:40Z)
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue
- **#10916**: Fleet singleton output watchdog: alert when any one-host fleet job stops producing output (not just when its host goes silent)
- **#11076**: Sweep released as dead while its wrapper retry is still running: OOM kill of one child stops the whole agent scope (OOMPolicy=stop)
- **#11094**: Work-finder RAM admission charges 2 GB per sweep, but a loom-daemon rustc peaks at ~13 GB: concurrent loom builds OOM 30 GB workers
- **#11115**: Bound CI backfill admission to exporter capacity and expose burst pressure
- **#11159**: ci_telemetry: the dedupe ledger's seen set is never pruned and is fully materialized every poll cycle (~250-330 B RSS per unit, grows with history)
- **#11179**: Dashboard session pane: links should open the dashboard view, not GitHub
- **#11205**: classify-error.sh misses real Claude Code 401/usage messages (OAuth access token invalid, session expired, org out of usage, bare 429)
- **#11208**: Ship the error classifier in loom-daemon (`loom-daemon classify-error`) so consumers stop vendoring classify-error.sh
- **#11283**: OpenCode 2.x guarded launches: load the native-tools plugin via the 2.x default-export shape, live-verify, flip guard_verified
- **#11284**: Z.ai GLM first for Builder/Doctor, never Judge: role preferences, admission exclusion, Doctor ticks, per-dispatch runtime, fill the seats
- **#11285**: Sweeps: run each phase on its role's runtime (GLM Builder/Doctor via a phase-worker subcommand; Judge/Curator never GLM)
- **#11286**: Z.ai api-keys pool fidelity: real exhaustion fixtures, reset-aligned cooldowns, per-account gauges, burn sampler misses guarded OpenCode stores
- **#11287**: Umbrella: Z.ai GLM-5.3-Flash seats landing code in the fleet via OpenCode
- **#11300**: Z.ai seat telemetry via the egress proxy in transparent observe mode: per-request tokens/latency/429s to SigNoz, OpenCode headers untouched
- **#11304**: Builder acquires a loom:building lease on an issue that is already closed ([related repository issue], 30 min after close)
- **#11358**: Mixed sweep review: independent Judge selection and checkpointed seat wait
- **#11359**: Mixed sweep Doctor: escalate to Claude after bounded GLM rejections
- **#11360**: Mixed sweep usage: verify phase launch records and per-tap journal accounting
- **#11367**: eta.stage_outcome: most wait-stage exits lack entered_at/dwell_sec (ready_wait 0 of 1,396), ready_wait 'unknown' floods, no pre-ready stages
- **#11368**: eta.stage_outcome: add pre-ready stages triage_wait and approval_wait

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8372**: Canary the uncommitted-work Stop guard in opted-in consumer workspaces with measured block outcomes
- **#8742**: loom:blocked is silently stripped from operator-ruled permanent blocks (unblock probe treats 'unparseable blocker' as 'no blocker')
- **#9255**: forge_events: event-gated per-workspace polling under a bounded staleness cap (ADR-0021 amendment step 2)
- **#9388**: Doctor rechecks the head SHA before pushing but not the labels before writing them
- **#9405**: guard: parse_force_ops() treats `|`, `&`, `||` inside $(cd <path> ... pwd) like `&&` — resolves cwd the shell never sets
- **#9487**: Doctor's --force-with-lease can succeed against a stale local tracking ref in a shared clone, silently overwriting a sibling Doctor's push
- **#9929**: Curator applies issue-lifecycle labels (loom:curating/loom:curated) to pull requests
- **#9973**: Flaky: worktree_cli::reset rescue test unexpectedly hits the live-process veto under nextest
- **#10023**: daemon: one stable host.id per machine, plus start/stop/heartbeat events in SigNoz
- **#10235**: Claim reconciler strips loom:reviewing after 30 min by label age alone, ignoring the Judge's activity (mid-review claim loss)
- **#10337**: Dispatcher yields to its own leaseless loom:building label and orphans the issue (3 starred issues 2026-10-05 00:40Z)
- **#10837**: Release pass (#10556) never released loom-ui#1695: all three park-record blockers closed ~22 h ago, still loom:blocked
- **#10916**: Fleet singleton output watchdog: alert when any one-host fleet job stops producing output (not just when its host goes silent)
- **#11086**: sweep-lease-renew.sh stop <arg> kills any PID, so an issue number passed by mistake signals an unrelated process
- **#11094**: Work-finder RAM admission charges 2 GB per sweep, but a loom-daemon rustc peaks at ~13 GB: concurrent loom builds OOM 30 GB workers
- **#11109**: Compatible-W3 hold follow-ups from #11084: resync_pending checkout clears the hold; late re-judge warning; roll-timer floor change
- **#11112**: Remove the safehouse (Matrix) integration from Loom: narration, ChatOps/Concierge, peer-claim channel (forge leases remain)
- **#11167**: merge-pr.sh diagnostics: fail-open roll-hint names its own floor (#9377) + wire Loom-Issue trailer warning (#9502)
- **#11232**: CI fails on Docker Hub's anonymous pull rate limit (Codex smoke, ClickHouse-backed OTLP tests): pull through a mirror with fallback and retry
- **#11284**: Z.ai GLM first for Builder/Doctor, never Judge: role preferences, admission exclusion, Doctor ticks, per-dispatch runtime, fill the seats
- **#11300**: Z.ai seat telemetry via the egress proxy in transparent observe mode: per-request tokens/latency/429s to SigNoz, OpenCode headers untouched
- **#11304**: Builder acquires a loom:building lease on an issue that is already closed ([related repository issue], 30 min after close)
- **#11345**: [Epic #9908] Phase 2a: Shared bounded journal core and verification seam

## In Progress

Issues currently being built (`loom:building`).

- **#8972**: tokens check --source probe does not probe a bad-marked account — it echoes the stored reason, so a stale bad-mark reads as confirmed
- **#11367**: eta.stage_outcome: most wait-stage exits lack entered_at/dwell_sec (ready_wait 0 of 1,396), ready_wait 'unknown' floods, no pre-ready stages

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#11157**: feat(guard): opt-in consumer canary for the uncommitted-work Stop guard (#8372)
- **#11321**: feat(doctor): add forge doctor-handback, a verified add-first hand-back verb
- **#11364**: fix(dispatch): post-claim closed-issue re-verification (#11304)

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#10173**: feat: daemon-enforced hold for no-op dispatch loops (#10156)
- **#10180**: feat(daemon): cleared-blocker re-check on any issue/PR close (#10150)
- **#10274**: loom update: provision user-scope skills and ff-sync machine checkout (#10238)
- **#10339**: chore(deps): update clickhouse/clickhouse-keeper docker tag to v25.12.11
- **#10355**: chore(deps): update rust to v1.98.1
- **#10359**: feat(guards): guards.enabled master opt-out + guard false-positive fixes (#10335)
- **#10602**: feat(stale_blocked): hand undocumented loom:blocked to Curator via loom:blocked-unnamed (#10558)
- **#10968**: feat(merge-pr): port the already-merged / closed terminal-state gate to Rust (#8191 slice)
- **#11010**: telemetry: carry IE1's seconds partition into the sweep_facts bundle (#9507)
- **#11063**: feat(fleet-alert): wire the singleton output watchdog to a real OutputSource (#10916)
- **#11130**: Bound CI backfill admission to exporter capacity and expose burst pressure
- **#11140**: CI gate: require CONTROL_VERSION bump when private-control POLICY changes (#8858)
- **#11152**: feat(guard): per-role tool restriction via loom-daemon role-tool-policy check (#8256)
- **#11165**: fix(ci_telemetry): stream the dedupe ledger and bound its seen set
- **#11184**: feat(spawn-claude): MemoryMax for agent scopes from observed per-repo peak (#11094 slice 2)
- **#11212**: test: isolate parallel-cargo-test shared state (skip counters, bad-mark covering, lease re-acquire) (#9409)
- **#11214**: fix(classify-error): classify real Claude Code 401 and usage messages (#11205)
- **#11228**: fix(guard): rm-scope ignores python heredoc payload (#10422)
- **#11229**: docs: drop retired 2am decision-record citations from defaults/
- **#11231**: feat(watchdog): worktree + agent tool-process liveness signals (#9533)
- **#11233**: feat(role-runner): deterministic per-repo lane rule for judge and doctor (#10630)
- **#11235**: Pull Docker Hub test images through mirror.gcr.io with a bounded Hub fallback (#11232)
- **#11236**: fix(daemon): reconstruct keeps the claim of a dead leader whose group is alive (#11076)
- **#11238**: Reap agent process residue after any agent exit (#10802)
- **#11244**: Every-tick resync follow-ups from #10998 (#11003)
- **#11246**: feat(comment-trust): promotion author gate (#10827)
- **#11258**: fix(guard): sed -e/-f option arguments are not write targets (#11074)
- **#11261**: fix(auto_update): stop unsupervised re-downloads; typed below-floor not-rolling reason (#11042)
- **#11277**: fix(park-record,stale-blocked): ignore park markers quoted in code; vet release writes under root credential (Part of #10837)
- **#11282**: fix(daemon-update): don't predict a --fetch hard-fail when the release is being installed (#11070)
- **#11292**: feat(labels): prune undeclared loom:* labels; retire urgent, heavy, operator-objective (#11105)
- **#11302**: fix(fleet-sync): hold a checkout with resync_pending regardless of versions
- **#11303**: feat(worker): worker run --role builder|doctor (Slice 1 of #11285)
- **#11305**: Retire $LOOM_HYPERPARAMS vector tier and hyperparams digest (#11107)
- **#11314**: refactor: remove safehouse ChatOps (slice 3 of #11112)
- **#11316**: feat(runtime): Z.ai GLM first for Builder/Doctor, Judge excluded from zai-* (slice 1 of #11284)
- **#11318**: fix(egress-proxy): deliver observe records from the worker launcher via trace journals (#11300 slice 1)
- **#11322**: Rate-limit version bumps to one per RELEASE_MIN_INTERVAL (daily catch-up, forced hotfix path)

## Proposed

Issues carrying `loom:curated`.

- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
- **#8256**: security: per-role tool allowlist — decision in a loom-daemon subcommand, thin guard/spawn enforcement (read-only roles can't reach ssh/aws/gh secret/~/.ssh) *(curated)*
- **#8372**: Canary the uncommitted-work Stop guard in opted-in consumer workspaces with measured block outcomes *(curated)*
- **#8434**: live-verify native-ephemeral containment: canary run in-container, two-worker filesystem disjointness, post-run writable-layer credential scan *(curated)*
- **#8525**: tracing: instrument sweep phases and role attempts, including Pi/OpenCode and repair cycles *(curated)*
- **#8570**: Guard: refuse a build/scratch dir assignment that resolves onto a tmpfs mount (upstream PR to rjwalters/repo, split from #8512) *(curated)*
- **#8698**: Live end-to-end verification of the credential egress proxy (AC5 of #8674) *(curated)*
- **#8742**: loom:blocked is silently stripped from operator-ruled permanent blocks (unblock probe treats 'unparseable blocker' as 'no blocker') *(curated)*
- **#8858**: private-control bundle: CONTROL_VERSION bump is unenforced convention, not checked by CI *(curated)*
- **#8876**: loom-daemon stashes can never retire anything on a private repo: it reads issue state via ambient gh auth, not the App credential the daemon already mints *(curated)*
- **#8913**: observability: live-verify ci.job.log reconstruction in SigNoz and record it in evidence.md (#8825 AC1) *(curated)*
- **#8942**: Champion Step 4 negation-guard reopen doesn't confirm the close came from this merge *(curated)*
- **#8950**: Live-verify Pi tokens_by_model against a real LOOM_RUNTIME=pi launch *(curated)*
- **#8972**: tokens check --source probe does not probe a bad-marked account — it echoes the stored reason, so a stale bad-mark reads as confirmed *(curated)*
- **#9006**: observability: live-verify the SigNoz queue-starvation alert rule fires and resolves (#8856 / PR #8935 follow-up) *(curated)*
- **#9049**: Security: no secrets under any repo/worktree — move all credential state to ~/.loom (21 Claude OAuth tokens leaked, 2nd incident) *(curated)*
- **#9136**: main ruleset: bypass_actors 'RepositoryRole:always' makes pull_request and required_status_checks advisory *(curated)*
- **#9255**: forge_events: event-gated per-workspace polling under a bounded staleness cap (ADR-0021 amendment step 2) *(curated)*
- **#9388**: Doctor rechecks the head SHA before pushing but not the labels before writing them *(curated)*
- **#9405**: guard: parse_force_ops() treats `|`, `&`, `||` inside $(cd <path> ... pwd) like `&&` — resolves cwd the shell never sets *(curated)*
- **#9409**: test: POOL_EXHAUSTED_SKIP_COUNT delta assertions race under parallel cargo test (flaky role_runner / runtime_preflight tests) *(curated)*
- **#9487**: Doctor's --force-with-lease can succeed against a stale local tracking ref in a shared clone, silently overwriting a sibling Doctor's push *(curated)*
- **#9507**: telemetry: split per-issue effort into clean/substantive/environmental SECONDS, not event counts (#9444 criterion 4) *(curated)*
- **#9533**: review-stall-watchdog kills healthy sweeps: sweep-log mtime is not a liveness signal *(curated)*
- **#9548**: Authenticate Loom's control signals: markers and phrases count only from trusted authors (work safely on any public repo) *(curated)*
- **#9549**: premise-check: a bare 'What happened' heading gates ordinary bug reports as incident reports *(curated)*
- **#9735**: auto_update: roll back candidates that fail startup and quarantine failed artifacts *(curated)*
- **#9908**: Consolidate persistence: one journal per concern, retire duplicated stores *(curated)*
- **#9929**: Curator applies issue-lifecycle labels (loom:curating/loom:curated) to pull requests *(curated)*
- **#9973**: Flaky: worktree_cli::reset rescue test unexpectedly hits the live-process veto under nextest *(curated)*
- **#9978**: feat(champion): optional per-repo merge-queue mode — enqueue Judge-approved PRs to GitHub's merge queue instead of merge-pr.sh's freshness/re-date loop *(curated)*
- **#10001**: "Only a human can do this" is for PO-level decisions only; Curator reviews every parked item and returns what agents can do *(curated)*
- **#10009**: Operator-label review (Lane B of #10001): give every parked item in the wired repos a verdict and return what agents can do *(curated)*
- **#10023**: daemon: one stable host.id per machine, plus start/stop/heartbeat events in SigNoz *(curated)*
- **#10150**: notify-cleared-blockers runs only from merge-pr.sh: merges made any other way never clear a block *(curated)*
- **#10156**: Daemon re-dispatches human-gated/blocked issues forever after no-op sweeps (275 after-curator loops, ~600M tokens/24h) *(curated)*
- **#10196**: telemetry: make SigNoz point-in-time reconstructable — knowable-at timestamps, full fleet-state snapshots over OTLP, per-host export-coverage records, replay contract *(curated)*
- **#10229**: sweep-lease-renew.sh spends the operator's personal REST pool at ~600 calls/h and climbing; one sweep id runs ~16 renew loops *(curated)*
- **#10235**: Claim reconciler strips loom:reviewing after 30 min by label age alone, ignoring the Judge's activity (mid-review claim loss) *(curated)*
- **#10238**: loom update never provisions or refreshes user-scope skills on an existing (daemonless) machine — /loom:star unfindable *(curated)*
- **#10257**: merge queue Phase C: combined-tree CI qualification and eligible-repo live pilot (parent #9978) *(curated)*
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day) *(curated)*
- **#10335**: Per-repo opt-out for the PreToolUse guard hooks that install/upgrade respects *(curated)*
- **#10337**: Dispatcher yields to its own leaseless loom:building label and orphans the issue (3 starred issues 2026-10-05 00:40Z) *(curated)*
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue *(curated)*
- **#10456**: Epic: the fleet keeps its own hold queue clean — only real operator asks reach Needs attention *(curated)*
- **#10470**: security(update): enforce independent release approval and artifact assurance for managed fleets *(curated)*
- **#10512**: Fleet reader-App pool near exhaustion: cut top billable pollers (visibility.repo cache, issue_state ETag, cross-host dedupe) — ~8k REST/h *(curated)*
- **#10558**: Undocumented loom:blocked (no blocker named) is handed to Curator each tick: name the blocker or release it *(curated)*
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
- **#10924**: Fleet singleton output watchdog, slice 2: wire fleet_outputs into fleet_alert + SigNoz rule *(curated)*
- **#11003**: Every-tick resync: follow-ups from #10998 (breaker re-check before fallback/claim, per-owner budget, host-level credential alert, empty and archived repos) *(curated)*
- **#11042**: Floor-loop follow-ups: unsupervised hosts re-download every tick; typed not-rolling reason; status enabled semantics *(curated)*
- **#11062**: ci_telemetry journal follow-ups from #11047: rotation/cursor crash race, cursor fsync, export starvation, guard gaps *(curated)*
- **#11064**: daemon-update floor guard follow-ups from #11050: check before the source sync, operator-side fleet roll warning, detection edges *(curated)*
- **#11070**: daemon-update --fetch: 'artifact path cannot reach current source' contradicts the fetch that follows *(curated)*
- **#11074**: Guard: worktree-write-confinement denies sed editing an external temporary PR body *(curated)*
- **#11076**: Sweep released as dead while its wrapper retry is still running: OOM kill of one child stops the whole agent scope (OOMPolicy=stop) *(curated)*
- **#11086**: sweep-lease-renew.sh stop <arg> kills any PID, so an issue number passed by mistake signals an unrelated process *(curated)*
- **#11094**: Work-finder RAM admission charges 2 GB per sweep, but a loom-daemon rustc peaks at ~13 GB: concurrent loom builds OOM 30 GB workers *(curated)*
- **#11103**: Priority model: loom:important / loom:very-important for everyone; weighted workspace pick, then level → oldest → number *(curated)*
- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default) *(curated)*
- **#11109**: Compatible-W3 hold follow-ups from #11084: resync_pending checkout clears the hold; late re-judge warning; roll-timer floor change *(curated)*
- **#11110**: Roll-pause hook wiring follow-ups from #11082: user-scope double registration, narrow-matcher coverage, settings hooks never resynced *(curated)*
- **#11112**: Remove the safehouse (Matrix) integration from Loom: narration, ChatOps/Concierge, peer-claim channel (forge leases remain) *(curated)*
- **#11115**: Bound CI backfill admission to exporter capacity and expose burst pressure *(curated)*
- **#11159**: ci_telemetry: the dedupe ledger's seen set is never pruned and is fully materialized every poll cycle (~250-330 B RSS per unit, grows with history) *(curated)*
- **#11167**: merge-pr.sh diagnostics: fail-open roll-hint names its own floor (#9377) + wire Loom-Issue trailer warning (#9502) *(curated)*
- **#11174**: Release cadence: at most one version bump per 24 h (accumulate merges; daily catch-up; manual hotfix override) *(curated)*
- **#11189**: Epic: fleet workers stop filling their disks — bound per-sweep build output and reclaim what the reaper misses (fleet worker, 2026-10-09) *(curated)*
- **#11193**: Worktree reaper keeps merged pr-<N> worktrees forever when an agent-made cargo target dir is inside (15 GB on fleet worker) *(curated)*
- **#11194**: Agent-made judge-<PR> worktrees are invisible to every reclaim path (loom-ui: 21 merged-PR worktrees, 8.3 GB) *(curated)*
- **#11208**: Ship the error classifier in loom-daemon (`loom-daemon classify-error`) so consumers stop vendoring classify-error.sh *(curated)*
- **#11283**: OpenCode 2.x guarded launches: load the native-tools plugin via the 2.x default-export shape, live-verify, flip guard_verified *(curated)*
- **#11284**: Z.ai GLM first for Builder/Doctor, never Judge: role preferences, admission exclusion, Doctor ticks, per-dispatch runtime, fill the seats *(curated)*
- **#11285**: Sweeps: run each phase on its role's runtime (GLM Builder/Doctor via a phase-worker subcommand; Judge/Curator never GLM) *(curated)*
- **#11286**: Z.ai api-keys pool fidelity: real exhaustion fixtures, reset-aligned cooldowns, per-account gauges, burn sampler misses guarded OpenCode stores *(curated)*
- **#11287**: Umbrella: Z.ai GLM-5.3-Flash seats landing code in the fleet via OpenCode *(curated)*
- **#11300**: Z.ai seat telemetry via the egress proxy in transparent observe mode: per-request tokens/latency/429s to SigNoz, OpenCode headers untouched *(curated)*
- **#11304**: Builder acquires a loom:building lease on an issue that is already closed ([related repository issue], 30 min after close) *(curated)*
- **#11345**: [Epic #9908] Phase 2a: Shared bounded journal core and verification seam *(curated)*
- **#11358**: Mixed sweep review: independent Judge selection and checkpointed seat wait *(curated)*
- **#11367**: eta.stage_outcome: most wait-stage exits lack entered_at/dwell_sec (ready_wait 0 of 1,396), ready_wait 'unknown' floods, no pre-ready stages *(curated)*

## Proposed (Architect / Hermit)

- **#9825**: Epic: audit actual agent build/test placement from evidence *(architect)*
- **#11345**: [Epic #9908] Phase 2a: Shared bounded journal core and verification seam *(architect)*
- **#11346**: [Epic #9908] Phase 2b: Execution journal shadow writes from daemon producers *(architect)*
- **#11347**: [Epic #9908] Phase 2c: Execution journal shadow appends from checkpoint CLI *(architect)*
- **#11348**: [Epic #9908] Phase 2d: Telemetry journal shadow streams and native append entry point *(architect)*
- **#11349**: [Epic #9908] Phase 2e: Configuration snapshot journal shadow records *(architect)*

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
- **#11189**: Epic: fleet workers stop filling their disks — bound per-sweep build output and reclaim what the reaper misses (fleet worker, 2026-10-09)
- **#11287**: Umbrella: Z.ai GLM-5.3-Flash seats landing code in the fleet via OpenCode

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 29 |
| Operator priority | 26 |
| Ready (`loom:issue`) | 25 |
| In Progress (`loom:building`) | 2 |
| PRs awaiting review | 3 |
| Approved PRs awaiting merge | 38 |
| Curated | 92 |
| Architect / Hermit proposals | 6 |
| Active epics | 12 |
<!-- guide:plan-body:end -->
