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
- **#7814**: add_worker.rs hard-codes operator identity defaults (feed egress sink URL, deny patterns) outside the #6650 scrub
- **#7815**: observability: refuse to export to reserved placeholder domains (example.com) instead of shipping the ingest key to them

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space
- **#7657**: Champion: close a proposal whose central premise is verified false instead of escalating it as an operator decision
- **#7814**: add_worker.rs hard-codes operator identity defaults (feed egress sink URL, deny patterns) outside the #6650 scrub

## In Progress

Issues currently being built (`loom:building`).

- **#7795**: Guard ASK tier: which sites steer toward a safe alternative, and which are a bare 'are you sure?' that stalls headless runs
- **#7815**: observability: refuse to export to reserved placeholder domains (example.com) instead of shipping the ingest key to them
- **#7844**: create-pr.sh's force-auto-merge path still hardcodes squash (follow-up to #7754 Part 1)
- **#7854**: [Epic #6896] Phase 4: migrate docker-requiring callers onto the run-job seam
- **#7860**: Investigate blocked-label removal and rapid redispatch on kicad-tools#5333
- **#7908**: epic #7810 PR 2: typed forge results over proc_exec — retire GhResult and two more ad-hoc runners

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#7904**: feat(champion): close proposals whose central premise is verified false
- **#7916**: guard: size the ASK tier to the decision log — 16 sites → 13 (#7795)
- **#7917**: fix(fleet): stop shipping this fleet's identity as `add-worker` egress defaults
- **#7918**: fix(observability): refuse to export to reserved placeholder domains

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#7910**: test(watchdog): detect read -d '' heredoc openers in the #7508 body scan

## Proposed

Issues carrying `loom:curated`.

- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space *(curated)*
- **#7657**: Champion: close a proposal whose central premise is verified false instead of escalating it as an operator decision *(curated)*
- **#7795**: Guard ASK tier: which sites steer toward a safe alternative, and which are a bare 'are you sure?' that stalls headless runs *(curated)*
- **#7814**: add_worker.rs hard-codes operator identity defaults (feed egress sink URL, deny patterns) outside the #6650 scrub *(curated)*
- **#7815**: observability: refuse to export to reserved placeholder domains (example.com) instead of shipping the ingest key to them *(curated)*
- **#7834**: test-loom-daemon-watchdog.sh: #7508 heredoc-body static scan passes vacuously for the read -d '' <<EOF || true bodies *(curated)*
- **#7844**: create-pr.sh's force-auto-merge path still hardcodes squash (follow-up to #7754 Part 1) *(curated)*
- **#7860**: Investigate blocked-label removal and rapid redispatch on kicad-tools#5333 *(curated)*
- **#7872**: branch_landed: rung-2/rung-3 unresolvable-tip asymmetry + minor cleanup in #7812's landed primitive *(curated)*
- **#7908**: epic #7810 PR 2: typed forge results over proc_exec — retire GhResult and two more ad-hoc runners *(curated)*

## Proposed (Architect / Hermit)

- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#7911**: [Epic #7810] Phase 1: Lossless subprocess boundary *(architect)*

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
| Ready (`loom:issue`) | 4 |
| In Progress (`loom:building`) | 6 |
| PRs awaiting review | 4 |
| Approved PRs awaiting merge | 1 |
| Curated | 11 |
| Architect / Hermit proposals | 3 |
| Active epics | 4 |
<!-- guide:plan-body:end -->
