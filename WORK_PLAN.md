# Work Plan

Backlog snapshot from current GitHub label state, maintained by the Guide role. Direction lives in the hand-written roadmap: [docs/ROADMAP.md](docs/ROADMAP.md).

The body below is a snapshot of GitHub label state as of the last merged docs-maintenance PR — it may lag live labels until the next one merges. The `_Generated:` stamp at the top of the generated region dates the snapshot; `git log -1 -- WORK_PLAN.md` finds the merge that wrote it.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
_Generated: 2026-11-19T09:19:38Z (snapshot; compare against live labels)_
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs under a hold (`loom:blocked` or `loom:operator`) — only a human merge decision is missing.

- **#13997**: docs(judge): split judge.md into always-read core + on-demand judge-* sub-skills
- **#14046**: refactor: remove the loom-api crate from the workspace
- **#14078**: fix: classify gh api -F/--raw-field as writes in gh-cached; flag stalled census reads
- **#14086**: feat: add workflow-lint subcommand and Doctor pre-push workflow gate
- **#14108**: fix(private_workspace): forward the freshest owner token, not a fixed primary-first one
- **#14136**: fix: ignore pnpm artifacts in the managed gitignore block of loom-workspace stub repos
- **#14171**: ci: add named nextest discovery step with stall diagnostics
- **#14181**: feat: seal the hook registration for private-clone Codex sessions
- **#14193**: fix: census RELEASED lists released ratification holds; notice gains a reason bullet
- **#14243**: fix: stamp the forge's canonical repo name into loom_source_remote
- **#14253**: fix: drop dead unreadable verdict from ratification help and defer row
- **#14260**: fix(daemon): contained launches draw only private-clone accounts registering the workspace repository
- **#14267**: fix: clear recycled-pid claim locks in loom-clean's stale-lock pass
- **#14276**: ci: gate .agents/skills mirrors on generate-agent-skills --check in lint
- **#14281**: docs: name 2AMLogic/loom in shipped files and add old-slug guard test
- **#14283**: docs: drop retired claude-md-budget and docs-defaults-parity CI jobs from judge prompt
- **#14289**: feat(daemon): publish the captain's token probe to fleet workers
- **#14293**: fix: test hold standing, not marker presence, in ratification-hold release path
- **#14295**: ci: gate docs/README.md index against the docs/ tree
- **#14297**: ci: forbid the current owner's repo URL in installed templates
- **#14319**: ci: cover all of defaults/ in the backend PR path filter
- **#14322**: fix: drop unreachable action=defer from champion-hold ratification --help
- **#14330**: daemon: detect a checkout whose origin owner drifted from its resolved credential
- **#14335**: docs: drop stale file-size ratchet comments from loom-daemon and ci.yml (slice c)
- **#14366**: fix: defer merge-pr --release-hold writes until required checks pass
- **#14368**: docs: correct merge-queue dormancy reason and cutover step 3 failures
- **#14381**: fix: don't carry an exhausted .ranking row over permanently-marked accounts; document carry-forward
- **#14384**: fix: require native ownership before handing a fresh worktree to a Builder
- **#14410**: fix: append --reviews-reconciled on changes-requested; word a repeat lease release as already released
- **#14411**: fix: carry exhausted weekly-hold row forward over a header-less 2xx probe
- **#14437**: fix: attribute stranded check runs to their failed run in the merge-group check
- **#14441**: fix: publish worker images under the repository owner's GHCR namespace
- **#14452**: fix: record held_back paths and exit 3 when resync withholds a locally-fixed file
- **#14470**: fix: remove stale by-owner writer credentials once the owner is no longer a cross-owner
- **#14478**: feat: serve hold diagnostics and scoped issue workflows from loom-tools
- **#14483**: fix: let recover list and clear dirty leftover worktrees that block a private-clone seat
- **#14498**: fix: pass over unprovisioned and sandbox-held Codex seats before a draw
- **#14514**: feat(tools): share worktree CLI operations and read routing
- **#14518**: fix(verdict-guard): don't read the verdict writer's own label as an operator send-back
- **#14532**: fix: verify WORK_LOG records against the target repo before Guide writes and Judge fast-paths
- **#14546**: fix: let a CLI process ask the daemon for its primary workspace root
- **#14576**: fix(lease): route installed issue lease reads through Loom mode
- **#14596**: feat: dispatch Claude steps into persistent account seats

## Very Important

`loom:very-important` (one counts per workspace); every stage takes these first.

_None._

## Important

`loom:important`; taken before unlabelled work.

- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue *(held)*
- **#12909**: ci_telemetry: 2amlogic owner discovery times out every cycle since Oct 17 (no loom-ui CI facts; 478 consecutive failures) *(held)*
- **#12993**: Shadow evaluation of the new Champion (2026-10-20): 1/95 holds cleared — ranked remaining causes + fixtures *(held)*
- **#13744**: Reads can still land on the writer: raw run_gh bypasses GhInvocation routing (writer budget exhausted 35 min of every hour on 1283)
- **#14566**: Telemetry: about 40% of OpenCode (GLM) runs on 0.19.1316 emit no loom.runtime.usage row
- **#14578**: Codex Builder/Doctor admission says 'account session is busy' while other seats on the host are idle (0.19.1316; #14492 covered read-only roles only)

## Ready

Human-approved issues (`loom:issue`) ready for implementation — excluding held (`loom:blocked`/`loom:operator`/`loom:frozen`), `loom:building`, and issues closed by an open `loom:pr` PR (listed under Approved).

- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default)
- **#12604**: Split status_render.rs: break up ~970-line print_status_human and move inline tests out
- **#14578**: Codex Builder/Doctor admission says 'account session is busy' while other seats on the host are idle (0.19.1316; #14492 covered read-only roles only)
- **#14613**: CI: backend path filter omits the role-prompt trees, so prompt-only PRs skip the 14 Rust tests that lint them

## In Progress

Issues currently being built (`loom:building`).

- **#13744**: Reads can still land on the writer: raw run_gh bypasses GhInvocation routing (writer budget exhausted 35 min of every hour on 1283)
- **#13760**: Guide WORK_LOG backfill wrote another repo's history; docs-only fast path approved it
- **#13820**: Remove the loom-api crate: reads a database the daemon does not write, 3 of 5 endpoints query tables nothing creates
- **#13827**: Remove stale file-size ratchet comments: 242 files cite a gate #12099 deleted, and new code still obeys it
- **#13875**: Held-PR digest cannot state a ratification hold's reason: Step 1 greps only the merge-risk and critical-file markers (every such row reads "reason unrecorded")
- **#13896**: Doctor conflict-resolution should structurally validate touched workflow files; a vanished check should not satisfy a green rollup
- **#13898**: Installer's managed .gitignore block ignores neither node_modules/ nor pnpm-lock.yaml, which the root package.json stub's documented pnpm entry points produce
- **#13933**: forge_auth forwards an expired token: a stale per-workspace gh-config-by-owner wins over the refreshed primary one
- **#13942**: Token pool: captain publishes its account probe; workers apply it (part 2 of #13893)
- **#13957**: Shrink always-read core of judge.md (173 KB) into on-demand sub-skills
- **#14011**: gh-cached: `gh api -F/--raw-field` writes are classified as reads (retried on timeout, and marked cacheable); two smaller #13930 follow-ups
- **#14056**: loom-clean's claim-lock pass uses bare pid liveness, so a recycled-pid lock is now never cleared
- **#14066**: test flake: nextest discovery (lib binary --list) stalls past the 10-minute Run tests cap on Rust Unit Tests (3/5) — recurrence of #11121
- **#14075**: docs: add a CI check that docs/README.md indexes every docs/ directory and top-level doc
- **#14101**: merge-pr.sh --release-hold strips the hold then cannot merge where loom/merge-authorization is required
- **#14105**: Private-clone Codex sessions still need the interactive hook-trust prompt: extend sealed registration, or state why the hook is needed in a container
- **#14110**: Contained roles: select only private-clone accounts registering the repository; clear refusal, status and docs
- **#14125**: champion-hold ratification: --help advertises a verdict=unreadable the verb can no longer emit; tick's defer row says 'no hold standing'
- **#14195**: A 2xx probe with no 5h-utilization header writes an evidence-free `available` row that collapses a weekly hold (sibling of #14145)
- **#14219**: Two owner-identity leftovers after the transfer: the installed-template URL denylist and a stale mirrored comment in ci.yml
- **#14221**: Shipped files, installer and resync output name 2AMLogic/loom; test rejects the old slug
- **#14222**: resync-installed.sh records the canonical source remote when the clone's origin is a stale redirect
- **#14223**: Daemon and doctor: detect a checkout whose origin owner differs from the forge's canonical owner
- **#14247**: docs: stale 'user-owned' reason in ci.yml comments, and cutover step 3 understates expected eligibility failures
- **#14248**: CI does not run `generate-agent-skills --check`, though two docs say it does
- **#14269**: docs: six stale references to scripts/check-claude-md-budget.sh, removed in #12106
- **#14271**: champion-hold ratification: --help still advertises action=defer, which the verb can no longer emit
- **#14301**: ci.yml backend path filter omits defaults/scripts/lib/bounded-run.sh, skipping the Rust tests that read it from disk
- **#14307**: Champion ratification hold: other_holds_standing() fails open on an unreadable comments read, removing the shared loom:blocked label
- **#14317**: `.ranking` carry-forward (#14145) is undocumented, and can label an auth-dead account `exhausted`
- **#14371**: Per-seat session container for Claude steps: launch in the configured session image, one login per container
- **#14376**: Refuse a peer-owned claim before handing a new worktree to a Builder
- **#14392**: post-verdict.sh drops --reviews-reconciled on a changes-requested verdict; lease release prints 'released 0' after a successful release
- **#14415**: Stranded check runs under a terminal-failed workflow run still read as pending outside `forge wait-checks`
- **#14419**: Release cannot publish container images since the transfer: pushes to ghcr.io/rjwalters are denied, so no session image exists after 0.19.1311
- **#14434**: resync-installed.sh restamps loom_version even when a file is blocked and the run exits 1
- **#14450**: Remove a by-owner credential directory once its owner is no longer a cross-owner (follow-up to #14435)
- **#14455**: Codex Curator steps are dispatched to stopped, unprovisioned seats and end SANDBOX_UNAVAILABLE every time
- **#14458**: A dirty leftover worktree with no unpublished commits blocks a private-clone seat for every repo, and recover cannot clear it
- **#14465**: Serve hold diagnostics, curation labels and blocker notifications from loom-tools
- **#14474**: Expose worktree operations through the shared Rust CLI
- **#14497**: Codex seats: trusted-seat selection for guarded roles, machine-level accounts disable, state-change release of the sandbox hold
- **#14512**: private_dispatch prepares a Codex seat twice after reserve_seat (#14481)
- **#14516**: Verdict staleness trusts an old rejection when a prepared Doctor commit is pushed later
- **#14519**: loom-tools: admit the shared non-queue forge actions
- **#14534**: recover --publish: CLI discovers the daemon's workspace root without an exported variable (AC1 of #14433)
- **#14560**: Work-finder completion gaps leave the Fleet queue stale
- **#14565**: Test which forge route serves installed issue leases in the three Loom modes
- **#14566**: Telemetry: about 40% of OpenCode (GLM) runs on 0.19.1316 emit no loom.runtime.usage row
- **#14570**: Installer: an option to install into a repo without replacing the machine's daemon or user-scope hooks
- **#14572**: accounts.session.extraMounts host sources are not denied for ~/.loom/tokens, ~/.ssh or a session profile/seat directory
- **#14580**: Reuse the claim guard’s issue-lease parser in the worktree co-occupancy guard
- **#14584**: Codex pool: an account's exhaustion horizon is learned per host, so each host must fail a real step to find it
- **#14585**: Codex seat selection does not prefer the account with the most 7d headroom among idle, eligible seats
- **#14601**: Test flake: grace_fails_open_without_touching_the_hysteresis fails on Rust Unit Tests (2/5)
- **#14608**: troubleshooting.md: stash-producer enumeration omits loom-rescue-<ts>, and self-heal is the first automated dropper
- **#14610**: Harden TestServer::dead_url() against ephemeral-port reuse (the #14601 flake class)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#12637**: refactor: split status_render inline tests into per-module files *(held)*
- **#12969**: feat: age-bound Codex rollout history in live profiles *(held)*
- **#14545**: feat: route script-helper gh calls through GhInvocation with a declared intent
- **#14588**: feat(install): add --repo-only to skip machine-level provisioning
- **#14589**: feat: journal a loom.runtime.usage_missing span when a run's usage is unreadable
- **#14594**: feat: fold Codex rollout snapshots into account health on the ranking-refresh tick
- **#14597**: feat: prefer the Codex seat with the most weekly headroom among healthy seats
- **#14599**: fix: consume held Codex seat, name repo-routing misses, count seat_busy refusals
- **#14600**: fix: refuse session extra mounts overlapping host credentials
- **#14605**: test: keep the failing endpoint bound in the fleet grace test
- **#14606**: refactor: reuse the claim guard's lease-comment parsers in lease co-occupancy
- **#14611**: docs: explain Fleet rescue-stash retention in troubleshooting
- **#14612**: fix: launch eager reclaim detached so it cannot hold back a work-finder tick

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#13582**: fix: honour a trusted loom:issue adoption in the promotion author gate — closes #13578
- **#13997**: docs(judge): split judge.md into always-read core + on-demand judge-* sub-skills — closes #13957
- **#14046**: refactor: remove the loom-api crate from the workspace — closes #13820
- **#14078**: fix: classify gh api -F/--raw-field as writes in gh-cached; flag stalled census reads — closes #14011
- **#14086**: feat: add workflow-lint subcommand and Doctor pre-push workflow gate — closes #13896
- **#14108**: fix(private_workspace): forward the freshest owner token, not a fixed primary-first one — closes #13933
- **#14136**: fix: ignore pnpm artifacts in the managed gitignore block of loom-workspace stub repos — closes #13898
- **#14171**: ci: add named nextest discovery step with stall diagnostics — closes #14066
- **#14181**: feat: seal the hook registration for private-clone Codex sessions — closes #14105
- **#14193**: fix: census RELEASED lists released ratification holds; notice gains a reason bullet — closes #13875
- **#14243**: fix: stamp the forge's canonical repo name into loom_source_remote — closes #14222
- **#14253**: fix: drop dead unreadable verdict from ratification help and defer row — closes #14125
- **#14260**: fix(daemon): contained launches draw only private-clone accounts registering the workspace repository — closes #14110
- **#14267**: fix: clear recycled-pid claim locks in loom-clean's stale-lock pass — closes #14056
- **#14276**: ci: gate .agents/skills mirrors on generate-agent-skills --check in lint — closes #14248
- **#14281**: docs: name 2AMLogic/loom in shipped files and add old-slug guard test — closes #14221
- **#14283**: docs: drop retired claude-md-budget and docs-defaults-parity CI jobs from judge prompt — closes #14269
- **#14289**: feat(daemon): publish the captain's token probe to fleet workers — closes #13942
- **#14293**: fix: test hold standing, not marker presence, in ratification-hold release path — closes #14249
- **#14295**: ci: gate docs/README.md index against the docs/ tree — closes #14075
- **#14297**: ci: forbid the current owner's repo URL in installed templates — closes #14219
- **#14316**: fix: fail ratification-hold's other-holds read closed before stripping loom:blocked
- **#14319**: ci: cover all of defaults/ in the backend PR path filter — closes #14301
- **#14322**: fix: drop unreachable action=defer from champion-hold ratification --help — closes #14271
- **#14330**: daemon: detect a checkout whose origin owner drifted from its resolved credential — closes #14223
- **#14335**: docs: drop stale file-size ratchet comments from loom-daemon and ci.yml (slice c) — closes #13827
- **#14366**: fix: defer merge-pr --release-hold writes until required checks pass — closes #14101
- **#14368**: docs: correct merge-queue dormancy reason and cutover step 3 failures — closes #14247
- **#14381**: fix: don't carry an exhausted .ranking row over permanently-marked accounts; document carry-forward — closes #14317
- **#14384**: fix: require native ownership before handing a fresh worktree to a Builder — closes #14376
- **#14410**: fix: append --reviews-reconciled on changes-requested; word a repeat lease release as already released — closes #14392
- **#14411**: fix: carry exhausted weekly-hold row forward over a header-less 2xx probe — closes #14195
- **#14437**: fix: attribute stranded check runs to their failed run in the merge-group check — closes #14415
- **#14441**: fix: publish worker images under the repository owner's GHCR namespace — closes #14419
- **#14452**: fix: record held_back paths and exit 3 when resync withholds a locally-fixed file — closes #14434
- **#14470**: fix: remove stale by-owner writer credentials once the owner is no longer a cross-owner — closes #14450
- **#14478**: feat: serve hold diagnostics and scoped issue workflows from loom-tools
- **#14483**: fix: let recover list and clear dirty leftover worktrees that block a private-clone seat — closes #14458
- **#14498**: fix: pass over unprovisioned and sandbox-held Codex seats before a draw — closes #14455
- **#14514**: feat(tools): share worktree CLI operations and read routing — closes #14474
- **#14518**: fix(verdict-guard): don't read the verdict writer's own label as an operator send-back — closes #14516
- **#14532**: fix: verify WORK_LOG records against the target repo before Guide writes and Judge fast-paths — closes #13760
- **#14546**: fix: let a CLI process ask the daemon for its primary workspace root
- **#14576**: fix(lease): route installed issue lease reads through Loom mode — closes #14565
- **#14596**: feat: dispatch Claude steps into persistent account seats

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
- **#12703**: Add a per-repo setting to turn off Champion's out-of-band acceptance-criteria hold (#6883) *(curated)* *(held)*
- **#12713**: Part-of merges re-queue the issue to Builders: landed-route marker is read as a Curator 'kept' decision *(curated)* *(held)*
- **#12909**: ci_telemetry: 2amlogic owner discovery times out every cycle since Oct 17 (no loom-ui CI facts; 478 consecutive failures) *(curated)* *(held)*
- **#12966**: Bound Codex session/thread history inside live profiles (the real codex-profiles growth) + uv cache *(curated)* *(held)*
- **#12978**: Codex shared-seat dispatch: live receipts and startup measurements (split from #10796) *(curated)* *(held)*
- **#13155**: [ci-daily] Flake Detection (shell suites, macos-latest, parallelism 1) *(curated)* *(held)*
- **#13317**: Audit Champion merge-risk self-releases two weeks after #12960 is live *(curated)* *(held)*
- **#13378**: Scope trust for github-actions[bot] to ci-daily tracker authorship instead of repo-wide trustedCommenters (alternative to #13340) *(curated)* *(held)*
- **#13486**: Champion resync waiver: a byte-identical test under .loom/scripts/tests/ is not a merge-gate path — every release shipping test-champion-resync-waiver.sh holds all resync PRs *(curated)* *(held)*
- **#13521**: Builder: ~11% of fleet sweeps still end without an outcome line after #13447 (worker-3 29%, worker-4 0%) *(curated)* *(held)*
- **#13578**: Promotion author gate re-holds an issue the operator already adopted, after landed-route re-curation *(curated)*
- **#13590**: Observe convert_awaiting_revision_parks on a live Guide tick (#10936 follow-up) *(curated)* *(held)*
- **#13640**: ci_telemetry: owner-kind probe users/2amlogic hangs 120s every cycle in the daemon (0.3s by hand) and a timeout never fails over — 2AMLogic CI dropped 20+ days *(curated)* *(held)*
- **#13668**: Replay strict resync waiver on recent fasterhenry resync PRs (follow-up to #13665) *(curated)* *(held)*
- **#13720**: worktree.issue_state_rest reads spike every 4 h (synchronized cache expiry?) *(curated)* *(held)*
- **#13744**: Reads can still land on the writer: raw run_gh bypasses GhInvocation routing (writer budget exhausted 35 min of every hour on 1283) *(curated)*
- **#13760**: Guide WORK_LOG backfill wrote another repo's history; docs-only fast path approved it *(curated)*
- **#13827**: Remove stale file-size ratchet comments: 242 files cite a gate #12099 deleted, and new code still obeys it *(curated)*
- **#13865**: [ci-daily] Supply Chain *(curated)* *(held)*
- **#13875**: Held-PR digest cannot state a ratification hold's reason: Step 1 greps only the merge-risk and critical-file markers (every such row reads "reason unrecorded") *(curated)*
- **#13896**: Doctor conflict-resolution should structurally validate touched workflow files; a vanished check should not satisfy a green rollup *(curated)*
- **#13898**: Installer's managed .gitignore block ignores neither node_modules/ nor pnpm-lock.yaml, which the root package.json stub's documented pnpm entry points produce *(curated)*
- **#13933**: forge_auth forwards an expired token: a stale per-workspace gh-config-by-owner wins over the refreshed primary one *(curated)*
- **#13942**: Token pool: captain publishes its account probe; workers apply it (part 2 of #13893) *(curated)*
- **#13957**: Shrink always-read core of judge.md (173 KB) into on-demand sub-skills *(curated)*
- **#13966**: Z.ai weekly-limit 429 is bad-marked for 60 s: exhausted keys are relaunched all day and the role never falls through to Claude *(curated)* *(held)*
- **#14011**: gh-cached: `gh api -F/--raw-field` writes are classified as reads (retried on timeout, and marked cacheable); two smaller #13930 follow-ups *(curated)*
- **#14056**: loom-clean's claim-lock pass uses bare pid liveness, so a recycled-pid lock is now never cleared *(curated)*
- **#14066**: test flake: nextest discovery (lib binary --list) stalls past the 10-minute Run tests cap on Rust Unit Tests (3/5) — recurrence of #11121 *(curated)*
- **#14075**: docs: add a CI check that docs/README.md indexes every docs/ directory and top-level doc *(curated)*
- **#14101**: merge-pr.sh --release-hold strips the hold then cannot merge where loom/merge-authorization is required *(curated)*
- **#14105**: Private-clone Codex sessions still need the interactive hook-trust prompt: extend sealed registration, or state why the hook is needed in a container *(curated)*
- **#14110**: Contained roles: select only private-clone accounts registering the repository; clear refusal, status and docs *(curated)*
- **#14112**: Private-clone session image follows the daemon release tag; recreate idle private sessions at a daemon roll *(curated)* *(held)*
- **#14125**: champion-hold ratification: --help advertises a verdict=unreadable the verb can no longer emit; tick's defer row says 'no hold standing' *(curated)*
- **#14195**: A 2xx probe with no 5h-utilization header writes an evidence-free `available` row that collapses a weekly hold (sibling of #14145) *(curated)*
- **#14218**: Verify the repaired publish paths actually fire after the org transfer (post-merge live check for #14158) *(curated)*
- **#14219**: Two owner-identity leftovers after the transfer: the installed-template URL denylist and a stale mirrored comment in ci.yml *(curated)*
- **#14221**: Shipped files, installer and resync output name 2AMLogic/loom; test rejects the old slug *(curated)*
- **#14222**: resync-installed.sh records the canonical source remote when the clone's origin is a stale redirect *(curated)*
- **#14223**: Daemon and doctor: detect a checkout whose origin owner differs from the forge's canonical owner *(curated)*
- **#14232**: Spike: does regenerating CODEX_HOME/config.toml with inline hooks per launch avoid the private-clone hook-trust prompt? *(curated)* *(held)*
- **#14235**: Design: let a roleDraw attempt table escalate on named outcome reasons, once #14233 has data *(curated)* *(held)*
- **#14244**: Live-verify telemetry-hold-transitions against the real reader (14-day window vs the 5 known self-releases) *(curated)* *(held)*
- **#14247**: docs: stale 'user-owned' reason in ci.yml comments, and cutover step 3 understates expected eligibility failures *(curated)*
- **#14248**: CI does not run `generate-agent-skills --check`, though two docs say it does *(curated)*
- **#14249**: champion-ratification-hold.md: the release branch removes the shared loom:blocked with no standing test (sibling of #14185) *(curated)*
- **#14269**: docs: six stale references to scripts/check-claude-md-budget.sh, removed in #12106 *(curated)*
- **#14271**: champion-hold ratification: --help still advertises action=defer, which the verb can no longer emit *(curated)*
- **#14292**: test flake: test-docs-guide-lock.sh Test 4 (#5573 race) fails on Shell Test Suites (hermetic, 2/2) — two concurrent acquires both win *(curated)* *(held)*
- **#14301**: ci.yml backend path filter omits defaults/scripts/lib/bounded-run.sh, skipping the Rust tests that read it from disk *(curated)*
- **#14307**: Champion ratification hold: other_holds_standing() fails open on an unreadable comments read, removing the shared loom:blocked label *(curated)*
- **#14317**: `.ranking` carry-forward (#14145) is undocumented, and can label an auth-dead account `exhausted` *(curated)*
- **#14371**: Per-seat session container for Claude steps: launch in the configured session image, one login per container *(curated)*
- **#14376**: Refuse a peer-owned claim before handing a new worktree to a Builder *(curated)*
- **#14392**: post-verdict.sh drops --reviews-reconciled on a changes-requested verdict; lease release prints 'released 0' after a successful release *(curated)*
- **#14415**: Stranded check runs under a terminal-failed workflow run still read as pending outside `forge wait-checks` *(curated)*
- **#14419**: Release cannot publish container images since the transfer: pushes to ghcr.io/rjwalters are denied, so no session image exists after 0.19.1311 *(curated)*
- **#14433**: accounts session recover --publish cannot authenticate: the seat's credential helper has no token outside a job *(curated)* *(held)*
- **#14434**: resync-installed.sh restamps loom_version even when a file is blocked and the run exits 1 *(curated)*
- **#14450**: Remove a by-owner credential directory once its owner is no longer a cross-owner (follow-up to #14435) *(curated)*
- **#14455**: Codex Curator steps are dispatched to stopped, unprovisioned seats and end SANDBOX_UNAVAILABLE every time *(curated)*
- **#14458**: A dirty leftover worktree with no unpublished commits blocks a private-clone seat for every repo, and recover cannot clear it *(curated)*
- **#14464**: Expose park records and operator decisions through loom-tools *(curated)* *(held)*
- **#14465**: Serve hold diagnostics, curation labels and blocker notifications from loom-tools *(curated)*
- **#14474**: Expose worktree operations through the shared Rust CLI *(curated)*
- **#14475**: Verify attended issue-lease routing in all three loom-tools modes *(curated)* *(held)*
- **#14497**: Codex seats: trusted-seat selection for guarded roles, machine-level accounts disable, state-change release of the sandbox hold *(curated)*
- **#14512**: private_dispatch prepares a Codex seat twice after reserve_seat (#14481) *(curated)*
- **#14516**: Verdict staleness trusts an old rejection when a prepared Doctor commit is pushed later *(curated)*
- **#14519**: loom-tools: admit the shared non-queue forge actions *(curated)*
- **#14534**: recover --publish: CLI discovers the daemon's workspace root without an exported variable (AC1 of #14433) *(curated)*
- **#14553**: Live-verify the migrated merge-queue actions in tools-only session mode (#14520 AC3 remainder) *(curated)* *(held)*
- **#14556**: [ci-daily] Compatibility Floor (SUPPORTS_INSTALLED) *(curated)* *(held)*
- **#14560**: Work-finder completion gaps leave the Fleet queue stale *(curated)*
- **#14565**: Test which forge route serves installed issue leases in the three Loom modes *(curated)*
- **#14566**: Telemetry: about 40% of OpenCode (GLM) runs on 0.19.1316 emit no loom.runtime.usage row *(curated)*
- **#14569**: telemetry-hold-transitions fails on the real reader: rows refused as 'no body string', and the 14-day window exceeds the store memory limit *(curated)* *(held)*
- **#14570**: Installer: an option to install into a repo without replacing the machine's daemon or user-scope hooks *(curated)*
- **#14572**: accounts.session.extraMounts host sources are not denied for ~/.loom/tokens, ~/.ssh or a session profile/seat directory *(curated)*
- **#14578**: Codex Builder/Doctor admission says 'account session is busy' while other seats on the host are idle (0.19.1316; #14492 covered read-only roles only) *(curated)*
- **#14583**: Keep quiet dispatch disposition rows inside the five-minute Fleet freshness target *(curated)* *(held)*
- **#14584**: Codex pool: an account's exhaustion horizon is learned per host, so each host must fail a real step to find it *(curated)*
- **#14585**: Codex seat selection does not prefer the account with the most 7d headroom among idle, eligible seats *(curated)*
- **#14601**: Test flake: grace_fails_open_without_touching_the_hysteresis fails on Rust Unit Tests (2/5) *(curated)*
- **#14608**: troubleshooting.md: stash-producer enumeration omits loom-rescue-<ts>, and self-heal is the first automated dropper *(curated)*
- **#14610**: Harden TestServer::dead_url() against ephemeral-port reuse (the #14601 flake class) *(curated)*
- **#14613**: CI: backend path filter omits the role-prompt trees, so prompt-only PRs skip the 14 Rust tests that lint them *(curated)*

## Proposed (Architect / Hermit)

- **#13071**: Peer claims: move machine coordination off rendered room messages (custom event type / state events) *(architect)* *(held)*
- **#14186**: Remove warn-out-of-set-deps.sh: advisory script with no caller *(hermit)* *(held)*

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
| Operator merge-risk holds | 43 |
| Very important | 0 |
| Important | 6 |
| Ready (`loom:issue`, net of holds/building/merge-pending) | 4 |
| In Progress (`loom:building`) | 57 |
| PRs awaiting review | 13 |
| Approved PRs awaiting merge | 45 |
| Curated | 116 |
| Architect / Hermit proposals | 2 |
| Active epics | 7 |
<!-- guide:plan-body:end -->
