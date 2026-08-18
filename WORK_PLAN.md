# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#6445**: fix(guard): resolve double-quoted $VAR write targets in worktree-write-confinement
- **#6422**: fix(merge): distinguish persistent check-runs 404 from transient fetch failure
- **#6405**: feat(scripts): add post-verdict.sh so verdict comments can't post without their loom:verdict-sha marker
- **#6367**: fix: deny osascript/AppleScript GUI automation by default, document TCC attribution
- **#6358**: fix(guard): stop scanning python/perl/ruby/node heredoc bodies for shell write idioms
- **#6305**: fix(guard): resolve_var() now unwraps double-quoted write targets before matching $VAR
- **#6296**: fix(daemon): bound auto_update's settle/gate-4 resets and surface staleness
- **#6290**: fix: name-allowlist printenv SECRET/TOKEN/KEY ask pattern to stop LOOM_TOKEN_NAME false positive
- **#6212**: fix(ci-settle-poll): guard against empty gh pr checks output false-settling
- **#6210**: feat(worktree): add a main target to stash-push/stash-pop and stop advising git stash pop in the primary clone

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#6068**: Guard false positive: catastrophic-tier positional masking doesn't cover echo/printf, so a heading echo containing the trigger phrase hard-denies

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#6444**: Guard worktree-write-confinement: same-command resolver doesn't track var assignments across newlines
- **#6389**: merge-pr.sh --auto polls to LOOM_AUTO_MERGE_TIMEOUT when the check-runs API persistently 404s (repo with no Actions)
- **#6382**: The loom:verdict-sha marker is easy to omit and only caught by self-inspection
- **#6366**: macOS TCC prompts attributed to loom-daemon: child sweeps sending AppleEvents + ad-hoc-signed binary re-prompts on every roll — deny GUI automation in spawned sessions, sign the daemon
- **#6353**: Guard worktree-write-confinement denies a read-only heredoc script with no writes at all
- **#6317**: [Epic #6165] Phase 4: Demote peer-claims to advisory in the reclamation path
- **#6299**: Guard: same-command $VAR resolver never resolves quoted write targets (> "$LOG"), causing false worktree-write-confinement-unresolved-var denials
- **#6261**: Merged fixes do not reach running daemons: auto_update rolled nothing across a 20-merge day; release-artifact path is 58 patch versions stale
- **#6169**: CI settle-polls false-settle on empty gh pr checks output — mandate a row-count guard
- **#6068**: Guard false positive: catastrophic-tier positional masking doesn't cover echo/printf, so a heading echo containing the trigger phrase hard-denies

## In Progress

Issues currently being built (`loom:building`).

- **#6454**: test-loom-dispatcher.sh Test 16f actually restarts the REAL production loom-daemon launchd job (live-daemon hazard, #6386-class)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

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

## Proposed

Issues carrying `loom:curated`.

- **#6418**: worktree reaper reports a closed-not-merged PR's worktree as 'preserved' a week after closure *(curated)*
- **#6389**: merge-pr.sh --auto polls to LOOM_AUTO_MERGE_TIMEOUT when the check-runs API persistently 404s (repo with no Actions) *(curated)*
- **#6382**: The loom:verdict-sha marker is easy to omit and only caught by self-inspection *(curated)*
- **#6377**: loom-daemon is DOWN on robb-studio and watchdog recovery is exhausted *(curated)*
- **#6374**: Role-runner host sharding: run each workspace's role rotation on exactly one host per interval (today's LOOM_ROLE_RUNNER=0 is the degenerate case) *(curated)*
- **#6366**: macOS TCC prompts attributed to loom-daemon: child sweeps sending AppleEvents + ad-hoc-signed binary re-prompts on every roll — deny GUI automation in spawned sessions, sign the daemon *(curated)*
- **#6353**: Guard worktree-write-confinement denies a read-only heredoc script with no writes at all *(curated)*
- **#6334**: Two builders can still enter the same issue worktree: the lease is evidence, not a mutex *(curated)*
- **#6320**: In-session /loom:sweep claims publish no lease record, so any daemon reclaims them — two builders in one worktree, uncommitted work lost *(curated)*
- **#6261**: Merged fixes do not reach running daemons: auto_update rolled nothing across a 20-merge day; release-artifact path is 58 patch versions stale *(curated)*
- **#6245**: Guard ask-pattern false positive: printenv of an account-label env var denied by credential-exposure TOKEN pattern, blocks headless runs *(curated)*
- **#6243**: Dispatcher repo-sharding by fleet_priority: make cross-host claim collisions structurally rare *(curated)*
- **#6199**: loom:building is never cleared when an issue closes — 20 stale claims on one consumer repo *(curated)*
- **#6169**: CI settle-polls false-settle on empty gh pr checks output — mandate a row-count guard *(curated)*
- **#6161**: Check-in on #6018: after the next fleet resync, verify the ownership classifier neither deleted repo-owned files nor over-preserved stale ones *(curated)*
- **#6159**: Check-in on #5999: has any live worktree been lost to a reinstall, and is the rm -rf fallback dead code? *(curated)*
- **#6158**: Efficacy review: after ~100 PRs, verify #6148's live-process gate still reclaims disk (over-detection fails silently) *(curated)*
- **#6156**: Efficacy review: after ~300 merges, verify #6118's mergeable-recheck is behaving (and never merged a real conflict) *(curated)*
- **#6134**: Guide/Judge/Champion: fast-path review+merge for docs-only WORK_LOG/WORK_PLAN PRs *(curated)*
- **#6076**: Guard friction: stash-scope:main-checkout ASKs recur in headless runs despite a documented bypass toggle existing *(curated)*
- **#6068**: Guard false positive: catastrophic-tier positional masking doesn't cover echo/printf, so a heading echo containing the trigger phrase hard-denies *(curated)*
- **#6062**: Pulse (2amlogic.com) narrates closed-unmerged duplicate efforts; should narrate on merge / dedup by merged-PR *(curated)*
- **#5897**: sweep skill: presumed-dead Builder Task can survive a session roll — verify task liveness before re-dispatch (duplicate-builder hazard) *(curated)*
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
| Operator merge-risk holds | 10 |
| Urgent | 1 |
| Ready (`loom:issue`) | 10 |
| In Progress (`loom:building`) | 1 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 12 |
| Curated | 27 |
| Architect / Hermit proposals | 3 |
| Active epics | 3 |
<!-- guide:plan-body:end -->
