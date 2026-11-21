# Work Plan

Backlog snapshot from current GitHub label state, maintained by the Guide role. Direction lives in the hand-written roadmap: [docs/ROADMAP.md](docs/ROADMAP.md).

The body below is a snapshot of GitHub label state as of the last merged docs-maintenance PR — it may lag live labels until the next one merges. The `_Generated:` stamp at the top of the generated region dates the snapshot; `git log -1 -- WORK_PLAN.md` finds the merge that wrote it.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
_Generated: 2026-11-20T13:23:48Z (snapshot; compare against live labels)_
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs under a hold (`loom:blocked` or `loom:operator`) — only a human merge decision is missing.

- **#11305**: Retire $LOOM_HYPERPARAMS vector tier and hyperparams digest (#11107)
- **#13997**: docs(judge): split judge.md into always-read core + on-demand judge-* sub-skills
- **#14046**: refactor: remove the loom-api crate from the workspace
- **#14078**: fix: classify gh api -F/--raw-field as writes in gh-cached; flag stalled census reads
- **#14108**: fix(private_workspace): forward the freshest owner token, not a fixed primary-first one
- **#14136**: fix: ignore pnpm artifacts in the managed gitignore block of loom-workspace stub repos
- **#14253**: fix: drop dead unreadable verdict from ratification help and defer row
- **#14260**: fix(daemon): contained launches draw only private-clone accounts registering the workspace repository
- **#14281**: docs: name 2AMLogic/loom in shipped files and add old-slug guard test
- **#14335**: docs: drop stale file-size ratchet comments from loom-daemon and ci.yml (slice c)
- **#14366**: fix: defer merge-pr --release-hold writes until required checks pass
- **#14384**: fix: require native ownership before handing a fresh worktree to a Builder
- **#14441**: fix: publish worker images under the repository owner's GHCR namespace
- **#14452**: fix: record held_back paths and exit 3 when resync withholds a locally-fixed file
- **#14470**: fix: remove stale by-owner writer credentials once the owner is no longer a cross-owner
- **#14478**: feat: serve hold diagnostics and scoped issue workflows from loom-tools
- **#14498**: fix: pass over unprovisioned and sandbox-held Codex seats before a draw
- **#14510**: fix: select hook-ready Codex seats and honor shared account disables
- **#14514**: feat(tools): share worktree CLI operations and read routing
- **#14518**: fix(verdict-guard): don't read the verdict writer's own label as an operator send-back
- **#14532**: fix: verify WORK_LOG records against the target repo before Guide writes and Judge fast-paths
- **#14546**: fix: let a CLI process ask the daemon for its primary workspace root
- **#14576**: fix(lease): route installed issue lease reads through Loom mode
- **#14588**: feat(install): add --repo-only to skip machine-level provisioning
- **#14620**: fix: refuse an uncreatable verdict-lock directory at once instead of waiting as contention
- **#14637**: fix(merge-queue): resolve the status-command route from a root of the target
- **#14650**: fix(champion): classify workflow write-all permission values
- **#14653**: fix(roles): pin drift recovery to the verified PR head
- **#14662**: docs(roles): gate Judge and Builder pre-push updates on workflow-lint
- **#14681**: feat(resync): add --source <dir> to name the defaults/ source checkout
- **#14696**: fix(dispatch): admit on the containment proof the walk already took
- **#14697**: Use account-local Codex usage in availability reports
- **#14715**: fix: send verified Champion-held Doctor repairs back to Judge
- **#14726**: feat: prove forge read authority for private seat mirror admission
- **#14728**: ci: capture kernel stack via sudo -n fallback in discovery-stall diagnostic
- **#14736**: fix: scope cycle escalation to issues on the loop and drop Part-of parent refs
- **#14747**: fix: stamp daemon compatibility on quick installs
- **#14749**: fix: use portable timeout for resync canonical lookup
- **#14752**: fix: report worktrees reaped before preparation refuses
- **#14753**: fix: mint a bootstrap reader so first-start fleet-config render can confirm the store
- **#14758**: fix(release): keep checksum reads on the reader route
- **#14760**: test: compare the canonical path of a reaped worktree
- **#14766**: fix: skip the gh-config bind for daemon-owned App credentials in new private seats
- **#14769**: fix: skip source-remote lookup on no-op resyncs
- **#14773**: fix(forge): recover admission reads after GraphQL exhaustion
- **#14776**: feat(daemon): reclaim private-seat scratch and merged-PR worktrees via a leased idle pass

## Very Important

`loom:very-important` (one counts per workspace); every stage takes these first.

_None._

## Important

`loom:important`; taken before unlabelled work.

- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue *(held)*
- **#12909**: ci_telemetry: 2amlogic owner discovery times out every cycle since Oct 17 (no loom-ui CI facts; 478 consecutive failures) *(held)*
- **#12993**: Shadow evaluation of the new Champion (2026-10-20): 1/95 holds cleared — ranked remaining causes + fixtures *(held)*
- **#14566**: Telemetry: about 40% of OpenCode (GLM) runs on 0.19.1316 emit no loom.runtime.usage row *(held)*
- **#14678**: Private-clone Codex seat volumes grow without bound: a full clone per repo per seat, and nothing reclaims them

## Ready

Human-approved issues (`loom:issue`) ready for implementation — excluding held (`loom:blocked`/`loom:operator`/`loom:frozen`), `loom:building`, and issues closed by an open `loom:pr` PR (listed under Approved).

- **#14632**: Merge-queue handoff can lose the status broker route on a fleet worker

## In Progress

Issues currently being built (`loom:building`).

- **#13760**: Guide WORK_LOG backfill wrote another repo's history; docs-only fast path approved it
- **#13820**: Remove the loom-api crate: reads a database the daemon does not write, 3 of 5 endpoints query tables nothing creates
- **#13827**: Remove stale file-size ratchet comments: 242 files cite a gate #12099 deleted, and new code still obeys it
- **#13898**: Installer's managed .gitignore block ignores neither node_modules/ nor pnpm-lock.yaml, which the root package.json stub's documented pnpm entry points produce
- **#13933**: forge_auth forwards an expired token: a stale per-workspace gh-config-by-owner wins over the refreshed primary one
- **#13957**: Shrink always-read core of judge.md (173 KB) into on-demand sub-skills
- **#14011**: gh-cached: `gh api -F/--raw-field` writes are classified as reads (retried on timeout, and marked cacheable); two smaller #13930 follow-ups
- **#14101**: merge-pr.sh --release-hold strips the hold then cannot merge where loom/merge-authorization is required
- **#14110**: Contained roles: select only private-clone accounts registering the repository; clear refusal, status and docs
- **#14125**: champion-hold ratification: --help advertises a verdict=unreadable the verb can no longer emit; tick's defer row says 'no hold standing'
- **#14156**: Judge's and Builder's own merge-then-push paths resolve conflicts without the workflow-lint gate #13896 adds to Doctor
- **#14195**: A 2xx probe with no 5h-utilization header writes an evidence-free `available` row that collapses a weekly hold (sibling of #14145)
- **#14221**: Shipped files, installer and resync output name 2AMLogic/loom; test rejects the old slug
- **#14258**: canonicalize_source_remote uses bare `timeout`, so the #14222 stale-remote fix is silently inert where timeout(1) is absent
- **#14371**: Per-seat session container for Claude steps: launch in the configured session image, one login per container
- **#14376**: Refuse a peer-owned claim before handing a new worktree to a Builder
- **#14419**: Release cannot publish container images since the transfer: pushes to ghcr.io/rjwalters are denied, so no session image exists after 0.19.1311
- **#14434**: resync-installed.sh restamps loom_version even when a file is blocked and the run exits 1
- **#14450**: Remove a by-owner credential directory once its owner is no longer a cross-owner (follow-up to #14435)
- **#14455**: Codex Curator steps are dispatched to stopped, unprovisioned seats and end SANDBOX_UNAVAILABLE every time
- **#14465**: Serve hold diagnostics, curation labels and blocker notifications from loom-tools
- **#14474**: Expose worktree operations through the shared Rust CLI
- **#14497**: Codex seats: trusted-seat selection for guarded roles, machine-level accounts disable, state-change release of the sandbox hold
- **#14516**: Verdict staleness trusts an old rejection when a prepared Doctor commit is pushed later
- **#14534**: recover --publish: CLI discovers the daemon's workspace root without an exported variable (AC1 of #14433)
- **#14560**: Work-finder completion gaps leave the Fleet queue stale
- **#14570**: Installer: an option to install into a repo without replacing the machine's daemon or user-scope hooks
- **#14615**: accounts check --ranking still shows borrowed weekly usage for a profile whose sessions dir is a symlink, while selection now ignores it
- **#14617**: forge verdict-lock waits 90 seconds and reports contention when its lock parent is read-only
- **#14626**: Private-clone admission contains the winning seat twice per admission
- **#14677**: resync-installed.sh --output has no sanctioned source path for an agent working from a managed worktree in a consumer repo
- **#14678**: Private-clone Codex seat volumes grow without bound: a full clone per repo per seat, and nothing reclaims them
- **#14708**: ci: discovery-stall diagnostic prints `(unreadable)` for /proc/<pid>/stack — needs sudo to capture the kernel stack
- **#14712**: resync: a no-op resync warns that it stamped the canonical loom_source_remote, but leaves the stale one
- **#14714**: Held Doctor repairs leave stale approvals outside the Judge queue
- **#14716**: Prove a stable forge read authority for private seat mirror admission
- **#14730**: First daemon start on a new host cannot fetch the fleet store: fleet-config render is shed when readers are configured and no daemon is running
- **#14732**: Champion routes an issue to the operator for a dependency cycle it is not on (a Part-of parent link read as an edge)
- **#14735**: private seat: a reaped worktree is unreported when preparation then refuses
- **#14739**: forge_identity: a release.checksum refusal withdraws the reader gateway for the whole owner, shedding every 2AMLogic read ~20 min/h since 0.20.0
- **#14740**: Fresh install writes no requires_daemon, so newly admitted repos are dispatch-held when the fleet floor moves
- **#14742**: Builder admission stops at GraphQL-only issue and split-parent reads when REST is available
- **#14744**: session_reconcile cannot create a private-clone seat on an App-only host: the daemon's own GH_CONFIG_DIR is inside its checkout
- **#14757**: private_workspace reap test asserts a non-canonical worktree path, failing where TMPDIR holds a symlink
- **#14783**: Reuse EnvGuard in the five tests that clear GH_CONFIG_DIR

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#12969**: feat: age-bound Codex rollout history in live profiles *(held)*
- **#14612**: fix: launch eager reclaim detached so it cannot hold back a work-finder tick *(held)*
- **#14786**: test: reuse EnvGuard in unregistered workspace fixtures

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#11305**: Retire $LOOM_HYPERPARAMS vector tier and hyperparams digest (#11107) — closes #11107
- **#13997**: docs(judge): split judge.md into always-read core + on-demand judge-* sub-skills — closes #13957
- **#14046**: refactor: remove the loom-api crate from the workspace — closes #13820
- **#14078**: fix: classify gh api -F/--raw-field as writes in gh-cached; flag stalled census reads — closes #14011
- **#14108**: fix(private_workspace): forward the freshest owner token, not a fixed primary-first one — closes #13933
- **#14136**: fix: ignore pnpm artifacts in the managed gitignore block of loom-workspace stub repos — closes #13898
- **#14253**: fix: drop dead unreadable verdict from ratification help and defer row — closes #14125
- **#14260**: fix(daemon): contained launches draw only private-clone accounts registering the workspace repository — closes #14110
- **#14281**: docs: name 2AMLogic/loom in shipped files and add old-slug guard test — closes #14221
- **#14335**: docs: drop stale file-size ratchet comments from loom-daemon and ci.yml (slice c) — closes #13827
- **#14366**: fix: defer merge-pr --release-hold writes until required checks pass — closes #14101
- **#14384**: fix: require native ownership before handing a fresh worktree to a Builder — closes #14376
- **#14441**: fix: publish worker images under the repository owner's GHCR namespace — closes #14419
- **#14452**: fix: record held_back paths and exit 3 when resync withholds a locally-fixed file — closes #14434
- **#14470**: fix: remove stale by-owner writer credentials once the owner is no longer a cross-owner — closes #14450
- **#14478**: feat: serve hold diagnostics and scoped issue workflows from loom-tools
- **#14498**: fix: pass over unprovisioned and sandbox-held Codex seats before a draw — closes #14455
- **#14510**: fix: select hook-ready Codex seats and honor shared account disables — closes #14497
- **#14514**: feat(tools): share worktree CLI operations and read routing — closes #14474
- **#14518**: fix(verdict-guard): don't read the verdict writer's own label as an operator send-back — closes #14516
- **#14532**: fix: verify WORK_LOG records against the target repo before Guide writes and Judge fast-paths — closes #13760
- **#14546**: fix: let a CLI process ask the daemon for its primary workspace root
- **#14576**: fix(lease): route installed issue lease reads through Loom mode — closes #14565
- **#14588**: feat(install): add --repo-only to skip machine-level provisioning — closes #14570
- **#14620**: fix: refuse an uncreatable verdict-lock directory at once instead of waiting as contention — closes #14617
- **#14637**: fix(merge-queue): resolve the status-command route from a root of the target
- **#14650**: fix(champion): classify workflow write-all permission values — closes #14648
- **#14653**: fix(roles): pin drift recovery to the verified PR head — closes #14651
- **#14662**: docs(roles): gate Judge and Builder pre-push updates on workflow-lint — closes #14156
- **#14681**: feat(resync): add --source <dir> to name the defaults/ source checkout — closes #14677
- **#14696**: fix(dispatch): admit on the containment proof the walk already took — closes #14626
- **#14697**: Use account-local Codex usage in availability reports — closes #14615
- **#14715**: fix: send verified Champion-held Doctor repairs back to Judge — closes #14714
- **#14726**: feat: prove forge read authority for private seat mirror admission — closes #14716
- **#14728**: ci: capture kernel stack via sudo -n fallback in discovery-stall diagnostic — closes #14708
- **#14736**: fix: scope cycle escalation to issues on the loop and drop Part-of parent refs — closes #14732
- **#14747**: fix: stamp daemon compatibility on quick installs
- **#14749**: fix: use portable timeout for resync canonical lookup — closes #14258
- **#14752**: fix: report worktrees reaped before preparation refuses — closes #14735
- **#14753**: fix: mint a bootstrap reader so first-start fleet-config render can confirm the store — closes #14730
- **#14758**: fix(release): keep checksum reads on the reader route
- **#14760**: test: compare the canonical path of a reaped worktree — closes #14757
- **#14766**: fix: skip the gh-config bind for daemon-owned App credentials in new private seats — closes #14744
- **#14769**: fix: skip source-remote lookup on no-op resyncs — closes #14712
- **#14773**: fix(forge): recover admission reads after GraphQL exhaustion — closes #14742
- **#14776**: feat(daemon): reclaim private-seat scratch and merged-PR worktrees via a leased idle pass — closes #14678

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
- **#12626**: docs: /repo:release Phase 1.5 CHANGELOG completeness gate has no seam for generated-notes policy *(curated)* *(held)*
- **#12703**: Add a per-repo setting to turn off Champion's out-of-band acceptance-criteria hold (#6883) *(curated)* *(held)*
- **#12713**: Part-of merges re-queue the issue to Builders: landed-route marker is read as a Curator 'kept' decision *(curated)* *(held)*
- **#12909**: ci_telemetry: 2amlogic owner discovery times out every cycle since Oct 17 (no loom-ui CI facts; 478 consecutive failures) *(curated)* *(held)*
- **#12966**: Bound Codex session/thread history inside live profiles (the real codex-profiles growth) + uv cache *(curated)* *(held)*
- **#12978**: Codex shared-seat dispatch: live receipts and startup measurements (split from #10796) *(curated)* *(held)*
- **#13317**: Audit Champion merge-risk self-releases two weeks after #12960 is live *(curated)* *(held)*
- **#13378**: Scope trust for github-actions[bot] to ci-daily tracker authorship instead of repo-wide trustedCommenters (alternative to #13340) *(curated)* *(held)*
- **#13486**: Champion resync waiver: a byte-identical test under .loom/scripts/tests/ is not a merge-gate path — every release shipping test-champion-resync-waiver.sh holds all resync PRs *(curated)* *(held)*
- **#13521**: Builder: ~11% of fleet sweeps still end without an outcome line after #13447 (worker-3 29%, worker-4 0%) *(curated)* *(held)*
- **#13590**: Observe convert_awaiting_revision_parks on a live Guide tick (#10936 follow-up) *(curated)* *(held)*
- **#13640**: ci_telemetry: owner-kind probe users/2amlogic hangs 120s every cycle in the daemon (0.3s by hand) and a timeout never fails over — 2AMLogic CI dropped 20+ days *(curated)* *(held)*
- **#13668**: Replay strict resync waiver on recent fasterhenry resync PRs (follow-up to #13665) *(curated)* *(held)*
- **#13720**: worktree.issue_state_rest reads spike every 4 h (synchronized cache expiry?) *(curated)* *(held)*
- **#13760**: Guide WORK_LOG backfill wrote another repo's history; docs-only fast path approved it *(curated)*
- **#13827**: Remove stale file-size ratchet comments: 242 files cite a gate #12099 deleted, and new code still obeys it *(curated)*
- **#13865**: [ci-daily] Supply Chain *(curated)* *(held)*
- **#13898**: Installer's managed .gitignore block ignores neither node_modules/ nor pnpm-lock.yaml, which the root package.json stub's documented pnpm entry points produce *(curated)*
- **#13933**: forge_auth forwards an expired token: a stale per-workspace gh-config-by-owner wins over the refreshed primary one *(curated)*
- **#13957**: Shrink always-read core of judge.md (173 KB) into on-demand sub-skills *(curated)*
- **#13966**: Z.ai weekly-limit 429 is bad-marked for 60 s: exhausted keys are relaunched all day and the role never falls through to Claude *(curated)* *(held)*
- **#14011**: gh-cached: `gh api -F/--raw-field` writes are classified as reads (retried on timeout, and marked cacheable); two smaller #13930 follow-ups *(curated)*
- **#14101**: merge-pr.sh --release-hold strips the hold then cannot merge where loom/merge-authorization is required *(curated)*
- **#14110**: Contained roles: select only private-clone accounts registering the repository; clear refusal, status and docs *(curated)*
- **#14112**: Private-clone session image follows the daemon release tag; recreate idle private sessions at a daemon roll *(curated)* *(held)*
- **#14125**: champion-hold ratification: --help advertises a verdict=unreadable the verb can no longer emit; tick's defer row says 'no hold standing' *(curated)*
- **#14156**: Judge's and Builder's own merge-then-push paths resolve conflicts without the workflow-lint gate #13896 adds to Doctor *(curated)*
- **#14195**: A 2xx probe with no 5h-utilization header writes an evidence-free `available` row that collapses a weekly hold (sibling of #14145) *(curated)*
- **#14218**: Verify the repaired publish paths actually fire after the org transfer (post-merge live check for #14158) *(curated)*
- **#14221**: Shipped files, installer and resync output name 2AMLogic/loom; test rejects the old slug *(curated)*
- **#14232**: Spike: does regenerating CODEX_HOME/config.toml with inline hooks per launch avoid the private-clone hook-trust prompt? *(curated)* *(held)*
- **#14235**: Design: let a roleDraw attempt table escalate on named outcome reasons, once #14233 has data *(curated)* *(held)*
- **#14258**: canonicalize_source_remote uses bare `timeout`, so the #14222 stale-remote fix is silently inert where timeout(1) is absent *(curated)*
- **#14292**: test flake: test-docs-guide-lock.sh Test 4 (#5573 race) fails on Shell Test Suites (hermetic, 2/2) — two concurrent acquires both win *(curated)* *(held)*
- **#14371**: Per-seat session container for Claude steps: launch in the configured session image, one login per container *(curated)*
- **#14376**: Refuse a peer-owned claim before handing a new worktree to a Builder *(curated)*
- **#14419**: Release cannot publish container images since the transfer: pushes to ghcr.io/rjwalters are denied, so no session image exists after 0.19.1311 *(curated)*
- **#14433**: accounts session recover --publish cannot authenticate: the seat's credential helper has no token outside a job *(curated)* *(held)*
- **#14434**: resync-installed.sh restamps loom_version even when a file is blocked and the run exits 1 *(curated)*
- **#14450**: Remove a by-owner credential directory once its owner is no longer a cross-owner (follow-up to #14435) *(curated)*
- **#14455**: Codex Curator steps are dispatched to stopped, unprovisioned seats and end SANDBOX_UNAVAILABLE every time *(curated)*
- **#14461**: Serve the remaining forge actions through the shared loom-tools dispatcher *(curated)* *(held)*
- **#14464**: Expose park records and operator decisions through loom-tools *(curated)* *(held)*
- **#14465**: Serve hold diagnostics, curation labels and blocker notifications from loom-tools *(curated)*
- **#14474**: Expose worktree operations through the shared Rust CLI *(curated)*
- **#14475**: Verify attended issue-lease routing in all three loom-tools modes *(curated)* *(held)*
- **#14497**: Codex seats: trusted-seat selection for guarded roles, machine-level accounts disable, state-change release of the sandbox hold *(curated)*
- **#14516**: Verdict staleness trusts an old rejection when a prepared Doctor commit is pushed later *(curated)*
- **#14530**: Live-verify the admitted loom-tools forge writes against a controlled repository *(curated)* *(held)*
- **#14534**: recover --publish: CLI discovers the daemon's workspace root without an exported variable (AC1 of #14433) *(curated)*
- **#14553**: Live-verify the migrated merge-queue actions in tools-only session mode (#14520 AC3 remainder) *(curated)* *(held)*
- **#14560**: Work-finder completion gaps leave the Fleet queue stale *(curated)*
- **#14565**: Test which forge route serves installed issue leases in the three Loom modes *(curated)*
- **#14566**: Telemetry: about 40% of OpenCode (GLM) runs on 0.19.1316 emit no loom.runtime.usage row *(curated)* *(held)*
- **#14570**: Installer: an option to install into a repo without replacing the machine's daemon or user-scope hooks *(curated)*
- **#14583**: Keep quiet dispatch disposition rows inside the five-minute Fleet freshness target *(curated)* *(held)*
- **#14613**: CI: backend path filter omits the role-prompt trees, so prompt-only PRs skip the 14 Rust tests that lint them *(curated)* *(held)*
- **#14615**: accounts check --ranking still shows borrowed weekly usage for a profile whose sessions dir is a symlink, while selection now ignores it *(curated)*
- **#14617**: forge verdict-lock waits 90 seconds and reports contention when its lock parent is read-only *(curated)*
- **#14626**: Private-clone admission contains the winning seat twice per admission *(curated)*
- **#14632**: Merge-queue handoff can lose the status broker route on a fleet worker *(curated)*
- **#14634**: Dashboard Builder repeatedly selects OpenCode and exits at preflight on loom-worker-3 *(curated)* *(held)*
- **#14648**: Champion workflow check mistakes write-all inside a test filename for write permissions *(curated)*
- **#14651**: judge.md's worktree drift recovery (`git pull --ff-only`) moves a PR branch onto main when upstream tracking has drifted *(curated)*
- **#14663**: [ci-daily] Flake Detection (shell suites, ubuntu-latest, parallelism 1) *(curated)* *(held)*
- **#14665**: Live verification: post-fix Codex usage-missing breakdown and the 17 unaccounted runs (#14664) *(curated)* *(held)*
- **#14677**: resync-installed.sh --output has no sanctioned source path for an agent working from a managed worktree in a consumer repo *(curated)*
- **#14678**: Private-clone Codex seat volumes grow without bound: a full clone per repo per seat, and nothing reclaims them *(curated)*
- **#14680**: Share origin-only Git objects across Codex seats through read-only per-repository mirrors *(curated)* *(held)*
- **#14685**: Live-verify Docker retention per-evaluation summaries on a containerd-store worker (#14679 follow-up) *(curated)* *(held)*
- **#14708**: ci: discovery-stall diagnostic prints `(unreadable)` for /proc/<pid>/stack — needs sudo to capture the kernel stack *(curated)*
- **#14712**: resync: a no-op resync warns that it stamped the canonical loom_source_remote, but leaves the stale one *(curated)*
- **#14714**: Held Doctor repairs leave stale approvals outside the Judge queue *(curated)*
- **#14716**: Prove a stable forge read authority for private seat mirror admission *(curated)*
- **#14730**: First daemon start on a new host cannot fetch the fleet store: fleet-config render is shed when readers are configured and no daemon is running *(curated)*
- **#14732**: Champion routes an issue to the operator for a dependency cycle it is not on (a Part-of parent link read as an edge) *(curated)*
- **#14735**: private seat: a reaped worktree is unreported when preparation then refuses *(curated)*
- **#14737**: Observed Loom step attempt spans omit the recorded failure class *(curated)* *(held)*
- **#14739**: forge_identity: a release.checksum refusal withdraws the reader gateway for the whole owner, shedding every 2AMLogic read ~20 min/h since 0.20.0 *(curated)*
- **#14740**: Fresh install writes no requires_daemon, so newly admitted repos are dispatch-held when the fleet floor moves *(curated)*
- **#14741**: pricing: gpt-6.2-astra is not on the pricing card; every Codex step is costed at the Sonnet fallback *(curated)* *(held)*
- **#14742**: Builder admission stops at GraphQL-only issue and split-parent reads when REST is available *(curated)*
- **#14744**: session_reconcile cannot create a private-clone seat on an App-only host: the daemon's own GH_CONFIG_DIR is inside its checkout *(curated)*
- **#14757**: private_workspace reap test asserts a non-canonical worktree path, failing where TMPDIR holds a symlink *(curated)*
- **#14785**: Guide work plan counts issues with open partial PRs as ready for new Builders *(curated)* *(held)*

## Proposed (Architect / Hermit)

- **#13071**: Peer claims: move machine coordination off rendered room messages (custom event type / state events) *(architect)* *(held)*
- **#14186**: Remove warn-out-of-set-deps.sh: advisory script with no caller *(hermit)* *(held)*
- **#14647**: Remove unused Radix dropdown and switch dependencies from both UI quickstarts *(hermit)* *(held)*
- **#14787**: Remove duplicate field mappings from eight status JSON entries *(hermit)*

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
| Operator merge-risk holds | 46 |
| Very important | 0 |
| Important | 5 |
| Ready (`loom:issue`, net of holds/building/merge-pending) | 1 |
| In Progress (`loom:building`) | 45 |
| PRs awaiting review | 3 |
| Approved PRs awaiting merge | 46 |
| Curated | 111 |
| Architect / Hermit proposals | 4 |
| Active epics | 7 |
<!-- guide:plan-body:end -->
