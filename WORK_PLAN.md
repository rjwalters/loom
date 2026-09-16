# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#7699**: fix(config): replace live safehouse room / observability endpoint with placeholders

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6646**: Sweep resync committed, rebased and bypass-pushed the primary clone's main while an operator session was active in that clone
- **#7431**: [Epic #6896] Phase 3: Fleet-default rollout — soak criteria, flip containment default on Linux fleet hosts

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6646**: Sweep resync committed, rebased and bypass-pushed the primary clone's main while an operator session was active in that clone
- **#6969**: auto_update drain-and-restart: one relaunch waited ~4 min for the watchdog instead of launchd (KeepAlive.SuccessfulExit) — single observation
- **#7431**: [Epic #6896] Phase 3: Fleet-default rollout — soak criteria, flip containment default on Linux fleet hosts
- **#7659**: Proposer roles: cite only paths that exist in the dispatched workspace; sibling checkouts named in a repo docs are context, never a citation target
- **#7664**: watchdog: peer-coordination escalation needs hysteresis and dedup — refiles a tracking issue on every self-recovering flap
- **#7666**: champion: a passing, already-decomposed epic is escalated to the operator after repeated identical stand-down passes
- **#7691**: [Phase B of #6704] Rank the role-runner ring from the live roster, generation-fenced, with bounded reassignment
- **#7694**: Builder: probe for a live sibling worktree before worktree.sh touches files (generalizing #6765)
- **#7726**: Restructure sweep.md under progressive disclosure
- **#7756**: classify-dependency-block.sh: 'prerequisite' phrase word false-positives a merits finding as a self-clearing dependency (regression risk: infinite re-evaluation loop)

## In Progress

Issues currently being built (`loom:building`).

- **#7748**: tokens_pool: blocking_entry_reports_exhaustion_class_and_cooldown_remaining flaked in CI (wall-clock jump, not code)
- **#7821**: epic #7810 PR 1: shared bounded subprocess execution — drain-safe, group-terminating, with self_update.rs as first migration

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#7670**: fix(champion): stop escalating a passing, already-decomposed epic to the operator
- **#7673**: docs(roles): citation-scope rules for hermit/architect; fix stale get_random_file example
- **#7680**: fix(watchdog): dedup peer-coordination escalations across the cooldown window (#7664)
- **#7685**: feat(resync): land resync commits conservatively, never rebase/bypass-push (#6646)
- **#7700**: Builder: probe for a live sibling worktree before worktree.sh touches files (generalizing #6765)
- **#7707**: fix(daemon): spawn a detached post-exit verifier on the auto-update drain-and-restart path
- **#7759**: docs(sweep): restructure sweep.md under progressive disclosure (#7726)
- **#7764**: fix: classify-dependency-block.sh 'prerequisite' false-positive on narrative co-occurrence (#7756)
- **#7769**: docs(runtime-adapters): document containment fleet-default soak criteria and rollback path
- **#7828**: feat(daemon): shared bounded subprocess execution — drain-safe, group-terminating (epic #7810 PR 1)
- **#7830**: fix(tests): widen wall-clock tolerance in cooldown-remaining assertions

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#7699**: fix(config): replace live safehouse room / observability endpoint with placeholders

## Proposed

Issues carrying `loom:curated`.

- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space *(curated)*
- **#6565**: Dogfood config: loom-repo curator starved 3d — runtime=codex admitted with no codex model configured (#5028 skip, DEBUG-silent) *(curated)*
- **#6646**: Sweep resync committed, rebased and bypass-pushed the primary clone's main while an operator session was active in that clone *(curated)*
- **#6650**: .loom/config.json commits a live Matrix room id and ingest URL — intentional, or move to the private overlay tier? *(curated)*
- **#6704**: Roster-driven role-runner shard assignment: reassign a dead host's slice within a bounded window (follow-up to #6374's static ring) *(curated)*
- **#6969**: auto_update drain-and-restart: one relaunch waited ~4 min for the watchdog instead of launchd (KeepAlive.SuccessfulExit) — single observation *(curated)*
- **#7431**: [Epic #6896] Phase 3: Fleet-default rollout — soak criteria, flip containment default on Linux fleet hosts *(curated)*
- **#7657**: Champion: close a proposal whose central premise is verified false instead of escalating it as an operator decision *(curated)*
- **#7659**: Proposer roles: cite only paths that exist in the dispatched workspace; sibling checkouts named in a repo docs are context, never a citation target *(curated)*
- **#7664**: watchdog: peer-coordination escalation needs hysteresis and dedup — refiles a tracking issue on every self-recovering flap *(curated)*
- **#7666**: champion: a passing, already-decomposed epic is escalated to the operator after repeated identical stand-down passes *(curated)*
- **#7691**: [Phase B of #6704] Rank the role-runner ring from the live roster, generation-fenced, with bounded reassignment *(curated)*
- **#7694**: Builder: probe for a live sibling worktree before worktree.sh touches files (generalizing #6765) *(curated)*
- **#7705**: Version-bump commits are landing with Cargo.lock / mcp-loom/package-lock.json desynced from the bumped version *(curated)*
- **#7716**: [tracking] Budget agent-facing markdown by tokens *(curated)*
- **#7726**: Restructure sweep.md under progressive disclosure *(curated)*
- **#7748**: tokens_pool: blocking_entry_reports_exhaustion_class_and_cooldown_remaining flaked in CI (wall-clock jump, not code) *(curated)*
- **#7756**: classify-dependency-block.sh: 'prerequisite' phrase word false-positives a merits finding as a self-clearing dependency (regression risk: infinite re-evaluation loop) *(curated)*
- **#7758**: Re-derive which bootstrap scripts must stay shell — auto_update.rs already duplicates loom-daemon-update.sh *(curated)*
- **#7794**: Seven scripts read their own source to print --help; the tear-race has broken CI three times *(curated)*

## Proposed (Architect / Hermit)

- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#7777**: ADR-0018 (draft): Rust owns behavior; shell exists only to reach it *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam
- **#7810**: [epic] Retire shell: 10 sequential PRs, two foundational then update + agent execution

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 1 |
| Urgent | 3 |
| Ready (`loom:issue`) | 11 |
| In Progress (`loom:building`) | 2 |
| PRs awaiting review | 11 |
| Approved PRs awaiting merge | 1 |
| Curated | 22 |
| Architect / Hermit proposals | 4 |
| Active epics | 4 |
<!-- guide:plan-body:end -->
