# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#10274**: loom update: provision user-scope skills and ff-sync machine checkout (#10238)
- **#10280**: Keep long-running review claims alive on Judge activity and claimant force-pushes (#10235)
- **#10339**: chore(deps): update clickhouse/clickhouse-keeper docker tag to v25.12.11
- **#10359**: feat(guards): guards.enabled master opt-out + guard false-positive fixes (#10335)
- **#10362**: chore(deps): update clickhouse/clickhouse-keeper docker tag to v26
- **#10410**: test+docs(lease-renew): define release signal, pin cached-read failure (Part of #10229)
- **#10602**: feat(stale_blocked): hand undocumented loom:blocked to Curator via loom:blocked-unnamed (#10558)
- **#10882**: feat(watchdog): opt-in bounded supervised recovery for a wedged daemon (#7855)
- **#10968**: feat(merge-pr): port the already-merged / closed terminal-state gate to Rust (#8191 slice)
- **#11010**: telemetry: carry IE1's seconds partition into the sweep_facts bundle (#9507)

## Operator Priority

Issues the operator starred (`loom:operator-priority`); land these first.

- **#8256**: security: per-role tool allowlist — decision in a loom-daemon subcommand, thin guard/spawn enforcement (read-only roles can't reach ssh/aws/gh secret/~/.ssh)
- **#9533**: review-stall-watchdog kills healthy sweeps: sweep-log mtime is not a liveness signal
- **#9935**: builder role_attempt spans are zero-duration by construction — SigNoz's builder stage distribution is unusable
- **#10156**: Daemon re-dispatches human-gated/blocked issues forever after no-op sweeps (275 after-curator loops, ~600M tokens/24h)
- **#10196**: telemetry: make SigNoz point-in-time reconstructable — knowable-at timestamps, full fleet-state snapshots over OTLP, per-host export-coverage records, replay contract
- **#10337**: Dispatcher yields to its own leaseless loom:building label and orphans the issue (3 starred issues 2026-10-05 00:40Z)
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue
- **#10916**: Fleet singleton output watchdog: alert when any one-host fleet job stops producing output (not just when its host goes silent)
- **#11075**: Quarantine stashes capture cargo target dirs (tens of GB into refs/stash): auto-gc repack then OOM-kills agents
- **#11076**: Sweep released as dead while its wrapper retry is still running: OOM kill of one child stops the whole agent scope (OOMPolicy=stop)
- **#11094**: Work-finder RAM admission charges 2 GB per sweep, but a loom-daemon rustc peaks at ~13 GB: concurrent loom builds OOM 30 GB workers
- **#11098**: Remove the ETA subsystem from Loom (moved to loom-ui) — staged
- **#11114**: Attribute residual Linux daemon memory after streamed CI journal export
- **#11115**: Bound CI backfill admission to exporter capacity and expose burst pressure

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8742**: loom:blocked is silently stripped from operator-ruled permanent blocks (unblock probe treats 'unparseable blocker' as 'no blocker')
- **#8849**: Read llm-monitor's new data dir (~/.llm-monitor / LOOM_LLM_MONITOR_DIR), keeping ~/.claude-monitor fallback
- **#8858**: private-control bundle: CONTROL_VERSION bump is unenforced convention, not checked by CI
- **#9063**: Epic: order overlapping PRs first, then add automatic merge consolidation
- **#9064**: fleet add-worker verify always fails on a work-finder-off worker: loom-daemon status exits 5 (EXIT_AUTONOMY_MISMATCH), read as 'not ready'
- **#9255**: forge_events: event-gated per-workspace polling under a bounded staleness cap (ADR-0021 amendment step 2)
- **#9356**: ci: 'Native Port Suites' steps lack !cancelled() guards; #9118 comment overstates coverage
- **#9405**: guard: parse_force_ops() treats `|`, `&`, `||` inside $(cd <path> ... pwd) like `&&` — resolves cwd the shell never sets
- **#9409**: test: POOL_EXHAUSTED_SKIP_COUNT delta assertions race under parallel cargo test (flaky role_runner / runtime_preflight tests)
- **#9418**: Renovate: Dependency Dashboard is enabled by config:recommended (unlabelled bot issue), and renovate.json5's loom:review-requested label has no structural guard
- **#9487**: Doctor's --force-with-lease can succeed against a stale local tracking ref in a shared clone, silently overwriting a sibling Doctor's push
- **#9518**: prless-retry fleet tally can double-count its own releases on a host whose identity is UNKNOWN_HOST
- **#9533**: review-stall-watchdog kills healthy sweeps: sweep-log mtime is not a liveness signal
- **#9714**: create-issue.sh falls back to REST only on rate-limit errors, not on other GraphQL failures
- **#9781**: Daemon dispatch: don't admit two same-repo candidates with overlapping Curator affected files in one tick (mirror sweep's #4161 wave rule)
- **#9927**: claim-staleness.sh stand-down message omits the bounded-fallback age floor, reading as 'overdue' when it is not
- **#9929**: Curator applies issue-lifecycle labels (loom:curating/loom:curated) to pull requests
- **#9973**: Flaky: worktree_cli::reset rescue test unexpectedly hits the live-process veto under nextest
- **#10023**: daemon: one stable host.id per machine, plus start/stop/heartbeat events in SigNoz
- **#10150**: notify-cleared-blockers runs only from merge-pr.sh: merges made any other way never clear a block
- **#10156**: Daemon re-dispatches human-gated/blocked issues forever after no-op sweeps (275 after-curator loops, ~600M tokens/24h)
- **#10196**: telemetry: make SigNoz point-in-time reconstructable — knowable-at timestamps, full fleet-state snapshots over OTLP, per-host export-coverage records, replay contract
- **#10229**: sweep-lease-renew.sh spends the operator's personal REST pool at ~600 calls/h and climbing; one sweep id runs ~16 renew loops
- **#10337**: Dispatcher yields to its own leaseless loom:building label and orphans the issue (3 starred issues 2026-10-05 00:40Z)
- **#10414**: Workers stuck on 0.19.701 since ~01:30Z (12 releases behind) and their fleet-refresh task went silent at the same moment — no auto-update telemetry or version-lag alert
- **#10415**: Auditor: check-ci-status reports success when CI is cancelled before build and tests
- **#10422**: Guard: rm-scope scans Python heredoc payload as a live rm command
- **#10630**: Judge/Doctor lanes per repo scale with that repo's own debt (deterministic; replaces the marginal-value allocator)
- **#10687**: Auditor: update executable fixtures when changing worker environment or CLI help
- **#10802**: General cleanup: reap agent process residue (dev servers, orphaned children) after any agent exit
- **#10827**: Promotion author gate: don't auto-promote issues authored by untrusted identities (signed decision markers dropped)
- **#10837**: Release pass (#10556) never released loom-ui#1695: all three park-record blockers closed ~22 h ago, still loom:blocked
- **#10845**: CI: the cfg(not(otlp)) targeted step should fail when tests-run != tests-derived; fix stale OTLP-family doc comments
- **#10846**: CI: move the loom-worker :buildcache write into a main-only job (no packages:write on PR tokens)
- **#10870**: MCP pre-flight smoke test ignores the server's .mcp.json env block
- **#10875**: Champion re-arms a released merge-risk hold after a tree-identical re-date commit
- **#10880**: auto-update persistence: follow-ups from #10877 (floor roll in a consumed window, rollback refetch loop, hardening)
- **#10916**: Fleet singleton output watchdog: alert when any one-host fleet job stops producing output (not just when its host goes silent)
- **#11003**: Every-tick resync: follow-ups from #10998 (breaker re-check before fallback/claim, per-owner budget, host-level credential alert, empty and archived repos)
- **#11024**: Build-slot test isolation follow-ups: one unserialised guard holder, and daemons spawned by the integration harness
- **#11029**: Floor roll target: walk back to the newest release >= floor with assets; make an unresolved below-floor host loud
- **#11042**: Floor-loop follow-ups: unsupervised hosts re-download every tick; typed not-rolling reason; status enabled semantics
- **#11066**: Flaky ci_telemetry tests: parallel runs race on process-global env vars
- **#11068**: Auditor: scoped test passes miss required Rust lint checks
- **#11074**: Guard: worktree-write-confinement denies sed editing an external temporary PR body
- **#11075**: Quarantine stashes capture cargo target dirs (tens of GB into refs/stash): auto-gc repack then OOM-kills agents
- **#11076**: Sweep released as dead while its wrapper retry is still running: OOM kill of one child stops the whole agent scope (OOMPolicy=stop)
- **#11083**: Parallel H4 stop follow-ups from #11081: parked-at-bound disposition, group-only kill at the bound, record race, forced-stop count
- **#11086**: sweep-lease-renew.sh stop <arg> kills any PID, so an issue number passed by mistake signals an unrelated process
- **#11087**: Remove mail from Loom: human asks are signaled by labels only (notifier moves to 2am workers)
- **#11094**: Work-finder RAM admission charges 2 GB per sweep, but a loom-daemon rustc peaks at ~13 GB: concurrent loom builds OOM 30 GB workers
- **#11103**: Priority model: loom:important / loom:very-important for everyone; weighted workspace pick, then level → oldest → number
- **#11105**: Simplify the label set: delete undeclared labels, retire unused ones (urgent, heavy, operator-objective)
- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default)
- **#11109**: Compatible-W3 hold follow-ups from #11084: resync_pending checkout clears the hold; late re-judge warning; roll-timer floor change
- **#11112**: Remove the safehouse (Matrix) integration from Loom: narration, ChatOps/Concierge, peer-claim channel (forge leases remain)

## In Progress

Issues currently being built (`loom:building`).

- **#8256**: security: per-role tool allowlist — decision in a loom-daemon subcommand, thin guard/spawn enforcement (read-only roles can't reach ssh/aws/gh secret/~/.ssh)
- **#9512**: loom-daemon: reaper_sweep_exited_event_carries_no_progress_classification can hang forever, wedging the whole lib suite behind its #[serial] lock
- **#9935**: builder role_attempt spans are zero-duration by construction — SigNoz's builder stage distribution is unusable
- **#11071**: Idle worktrees keep full cargo target/ caches indefinitely — 40 GB of worker-1's 2026-10-09 fill, uncovered by the leak fixes
- **#11098**: Remove the ETA subsystem from Loom (moved to loom-ui) — staged
- **#11111**: Deliver #11058's supervision settings to existing hosts: daemon writes a systemd drop-in at startup; reset-failed before operator start
- **#11114**: Attribute residual Linux daemon memory after streamed CI journal export
- **#11115**: Bound CI backfill admission to exporter capacity and expose burst pressure

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#10065**: feat(daemon): stable host.id per machine plus start/shutdown/heartbeat telemetry (#10023)
- **#11099**: fix(quarantine): never stash cargo target trees into refs/stash (#11075)
- **#11102**: spawn-claude: OOMPolicy=continue for agent scopes (#11076)
- **#11106**: feat(ram): observed per-repo memory peak admission charge (#11094 slice 1)

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#10274**: loom update: provision user-scope skills and ff-sync machine checkout (#10238)
- **#10280**: Keep long-running review claims alive on Judge activity and claimant force-pushes (#10235)
- **#10283**: feat(telemetry): fleet.state OTLP kind with hourly anchor (slice 2 of #10196)
- **#10339**: chore(deps): update clickhouse/clickhouse-keeper docker tag to v25.12.11
- **#10355**: chore(deps): update rust to v1.98.1
- **#10359**: feat(guards): guards.enabled master opt-out + guard false-positive fixes (#10335)
- **#10362**: chore(deps): update clickhouse/clickhouse-keeper docker tag to v26
- **#10410**: test+docs(lease-renew): define release signal, pin cached-read failure (Part of #10229)
- **#10602**: feat(stale_blocked): hand undocumented loom:blocked to Curator via loom:blocked-unnamed (#10558)
- **#10882**: feat(watchdog): opt-in bounded supervised recovery for a wedged daemon (#7855)
- **#10968**: feat(merge-pr): port the already-merged / closed terminal-state gate to Rust (#8191 slice)
- **#11010**: telemetry: carry IE1's seconds partition into the sweep_facts bundle (#9507)
- **#11063**: feat(fleet-alert): wire the singleton output watchdog to a real OutputSource (#10916)
- **#11082**: fix(roll-pause): wire the pause hook at launch so consumer-repo sweeps reach a safe point
- **#11085**: fix(daemon_update): stale-entry-point advisory no longer flags the rollback copy or says rm for unprunable entries (#11069)
- **#11093**: fix(daemon): survive a child OOM kill and relaunch a failed daemon (#11058)

## Proposed

Issues carrying `loom:curated`.

- **#7855**: robb-pro daemon stopped heartbeating and answering IPC for 25 min while still logging; the watchdog CONFIRMED the wedge twice and never restarted it *(curated)*
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
- **#8256**: security: per-role tool allowlist — decision in a loom-daemon subcommand, thin guard/spawn enforcement (read-only roles can't reach ssh/aws/gh secret/~/.ssh) *(curated)*
- **#8372**: Wire the uncommitted-work Stop guard into consumer repos once its false-positive rate is known *(curated)*
- **#8434**: live-verify native-ephemeral containment: canary run in-container, two-worker filesystem disjointness, post-run writable-layer credential scan *(curated)*
- **#8525**: tracing: instrument sweep phases and role attempts, including Pi/OpenCode and repair cycles *(curated)*
- **#8570**: Guard: refuse a build/scratch dir assignment that resolves onto a tmpfs mount (upstream PR to rjwalters/repo, split from #8512) *(curated)*
- **#8698**: Live end-to-end verification of the credential egress proxy (AC5 of #8674) *(curated)*
- **#8726**: resync-ignore pins record no fork point: 'can this pin be lifted yet?' is archaeology, not a diff *(curated)*
- **#8742**: loom:blocked is silently stripped from operator-ruled permanent blocks (unblock probe treats 'unparseable blocker' as 'no blocker') *(curated)*
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
- **#9136**: main ruleset: bypass_actors 'RepositoryRole:always' makes pull_request and required_status_checks advisory *(curated)*
- **#9152**: worktree-link: an already-created worktree keeps its pnpm node_modules alias (#8944 leaves existing worktrees unfixed) *(curated)*
- **#9192**: merge-pr.sh: "Could not fetch PR #N" hides the real cause — forge_get_pr_nocache discards stderr *(curated)*
- **#9255**: forge_events: event-gated per-workspace polling under a bounded staleness cap (ADR-0021 amendment step 2) *(curated)*
- **#9308**: dep-recheck-fingerprint: CONCLUSION_HASH not stable across loom-daemon versions for identical input, defeats Curator idempotency suppression *(curated)*
- **#9356**: ci: 'Native Port Suites' steps lack !cancelled() guards; #9118 comment overstates coverage *(curated)*
- **#9405**: guard: parse_force_ops() treats `|`, `&`, `||` inside $(cd <path> ... pwd) like `&&` — resolves cwd the shell never sets *(curated)*
- **#9409**: test: POOL_EXHAUSTED_SKIP_COUNT delta assertions race under parallel cargo test (flaky role_runner / runtime_preflight tests) *(curated)*
- **#9418**: Renovate: Dependency Dashboard is enabled by config:recommended (unlabelled bot issue), and renovate.json5's loom:review-requested label has no structural guard *(curated)*
- **#9487**: Doctor's --force-with-lease can succeed against a stale local tracking ref in a shared clone, silently overwriting a sibling Doctor's push *(curated)*
- **#9492**: Decide which other required CI jobs the local build gate should mirror (follow-up to #9140) *(curated)*
- **#9507**: telemetry: split per-issue effort into clean/substantive/environmental SECONDS, not event counts (#9444 criterion 4) *(curated)*
- **#9512**: loom-daemon: reaper_sweep_exited_event_carries_no_progress_classification can hang forever, wedging the whole lib suite behind its #[serial] lock *(curated)*
- **#9518**: prless-retry fleet tally can double-count its own releases on a host whose identity is UNKNOWN_HOST *(curated)*
- **#9533**: review-stall-watchdog kills healthy sweeps: sweep-log mtime is not a liveness signal *(curated)*
- **#9548**: Authenticate Loom's control signals: markers and phrases count only from trusted authors (work safely on any public repo) *(curated)*
- **#9714**: create-issue.sh falls back to REST only on rate-limit errors, not on other GraphQL failures *(curated)*
- **#9735**: auto_update: roll back candidates that fail startup and quarantine failed artifacts *(curated)*
- **#9927**: claim-staleness.sh stand-down message omits the bounded-fallback age floor, reading as 'overdue' when it is not *(curated)*
- **#9929**: Curator applies issue-lifecycle labels (loom:curating/loom:curated) to pull requests *(curated)*
- **#9935**: builder role_attempt spans are zero-duration by construction — SigNoz's builder stage distribution is unusable *(curated)*
- **#9973**: Flaky: worktree_cli::reset rescue test unexpectedly hits the live-process veto under nextest *(curated)*
- **#9978**: feat(champion): optional per-repo merge-queue mode — enqueue Judge-approved PRs to GitHub's merge queue instead of merge-pr.sh's freshness/re-date loop *(curated)*
- **#10000**: An agent asks a human only by a decision or a mail; operator labels stay engine-internal *(curated)*
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
- **#10414**: Workers stuck on 0.19.701 since ~01:30Z (12 releases behind) and their fleet-refresh task went silent at the same moment — no auto-update telemetry or version-lag alert *(curated)*
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
- **#10845**: CI: the cfg(not(otlp)) targeted step should fail when tests-run != tests-derived; fix stale OTLP-family doc comments *(curated)*
- **#10846**: CI: move the loom-worker :buildcache write into a main-only job (no packages:write on PR tokens) *(curated)*
- **#10870**: MCP pre-flight smoke test ignores the server's .mcp.json env block *(curated)*
- **#10875**: Champion re-arms a released merge-risk hold after a tree-identical re-date commit *(curated)*
- **#10880**: auto-update persistence: follow-ups from #10877 (floor roll in a consumed window, rollback refetch loop, hardening) *(curated)*
- **#10913**: forge_events: hold read caches (gh-cached, pipeline_snapshot) under the poll-gating health signal (#9255 slice 2) *(curated)*
- **#10916**: Fleet singleton output watchdog: alert when any one-host fleet job stops producing output (not just when its host goes silent) *(curated)*
- **#10924**: Fleet singleton output watchdog, slice 2: wire fleet_outputs into fleet_alert + SigNoz rule *(curated)*
- **#11003**: Every-tick resync: follow-ups from #10998 (breaker re-check before fallback/claim, per-owner budget, host-level credential alert, empty and archived repos) *(curated)*
- **#11024**: Build-slot test isolation follow-ups: one unserialised guard holder, and daemons spawned by the integration harness *(curated)*
- **#11029**: Floor roll target: walk back to the newest release >= floor with assets; make an unresolved below-floor host loud *(curated)*
- **#11042**: Floor-loop follow-ups: unsupervised hosts re-download every tick; typed not-rolling reason; status enabled semantics *(curated)*
- **#11062**: ci_telemetry journal follow-ups from #11047: rotation/cursor crash race, cursor fsync, export starvation, guard gaps *(curated)*
- **#11064**: daemon-update floor guard follow-ups from #11050: check before the source sync, operator-side fleet roll warning, detection edges *(curated)*
- **#11066**: Flaky ci_telemetry tests: parallel runs race on process-global env vars *(curated)*
- **#11069**: Stale-entry-point advisory flags the rollback copy loom-daemon.previous and tells the operator to rm it *(curated)*
- **#11070**: daemon-update --fetch: 'artifact path cannot reach current source' contradicts the fetch that follows *(curated)*
- **#11071**: Idle worktrees keep full cargo target/ caches indefinitely — 40 GB of worker-1's 2026-10-09 fill, uncovered by the leak fixes *(curated)*
- **#11075**: Quarantine stashes capture cargo target dirs (tens of GB into refs/stash): auto-gc repack then OOM-kills agents *(curated)*
- **#11076**: Sweep released as dead while its wrapper retry is still running: OOM kill of one child stops the whole agent scope (OOMPolicy=stop) *(curated)*
- **#11083**: Parallel H4 stop follow-ups from #11081: parked-at-bound disposition, group-only kill at the bound, record race, forced-stop count *(curated)*
- **#11086**: sweep-lease-renew.sh stop <arg> kills any PID, so an issue number passed by mistake signals an unrelated process *(curated)*
- **#11094**: Work-finder RAM admission charges 2 GB per sweep, but a loom-daemon rustc peaks at ~13 GB: concurrent loom builds OOM 30 GB workers *(curated)*
- **#11098**: Remove the ETA subsystem from Loom (moved to loom-ui) — staged *(curated)*
- **#11109**: Compatible-W3 hold follow-ups from #11084: resync_pending checkout clears the hold; late re-judge warning; roll-timer floor change *(curated)*
- **#11110**: Roll-pause hook wiring follow-ups from #11082: user-scope double registration, narrow-matcher coverage, settings hooks never resynced *(curated)*
- **#11111**: Deliver #11058's supervision settings to existing hosts: daemon writes a systemd drop-in at startup; reset-failed before operator start *(curated)*
- **#11114**: Attribute residual Linux daemon memory after streamed CI journal export *(curated)*
- **#11115**: Bound CI backfill admission to exporter capacity and expose burst pressure *(curated)*

## Proposed (Architect / Hermit)

- **#9825**: audit-agents: Rust audit of operator-driven build and test placement, OpenCode first *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam
- **#7810**: [epic] Retire shell: 10 sequential PRs, two foundational then update + agent execution
- **#9063**: Epic: order overlapping PRs first, then add automatic merge consolidation
- **#9908**: Consolidate persistence: one journal per concern, retire duplicated stores
- **#9978**: feat(champion): optional per-repo merge-queue mode — enqueue Judge-approved PRs to GitHub's merge queue instead of merge-pr.sh's freshness/re-date loop
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day)
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue
- **#10456**: Epic: the fleet keeps its own hold queue clean — only real operator asks reach Needs attention

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 10 |
| Operator priority | 14 |
| Ready (`loom:issue`) | 58 |
| In Progress (`loom:building`) | 8 |
| PRs awaiting review | 4 |
| Approved PRs awaiting merge | 16 |
| Curated | 101 |
| Architect / Hermit proposals | 1 |
| Active epics | 10 |
<!-- guide:plan-body:end -->
