# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#6290**: fix: name-allowlist printenv SECRET/TOKEN/KEY ask pattern to stop LOOM_TOKEN_NAME false positive
- **#7404**: feat(codex): add session-exec dispatch mode to spawn-codex.sh

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#6926**: [Epic #6896] Phase 2: spawn-codex.sh session-exec mode (headless docker exec dispatch)
- **#7389**: [Epic #6896] Phase 2: operator interactive session — workspace mount in `session start`, `accounts session shell`, and the `codex-agent <account>` alias
- **#7414**: Post-merge verification: #6956 was squash-merged at loom:review-requested (unreviewed head) — confirm #6953 fix on main

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#7389**: [Epic #6896] Phase 2: operator interactive session — workspace mount in `session start`, `accounts session shell`, and the `codex-agent <account>` alias

## In Progress

Issues currently being built (`loom:building`).

_None._

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#7408**: feat(daemon): mount workspace in session start and add session shell + codex-agent

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#6290**: fix: name-allowlist printenv SECRET/TOKEN/KEY ask pattern to stop LOOM_TOKEN_NAME false positive
- **#7404**: feat(codex): add session-exec dispatch mode to spawn-codex.sh

## Proposed

Issues carrying `loom:curated`.

- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#5660**: Vendored guard-destructive-generic.sh has drifted ~2,200 lines ahead of its upstream, and the single-marker capability probe makes partial reconciliation unsafe *(curated)*
- **#6245**: Guard ask-pattern false positive: printenv of an account-label env var denied by credential-exposure TOKEN pattern, blocks headless runs *(curated)*
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space *(curated)*
- **#6565**: Dogfood config: loom-repo curator starved 3d — runtime=codex admitted with no codex model configured (#5028 skip, DEBUG-silent) *(curated)*
- **#6646**: Sweep resync committed, rebased and bypass-pushed the primary clone's main while an operator session was active in that clone *(curated)*
- **#6650**: .loom/config.json commits a live Matrix room id and ingest URL — intentional, or move to the private overlay tier? *(curated)*
- **#6656**: Enable Dependabot vulnerability alerts and security updates (both currently disabled) *(curated)*
- **#6704**: Roster-driven role-runner shard assignment: reassign a dead host's slice within a bounded window (follow-up to #6374's static ring) *(curated)*
- **#6969**: auto_update drain-and-restart: one relaunch waited ~4 min for the watchdog instead of launchd (KeepAlive.SuccessfulExit) — single observation *(curated)*
- **#7328**: test-guard-destructive.sh: #6472 assert_allow leaves a stray file in repo root as a side effect *(curated)*
- **#7356**: Guard friction: worktree-write-confinement-unresolved-var denies mktemp/tmp-scoped writes (44/126 = top guard-decision volume) *(curated)*
- **#7359**: merge=ours driver on .loom/install-metadata.json can silently drop non-loom_version field edits during rebase, uncaught by version-check-gate.sh *(curated)*
- **#7389**: [Epic #6896] Phase 2: operator interactive session — workspace mount in `session start`, `accounts session shell`, and the `codex-agent <account>` alias *(curated)*
- **#7414**: Post-merge verification: #6956 was squash-merged at loom:review-requested (unreviewed head) — confirm #6953 fix on main *(curated)*
- **#7416**: guard-destructive-generic.sh: .loom/hooks copy missing PR #7378's embedded-apostrophe fix (defaults/.loom drift, no CI parity check) *(curated)*

## Proposed (Architect / Hermit)

- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 2 |
| Urgent | 3 |
| Ready (`loom:issue`) | 1 |
| In Progress (`loom:building`) | 0 |
| PRs awaiting review | 1 |
| Approved PRs awaiting merge | 2 |
| Curated | 18 |
| Architect / Hermit proposals | 3 |
| Active epics | 3 |
<!-- guide:plan-body:end -->
