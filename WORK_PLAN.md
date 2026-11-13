# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs under a hold (`loom:blocked` or `loom:operator`) — only a human merge decision is missing.

- **#12969**: feat: age-bound Codex rollout history in live profiles
- **#13582**: fix: honour a trusted loom:issue adoption in the promotion author gate
- **#13802**: fix: list user-level writes in the install preview
- **#13816**: fix(hooks): treat >(cmd) process substitution as non-target in vendored guard
- **#13830**: fix: never re-anchor a CI-gate-only verdict across a head move
- **#13847**: chore(deps): drop the criterion dev-dependency and its no-op bench

## Very Important

`loom:very-important` (one counts per workspace); every stage takes these first.

_None._

## Important

`loom:important`; taken before unlabelled work.

- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue
- **#12909**: ci_telemetry: 2amlogic owner discovery times out every cycle since Oct 17 (no loom-ui CI facts; 478 consecutive failures)
- **#12993**: Shadow evaluation of the new Champion (2026-10-20): 1/95 holds cleared — ranked remaining causes + fixtures
- **#13734**: ratification_lane.tick: outcome attributes never reach the store, and no keys posted on 0.19.1283 after 2 h
- **#13744**: Reads can still land on the writer: raw run_gh bypasses GhInvocation routing (writer budget exhausted 35 min of every hour on 1283)

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default)
- **#12167**: telemetry: export peer_coordination verdict and degraded duration on host.health
- **#12604**: Split status_render.rs: break up ~970-line print_status_human and move inline tests out
- **#12638**: Test: lease_refresh_child_carries_the_root_owners_gh_config_dir fails on main (child sees no GH_CONFIG_DIR)
- **#13744**: Reads can still land on the writer: raw run_gh bypasses GhInvocation routing (writer budget exhausted 35 min of every hour on 1283)
- **#13746**: Writer rate limit is misreported: lease claim says 'could not read lease records', and the finder keeps drawing steps it must refuse
- **#13760**: Guide WORK_LOG backfill wrote another repo's history; docs-only fast path approved it
- **#13857**: Split claude_config out of terminal.rs: 1190 of its 2748 lines are non-terminal code reached through six forwarding wrappers
- **#13859**: Replace 47 inline env-truthy copies in loom-daemon with env_flag::parse_truthy (follow-up to #13376)
- **#13860**: Remove has_unnegated_closing_ref_in_repo: its only caller was deleted by #11301

## In Progress

Issues currently being built (`loom:building`).

- **#13800**: install.sh --dry-run omits the writes made outside the target repo
- **#13814**: guard-destructive-generic.sh (vendored fallback): >(cmd) process substitution still misread as a worktree write
- **#13822**: CI-gate-only verdict re-anchor defeats ci_gate_recovery, stranding green PRs in the Doctor queue
- **#13826**: Remove hollow assertions from test-pr-worktree-isolation.sh: they test comments, a local regex copy, and git itself
- **#13827**: Remove stale file-size ratchet comments: 242 files cite a gate #12099 deleted, and new code still obeys it
- **#13835**: checkout_ff stranded self-heal: forge lookup on every pass, rescue ref per failed checkout, doc kinds
- **#13837**: Remove EagerTrigger unit struct and one-variant TriggerReason: stateless residue of #12406
- **#13841**: Remove the vestigial criterion bench: it measures no Loom code and nothing runs it
- **#13848**: Simplify dep_classify/fingerprint.rs: reuse short_hash::short_sha16 and hex::encode instead of hand-rolled SHA-256 hex loops
- **#13850**: quickstarts/api: tsc --noEmit fails with 14 errors in src/routes/users.ts on a pristine checkout
- **#13851**: CI never runs the quickstart template test suites (quickstarts/webapp vitest)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#12631**: fix: release a Champion CI wait as requeued so it holds flat
- **#12637**: refactor: split status_render inline tests into per-module files
- **#12639**: test: create registered roots before registering their gh config dir
- **#13856**: fix(checkout_ff): ask the merged-PR lookup last and reuse a stranded HEAD's rescue ref

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#12969**: feat: age-bound Codex rollout history in live profiles
- **#13582**: fix: honour a trusted loom:issue adoption in the promotion author gate
- **#13802**: fix: list user-level writes in the install preview
- **#13816**: fix(hooks): treat >(cmd) process substitution as non-target in vendored guard
- **#13830**: fix: never re-anchor a CI-gate-only verdict across a head move
- **#13842**: docs: drop stale file-size ratchet comments from defaults, tests, install.sh, mcp-loom
- **#13843**: test: drop hollow assertions from test-pr-worktree-isolation.sh
- **#13847**: chore(deps): drop the criterion dev-dependency and its no-op bench
- **#13852**: refactor: replace eager-reclaim EagerTrigger/TriggerReason with free functions
- **#13855**: fix(quickstarts/api): register admin middleware via route config, not the handler slot
- **#13858**: ci: run the quickstarts/webapp suite and typecheck in Node Packages

## Proposed

Issues carrying `loom:curated`.

- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
- **#8525**: tracing: instrument sweep phases and role attempts, including Pi/OpenCode and repair cycles *(curated)*
- **#8698**: Live end-to-end verification of the credential egress proxy (AC5 of #8674) *(curated)*
- **#9136**: main ruleset: bypass_actors 'RepositoryRole:always' makes pull_request and required_status_checks advisory *(curated)*
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day) *(curated)*
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue *(curated)*
- **#10698**: Fleet version floor: the daemon enforces `loom_min_version` from the fleet store, rolls itself forward, and brings each repo's installed Loom up to its own version *(curated)*
- **#10838**: merge_pr_reconcile_differential: claim-gate differential disagrees on case-variant duplicate label (loom:buildinG) *(curated)*
- **#10941**: No-PR retry hold fires when the only remaining work is a live check or a cross-repo PR *(curated)*
- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default) *(curated)*
- **#11112**: Remove the safehouse (Matrix) integration from Loom: narration, ChatOps/Concierge, peer-claim channel (forge leases remain) *(curated)*
- **#11283**: OpenCode 2.x guarded launches: load the native-tools plugin via the 2.x default-export shape, live-verify, flip guard_verified *(curated)*
- **#11284**: Runtime readiness: per-repo, per-stage weighted model selection in fleet-gitops *(curated)*
- **#11287**: Umbrella: Z.ai GLM-5.3-Flash seats landing code in the fleet via OpenCode *(curated)*
- **#11300**: Z.ai seat telemetry via the egress proxy in transparent observe mode: per-request tokens/latency/429s to SigNoz, OpenCode headers untouched *(curated)*
- **#11595**: Pause-and-roll budget path can still kill an in-flight merge-pr.sh (follow-up to #11263) *(curated)*
- **#11703**: Config precedence: fleet-gitops values must override repo-committed .loom/config.json (general rule + enforcement) *(curated)*
- **#11911**: Finish retiring daemon-spawned /loom:sweep and merge the two concurrency budgets *(curated)*
- **#12029**: Telemetry: merge runs record PR number and observed result (merge-pr chokepoint) — Judge parts moved to #12362; held behind #12063 *(curated)*
- **#12063**: Umbrella: refactor merge-pr to a minimal contract (deletion-first slices; merge queue after the 2AMLogic move) *(curated)*
- **#12094**: Retire the telemetry replay subsystem and the kinds left without a producer by #11301 *(curated)*
- **#12248**: Retired hold labels: remove core registry/work_finder readers (1/3 of #11843) *(curated)*
- **#12304**: Epic: fleet discovery from one mirror read per tick — zero forge reads for discovery; delete per-host listing paths *(curated)*
- **#12320**: Cleanup once every worker reads the worklist: delete the per-call `forge_mirror` discovery path, `maxStalenessSecs` and `per_call`; docs and ADR-0024; direct backend preserved *(curated)*
- **#12408**: Live-verify epic child discovery against sky130-sar-adc#23 (follow-up to #12388) *(curated)*
- **#12418**: Verify epic #10698 acceptance: completed-vs-not-planned dispositions and live fleet receipts *(curated)*
- **#12604**: Split status_render.rs: break up ~970-line print_status_human and move inline tests out *(curated)*
- **#12626**: docs: /repo:release Phase 1.5 CHANGELOG completeness gate has no seam for generated-notes policy *(curated)*
- **#12638**: Test: lease_refresh_child_carries_the_root_owners_gh_config_dir fails on main (child sees no GH_CONFIG_DIR) *(curated)*
- **#12703**: Add a per-repo setting to turn off Champion's out-of-band acceptance-criteria hold (#6883) *(curated)*
- **#12713**: Part-of merges re-queue the issue to Builders: landed-route marker is read as a Curator 'kept' decision *(curated)*
- **#12909**: ci_telemetry: 2amlogic owner discovery times out every cycle since Oct 17 (no loom-ui CI facts; 478 consecutive failures) *(curated)*
- **#12966**: Bound Codex session/thread history inside live profiles (the real codex-profiles growth) + uv cache *(curated)*
- **#12978**: Codex shared-seat dispatch: live receipts and startup measurements (split from #10796) *(curated)*
- **#13155**: [ci-daily] Flake Detection (shell suites, macos-latest, parallelism 1) *(curated)*
- **#13317**: Audit Champion merge-risk self-releases two weeks after #12960 is live *(curated)*
- **#13378**: Scope trust for github-actions[bot] to ci-daily tracker authorship instead of repo-wide trustedCommenters (alternative to #13340) *(curated)*
- **#13486**: Champion resync waiver: a byte-identical test under .loom/scripts/tests/ is not a merge-gate path — every release shipping test-champion-resync-waiver.sh holds all resync PRs *(curated)*
- **#13521**: Builder: ~11% of fleet sweeps still end without an outcome line after #13447 (worker-3 29%, worker-4 0%) *(curated)*
- **#13578**: Promotion author gate re-holds an issue the operator already adopted, after landed-route re-curation *(curated)*
- **#13590**: Observe convert_awaiting_revision_parks on a live Guide tick (#10936 follow-up) *(curated)*
- **#13640**: ci_telemetry: owner-kind probe users/2amlogic hangs 120s every cycle in the daemon (0.3s by hand) and a timeout never fails over — 2AMLogic CI dropped 20+ days *(curated)*
- **#13668**: Replay strict resync waiver on recent fasterhenry resync PRs (follow-up to #13665) *(curated)*
- **#13720**: worktree.issue_state_rest reads spike every 4 h (synchronized cache expiry?) *(curated)*
- **#13734**: ratification_lane.tick: outcome attributes never reach the store, and no keys posted on 0.19.1283 after 2 h *(curated)*
- **#13736**: ratification_lane on 0.19.1283: every key withheld (pin 'could not resolve' under the daemon) and 60 PRs unreadable; ~390 WARN/h per host, no keys posted *(curated)*
- **#13762**: Ratification lane posts ratify:key markers with no verdict, as the PR author, with wrapper logs as the body *(curated)*
- **#13800**: install.sh --dry-run omits the writes made outside the target repo *(curated)*
- **#13814**: guard-destructive-generic.sh (vendored fallback): >(cmd) process substitution still misread as a worktree write *(curated)*
- **#13822**: CI-gate-only verdict re-anchor defeats ci_gate_recovery, stranding green PRs in the Doctor queue *(curated)*
- **#13835**: checkout_ff stranded self-heal: forge lookup on every pass, rescue ref per failed checkout, doc kinds *(curated)*
- **#13850**: quickstarts/api: tsc --noEmit fails with 14 errors in src/routes/users.ts on a pristine checkout *(curated)*
- **#13851**: CI never runs the quickstart template test suites (quickstarts/webapp vitest) *(curated)*

## Proposed (Architect / Hermit)

- **#13071**: Peer claims: move machine coordination off rendered room messages (custom event type / state events) *(architect)*
- **#13776**: Remove write-only prompt_changes pipeline (git_parser, CaptureGitChanges): no reader, uncorrelatable rows *(hermit)*
- **#13818**: Remove dead spawn-loop-state.json reader: loom-clean deletes live claim locks because its only liveness source is never present *(hermit)*
- **#13820**: Remove the loom-api crate: reads a database the daemon does not write, 3 of 5 endpoints query tables nothing creates *(hermit)*
- **#13829**: Remove the inert per-host overflow slot: nothing can set the flag since #11508, but its write, restart and report paths remain *(hermit)*
- **#13831**: Remove SweepState::Pending: a lifecycle state nothing constructs, matched at 25 production sites *(hermit)*
- **#13832**: Remove the single-workspace role and main-health cadence loops: production runs only the multi-workspace ones, and 7 tests exercise the stale copy *(hermit)*
- **#13833**: Consolidate eight copies of the loom-dir resolution rule (LOOM_SOCKET_PATH parent, else ~/.loom) into one helper *(hermit)*
- **#13861**: Remove NoopCooldownState::consecutive: a second no-op streak counter nothing acts on, whose documented reset does not exist *(hermit)*
- **#13862**: Consolidate seven private kill(pid, 0) liveness probes in loom-daemon onto the shared is_pid_alive (copies disagree on EPERM) *(hermit)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day)
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue
- **#10698**: Fleet version floor: the daemon enforces `loom_min_version` from the fleet store, rolls itself forward, and brings each repo's installed Loom up to its own version
- **#11287**: Umbrella: Z.ai GLM-5.3-Flash seats landing code in the fleet via OpenCode
- **#12304**: Epic: fleet discovery from one mirror read per tick — zero forge reads for discovery; delete per-host listing paths
- **#12708**: Epic: loom-tools CLI for agents, shared Rust implementation with the daemon

## Backlog Balance

| Bucket | Count |
|------|-------|
| Operator merge-risk holds | 6 |
| Very important | 0 |
| Important | 5 |
| Ready (`loom:issue`) | 10 |
| In Progress (`loom:building`) | 11 |
| PRs awaiting review | 4 |
| Approved PRs awaiting merge | 11 |
| Curated | 53 |
| Architect / Hermit proposals | 10 |
| Active epics | 7 |
<!-- guide:plan-body:end -->
