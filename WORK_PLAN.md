# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

The body below is a snapshot of GitHub label state as of the last merged docs-maintenance PR — it may lag live labels until the next one merges. Date the snapshot with `git log -1 -- WORK_PLAN.md`.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs under a hold (`loom:blocked` or `loom:operator`) — only a human merge decision is missing.

_None._

## Very Important

`loom:very-important` (one counts per workspace); every stage takes these first.

_None._

## Important

`loom:important`; taken before unlabelled work.

- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue *(held)*
- **#12909**: ci_telemetry: 2amlogic owner discovery times out every cycle since Oct 17 (no loom-ui CI facts; 478 consecutive failures) *(held)*
- **#12993**: Shadow evaluation of the new Champion (2026-10-20): 1/95 holds cleared — ranked remaining causes + fixtures *(held)*
- **#13744**: Reads can still land on the writer: raw run_gh bypasses GhInvocation routing (writer budget exhausted 35 min of every hour on 1283)

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default)
- **#12167**: telemetry: export peer_coordination verdict and degraded duration on host.health
- **#12604**: Split status_render.rs: break up ~970-line print_status_human and move inline tests out
- **#12638**: Test: lease_refresh_child_carries_the_root_owners_gh_config_dir fails on main (child sees no GH_CONFIG_DIR)
- **#13578**: Promotion author gate re-holds an issue the operator already adopted, after landed-route re-curation
- **#13744**: Reads can still land on the writer: raw run_gh bypasses GhInvocation routing (writer budget exhausted 35 min of every hour on 1283)
- **#13746**: Writer rate limit is misreported: lease claim says 'could not read lease records', and the finder keeps drawing steps it must refuse
- **#13760**: Guide WORK_LOG backfill wrote another repo's history; docs-only fast path approved it
- **#13776**: Remove write-only prompt_changes pipeline (git_parser, CaptureGitChanges): no reader, uncorrelatable rows
- **#13831**: Remove SweepState::Pending: a lifecycle state nothing constructs, matched at 25 production sites
- **#13832**: Remove the single-workspace role and main-health cadence loops: production runs only the multi-workspace ones, and 7 tests exercise the stale copy
- **#13861**: Remove NoopCooldownState::consecutive: a second no-op streak counter nothing acts on, whose documented reset does not exist
- **#13865**: [ci-daily] Supply Chain
- **#13867**: Remove five senderless terminal IPC requests and the unreachable worktree removal in destroy_terminal (no sender since #3353)
- **#13875**: Held-PR digest cannot state a ratification hold's reason: Step 1 greps only the merge-risk and critical-file markers (every such row reads "reason unrecorded")
- **#13876**: Concurrent Champion passes create duplicate merge-risk-hold digest issues, and Step 0 then silently ignores the loser forever
- **#13879**: docs: docs/README.md index is stale (ADR range, design notes, missing directories)
- **#13880**: Champion capped-PR keep-parked marker key is free-form: five duplicate verdicts on one PR, four mis-keyed (class of #6860/#6872)
- **#13881**: Champion re-discovers a proposal it just sent to Curator: Priority 3 does not exclude loom:triage, so a loom:hermit/architect/auditor revise verdict walks the whole bound ladder in ~30 min
- **#13883**: Judge: when a pull request has no checks at all, run the repo's test command before approving
- **#13885**: Champion held-PR digest reports 'reason unrecorded' for a ratification-held PR: Step 1's marker list was never extended by #12992
- **#13889**: check-stale-blocked: 0 of 6 'STALE BLOCK' flags were actually clear to unpark; partition partial-resolution, honour loom:permanent-block, and check hold comments that post-date the blocker
- **#13896**: Doctor conflict-resolution should structurally validate touched workflow files; a vanished check should not satisfy a green rollup
- **#13897**: Guide docs phase: hardcoded staging set ignores repo-side evidence pinning, and the open-docs-PR skip is mergeability-blind
- **#13898**: Installer's managed .gitignore block ignores neither node_modules/ nor pnpm-lock.yaml, which the root package.json stub's documented pnpm entry points produce
- **#13899**: Simplify .loom/bin/loom: collapse repeated dispatch arms and duplicate inline help
- **#13902**: Champion Priority 3 re-draws a proposal it just routed to Curator, spending the revision ladder before Curator's turn
- **#13910**: generate-agents-md.sh's relative paths assume the source checkout layout and break in every installed repo
- **#13911**: resync ships source-tree-only generate-agents-md.sh into every consumer .loom/scripts/, where it exits 1 on every invocation
- **#13912**: Dedupe daemon-reply error arms copy-pasted across loom-daemon CLI handlers
- **#13914**: Hermit's dead-code check (name-reference grep) reports convention-discovered code as "run by nothing"; it inverted the premise of a live gate
- **#13918**: Captain hosts the fleet issue-filing lock (last signal on the safehouse claims room)
- **#13928**: WORK_PLAN.md Ready section/row apply the #7083 exclusion silently — the artifact reads lower than the loom:issue query it is named after, and never says why
- **#13929**: forge may-write false negative: refuses a repo a configured per-owner writer (#12446) can mint for, and the refusal does not name the mintable route
- **#13932**: champion-hold ratification returns a non-deterministic files= evidence list at rc=0: a subset of the changed paths is silently omitted against an unchanged head
- **#13933**: forge_auth forwards an expired token: a stale per-workspace gh-config-by-owner wins over the refreshed primary one
- **#13939**: create-pr.sh superseded-target check misfires on past-tense 'fixed #N' prose in PR bodies
- **#13944**: Remove write-only Nudge fields and MISSED_TYPES from forge_mirror wake.rs
- **#13980**: verdict-gate refuses a body hash computed the way judge.md says is accepted (jq trailing newline), reporting it as a spec change
- **#13983**: Pool preflight holds a codex-only repo on the host that admits Codex: the probe has no containment preparer
- **#13992**: verdict-gate refuses the trailing-newline --issue-body-sha form judge.md says it accepts, and reports it as a spec change
- **#13998**: check-claim owned lease: unknown-host filter is skipped at runtime; lease-host resolution loose ends
- **#14004**: classify-ac-verification: underscore-emphasised phrase (`_next run_`) no longer matches the AC gate vocabulary
- **#14026**: Admission refusal reason is truncated in the daemon log, and runtime_rejected names no candidate or reason
- **#14027**: roles draw --explain shows a runtime at full weight on a host where it can never be admitted

## In Progress

Issues currently being built (`loom:building`).

- **#13762**: Ratification lane posts ratify:key markers with no verdict, as the PR author, with wrapper logs as the body
- **#13818**: Remove dead spawn-loop-state.json reader: loom-clean deletes live claim locks because its only liveness source is never present
- **#13820**: Remove the loom-api crate: reads a database the daemon does not write, 3 of 5 endpoints query tables nothing creates
- **#13827**: Remove stale file-size ratchet comments: 242 files cite a gate #12099 deleted, and new code still obeys it
- **#13829**: Remove the inert per-host overflow slot: nothing can set the flag since #11508, but its write, restart and report paths remain
- **#13833**: Consolidate eight copies of the loom-dir resolution rule (LOOM_SOCKET_PATH parent, else ~/.loom) into one helper
- **#13871**: Remove the GitHub metrics collector: writes github_events every 15 minutes for no reader, and its activity correlation can never match
- **#13884**: Hermit stateless-ceremony detector has ~0% precision: 237 flags, 0 true positives — the state test misses 3 of 4 assignment forms and there are no type exclusions (successor to #3198)
- **#13887**: [ci-daily] Flake Detection (Rust)
- **#13892**: Guide docs tick's fixed staging set cannot satisfy a consuming repo's CI integrity check on a staged file
- **#13893**: Token exhaustion: captain-coordinated probe shared by all workers; weekly marks follow probe evidence, not a fixed 6 h timer
- **#13895**: park-record: no supported way to withdraw a declared blocker — a stale record stays in the parsed set and keeps the issue parked forever
- **#13901**: verify-proposal-refs.sh: a directory citation followed by an ellipsis (`dir/...`) reports MISSING FILE — third input of the #10191/#11338 class
- **#13909**: Guide: emit a generated-at stamp inside WORK_PLAN's generated region — excluded from the byte-comparison (stamp on write, not on render)
- **#13913**: Champion Pass 0c re-scans the same 5 lowest-numbered issues every pass; a real promotion mismatch deeper in the shortlist is never reached
- **#13915**: Codex hook-trust docker command in guardrail-parity-codex.md never starts Codex (session image entrypoint ignores the argument)
- **#13919**: Captain counts the fleet fast-fail streak and triggers the park (ADR-0027 lane; peer lane dark since 11-14)
- **#13923**: Guide docs maintenance: shared-host /tmp scratch collision + unpinned-repo queries + no provenance self-check on WORK_PLAN.md
- **#13936**: doctor-handback cannot complete a review fix resolved outside the git tree: a changes-requested verdict marker at the unchanged head reads as a fresh rejection on every Doctor pass
- **#13940**: wait-checks: a previously-present check vanishing should not satisfy GREEN
- **#13957**: Shrink always-read core of judge.md (173 KB) into on-demand sub-skills
- **#13977**: self-heal's loom-rescue-* stashes accumulate forever, and #13970 adds a recurring generator
- **#14032**: Step refused at admission for a standing reason is retried every tick with no back-off

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#12631**: fix: release a Champion CI wait as requeued so it holds flat
- **#12637**: refactor: split status_render inline tests into per-module files
- **#12639**: test: create registered roots before registering their gh config dir
- **#13582**: fix: honour a trusted loom:issue adoption in the promotion author gate
- **#13948**: fix: pin Guide docs queries to docs-worktree repo and verify WORK_PLAN provenance
- **#13964**: feat: captain counts fleet fast-fail streak and orders the park
- **#14006**: feat(park-record): add `retract` to withdraw one declared blocker
- **#14008**: feat: add repo-provided post-stage hook to the Guide docs tick staging
- **#14021**: fix: complete a doctor hand-back whose fix needed no commit via claim-time ordering
- **#14037**: fix: hold weekly-exhausted token marks until the probe-recorded 7d reset
- **#14038**: feat: stamp WORK_PLAN's generated region at write time, outside the byte-comparison
- **#14039**: refactor: consolidate eight loom-dir resolution copies into autonomy_marker::resolve_loom_dir
- **#14040**: Remove the default-on GitHub metrics collector
- **#14042**: fix: prune expired loom-rescue stash entries in the self-heal sweep
- **#14044**: fix: judge claim locks by their own owner.json liveness; drop retired spawn-loop-state reader
- **#14045**: fix(hermit): widen stateless-ceremony state test, add type + test-file exclusions

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#14033**: fix: strip dot-stripped trailing slash from directory citations in verify-proposal-refs
- **#14041**: champion: filter Pass 0c rescan shortlist to plausible mismatches

## Proposed

Issues carrying `loom:curated`.

- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)* *(held)*
- **#8525**: tracing: instrument sweep phases and role attempts, including Pi/OpenCode and repair cycles *(curated)* *(held)*
- **#8698**: Live end-to-end verification of the credential egress proxy (AC5 of #8674) *(curated)* *(held)*
- **#9136**: main ruleset: bypass_actors 'RepositoryRole:always' makes pull_request and required_status_checks advisory *(curated)* *(held)*
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day) *(curated)* *(held)*
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue *(curated)* *(held)*
- **#10698**: Fleet version floor: the daemon enforces `loom_min_version` from the fleet store, rolls itself forward, and brings each repo's installed Loom up to its own version *(curated)*
- **#10838**: merge_pr_reconcile_differential: claim-gate differential disagrees on case-variant duplicate label (loom:buildinG) *(curated)* *(held)*
- **#10941**: No-PR retry hold fires when the only remaining work is a live check or a cross-repo PR *(curated)* *(held)*
- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default) *(curated)*
- **#11112**: Remove the safehouse (Matrix) integration from Loom: narration, ChatOps/Concierge, peer-claim channel (forge leases remain) *(curated)*
- **#11283**: OpenCode 2.x guarded launches: load the native-tools plugin via the 2.x default-export shape, live-verify, flip guard_verified *(curated)* *(held)*
- **#11284**: Runtime readiness: per-repo, per-stage weighted model selection in fleet-gitops *(curated)* *(held)*
- **#11287**: Umbrella: Z.ai GLM-5.3-Flash seats landing code in the fleet via OpenCode *(curated)* *(held)*
- **#11300**: Z.ai seat telemetry via the egress proxy in transparent observe mode: per-request tokens/latency/429s to SigNoz, OpenCode headers untouched *(curated)* *(held)*
- **#11595**: Pause-and-roll budget path can still kill an in-flight merge-pr.sh (follow-up to #11263) *(curated)* *(held)*
- **#11703**: Config precedence: fleet-gitops values must override repo-committed .loom/config.json (general rule + enforcement) *(curated)* *(held)*
- **#11911**: Finish retiring daemon-spawned /loom:sweep and merge the two concurrency budgets *(curated)* *(held)*
- **#12029**: Telemetry: merge runs record PR number and observed result (merge-pr chokepoint) — Judge parts moved to #12362; held behind #12063 *(curated)* *(held)*
- **#12063**: Umbrella: refactor merge-pr to a minimal contract (deletion-first slices; merge queue after the 2AMLogic move) *(curated)*
- **#12094**: Retire the telemetry replay subsystem and the kinds left without a producer by #11301 *(curated)* *(held)*
- **#12248**: Retired hold labels: remove core registry/work_finder readers (1/3 of #11843) *(curated)* *(held)*
- **#12304**: Epic: fleet discovery from one mirror read per tick — zero forge reads for discovery; delete per-host listing paths *(curated)*
- **#12320**: Cleanup once every worker reads the worklist: delete the per-call `forge_mirror` discovery path, `maxStalenessSecs` and `per_call`; docs and ADR-0024; direct backend preserved *(curated)* *(held)*
- **#12408**: Live-verify epic child discovery against sky130-sar-adc#23 (follow-up to #12388) *(curated)* *(held)*
- **#12418**: Verify epic #10698 acceptance: completed-vs-not-planned dispositions and live fleet receipts *(curated)* *(held)*
- **#12604**: Split status_render.rs: break up ~970-line print_status_human and move inline tests out *(curated)*
- **#12626**: docs: /repo:release Phase 1.5 CHANGELOG completeness gate has no seam for generated-notes policy *(curated)* *(held)*
- **#12638**: Test: lease_refresh_child_carries_the_root_owners_gh_config_dir fails on main (child sees no GH_CONFIG_DIR) *(curated)*
- **#12703**: Add a per-repo setting to turn off Champion's out-of-band acceptance-criteria hold (#6883) *(curated)* *(held)*
- **#12713**: Part-of merges re-queue the issue to Builders: landed-route marker is read as a Curator 'kept' decision *(curated)* *(held)*
- **#12909**: ci_telemetry: 2amlogic owner discovery times out every cycle since Oct 17 (no loom-ui CI facts; 478 consecutive failures) *(curated)* *(held)*
- **#12966**: Bound Codex session/thread history inside live profiles (the real codex-profiles growth) + uv cache *(curated)* *(held)*
- **#12978**: Codex shared-seat dispatch: live receipts and startup measurements (split from #10796) *(curated)* *(held)*
- **#13155**: [ci-daily] Flake Detection (shell suites, macos-latest, parallelism 1) *(curated)*
- **#13317**: Audit Champion merge-risk self-releases two weeks after #12960 is live *(curated)*
- **#13378**: Scope trust for github-actions[bot] to ci-daily tracker authorship instead of repo-wide trustedCommenters (alternative to #13340) *(curated)* *(held)*
- **#13486**: Champion resync waiver: a byte-identical test under .loom/scripts/tests/ is not a merge-gate path — every release shipping test-champion-resync-waiver.sh holds all resync PRs *(curated)* *(held)*
- **#13521**: Builder: ~11% of fleet sweeps still end without an outcome line after #13447 (worker-3 29%, worker-4 0%) *(curated)* *(held)*
- **#13578**: Promotion author gate re-holds an issue the operator already adopted, after landed-route re-curation *(curated)*
- **#13590**: Observe convert_awaiting_revision_parks on a live Guide tick (#10936 follow-up) *(curated)* *(held)*
- **#13640**: ci_telemetry: owner-kind probe users/2amlogic hangs 120s every cycle in the daemon (0.3s by hand) and a timeout never fails over — 2AMLogic CI dropped 20+ days *(curated)* *(held)*
- **#13668**: Replay strict resync waiver on recent fasterhenry resync PRs (follow-up to #13665) *(curated)* *(held)*
- **#13720**: worktree.issue_state_rest reads spike every 4 h (synchronized cache expiry?) *(curated)* *(held)*
- **#13762**: Ratification lane posts ratify:key markers with no verdict, as the PR author, with wrapper logs as the body *(curated)*
- **#13827**: Remove stale file-size ratchet comments: 242 files cite a gate #12099 deleted, and new code still obeys it *(curated)*
- **#13865**: [ci-daily] Supply Chain *(curated)*
- **#13873**: Daemon path: fall back one model rung (fable → opus → sonnet) when the configured model's limit is hit, instead of holding the pool *(curated)*
- **#13875**: Held-PR digest cannot state a ratification hold's reason: Step 1 greps only the merge-risk and critical-file markers (every such row reads "reason unrecorded") *(curated)*
- **#13876**: Concurrent Champion passes create duplicate merge-risk-hold digest issues, and Step 0 then silently ignores the loser forever *(curated)*
- **#13879**: docs: docs/README.md index is stale (ADR range, design notes, missing directories) *(curated)*
- **#13880**: Champion capped-PR keep-parked marker key is free-form: five duplicate verdicts on one PR, four mis-keyed (class of #6860/#6872) *(curated)*
- **#13881**: Champion re-discovers a proposal it just sent to Curator: Priority 3 does not exclude loom:triage, so a loom:hermit/architect/auditor revise verdict walks the whole bound ladder in ~30 min *(curated)*
- **#13883**: Judge: when a pull request has no checks at all, run the repo's test command before approving *(curated)*
- **#13884**: Hermit stateless-ceremony detector has ~0% precision: 237 flags, 0 true positives — the state test misses 3 of 4 assignment forms and there are no type exclusions (successor to #3198) *(curated)*
- **#13885**: Champion held-PR digest reports 'reason unrecorded' for a ratification-held PR: Step 1's marker list was never extended by #12992 *(curated)*
- **#13887**: [ci-daily] Flake Detection (Rust) *(curated)*
- **#13889**: check-stale-blocked: 0 of 6 'STALE BLOCK' flags were actually clear to unpark; partition partial-resolution, honour loom:permanent-block, and check hold comments that post-date the blocker *(curated)*
- **#13892**: Guide docs tick's fixed staging set cannot satisfy a consuming repo's CI integrity check on a staged file *(curated)*
- **#13893**: Token exhaustion: captain-coordinated probe shared by all workers; weekly marks follow probe evidence, not a fixed 6 h timer *(curated)*
- **#13895**: park-record: no supported way to withdraw a declared blocker — a stale record stays in the parsed set and keeps the issue parked forever *(curated)*
- **#13896**: Doctor conflict-resolution should structurally validate touched workflow files; a vanished check should not satisfy a green rollup *(curated)*
- **#13897**: Guide docs phase: hardcoded staging set ignores repo-side evidence pinning, and the open-docs-PR skip is mergeability-blind *(curated)*
- **#13898**: Installer's managed .gitignore block ignores neither node_modules/ nor pnpm-lock.yaml, which the root package.json stub's documented pnpm entry points produce *(curated)*
- **#13901**: verify-proposal-refs.sh: a directory citation followed by an ellipsis (`dir/...`) reports MISSING FILE — third input of the #10191/#11338 class *(curated)*
- **#13902**: Champion Priority 3 re-draws a proposal it just routed to Curator, spending the revision ladder before Curator's turn *(curated)*
- **#13909**: Guide: emit a generated-at stamp inside WORK_PLAN's generated region — excluded from the byte-comparison (stamp on write, not on render) *(curated)*
- **#13910**: generate-agents-md.sh's relative paths assume the source checkout layout and break in every installed repo *(curated)*
- **#13911**: resync ships source-tree-only generate-agents-md.sh into every consumer .loom/scripts/, where it exits 1 on every invocation *(curated)*
- **#13913**: Champion Pass 0c re-scans the same 5 lowest-numbered issues every pass; a real promotion mismatch deeper in the shortlist is never reached *(curated)*
- **#13914**: Hermit's dead-code check (name-reference grep) reports convention-discovered code as "run by nothing"; it inverted the premise of a live gate *(curated)*
- **#13915**: Codex hook-trust docker command in guardrail-parity-codex.md never starts Codex (session image entrypoint ignores the argument) *(curated)*
- **#13918**: Captain hosts the fleet issue-filing lock (last signal on the safehouse claims room) *(curated)*
- **#13919**: Captain counts the fleet fast-fail streak and triggers the park (ADR-0027 lane; peer lane dark since 11-14) *(curated)*
- **#13923**: Guide docs maintenance: shared-host /tmp scratch collision + unpinned-repo queries + no provenance self-check on WORK_PLAN.md *(curated)*
- **#13928**: WORK_PLAN.md Ready section/row apply the #7083 exclusion silently — the artifact reads lower than the loom:issue query it is named after, and never says why *(curated)*
- **#13929**: forge may-write false negative: refuses a repo a configured per-owner writer (#12446) can mint for, and the refusal does not name the mintable route *(curated)*
- **#13932**: champion-hold ratification returns a non-deterministic files= evidence list at rc=0: a subset of the changed paths is silently omitted against an unchanged head *(curated)*
- **#13933**: forge_auth forwards an expired token: a stale per-workspace gh-config-by-owner wins over the refreshed primary one *(curated)*
- **#13936**: doctor-handback cannot complete a review fix resolved outside the git tree: a changes-requested verdict marker at the unchanged head reads as a fresh rejection on every Doctor pass *(curated)*
- **#13939**: create-pr.sh superseded-target check misfires on past-tense 'fixed #N' prose in PR bodies *(curated)*
- **#13940**: wait-checks: a previously-present check vanishing should not satisfy GREEN *(curated)*
- **#13957**: Shrink always-read core of judge.md (173 KB) into on-demand sub-skills *(curated)*
- **#13961**: pick.decision reports an extraSkipLabels veto as host_constraint on the step path and operator_hold on the role-tick path *(curated)*
- **#13966**: Z.ai weekly-limit 429 is bad-marked for 60 s: exhausted keys are relaunched all day and the role never falls through to Claude *(curated)*
- **#13974**: Assert the Detect Changes fail-open shape in the merge_group_ci workflow tests (drift guard for #13971) *(curated)*
- **#13977**: self-heal's loom-rescue-* stashes accumulate forever, and #13970 adds a recurring generator *(curated)*
- **#13980**: verdict-gate refuses a body hash computed the way judge.md says is accepted (jq trailing newline), reporting it as a spec change *(curated)*
- **#13982**: Resync waiver refuses merge-pr.sh even when it is byte-identical to the upstream tag, so every release that touches it holds the whole fleet *(curated)*
- **#13983**: Pool preflight holds a codex-only repo on the host that admits Codex: the probe has no containment preparer *(curated)*
- **#13992**: verdict-gate refuses the trailing-newline --issue-body-sha form judge.md says it accepts, and reports it as a spec change *(curated)*
- **#13998**: check-claim owned lease: unknown-host filter is skipped at runtime; lease-host resolution loose ends *(curated)*
- **#14004**: classify-ac-verification: underscore-emphasised phrase (`_next run_`) no longer matches the AC gate vocabulary *(curated)*
- **#14009**: Review the WORK_PLAN.md and ROADMAP.md conventions: what Guide should do in a repo that keeps a hand-written roadmap *(curated)*
- **#14010**: duplicate-hook-wiring: the defaults/hooks/ exclusion is per-file, not per-command (false negative on a mixed project settings.json) *(curated)*
- **#14016**: Floor-driven artifact fetch logs "backing off 60s" but retries only at the next update tick *(curated)*
- **#14018**: Completion envelopes carry no tokens or runtime under step dispatch: the emitter reads only the merging host's Claude sessions *(curated)*
- **#14019**: Contained sessions: configurable session image (digest-pinned, bundle verified) and extra read-only mounts *(curated)*
- **#14022**: Pin a test on 'a dispatched child's LOOM_MACHINE_CHECKOUT never reaches a rendered daemon unit' *(curated)*
- **#14026**: Admission refusal reason is truncated in the daemon log, and runtime_rejected names no candidate or reason *(curated)*
- **#14027**: roles draw --explain shows a runtime at full weight on a host where it can never be admitted *(curated)*
- **#14030**: Private clone with unpushed commits refuses every later job and has no supported recovery verb *(curated)*
- **#14032**: Step refused at admission for a standing reason is retried every tick with no back-off *(curated)*

## Proposed (Architect / Hermit)

- **#13071**: Peer claims: move machine coordination off rendered room messages (custom event type / state events) *(architect)* *(held)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management *(held)*
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day) *(held)*
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue *(held)*
- **#10698**: Fleet version floor: the daemon enforces `loom_min_version` from the fleet store, rolls itself forward, and brings each repo's installed Loom up to its own version
- **#11287**: Umbrella: Z.ai GLM-5.3-Flash seats landing code in the fleet via OpenCode *(held)*
- **#12304**: Epic: fleet discovery from one mirror read per tick — zero forge reads for discovery; delete per-host listing paths
- **#12708**: Epic: loom-tools CLI for agents, shared Rust implementation with the daemon

## Backlog Balance

| Bucket | Count |
|------|-------|
| Operator merge-risk holds | 0 |
| Very important | 0 |
| Important | 4 |
| Ready (`loom:issue`) | 45 |
| In Progress (`loom:building`) | 23 |
| PRs awaiting review | 16 |
| Approved PRs awaiting merge | 2 |
| Curated | 103 |
| Architect / Hermit proposals | 1 |
| Active epics | 7 |
<!-- guide:plan-body:end -->
