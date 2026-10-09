# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

_None._

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
- **#11098**: Remove the ETA subsystem from Loom (moved to loom-ui) — staged
- **#11115**: Bound CI backfill admission to exporter capacity and expose burst pressure
- **#11126**: telemetry: re-home pr.resolved and eta.stage_outcome producers outside eta/ (#10196 R5)
- **#11128**: telemetry: telemetry replay --check and 24h agreement report (#10196 R7)
- **#11159**: ci_telemetry: the dedupe ledger's seen set is never pruned and is fully materialized every poll cycle (~250-330 B RSS per unit, grows with history)
- **#11161**: telemetry: fleet.state at work-finder cadence (60 s) with 5-minute anchors (#10196 R9)
- **#11179**: Dashboard session pane: links should open the dashboard view, not GitHub
- **#11205**: classify-error.sh misses real Claude Code 401/usage messages (OAuth access token invalid, session expired, org out of usage, bare 429)
- **#11208**: Ship the error classifier in loom-daemon (`loom-daemon classify-error`) so consumers stop vendoring classify-error.sh

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8372**: Canary the uncommitted-work Stop guard in opted-in consumer workspaces with measured block outcomes
- **#8742**: loom:blocked is silently stripped from operator-ruled permanent blocks (unblock probe treats 'unparseable blocker' as 'no blocker')
- **#9255**: forge_events: event-gated per-workspace polling under a bounded staleness cap (ADR-0021 amendment step 2)
- **#9405**: guard: parse_force_ops() treats `|`, `&`, `||` inside $(cd <path> ... pwd) like `&&` — resolves cwd the shell never sets
- **#9487**: Doctor's --force-with-lease can succeed against a stale local tracking ref in a shared clone, silently overwriting a sibling Doctor's push
- **#9929**: Curator applies issue-lifecycle labels (loom:curating/loom:curated) to pull requests
- **#9973**: Flaky: worktree_cli::reset rescue test unexpectedly hits the live-process veto under nextest
- **#10023**: daemon: one stable host.id per machine, plus start/stop/heartbeat events in SigNoz
- **#10150**: notify-cleared-blockers runs only from merge-pr.sh: merges made any other way never clear a block
- **#10156**: Daemon re-dispatches human-gated/blocked issues forever after no-op sweeps (275 after-curator loops, ~600M tokens/24h)
- **#10229**: sweep-lease-renew.sh spends the operator's personal REST pool at ~600 calls/h and climbing; one sweep id runs ~16 renew loops
- **#10337**: Dispatcher yields to its own leaseless loom:building label and orphans the issue (3 starred issues 2026-10-05 00:40Z)
- **#10414**: Workers stuck on 0.19.701 since ~01:30Z (12 releases behind) and their fleet-refresh task went silent at the same moment — no auto-update telemetry or version-lag alert
- **#10687**: Auditor: update executable fixtures when changing worker environment or CLI help
- **#10802**: General cleanup: reap agent process residue (dev servers, orphaned children) after any agent exit
- **#10916**: Fleet singleton output watchdog: alert when any one-host fleet job stops producing output (not just when its host goes silent)
- **#11029**: Floor roll target: walk back to the newest release >= floor with assets; make an unresolved below-floor host loud
- **#11042**: Floor-loop follow-ups: unsupervised hosts re-download every tick; typed not-rolling reason; status enabled semantics
- **#11066**: Flaky ci_telemetry tests: parallel runs race on process-global env vars
- **#11068**: Auditor: scoped test passes miss required Rust lint checks
- **#11070**: daemon-update --fetch: 'artifact path cannot reach current source' contradicts the fetch that follows
- **#11074**: Guard: worktree-write-confinement denies sed editing an external temporary PR body
- **#11083**: Parallel H4 stop follow-ups from #11081: parked-at-bound disposition, group-only kill at the bound, record race, forced-stop count
- **#11086**: sweep-lease-renew.sh stop <arg> kills any PID, so an issue number passed by mistake signals an unrelated process
- **#11087**: Remove mail from Loom: human asks are signaled by labels only (notifier moves to 2am workers)
- **#11094**: Work-finder RAM admission charges 2 GB per sweep, but a loom-daemon rustc peaks at ~13 GB: concurrent loom builds OOM 30 GB workers
- **#11103**: Priority model: loom:important / loom:very-important for everyone; weighted workspace pick, then level → oldest → number
- **#11105**: Simplify the label set: delete undeclared labels, retire unused ones (urgent, heavy, operator-objective)
- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default)
- **#11109**: Compatible-W3 hold follow-ups from #11084: resync_pending checkout clears the hold; late re-judge warning; roll-timer floor change
- **#11112**: Remove the safehouse (Matrix) integration from Loom: narration, ChatOps/Concierge, peer-claim channel (forge leases remain)
- **#11136**: Guard: pipe-curl-to-shell rule denies read-only curl piped through sed
- **#11148**: Builder preflight: check vendored documentation links before requesting review
- **#11149**: check-main-clean build-tree filter: renames into a build tree drop the source deletion; partial daemon output vs warning (#11099 follow-ups)
- **#11159**: ci_telemetry: the dedupe ledger's seen set is never pruned and is fully materialized every poll cycle (~250-330 B RSS per unit, grows with history)
- **#11174**: Release cadence: at most one version bump per 24 h (accumulate merges; daily catch-up; manual hotfix override)
- **#11182**: RAM admission follow-ups from #11106: role-keyed history, one-tick gap, heavy-repo starvation, downgrade-safe store, stale unreadable scopes
- **#11195**: Docker image retention removes nothing on containerd-store workers: digest-only untagged images and unused third-party images (37.9 GB reclaimable on worker-2)

## In Progress

Issues currently being built (`loom:building`).

- **#8961**: resync-installed.sh hard-fails on a missing gitignored loom-source-path instead of falling back to the machine-level defaults mirror (#5389)
- **#9078**: defaults/optional/github-workflows/label-external-issues.yml has invalid YAML (unindented multi-line JS template literal)
- **#9126**: Document the quoted-heredoc rule in comment-body-literal-path.md: an unquoted delimiter silently executes and deletes backticked prose
- **#9258**: A verdict body of `@-` yields an approval with no rationale that still gates a merge
- **#9308**: dep-recheck-fingerprint: CONCLUSION_HASH not stable across loom-daemon versions for identical input, defeats Curator idempotency suppression
- **#9510**: insert_nonempty_bounded truncates by chars, span allowlist bounds by bytes (256 mismatch)
- **#9611**: Champion criterion #3: version_only_diff fails OPEN when its gh api call errors, so a critical-file hold can auto-release
- **#10827**: Promotion author gate: don't auto-promote issues authored by untrusted identities (signed decision markers dropped)
- **#10837**: Release pass (#10556) never released loom-ui#1695: all three park-record blockers closed ~22 h ago, still loom:blocked
- **#10880**: auto-update persistence: follow-ups from #10877 (floor roll in a consumed window, rollback refetch loop, hardening)
- **#11003**: Every-tick resync: follow-ups from #10998 (breaker re-check before fallback/claim, per-owner budget, host-level credential alert, empty and archived repos)
- **#11024**: Build-slot test isolation follow-ups: one unserialised guard holder, and daemons spawned by the integration harness
- **#11191**: Disk admission charges a flat 8 GB per sweep and ignores in-flight growth: loom sweeps dispatched at ~50 GB free while two builds grew by 50 GB
- **#11205**: classify-error.sh misses real Claude Code 401/usage messages (OAuth access token invalid, session expired, org out of usage, bare 429)
- **#11221**: Chain-head merge lock cap (1200 s) is shorter than required CI (~45 min): approved PRs starve for hours
- **#11232**: CI fails on Docker Hub's anonymous pull rate limit (Codex smoke, ClickHouse-backed OTLP tests): pull through a mirror with fallback and retry

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#11235**: Pull Docker Hub test images through mirror.gcr.io with a bounded Hub fallback (#11232)
- **#11238**: Reap agent process residue after any agent exit (#10802)
- **#11241**: chore(harness): track latest CLIs — codex 0.162.0→0.162.1
- **#11243**: docs: explain quoted heredoc delimiter in comment-body-literal-path.md

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#10274**: loom update: provision user-scope skills and ff-sync machine checkout (#10238)
- **#10280**: Keep long-running review claims alive on Judge activity and claimant force-pushes (#10235)
- **#10339**: chore(deps): update clickhouse/clickhouse-keeper docker tag to v25.12.11
- **#10355**: chore(deps): update rust to v1.98.1
- **#10359**: feat(guards): guards.enabled master opt-out + guard false-positive fixes (#10335)
- **#10410**: test+docs(lease-renew): define release signal, pin cached-read failure (Part of #10229)
- **#10602**: feat(stale_blocked): hand undocumented loom:blocked to Curator via loom:blocked-unnamed (#10558)
- **#10968**: feat(merge-pr): port the already-merged / closed terminal-state gate to Rust (#8191 slice)
- **#11010**: telemetry: carry IE1's seconds partition into the sweep_facts bundle (#9507)
- **#11063**: feat(fleet-alert): wire the singleton output watchdog to a real OutputSource (#10916)
- **#11130**: Bound CI backfill admission to exporter capacity and expose burst pressure
- **#11140**: CI gate: require CONTROL_VERSION bump when private-control POLICY changes (#8858)
- **#11150**: fix(claude-wrapper): apply the server's declared .mcp.json env in MCP pre-flight
- **#11152**: feat(guard): per-role tool restriction via loom-daemon role-tool-policy check (#8256)
- **#11157**: feat(guard): opt-in consumer canary for the uncommitted-work Stop guard (#8372)
- **#11163**: telemetry: fleet.state at work-finder cadence with 5-minute anchors (#10196 R9)
- **#11171**: telemetry: re-home pr.resolved and eta.stage_outcome outside eta/ (#10196 R5)
- **#11183**: test(dep-recheck): pin CONCLUSION_HASH golden vectors, document the #8320 transition
- **#11184**: feat(spawn-claude): MemoryMax for agent scopes from observed per-repo peak (#11094 slice 2)
- **#11187**: fix(stale-blocked): vet the release comment under the root's own credential (#10837)
- **#11201**: Daemon dispatch: defer same-repo candidates with overlapping Affected Files (#9781)
- **#11212**: test: isolate parallel-cargo-test shared state (skip counters, bad-mark covering, lease re-acquire) (#9409)
- **#11213**: feat(telemetry): telemetry-replay --check and the 24h agreement report (#10196 R7)
- **#11214**: fix(classify-error): classify real Claude Code 401 and usage messages (#11205)
- **#11215**: Disk admission: charge each repo its measured footprint and reserve in-flight growth (#11191)
- **#11217**: fix: make label-external-issues.yml template parse as YAML (#9078)
- **#11220**: fix(champion): make criterion #3's version-only carve-out fail closed (#9611)
- **#11223**: fix: empty/@- verdict bodies and markerless approvals can no longer gate a merge (#9258)
- **#11224**: fix(merge-pr): raise chain-head lock cap to 2 hours
- **#11228**: fix(guard): rm-scope ignores python heredoc payload (#10422)
- **#11231**: feat(watchdog): worktree + agent tool-process liveness signals (#9533)
- **#11233**: feat(role-runner): deterministic per-repo lane rule for judge and doctor (#10630)
- **#11236**: fix(daemon): reconstruct keeps the claim of a dead leader whose group is alive (#11076)

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
- **#8913**: observability: live-verify ci.job.log reconstruction in SigNoz and record it in evidence.md (#8825 AC1) *(curated)*
- **#8950**: Live-verify Pi tokens_by_model against a real LOOM_RUNTIME=pi launch *(curated)*
- **#8961**: resync-installed.sh hard-fails on a missing gitignored loom-source-path instead of falling back to the machine-level defaults mirror (#5389) *(curated)*
- **#9006**: observability: live-verify the SigNoz queue-starvation alert rule fires and resolves (#8856 / PR #8935 follow-up) *(curated)*
- **#9041**: dep-recheck-fingerprint: BLOCK_REASON and ORTHOGONAL are emitted unquoted, so the documented eval word-splits them and executes the remainder (#8323's fix missed the two pass-throughs) *(curated)*
- **#9049**: Security: no secrets under any repo/worktree — move all credential state to ~/.loom (21 Claude OAuth tokens leaked, 2nd incident) *(curated)*
- **#9065**: CI: cut PR wall time from ~8.5 min to ≤5 min (build once, dedupe nextest, shard serial suites) and stop false-stale merges *(curated)*
- **#9078**: defaults/optional/github-workflows/label-external-issues.yml has invalid YAML (unindented multi-line JS template literal) *(curated)*
- **#9126**: Document the quoted-heredoc rule in comment-body-literal-path.md: an unquoted delimiter silently executes and deletes backticked prose *(curated)*
- **#9136**: main ruleset: bypass_actors 'RepositoryRole:always' makes pull_request and required_status_checks advisory *(curated)*
- **#9255**: forge_events: event-gated per-workspace polling under a bounded staleness cap (ADR-0021 amendment step 2) *(curated)*
- **#9258**: A verdict body of `@-` yields an approval with no rationale that still gates a merge *(curated)*
- **#9308**: dep-recheck-fingerprint: CONCLUSION_HASH not stable across loom-daemon versions for identical input, defeats Curator idempotency suppression *(curated)*
- **#9405**: guard: parse_force_ops() treats `|`, `&`, `||` inside $(cd <path> ... pwd) like `&&` — resolves cwd the shell never sets *(curated)*
- **#9409**: test: POOL_EXHAUSTED_SKIP_COUNT delta assertions race under parallel cargo test (flaky role_runner / runtime_preflight tests) *(curated)*
- **#9487**: Doctor's --force-with-lease can succeed against a stale local tracking ref in a shared clone, silently overwriting a sibling Doctor's push *(curated)*
- **#9492**: Decide which other required CI jobs the local build gate should mirror (follow-up to #9140) *(curated)*
- **#9507**: telemetry: split per-issue effort into clean/substantive/environmental SECONDS, not event counts (#9444 criterion 4) *(curated)*
- **#9510**: insert_nonempty_bounded truncates by chars, span allowlist bounds by bytes (256 mismatch) *(curated)*
- **#9533**: review-stall-watchdog kills healthy sweeps: sweep-log mtime is not a liveness signal *(curated)*
- **#9548**: Authenticate Loom's control signals: markers and phrases count only from trusted authors (work safely on any public repo) *(curated)*
- **#9611**: Champion criterion #3: version_only_diff fails OPEN when its gh api call errors, so a critical-file hold can auto-release *(curated)*
- **#9735**: auto_update: roll back candidates that fail startup and quarantine failed artifacts *(curated)*
- **#9781**: Daemon dispatch: don't admit two same-repo candidates with overlapping Curator affected files in one tick (mirror sweep's #4161 wave rule) *(curated)*
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
- **#10870**: MCP pre-flight smoke test ignores the server's .mcp.json env block *(curated)*
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
- **#11070**: daemon-update --fetch: 'artifact path cannot reach current source' contradicts the fetch that follows *(curated)*
- **#11076**: Sweep released as dead while its wrapper retry is still running: OOM kill of one child stops the whole agent scope (OOMPolicy=stop) *(curated)*
- **#11083**: Parallel H4 stop follow-ups from #11081: parked-at-bound disposition, group-only kill at the bound, record race, forced-stop count *(curated)*
- **#11086**: sweep-lease-renew.sh stop <arg> kills any PID, so an issue number passed by mistake signals an unrelated process *(curated)*
- **#11094**: Work-finder RAM admission charges 2 GB per sweep, but a loom-daemon rustc peaks at ~13 GB: concurrent loom builds OOM 30 GB workers *(curated)*
- **#11098**: Remove the ETA subsystem from Loom (moved to loom-ui) — staged *(curated)*
- **#11109**: Compatible-W3 hold follow-ups from #11084: resync_pending checkout clears the hold; late re-judge warning; roll-timer floor change *(curated)*
- **#11110**: Roll-pause hook wiring follow-ups from #11082: user-scope double registration, narrow-matcher coverage, settings hooks never resynced *(curated)*
- **#11115**: Bound CI backfill admission to exporter capacity and expose burst pressure *(curated)*
- **#11126**: telemetry: re-home pr.resolved and eta.stage_outcome producers outside eta/ (#10196 R5) *(curated)*
- **#11128**: telemetry: telemetry replay --check and 24h agreement report (#10196 R7) *(curated)*
- **#11149**: check-main-clean build-tree filter: renames into a build tree drop the source deletion; partial daemon output vs warning (#11099 follow-ups) *(curated)*
- **#11159**: ci_telemetry: the dedupe ledger's seen set is never pruned and is fully materialized every poll cycle (~250-330 B RSS per unit, grows with history) *(curated)*
- **#11161**: telemetry: fleet.state at work-finder cadence (60 s) with 5-minute anchors (#10196 R9) *(curated)*
- **#11167**: merge-pr.sh diagnostics: fail-open roll-hint names its own floor (#9377) + wire Loom-Issue trailer warning (#9502) *(curated)*
- **#11174**: Release cadence: at most one version bump per 24 h (accumulate merges; daily catch-up; manual hotfix override) *(curated)*
- **#11182**: RAM admission follow-ups from #11106: role-keyed history, one-tick gap, heavy-repo starvation, downgrade-safe store, stale unreadable scopes *(curated)*
- **#11189**: Epic: fleet workers stop filling their disks — bound per-sweep build output and reclaim what the reaper misses (loom-worker-1, 2026-10-09) *(curated)*
- **#11191**: Disk admission charges a flat 8 GB per sweep and ignores in-flight growth: loom sweeps dispatched at ~50 GB free while two builds grew by 50 GB *(curated)*
- **#11193**: Worktree reaper keeps merged pr-<N> worktrees forever when an agent-made cargo target dir is inside (15 GB on loom-worker-1) *(curated)*
- **#11194**: Agent-made judge-<PR> worktrees are invisible to every reclaim path (loom-ui: 21 merged-PR worktrees, 8.3 GB) *(curated)*
- **#11195**: Docker image retention removes nothing on containerd-store workers: digest-only untagged images and unused third-party images (37.9 GB reclaimable on worker-2) *(curated)*
- **#11208**: Ship the error classifier in loom-daemon (`loom-daemon classify-error`) so consumers stop vendoring classify-error.sh *(curated)*
- **#11221**: Chain-head merge lock cap (1200 s) is shorter than required CI (~45 min): approved PRs starve for hours *(curated)*

## Proposed (Architect / Hermit)

- **#9825**: audit-agents: Rust audit of operator-driven build and test placement, OpenCode first *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam
- **#7810**: [epic] Retire shell: 10 sequential PRs, two foundational then update + agent execution
- **#9908**: Consolidate persistence: one journal per concern, retire duplicated stores
- **#9978**: feat(champion): optional per-repo merge-queue mode — enqueue Judge-approved PRs to GitHub's merge queue instead of merge-pr.sh's freshness/re-date loop
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day)
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue
- **#10456**: Epic: the fleet keeps its own hold queue clean — only real operator asks reach Needs attention
- **#11189**: Epic: fleet workers stop filling their disks — bound per-sweep build output and reclaim what the reaper misses (loom-worker-1, 2026-10-09)

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 0 |
| Operator priority | 18 |
| Ready (`loom:issue`) | 40 |
| In Progress (`loom:building`) | 16 |
| PRs awaiting review | 4 |
| Approved PRs awaiting merge | 33 |
| Curated | 98 |
| Architect / Hermit proposals | 1 |
| Active epics | 10 |
<!-- guide:plan-body:end -->
