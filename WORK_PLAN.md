# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#6207**: fix(guard): mask echo/printf positional args in catastrophic-tier scan
- **#6210**: feat(worktree): add a main target to stash-push/stash-pop and stop advising git stash pop in the primary clone
- **#6212**: fix(ci-settle-poll): guard against empty gh pr checks output false-settling
- **#6290**: fix: name-allowlist printenv SECRET/TOKEN/KEY ask pattern to stop LOOM_TOKEN_NAME false positive
- **#6422**: fix(merge): distinguish persistent check-runs 404 from transient fetch failure
- **#6621**: feat: restamp root CLAUDE.md's Loom Version header on resync
- **#6631**: fix(resync): distinguish retired-but-unlisted payload files from shipped payload
- **#6732**: fix: resolve NAME=$(pwd) cwd capture in guard force-op:detached parsing
- **#6817**: fix(guard): resolve rm targets built from a var plus a literal path suffix
- **#6956**: fix(guard): double-quoted-RHS $(...) same-command assignment no longer corrupts a later write-target token
- **#7026**: fix(verdict): strip all terminal verdict labels on clear, not just the one detected as stale
- **#7246**: feat(daemon): add loom-daemon accounts session start|stop|status|attach

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#6320**: In-session /loom:sweep claims publish no lease record, so any daemon reclaims them — two builders in one worktree, uncommitted work lost
- **#6389**: merge-pr.sh --auto polls to LOOM_AUTO_MERGE_TIMEOUT when the check-runs API persistently 404s (repo with no Actions)
- **#6472**: guard-destructive false positive: '>' inside a quoted awk program, and sed -n without -i, are denied as a write to target '|'

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#6320**: In-session /loom:sweep claims publish no lease record, so any daemon reclaims them — two builders in one worktree, uncommitted work lost
- **#6472**: guard-destructive false positive: '>' inside a quoted awk program, and sed -n without -i, are denied as a write to target '|'

## In Progress

Issues currently being built (`loom:building`).

- **#7161**: Add uninstall-loom.sh test coverage for .claude/settings.json permissions.deny removal
- **#7294**: Guard: same-command cd into a var-assigned dir doesn't propagate resolved cwd to later relative writes

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#6207**: fix(guard): mask echo/printf positional args in catastrophic-tier scan
- **#6210**: feat(worktree): add a main target to stash-push/stash-pop and stop advising git stash pop in the primary clone
- **#6212**: fix(ci-settle-poll): guard against empty gh pr checks output false-settling
- **#6290**: fix: name-allowlist printenv SECRET/TOKEN/KEY ask pattern to stop LOOM_TOKEN_NAME false positive
- **#6422**: fix(merge): distinguish persistent check-runs 404 from transient fetch failure
- **#6532**: fix(scripts): honor repo-relative resync-ignore pins and warn on dead pins
- **#6621**: feat: restamp root CLAUDE.md's Loom Version header on resync
- **#6631**: fix(resync): distinguish retired-but-unlisted payload files from shipped payload
- **#6732**: fix: resolve NAME=$(pwd) cwd capture in guard force-op:detached parsing
- **#6742**: feat(forge-helpers): add forge_gh_repo_safe wrong-repo GH_CONFIG_DIR escalation
- **#6817**: fix(guard): resolve rm targets built from a var plus a literal path suffix
- **#6956**: fix(guard): double-quoted-RHS $(...) same-command assignment no longer corrupts a later write-target token
- **#7026**: fix(verdict): strip all terminal verdict labels on clear, not just the one detected as stale
- **#7246**: feat(daemon): add loom-daemon accounts session start|stop|status|attach
- **#7291**: fix(curator): extract dep-recheck fingerprint into a shared, tested script

## Proposed

Issues carrying `loom:curated`.

- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#5660**: Vendored guard-destructive-generic.sh has drifted ~2,200 lines ahead of its upstream, and the single-marker capability probe makes partial reconciliation unsafe *(curated)*
- **#6068**: Guard false positive: catastrophic-tier positional masking doesn't cover echo/printf, so a heading echo containing the trigger phrase hard-denies *(curated)*
- **#6076**: Guard friction: stash-scope:main-checkout ASKs recur in headless runs despite a documented bypass toggle existing *(curated)*
- **#6169**: CI settle-polls false-settle on empty gh pr checks output — mandate a row-count guard *(curated)*
- **#6245**: Guard ask-pattern false positive: printenv of an account-label env var denied by credential-exposure TOKEN pattern, blocks headless runs *(curated)*
- **#6320**: In-session /loom:sweep claims publish no lease record, so any daemon reclaims them — two builders in one worktree, uncommitted work lost *(curated)*
- **#6389**: merge-pr.sh --auto polls to LOOM_AUTO_MERGE_TIMEOUT when the check-runs API persistently 404s (repo with no Actions) *(curated)*
- **#6472**: guard-destructive false positive: '>' inside a quoted awk program, and sed -n without -i, are denied as a write to target '|' *(curated)*
- **#6515**: resync-ignore: pins in repo-relative form silently never match — clobbered a pinned file and broke a repo's role ticks *(curated)*
- **#6565**: Dogfood config: loom-repo curator starved 3d — runtime=codex admitted with no codex model configured (#5028 skip, DEBUG-silent) *(curated)*
- **#6612**: resync: version stamp in installed CLAUDE.md stays stale — give the version line managed-section markers *(curated)*
- **#6613**: resync orphan warning: distinguish formerly-shipped-then-removed files from project-local ones *(curated)*
- **#6646**: Sweep resync committed, rebased and bypass-pushed the primary clone's main while an operator session was active in that clone *(curated)*
- **#6650**: .loom/config.json commits a live Matrix room id and ingest URL — intentional, or move to the private overlay tier? *(curated)*
- **#6656**: Enable Dependabot vulnerability alerts and security updates (both currently disabled) *(curated)*
- **#6704**: Roster-driven role-runner shard assignment: reassign a dead host's slice within a bounded window (follow-up to #6374's static ring) *(curated)*
- **#6724**: Guard force-op:detached fires on cd+pwd-captured worktree path before git -C reset --hard *(curated)*
- **#6953**: Guard: double-quoted RHS same-command assignment wrapping $(...) corrupts a later write-target token (worktree-write-confinement) *(curated)*
- **#6969**: auto_update drain-and-restart: one relaunch waited ~4 min for the watchdog instead of launchd (KeepAlive.SuccessfulExit) — single observation *(curated)*
- **#7018**: Stray loom:pr labels surviving operator-ruling label transitions (mutual-exclusion violation) *(curated)*
- **#7161**: Add uninstall-loom.sh test coverage for .claude/settings.json permissions.deny removal *(curated)*
- **#7281**: Curator dep-recheck fingerprint has no shared script — hash churn defeats idempotency guard, spams duplicate comments *(curated)*
- **#7287**: Flaky: "Shell Test Suites (hermetic)" intermittently fails unrelated suites in CI (test-loom-daemon-start.sh DEK6b, test-install-stash-scope.sh) *(curated)*
- **#7294**: Guard: same-command cd into a var-assigned dir doesn't propagate resolved cwd to later relative writes *(curated)*

## Proposed (Architect / Hermit)

- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#6926**: [Epic #6896] Phase 2: spawn-codex.sh session-exec mode (headless docker exec dispatch) *(architect)*
- **#6927**: [Epic #6896] Phase 2: Codex auth-state health probe + re-auth runbook *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 12 |
| Urgent | 3 |
| Ready (`loom:issue`) | 2 |
| In Progress (`loom:building`) | 2 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 15 |
| Curated | 27 |
| Architect / Hermit proposals | 5 |
| Active epics | 3 |
<!-- guide:plan-body:end -->
