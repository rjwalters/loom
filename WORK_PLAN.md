# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs under a hold (`loom:blocked` or `loom:operator`) — only a human merge decision is missing.

- **#10346**: fix(dispatch): retain attributed own claims before leaseless yield
- **#11830**: fix(stale_blocked): reuse budget probes per credential
- **#12062**: build: line-tables-only debuginfo for dev/test to cap rustc peak RAM (#11094 item 4)
- **#12096**: fix: mask quoted '>' inside backtick substitutions in write-confinement guard
- **#12116**: ci: remove Lines of Code workflow and README badge
- **#12151**: fix: deny instead of ask on a shared cargo build.target-dir

## Very Important

`loom:very-important` (one counts per workspace); every stage takes these first.

_None._

## Important

`loom:important`; taken before unlabelled work.

- **#9049**: Security: no secrets under any repo/worktree — move all credential state to ~/.loom (21 Claude OAuth tokens leaked, 2nd incident)
- **#9081**: guard-background-subagents misses a background Bash started by a foreground subagent: headless sweep exits, Builder's make verify reaped
- **#9957**: daemon: deliberate stops (SIGTERM) are silent autonomy losses — the watchdog goes down with it, the intent marker is deleted, and bootstrap cannot recover
- **#10083**: CI path filter skips label_registry Rust tests on label-file-only PRs (how #10072 turned main red)
- **#10337**: Dispatcher yields to its own leaseless loom:building label and orphans the issue (3 starred issues 2026-10-05 00:40Z)
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue
- **#10890**: Daemon dispatches loom:issue + loom:needs-capability items; pre-flight skip then reads as insta-crash and quarantine release re-queues them
- **#11094**: Work-finder RAM admission charges 2 GB per sweep, but a loom-daemon rustc peaks at ~13 GB: concurrent loom builds OOM 30 GB workers
- **#11301**: check-stale-blocked: the per-repo budget probe spends ~1.4k GraphQL points/h on the writer bucket that runs dry
- **#11426**: workspace_resync fails forever on repos with symlinked .claude/commands or .claude/skills (git add: 'beyond a symbolic link') — miniset never dispatched
- **#11787**: issue.estimate coverage gap: operator-session curation sets loom:curated by hand and bypasses curation-complete (loom-ui: 58 of 62 curations since the gate have no estimate)
- **#12059**: ETA telemetry: operator_recency/hold_release_burst (#11956) not surfacing in SigNoz on floor 1161

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#9081**: guard-background-subagents misses a background Bash started by a foreground subagent: headless sweep exits, Builder's make verify reaped
- **#9957**: daemon: deliberate stops (SIGTERM) are silent autonomy losses — the watchdog goes down with it, the intent marker is deleted, and bootstrap cannot recover
- **#10079**: locate-daemon-bin prefers a stale target/ build over the installed daemon — post-comment.sh fails for every role session on that host
- **#10083**: CI path filter skips label_registry Rust tests on label-file-only PRs (how #10072 turned main red)
- **#10352**: loom-daemon reads tracked .loom/config.json from a stale main checkout, silently overriding up-to-date worktrees
- **#10699**: test-spawn-codex.sh hangs forever when the caller's stdin stays open (mock codex blocks on cat)
- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default)
- **#11112**: Remove the safehouse (Matrix) integration from Loom: narration, ChatOps/Concierge, peer-claim channel (forge leases remain)
- **#11325**: Stale operator-stop record keeps a startup drain hold after the fleet store returns the host to running
- **#11426**: workspace_resync fails forever on repos with symlinked .claude/commands or .claude/skills (git add: 'beyond a symbolic link') — miniset never dispatched
- **#11579**: Retire the inert captain-gauges star-facts job (its only reader, star-liveness, was removed by #11104)
- **#11652**: test: later_check_runs_pages_are_spaced flakes under host load, failing the pre-PR gate
- **#11737**: Remove dead budget/cost-rollup API from ActivityDb (#1064): ~600 LOC never called
- **#11760**: pre-commit hook still runs pnpm biome on defaults/**/*.json, but biome was removed (#3353): blocks every merge of main that touches defaults JSON
- **#11787**: issue.estimate coverage gap: operator-session curation sets loom:curated by hand and bypasses curation-complete (loom-ui: 58 of 62 curations since the gate have no estimate)
- **#11798**: dependabot: ignore non-LTS ubuntu bumps for docker base images (from PR #11796)
- **#11837**: forge wait-checks: required-context lookup 403s (needs Administration:read) — post-verdict refuses every approval
- **#11841**: otlp fleet_provenance tests race under plain cargo test (hidden by nextest in CI)
- **#11862**: Codex role runs emit no usage span or session summary
- **#11913**: Session ledger and soft host affinity for resumed role steps
- **#11989**: Role sessions run prompts from stale per-host repo checkouts (re-adds retired labels after migration)
- **#12038**: Telemetry: hold-age gauge (loom.forge.hold_age_seconds) from fleet.state held_since + mirror labels_at for parked issues (no behaviour change; split from #12036)
- **#12041**: CONFLICTING loom:pr PRs are never labelled loom:merge-conflict unless Champion attempts a merge, so they never reach Doctor
- **#12049**: Step dispatch: poisoned-lock handling and stale role reporting
- **#12051**: pass_shard: run the stale-blocked release pass and fleet.state listings on one host per repo
- **#12063**: Umbrella: refactor merge-pr to a minimal contract (deletion-first slices; merge queue after the 2AMLogic move)
- **#12081**: Repeated-rejection escalation counts a silent skip as a second rejection
- **#12083**: Telemetry(dispatch): un-lump pick.decision skip disposition (QueueDisposition/split cap+halted) + held_until on skipped rows (ETA ready_wait, loom-ui#4063 #1)
- **#12085**: Telemetry(doctor): structured parked-PR cause (park reason, doctor_cycles, queue gate, treating age) (ETA doctor, loom-ui#4063 #6)
- **#12093**: Simplify: retire the dispatch-inert tier:* axis (Level::of ignores it; no consumer) — it only adds confusion beside the real priority labels
- **#12117**: Telemetry: inventory every kind; fix or delete role_tick.outcome, ci.job.log, session.output
- **#12121**: Telemetry: daemon derives operator.presence and in-session sweep.* from observed sessions
- **#12127**: ci: rebalance Native Port Suites legs so none runs over ~3 min (next PR critical path after #12089)
- **#12163**: Route 15 loom-daemon integration crates through a fake gh, then restore the live-gh guard
- **#12167**: telemetry: export peer_coordination verdict and degraded duration on host.health

## In Progress

Issues currently being built (`loom:building`).

- **#9865**: retire the gh ladder in forge_gh_comment_rl_safe once every fleet host is >= 0.19.598 (#9774 follow-up)
- **#10127**: test-merge-pr-stale-mergeable.sh intermittently dies with SIGSEGV (rc=139) under bash 5.3.20 after check_branch_name returns
- **#10191**: verify-proposal-refs reports existing directory citations as MISSING FILE
- **#10225**: loom-daemon integration tests (tests/*.rs) may still reach the real gh; extend the #10138 live-gh guard to them
- **#10309**: Managed gitignore misses .loom/private-jobs/ — private-clone runs leave checkouts dirty
- **#10575**: write_scope test fails when sweep env sets LOOM_WRITE_SCOPE_TTL_SECS (test isolation)
- **#10612**: CI: Lines of Code badge push is rejected by the main ruleset on every run but the step reports success
- **#10703**: Three harness suites fail locally when CARGO_TARGET_DIR is exported (on main too)
- **#10770**: resync-installed.sh writes forge labels by default; make forge writes opt-in or list them in --dry-run
- **#10881**: telemetry/kinds.rs merge=union can silently drop/splice lines of multi-line rows (not just duplicate)
- **#11007**: SQL DDL guard denies source edits and read-only searches containing the phrase; agents bypass it via script files
- **#11022**: test(fleet_sync): workspace_resync test fails inside a sweep because GIT_CONFIG_* provenance hooksPath leaks into the scratch repo
- **#11055**: test-land-resync-commit.sh is not hermetic: its fixture reaches the host's installed loom-daemon write-scope gate
- **#11121**: CI: Rust Unit Tests shard stalled in nextest test discovery (lib binary --list), not covered by slow-timeout
- **#11137**: DurableQueue rewrites and fsyncs the whole queue file on every push (O(capacity) per offer)
- **#11219**: Retire classify-error.sh's shell fallback tables once consumers pin loom-daemon classify-error
- **#11227**: role-tool-policy: Edit/Write check sees only the canonical path, so a symlinked credential directory is writable by a read-only role (#8256 follow-up)
- **#11329**: worktree.sh post-worktree hook runs a release build into the worktree-local target/, ignoring CARGO_TARGET_DIR
- **#11332**: A real fleet commit is authored as the test-fixture identity Loom CI <ci@loom.test>
- **#11333**: Unit tests that fail under host load or low disk and pass in isolation (subprocess-timing tests)
- **#11481**: persist insta-crash quarantine and no-op cooldown across daemon restarts
- **#11549**: launch_outcome: every OpenCode launch is classified toolless — tool_name() misses OpenCode's nested part.tool, so working Doctor runs fail and arm launch-failure holds
- **#11583**: fleet_sync: fix the memory-hungry git operation itself (follow-up to #11059 AC 3)
- **#11625**: cargo-shared-target guard: headless/in-session ask is unactionable (retier to deny or surface prefix to the model)
- **#11635**: check-claim treats the calling sweep's own lease as a blocker
- **#11638**: loom-daemon worker run --role builder exits 78 "unknown model profile" when no modelProfile is configured
- **#11710**: Review and simplify CI: inventory, single required-check contract, cut duplicate and always-red jobs
- **#11746**: sync-labels.sh exits 0 when label writes fail (615 failed writes under secondary rate limit reported as success)
- **#11797**: Per-repo fleet tier follow-ups: paused host released on malformed fleet.json; stale fleet-repo.json; journaling
- **#11928**: maintain_only dispatch aborts emit error-level telemetry for an intentional, configured refusal
- **#11947**: Judge bounded-fallback reclaim fires against a live, renewing lease (empty claim timestamp in the reclaim message)
- **#11982**: Provenance out of Loom, stage 1: facts contract on spans and telemetry-schema doc
- **#12028**: role_tick.outcome failures carry no reason (167/6h vs 12 role_attempt failures)
- **#12034**: Champion post-claim re-check should also skip loom:blocked / loom:frozen (follow-up to #11996)
- **#12046**: Step dispatch: main-red halt now stops every role, not just Builder
- **#12048**: Step dispatch: restore the missing-commands guard (#4027/#6440)
- **#12059**: ETA telemetry: operator_recency/hold_release_burst (#11956) not surfacing in SigNoz on floor 1161
- **#12072**: Fleet floor: an unsatisfiable floor isn't re-checked when the platform asset lands
- **#12076**: Worktree-isolation guard treats a '>' inside a quoted argument as a shell redirect
- **#12079**: Add an in-process task/stack dump for a hung daemon tick (hosts restrict ptrace)
- **#12080**: Document that loom:operator must carry a stated operator-only reason
- **#12084**: Telemetry(champion): emit effective per-repo Champion cadence + un-lumped merge-pass skip reasons (ETA merge_wait, loom-ui#4063 #5)
- **#12087**: hooks_home: a LOOM_HOOKS_HOME env override skips the trust check without a warning
- **#12103**: Lease: add 'lease release', and stop a subagent's --watch-pid lease outliving the subagent
- **#12106**: Simplify CI: drop ratchet/budget/process checks from the PR path and merge jobs
- **#12111**: Record each role tick's prompt tree (loom.prompt.*) on role_tick telemetry
- **#12118**: Telemetry: daemon emits issue.estimate from observed curation, not curation-complete
- **#12119**: Telemetry: daemon journals the PR queue at role-tick dispatch, not via pr-queue
- **#12124**: Gateway failure counter: treat a parsed HTTP status >= 500 as unanswered regardless of exit code

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#12107**: fix: drop merge=union on the telemetry kind registry
- **#12139**: fix: gate SQL DDL guard on what executes the statement
- **#12144**: fix: make check_branch_name locale-independent without LC_ALL override
- **#12155**: feat: warn when effective git user.email is a fixture address
- **#12164**: perf(observability): append+compact journal for DurableQueue
- **#12171**: refactor: delete shell classifier tables, keep native delegation
- **#12172**: fix: keep review and repair steps running while main is red
- **#12174**: feat: journal the PR queue from the daemon at role-tick dispatch
- **#12177**: fix: refuse step spawns whose role command is missing from the workspace
- **#12187**: fix(auto-update): re-check a standing unsatisfiable floor every 2 minutes
- **#12188**: feat(telemetry): export failure class on failed role ticks
- **#12191**: feat(telemetry): carry provenance join facts as plain span attributes
- **#12192**: test(write-scope): clear ambient TTL override in cache tests
- **#12200**: ci: bound nextest discovery with a separate compile step and run timeout
- **#12201**: feat(telemetry): daemon emits issue.estimate from observed curation
- **#12206**: fix: log maintain-only dispatch refusals at info, not warn
- **#12209**: fix(claim-staleness): treat a live PR lease as fresh and fail closed on empty claim timestamp
- **#12210**: fix: treat existing directory citations as present in verify-proposal-refs
- **#12212**: feat(hooks-home): warn when a LOOM_HOOKS_HOME pin skips the trust check
- **#12213**: feat(telemetry): record each role tick's prompt tree on role_tick.outcome
- **#12214**: fix(hooks): judge raw and canonical Edit/Write paths for role-tool-policy
- **#12216**: work_finder: stop landed_closing_refs re-asking every tick; gate it off by default (#12205)
- **#12218**: fix(agent-gh): route dispatched agents' gh reads through the gateway and readers, not the writer

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#10346**: fix(dispatch): retain attributed own claims before leaseless yield
- **#11277**: fix(park-record): ignore park markers quoted in Markdown code (Part of #10837)
- **#11313**: fix(lease-renew): make stop verify the target is a renewer before signalling (#11086)
- **#11830**: fix(stale_blocked): reuse budget probes per credential
- **#12000**: C18: finish retiring the old dispatch paths, one host budget
- **#12062**: build: line-tables-only debuginfo for dev/test to cap rustc peak RAM (#11094 item 4)
- **#12096**: fix: mask quoted '>' inside backtick substitutions in write-confinement guard
- **#12116**: ci: remove Lines of Code workflow and README badge
- **#12125**: fix: exit 5 from sync-labels.sh when label writes fail, retry rate limits
- **#12128**: refactor: retire the gh ladder from forge_gh_comment_rl_safe
- **#12130**: feat(telemetry): emit skipped_deferred role_tick.outcome for budget-deferred repos
- **#12141**: fix: ignore .loom/private-jobs/ runtime state in managed gitignore block
- **#12142**: fix: champion rechecks skip loom:blocked and loom:frozen
- **#12146**: test: register land-resync fixtures with the real write-scope gate
- **#12149**: test(fleet_sync): tolerate injected hook trailers in resync commit assertion
- **#12150**: fix: re-arm the no-op cooldown from the outcome journal on restart
- **#12151**: fix: deny instead of ask on a shared cargo build.target-dir
- **#12153**: test: unset ambient CARGO_TARGET_DIR in three target-dir suites
- **#12160**: fix(worker): name profile, source and known profiles in unknown-profile error
- **#12161**: fix(gateway): classify note_attempt by parsed status before exit code
- **#12173**: feat(fleet.state): distinguish operator hold-release from left-census (#12059)
- **#12178**: fix: ignore pre-exec fork children when sampling git peak RSS
- **#12180**: test: widen polling bounds and wait on child readiness in subprocess-timing tests
- **#12182**: ci: merge jobs into Lint, two native-suite legs and one change detector (#12106)
- **#12190**: fix(worker_spawn): read OpenCode's nested part.tool in toolless-launch classifier
- **#12195**: fix(hooks): never build loom-daemon in the post-worktree hook
- **#12199**: docs: add CI contract with required set, layer authority and cut rules

## Proposed

Issues carrying `loom:curated`.

- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
- **#8434**: live-verify native-ephemeral containment: canary run in-container, two-worker filesystem disjointness, post-run writable-layer credential scan *(curated)*
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
- **#9865**: retire the gh ladder in forge_gh_comment_rl_safe once every fleet host is >= 0.19.598 (#9774 follow-up) *(curated)*
- **#9908**: Consolidate persistence: one journal per concern, retire duplicated stores *(curated)*
- **#9957**: daemon: deliberate stops (SIGTERM) are silent autonomy losses — the watchdog goes down with it, the intent marker is deleted, and bootstrap cannot recover *(curated)*
- **#9978**: feat(champion): optional per-repo merge-queue mode — enqueue Judge-approved PRs to GitHub's merge queue instead of merge-pr.sh's freshness/re-date loop *(curated)*
- **#10023**: daemon: one stable host.id per machine, plus start/stop/heartbeat events in SigNoz *(curated)*
- **#10079**: locate-daemon-bin prefers a stale target/ build over the installed daemon — post-comment.sh fails for every role session on that host *(curated)*
- **#10083**: CI path filter skips label_registry Rust tests on label-file-only PRs (how #10072 turned main red) *(curated)*
- **#10127**: test-merge-pr-stale-mergeable.sh intermittently dies with SIGSEGV (rc=139) under bash 5.3.20 after check_branch_name returns *(curated)*
- **#10150**: notify-cleared-blockers runs only from merge-pr.sh: merges made any other way never clear a block *(curated)*
- **#10181**: ci: backend paths filter omits defaults/forge/**, so forge_inventory tests skip on manifest/baseline-only PRs *(curated)*
- **#10191**: verify-proposal-refs reports existing directory citations as MISSING FILE *(curated)*
- **#10196**: telemetry: make SigNoz point-in-time reconstructable — knowable-at timestamps, full fleet-state snapshots over OTLP, per-host export-coverage records, replay contract *(curated)*
- **#10225**: loom-daemon integration tests (tests/*.rs) may still reach the real gh; extend the #10138 live-gh guard to them *(curated)*
- **#10257**: merge queue Phase C: combined-tree CI qualification and eligible-repo live pilot (parent #9978) *(curated)*
- **#10309**: Managed gitignore misses .loom/private-jobs/ — private-clone runs leave checkouts dirty *(curated)*
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day) *(curated)*
- **#10335**: Per-repo opt-out for the PreToolUse guard hooks that install/upgrade respects *(curated)*
- **#10337**: Dispatcher yields to its own leaseless loom:building label and orphans the issue (3 starred issues 2026-10-05 00:40Z) *(curated)*
- **#10352**: loom-daemon reads tracked .loom/config.json from a stale main checkout, silently overriding up-to-date worktrees *(curated)*
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue *(curated)*
- **#10456**: Epic: the fleet keeps its own hold queue clean — only real operator asks reach Needs attention *(curated)*
- **#10512**: Fleet reader-App pool near exhaustion: cut top billable pollers (visibility.repo cache, issue_state ETag, cross-host dedupe) — ~8k REST/h *(curated)*
- **#10575**: write_scope test fails when sweep env sets LOOM_WRITE_SCOPE_TTL_SECS (test isolation) *(curated)*
- **#10612**: CI: Lines of Code badge push is rejected by the main ruleset on every run but the step reports success *(curated)*
- **#10698**: Fleet version floor: the daemon enforces `loom_min_version` from the fleet store, rolls itself forward, and brings each repo's installed Loom up to its own version *(curated)*
- **#10699**: test-spawn-codex.sh hangs forever when the caller's stdin stays open (mock codex blocks on cat) *(curated)*
- **#10703**: Three harness suites fail locally when CARGO_TARGET_DIR is exported (on main too) *(curated)*
- **#10770**: resync-installed.sh writes forge labels by default; make forge writes opt-in or list them in --dry-run *(curated)*
- **#10802**: General cleanup: reap agent process residue (dev servers, orphaned children) after any agent exit *(curated)*
- **#10827**: Promotion author gate: don't auto-promote issues authored by untrusted identities (signed decision markers dropped) *(curated)*
- **#10838**: merge_pr_reconcile_differential: claim-gate differential disagrees on case-variant duplicate label (loom:buildinG) *(curated)*
- **#10881**: telemetry/kinds.rs merge=union can silently drop/splice lines of multi-line rows (not just duplicate) *(curated)*
- **#10916**: Fleet singleton output watchdog: alert when any one-host fleet job stops producing output (not just when its host goes silent) *(curated)*
- **#11007**: SQL DDL guard denies source edits and read-only searches containing the phrase; agents bypass it via script files *(curated)*
- **#11022**: test(fleet_sync): workspace_resync test fails inside a sweep because GIT_CONFIG_* provenance hooksPath leaks into the scratch repo *(curated)*
- **#11055**: test-land-resync-commit.sh is not hermetic: its fixture reaches the host's installed loom-daemon write-scope gate *(curated)*
- **#11086**: sweep-lease-renew.sh stop <arg> kills any PID, so an issue number passed by mistake signals an unrelated process *(curated)*
- **#11094**: Work-finder RAM admission charges 2 GB per sweep, but a loom-daemon rustc peaks at ~13 GB: concurrent loom builds OOM 30 GB workers *(curated)*
- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default) *(curated)*
- **#11112**: Remove the safehouse (Matrix) integration from Loom: narration, ChatOps/Concierge, peer-claim channel (forge leases remain) *(curated)*
- **#11118**: Umbrella: claim and lease state is inferred from label age and presence — live work gets orphaned, stripped or doubled (13 bugs) *(curated)*
- **#11121**: CI: Rust Unit Tests shard stalled in nextest test discovery (lib binary --list), not covered by slow-timeout *(curated)*
- **#11137**: DurableQueue rewrites and fsyncs the whole queue file on every push (O(capacity) per offer) *(curated)*
- **#11219**: Retire classify-error.sh's shell fallback tables once consumers pin loom-daemon classify-error *(curated)*
- **#11227**: role-tool-policy: Edit/Write check sees only the canonical path, so a symlinked credential directory is writable by a read-only role (#8256 follow-up) *(curated)*
- **#11240**: Verify secret-scanning and push-protection settings for #9049 with authoritative live evidence *(curated)*
- **#11283**: OpenCode 2.x guarded launches: load the native-tools plugin via the 2.x default-export shape, live-verify, flip guard_verified *(curated)*
- **#11284**: Z.ai seats: per-dispatch runtime/modelProfile and keep free seats busy (remaining scope after #11412/#11303/fleet-gitops#196) *(curated)*
- **#11285**: Sweeps: run each phase on its role's runtime (GLM Builder/Doctor via a phase-worker subcommand; Judge/Curator never GLM) *(curated)*
- **#11286**: Z.ai api-keys pool fidelity: real exhaustion fixtures, reset-aligned cooldowns, per-account gauges, burn sampler misses guarded OpenCode stores *(curated)*
- **#11287**: Umbrella: Z.ai GLM-5.3-Flash seats landing code in the fleet via OpenCode *(curated)*
- **#11300**: Z.ai seat telemetry via the egress proxy in transparent observe mode: per-request tokens/latency/429s to SigNoz, OpenCode headers untouched *(curated)*
- **#11325**: Stale operator-stop record keeps a startup drain hold after the fleet store returns the host to running *(curated)*
- **#11329**: worktree.sh post-worktree hook runs a release build into the worktree-local target/, ignoring CARGO_TARGET_DIR *(curated)*
- **#11332**: A real fleet commit is authored as the test-fixture identity Loom CI <ci@loom.test> *(curated)*
- **#11333**: Unit tests that fail under host load or low disk and pass in isolation (subprocess-timing tests) *(curated)*
- **#11406**: captain gauges (W12) cover only the captain's registered workspaces — 2 of 61 repos on the 2AMLogic fleet, so stand-down saves ~nothing *(curated)*
- **#11426**: workspace_resync fails forever on repos with symlinked .claude/commands or .claude/skills (git add: 'beyond a symbolic link') — miniset never dispatched *(curated)*
- **#11441**: Fleet policy lives in fleet-gitops only: move runtimes/autonomous policy out of committed .loom/config.json (tier 2 silently shadows the fleet tier) *(curated)*
- **#11481**: persist insta-crash quarantine and no-op cooldown across daemon restarts *(curated)*
- **#11514**: Decide ownership and convergence model for Loom hook entries in consumer Claude settings *(curated)*
- **#11549**: launch_outcome: every OpenCode launch is classified toolless — tool_name() misses OpenCode's nested part.tool, so working Doctor runs fail and arm launch-failure holds *(curated)*
- **#11579**: Retire the inert captain-gauges star-facts job (its only reader, star-liveness, was removed by #11104) *(curated)*
- **#11583**: fleet_sync: fix the memory-hungry git operation itself (follow-up to #11059 AC 3) *(curated)*
- **#11595**: Pause-and-roll budget path can still kill an in-flight merge-pr.sh (follow-up to #11263) *(curated)*
- **#11625**: cargo-shared-target guard: headless/in-session ask is unactionable (retier to deny or surface prefix to the model) *(curated)*
- **#11635**: check-claim treats the calling sweep's own lease as a blocker *(curated)*
- **#11638**: loom-daemon worker run --role builder exits 78 "unknown model profile" when no modelProfile is configured *(curated)*
- **#11652**: test: later_check_runs_pages_are_spaced flakes under host load, failing the pre-PR gate *(curated)*
- **#11678**: Write-scope vets cross-owner roots under the daemon's process credential: 4th site (roll lease refresh) and a class-level fix *(curated)*
- **#11710**: Review and simplify CI: inventory, single required-check contract, cut duplicate and always-red jobs *(curated)*
- **#11712**: Daemon: export OTLP only and remove inbound control channels other than config (2am#4300) *(curated)*
- **#11737**: Remove dead budget/cost-rollup API from ActivityDb (#1064): ~600 LOC never called *(curated)*
- **#11746**: sync-labels.sh exits 0 when label writes fail (615 failed writes under secondary rate limit reported as success) *(curated)*
- **#11760**: pre-commit hook still runs pnpm biome on defaults/**/*.json, but biome was removed (#3353): blocks every merge of main that touches defaults JSON *(curated)*
- **#11761**: Keep-it-simple beyond Curator: Judge and Champion gate on unjustified complexity; Curator checks 'already covered' first (follow-up to #11662) *(curated)*
- **#11787**: issue.estimate coverage gap: operator-session curation sets loom:curated by hand and bypasses curation-complete (loom-ui: 58 of 62 curations since the gate have no estimate) *(curated)*
- **#11797**: Per-repo fleet tier follow-ups: paused host released on malformed fleet.json; stale fleet-repo.json; journaling *(curated)*
- **#11798**: dependabot: ignore non-LTS ubuntu bumps for docker base images (from PR #11796) *(curated)*
- **#11837**: forge wait-checks: required-context lookup 403s (needs Administration:read) — post-verdict refuses every approval *(curated)*
- **#11839**: Emit a fact when the daemon exits: signal, requester if known, supervised relaunch or not, in-flight sweeps killed *(curated)*
- **#11841**: otlp fleet_provenance tests race under plain cargo test (hidden by nextest in CI) *(curated)*
- **#11843**: Remove the transitional readers of retired hold labels once the #11795 migration is done *(curated)*
- **#11862**: Codex role runs emit no usage span or session summary *(curated)*
- **#11913**: Session ledger and soft host affinity for resumed role steps *(curated)*
- **#11928**: maintain_only dispatch aborts emit error-level telemetry for an intentional, configured refusal *(curated)*
- **#11947**: Judge bounded-fallback reclaim fires against a live, renewing lease (empty claim timestamp in the reclaim message) *(curated)*
- **#11982**: Provenance out of Loom, stage 1: facts contract on spans and telemetry-schema doc *(curated)*
- **#11989**: Role sessions run prompts from stale per-host repo checkouts (re-adds retired labels after migration) *(curated)*
- **#12014**: ci: approved PRs touching workflow files need several CI rounds to land as main moves (no merge queue on a user-owned repo) *(curated)*
- **#12016**: Deduplicate the pending-CI page loop (pr_planning/ci_gate.rs vs cli/pr_lease/backstop.rs) *(curated)*
- **#12019**: [ci-daily] Flake Detection (shell suites, macos-latest, parallelism 1) *(curated)*
- **#12028**: role_tick.outcome failures carry no reason (167/6h vs 12 role_attempt failures) *(curated)*
- **#12029**: Telemetry: Judge/merge runs must record found-work, PR, head_sha and verdict (no-op ticks indistinguishable today) *(curated)*
- **#12034**: Champion post-claim re-check should also skip loom:blocked / loom:frozen (follow-up to #11996) *(curated)*
- **#12038**: Telemetry: hold-age gauge (loom.forge.hold_age_seconds) from fleet.state held_since + mirror labels_at for parked issues (no behaviour change; split from #12036) *(curated)*
- **#12041**: CONFLICTING loom:pr PRs are never labelled loom:merge-conflict unless Champion attempts a merge, so they never reach Doctor *(curated)*
- **#12043**: macOS bash 3.2: 7 shell suites fail on bash-4+ semantics (empty arrays under set -u, ${x,,}, \001 array sentinel, heredoc apostrophe in $(), mapfile, source <()) *(curated)*
- **#12046**: Step dispatch: main-red halt now stops every role, not just Builder *(curated)*
- **#12048**: Step dispatch: restore the missing-commands guard (#4027/#6440) *(curated)*
- **#12049**: Step dispatch: poisoned-lock handling and stale role reporting *(curated)*
- **#12059**: ETA telemetry: operator_recency/hold_release_burst (#11956) not surfacing in SigNoz on floor 1161 *(curated)*
- **#12072**: Fleet floor: an unsatisfiable floor isn't re-checked when the platform asset lands *(curated)*
- **#12076**: Worktree-isolation guard treats a '>' inside a quoted argument as a shell redirect *(curated)*
- **#12079**: Add an in-process task/stack dump for a hung daemon tick (hosts restrict ptrace) *(curated)*
- **#12080**: Document that loom:operator must carry a stated operator-only reason *(curated)*
- **#12081**: Repeated-rejection escalation counts a silent skip as a second rejection *(curated)*
- **#12082**: Worktree-isolation guard blocks Bash writes to paths outside the repo when the cwd is the main checkout *(curated)*
- **#12087**: hooks_home: a LOOM_HOOKS_HOME env override skips the trust check without a warning *(curated)*
- **#12103**: Lease: add 'lease release', and stop a subagent's --watch-pid lease outliving the subagent *(curated)*
- **#12104**: verdict-staleness guard reads verdict=approve as no verdict; share one marker parser *(curated)*
- **#12111**: Record each role tick's prompt tree (loom.prompt.*) on role_tick telemetry *(curated)*
- **#12112**: host_status (slice a): host_status sampler, EWMA filters and OTel metric export (no consumers) *(curated)*
- **#12118**: Telemetry: daemon emits issue.estimate from observed curation, not curation-complete *(curated)*
- **#12119**: Telemetry: daemon journals the PR queue at role-tick dispatch, not via pr-queue *(curated)*
- **#12121**: Telemetry: daemon derives operator.presence and in-session sweep.* from observed sessions *(curated)*
- **#12124**: Gateway failure counter: treat a parsed HTTP status >= 500 as unanswered regardless of exit code *(curated)*
- **#12127**: ci: rebalance Native Port Suites legs so none runs over ~3 min (next PR critical path after #12089) *(curated)*
- **#12132**: Version bump shares the fleet dispatch App's rate limit, so a fleet-wide read drain blocks the release that fixes it *(curated)*
- **#12133**: Step dispatch: several hosts spawned concurrent Builders on one item while lease reads were throttled (1162) *(curated)*
- **#12137**: merge-pr.sh and Champion criterion 6 disagree on non-required failing checks (pending-to-failed during --auto) *(curated)*
- **#12147**: Pause-and-roll polish: H5 status line reads as stuck; gh version check cached for the process life (from #11812) *(curated)*
- **#12148**: Delete the operator wait-for-zero drain path once pause-and-roll resumes work (from #11812) *(curated)*
- **#12163**: Route 15 loom-daemon integration crates through a fake gh, then restore the live-gh guard *(curated)*
- **#12168**: telemetry: emit a counted event per write refused by the #9548 credential-scope guard *(curated)*
- **#12193**: stage_dwell: /issues listing reads still use the dispatch App after #12138 *(curated)*

## Proposed (Architect / Hermit)

- **#11347**: [Epic #9908] Phase 2c: Execution journal shadow appends from checkpoint CLI *(architect)*
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

## Backlog Balance

| Bucket | Count |
|------|-------|
| Operator merge-risk holds | 6 |
| Very important | 0 |
| Important | 12 |
| Ready (`loom:issue`) | 35 |
| In Progress (`loom:building`) | 49 |
| PRs awaiting review | 23 |
| Approved PRs awaiting merge | 27 |
| Curated | 138 |
| Architect / Hermit proposals | 2 |
| Active epics | 10 |
<!-- guide:plan-body:end -->
