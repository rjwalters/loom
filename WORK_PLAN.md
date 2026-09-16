# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

_None._

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6646**: Sweep resync committed, rebased and bypass-pushed the primary clone's main while an operator session was active in that clone
- **#7664**: watchdog: peer-coordination escalation needs hysteresis and dedup — refiles a tracking issue on every self-recovering flap

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6646**: Sweep resync committed, rebased and bypass-pushed the primary clone's main while an operator session was active in that clone
- **#6650**: .loom/config.json commits a live Matrix room id and ingest URL — intentional, or move to the private overlay tier?
- **#6969**: auto_update drain-and-restart: one relaunch waited ~4 min for the watchdog instead of launchd (KeepAlive.SuccessfulExit) — single observation
- **#7431**: [Epic #6896] Phase 3: Fleet-default rollout — soak criteria, flip containment default on Linux fleet hosts
- **#7659**: Proposer roles: cite only paths that exist in the dispatched workspace; sibling checkouts named in a repo docs are context, never a citation target
- **#7664**: watchdog: peer-coordination escalation needs hysteresis and dedup — refiles a tracking issue on every self-recovering flap
- **#7666**: champion: a passing, already-decomposed epic is escalated to the operator after repeated identical stand-down passes
- **#7691**: [Phase B of #6704] Rank the role-runner ring from the live roster, generation-fenced, with bounded reassignment
- **#7694**: Builder: probe for a live sibling worktree before worktree.sh touches files (generalizing #6765)
- **#7726**: Restructure sweep.md under progressive disclosure
- **#7730**: cargo test --lib fails on macOS: embedded shell driver uses bash 4 'declare -A' (bash 3.2)
- **#7756**: classify-dependency-block.sh: 'prerequisite' phrase word false-positives a merits finding as a self-clearing dependency (regression risk: infinite re-evaluation loop)
- **#7783**: post-verdict.sh: ${_gate_all_ids[*]} is an unbound variable on macOS bash 3.2 — blocks every Judge verdict on the clean path
- **#7801**: ci: markdown anchor fragments are never validated — 15 broken anchors on main today
- **#7804**: Record a 'dumb reliable CI' principle in CLAUDE.md: three failures today came from individually-justified cleverness

## In Progress

Issues currently being built (`loom:building`).

- **#7811**: main is red: guard suites read the repo's own committed config, so #7799's opt-out broke them

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#7670**: fix(champion): stop escalating a passing, already-decomposed epic to the operator
- **#7673**: docs(roles): citation-scope rules for hermit/architect; fix stale get_random_file example
- **#7680**: fix(watchdog): dedup peer-coordination escalations across the cooldown window (#7664)
- **#7685**: feat(resync): land resync commits conservatively, never rebase/bypass-push (#6646)
- **#7699**: fix(config): replace live safehouse room / observability endpoint with placeholders
- **#7700**: Builder: probe for a live sibling worktree before worktree.sh touches files (generalizing #6765)
- **#7707**: fix(daemon): spawn a detached post-exit verifier on the auto-update drain-and-restart path
- **#7759**: docs(sweep): restructure sweep.md under progressive disclosure (#7726)
- **#7764**: fix: classify-dependency-block.sh 'prerequisite' false-positive on narrative co-occurrence (#7756)
- **#7778**: fix: make champion:hold-state pipeline pipefail-safe in merge-pr.sh
- **#7782**: fix(daemon): drop bash-4 declare -A from the shell_is_ignored test driver
- **#7805**: docs: record a 'dumb reliable CI' principle in CLAUDE.md, paid for within the 320-line budget

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

_None._

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
- **#7730**: cargo test --lib fails on macOS: embedded shell driver uses bash 4 'declare -A' (bash 3.2) *(curated)*
- **#7756**: classify-dependency-block.sh: 'prerequisite' phrase word false-positives a merits finding as a self-clearing dependency (regression risk: infinite re-evaluation loop) *(curated)*
- **#7758**: Re-derive which bootstrap scripts must stay shell — auto_update.rs already duplicates loom-daemon-update.sh *(curated)*
- **#7783**: post-verdict.sh: ${_gate_all_ids[*]} is an unbound variable on macOS bash 3.2 — blocks every Judge verdict on the clean path *(curated)*
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
| Operator merge-risk holds | 0 |
| Urgent | 3 |
| Ready (`loom:issue`) | 16 |
| In Progress (`loom:building`) | 1 |
| PRs awaiting review | 12 |
| Approved PRs awaiting merge | 0 |
| Curated | 23 |
| Architect / Hermit proposals | 4 |
| Active epics | 4 |
<!-- guide:plan-body:end -->
