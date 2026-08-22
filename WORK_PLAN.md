# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#6732**: fix: resolve NAME=$(pwd) cwd capture in guard force-op:detached parsing
- **#6639**: perf(ci): parallelize run-ci-suites.sh, report in manifest order
- **#6631**: fix(resync): distinguish retired-but-unlisted payload files from shipped payload
- **#6621**: feat: restamp root CLAUDE.md's Loom Version header on resync
- **#6534**: feat(roles): check epic completion before structural criteria, escalate orthogonal blockers
- **#6532**: fix(scripts): honor repo-relative resync-ignore pins and warn on dead pins
- **#6525**: fix: base stale-claim liveness on claimant activity via a shared evaluator
- **#6484**: fix(guard): fix qsplit $((...)) pipe-swallowing and mask_gt backslash-escaped quote toggling (#6472)
- **#6445**: fix(guard): resolve double-quoted $VAR write targets in worktree-write-confinement
- **#6422**: fix(merge): distinguish persistent check-runs 404 from transient fetch failure
- **#6405**: feat(scripts): add post-verdict.sh so verdict comments can't post without their loom:verdict-sha marker
- **#6367**: fix: deny osascript/AppleScript GUI automation by default, document TCC attribution
- **#6358**: fix(guard): stop scanning python/perl/ruby/node heredoc bodies for shell write idioms
- **#6333**: feat(lease): publish a lease record from the in-session sweep path
- **#6325**: feat(daemon): demote peer-claims to advisory in the reclamation path (Epic #6165 Phase 4)
- **#6305**: fix(guard): resolve_var() now unwraps double-quoted write targets before matching $VAR
- **#6296**: fix(daemon): bound auto_update's settle/gate-4 resets and surface staleness
- **#6290**: fix: name-allowlist printenv SECRET/TOKEN/KEY ask pattern to stop LOOM_TOKEN_NAME false positive
- **#6212**: fix(ci-settle-poll): guard against empty gh pr checks output false-settling
- **#6210**: feat(worktree): add a main target to stash-push/stash-pop and stop advising git stash pop in the primary clone
- **#6207**: fix(guard): mask echo/printf positional args in catastrophic-tier scan

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#6670**: Work finder re-dispatches a standing tracking issue with no cooldown after a successful no-op re-verification pass
- **#6653**: Worktrees of closed issues (4449, 5179, 6199) unreaped since 2026-08-04, two of them dirty

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#6724**: Guard force-op:detached fires on cd+pwd-captured worktree path before git -C reset --hard
- **#6670**: Work finder re-dispatches a standing tracking issue with no cooldown after a successful no-op re-verification pass
- **#6653**: Worktrees of closed issues (4449, 5179, 6199) unreaped since 2026-08-04, two of them dirty
- **#6622**: CI: the 10-minute floor is run-ci-suites.sh running every shell suite sequentially on an idle 4-vCPU runner — parallelize across suites, report in manifest order (613 s p50, 2.3× the next job)
- **#6613**: resync orphan warning: distinguish formerly-shipped-then-removed files from project-local ones
- **#6612**: resync: version stamp in installed CLAUDE.md stays stale — give the version line managed-section markers
- **#6516**: Completed epics have no closure path: Champion format gate + Curator 'stale blocker' heartbeats deadlock finished work
- **#6515**: resync-ignore: pins in repo-relative form silently never match — clobbered a pinned file and broke a repo's role ticks
- **#6514**: judge.md Stale-reviewing-claim check can livelock: a post-claim Builder comment permanently blocks staleness reclaim
- **#6472**: guard-destructive false positive: '>' inside a quoted awk program, and sed -n without -i, are denied as a write to target '|'
- **#6444**: Guard worktree-write-confinement: same-command resolver doesn't track var assignments across newlines
- **#6389**: merge-pr.sh --auto polls to LOOM_AUTO_MERGE_TIMEOUT when the check-runs API persistently 404s (repo with no Actions)
- **#6382**: The loom:verdict-sha marker is easy to omit and only caught by self-inspection
- **#6366**: macOS TCC prompts attributed to loom-daemon: child sweeps sending AppleEvents + ad-hoc-signed binary re-prompts on every roll — deny GUI automation in spawned sessions, sign the daemon
- **#6353**: Guard worktree-write-confinement denies a read-only heredoc script with no writes at all
- **#6317**: [Epic #6165] Phase 4: Demote peer-claims to advisory in the reclamation path
- **#6299**: Guard: same-command $VAR resolver never resolves quoted write targets (> "$LOG"), causing false worktree-write-confinement-unresolved-var denials
- **#6261**: Merged fixes do not reach running daemons: auto_update rolled nothing across a 20-merge day; release-artifact path is 58 patch versions stale
- **#6068**: Guard false positive: catastrophic-tier positional masking doesn't cover echo/printf, so a heading echo containing the trigger phrase hard-denies

## In Progress

Issues currently being built (`loom:building`).

_None._

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#6739**: feat(daemon): add a no-op re-dispatch cooldown, distinct from crash-loop quarantine

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#6732**: fix: resolve NAME=$(pwd) cwd capture in guard force-op:detached parsing
- **#6639**: perf(ci): parallelize run-ci-suites.sh, report in manifest order
- **#6631**: fix(resync): distinguish retired-but-unlisted payload files from shipped payload
- **#6621**: feat: restamp root CLAUDE.md's Loom Version header on resync
- **#6534**: feat(roles): check epic completion before structural criteria, escalate orthogonal blockers
- **#6532**: fix(scripts): honor repo-relative resync-ignore pins and warn on dead pins
- **#6525**: fix: base stale-claim liveness on claimant activity via a shared evaluator
- **#6484**: fix(guard): fix qsplit $((...)) pipe-swallowing and mask_gt backslash-escaped quote toggling (#6472)
- **#6445**: fix(guard): resolve double-quoted $VAR write targets in worktree-write-confinement
- **#6422**: fix(merge): distinguish persistent check-runs 404 from transient fetch failure
- **#6405**: feat(scripts): add post-verdict.sh so verdict comments can't post without their loom:verdict-sha marker
- **#6367**: fix: deny osascript/AppleScript GUI automation by default, document TCC attribution
- **#6358**: fix(guard): stop scanning python/perl/ruby/node heredoc bodies for shell write idioms
- **#6333**: feat(lease): publish a lease record from the in-session sweep path
- **#6325**: feat(daemon): demote peer-claims to advisory in the reclamation path (Epic #6165 Phase 4)
- **#6305**: fix(guard): resolve_var() now unwraps double-quoted write targets before matching $VAR
- **#6296**: fix(daemon): bound auto_update's settle/gate-4 resets and surface staleness
- **#6290**: fix: name-allowlist printenv SECRET/TOKEN/KEY ask pattern to stop LOOM_TOKEN_NAME false positive
- **#6212**: fix(ci-settle-poll): guard against empty gh pr checks output false-settling
- **#6210**: feat(worktree): add a main target to stash-push/stash-pop and stop advising git stash pop in the primary clone
- **#6207**: fix(guard): mask echo/printf positional args in catastrophic-tier scan

## Proposed

Issues carrying `loom:curated`.

- **#6724**: Guard force-op:detached fires on cd+pwd-captured worktree path before git -C reset --hard *(curated)*
- **#6704**: Roster-driven role-runner shard assignment: reassign a dead host's slice within a bounded window (follow-up to #6374's static ring) *(curated)*
- **#6670**: Work finder re-dispatches a standing tracking issue with no cooldown after a successful no-op re-verification pass *(curated)*
- **#6657**: docker/ lacks a top-level README (only docker/worker/README.md) *(curated)*
- **#6656**: Enable Dependabot vulnerability alerts and security updates (both currently disabled) *(curated)*
- **#6655**: .loom/logs/ has no retention policy — 310 MB / 4,114 files back to January on one host *(curated)*
- **#6653**: Worktrees of closed issues (4449, 5179, 6199) unreaped since 2026-08-04, two of them dirty *(curated)*
- **#6650**: .loom/config.json commits a live Matrix room id and ingest URL — intentional, or move to the private overlay tier? *(curated)*
- **#6649**: Untrack .loom/config.json.bak-* backups and gitignore the pattern *(curated)*
- **#6647**: scrub: fleet tests hardcode a live EC2 instance id as a fixture — replace with a synthetic id *(curated)*
- **#6646**: Sweep resync committed, rebased and bypass-pushed the primary clone's main while an operator session was active in that clone *(curated)*
- **#6628**: defaults/scripts/validate-toolchain.sh has no callers and is restored on every consumer resync — wire a caller or retire it *(curated)*
- **#6622**: CI: the 10-minute floor is run-ci-suites.sh running every shell suite sequentially on an idle 4-vCPU runner — parallelize across suites, report in manifest order (613 s p50, 2.3× the next job) *(curated)*
- **#6613**: resync orphan warning: distinguish formerly-shipped-then-removed files from project-local ones *(curated)*
- **#6612**: resync: version stamp in installed CLAUDE.md stays stale — give the version line managed-section markers *(curated)*
- **#6565**: Dogfood config: loom-repo curator starved 3d — runtime=codex admitted with no codex model configured (#5028 skip, DEBUG-silent) *(curated)*
- **#6516**: Completed epics have no closure path: Champion format gate + Curator 'stale blocker' heartbeats deadlock finished work *(curated)*
- **#6515**: resync-ignore: pins in repo-relative form silently never match — clobbered a pinned file and broke a repo's role ticks *(curated)*
- **#6514**: judge.md Stale-reviewing-claim check can livelock: a post-claim Builder comment permanently blocks staleness reclaim *(curated)*
- **#6472**: guard-destructive false positive: '>' inside a quoted awk program, and sed -n without -i, are denied as a write to target '|' *(curated)*
- **#6389**: merge-pr.sh --auto polls to LOOM_AUTO_MERGE_TIMEOUT when the check-runs API persistently 404s (repo with no Actions) *(curated)*
- **#6382**: The loom:verdict-sha marker is easy to omit and only caught by self-inspection *(curated)*
- **#6353**: Guard worktree-write-confinement denies a read-only heredoc script with no writes at all *(curated)*
- **#6320**: In-session /loom:sweep claims publish no lease record, so any daemon reclaims them — two builders in one worktree, uncommitted work lost *(curated)*
- **#6261**: Merged fixes do not reach running daemons: auto_update rolled nothing across a 20-merge day; release-artifact path is 58 patch versions stale *(curated)*
- **#6245**: Guard ask-pattern false positive: printenv of an account-label env var denied by credential-exposure TOKEN pattern, blocks headless runs *(curated)*
- **#6169**: CI settle-polls false-settle on empty gh pr checks output — mandate a row-count guard *(curated)*
- **#6156**: Efficacy review: after ~300 merges, verify #6118's mergeable-recheck is behaving (and never merged a real conflict) *(curated)*
- **#6076**: Guard friction: stash-scope:main-checkout ASKs recur in headless runs despite a documented bypass toggle existing *(curated)*
- **#6068**: Guard false positive: catastrophic-tier positional masking doesn't cover echo/printf, so a heading echo containing the trigger phrase hard-denies *(curated)*
- **#5660**: Vendored guard-destructive-generic.sh has drifted ~2,200 lines ahead of its upstream, and the single-marker capability probe makes partial reconciliation unsafe *(curated)*
- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*

## Proposed (Architect / Hermit)

- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*

## Epics

- **#6165**: Complete #4028: give the forge claim a liveness dimension (a lease), so cross-host correctness stops depending on the safehouse channel
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 21 |
| Urgent | 2 |
| Ready (`loom:issue`) | 19 |
| In Progress (`loom:building`) | 0 |
| PRs awaiting review | 1 |
| Approved PRs awaiting merge | 21 |
| Curated | 34 |
| Architect / Hermit proposals | 3 |
| Active epics | 3 |
<!-- guide:plan-body:end -->


