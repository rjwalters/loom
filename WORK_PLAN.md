# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#7699**: fix(config): replace live safehouse room / observability endpoint with placeholders
- **#7707**: fix(daemon): spawn a detached post-exit verifier on the auto-update drain-and-restart path
- **#7871**: fix(ci): publish version bumps with repository App identity

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#7765**: worktree.sh silently creates a main-HEAD branch shadowing an open cross-repo PR's branch name
- **#7853**: [Epic #6896] Phase 4: run-job seam contract + host executor

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#7765**: worktree.sh silently creates a main-HEAD branch shadowing an open cross-repo PR's branch name
- **#7829**: Activate version-bump-on-merge: push via the loom-fleet-dispatch App token (GITHUB_TOKEN cannot bypass the main ruleset on a user-owned repo)
- **#7853**: [Epic #6896] Phase 4: run-job seam contract + host executor

## In Progress

Issues currently being built (`loom:building`).

- **#7791**: Retry-once-and-record for the shared shell suite: one flaky assertion currently blocks every concurrent PR
- **#7792**: tokens_pool: empty_pool_error_enumerates_per_token_exclusion_detail flakes on the 4h/session-limit-window hour boundary
- **#7862**: flaky CI: assert_contains's 'printf | grep -q' races SIGPIPE under pipefail, reporting present substrings as absent
- **#7882**: Guard decision-log defaults into defaults/logs/ when invoked from source, polluting the vendored tree

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#7875**: feat(run-job): ship the run-job seam contract + loopback/SSH host executor
- **#7886**: feat(ci): retry-once-and-record for the shared shell CI suite

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#7699**: fix(config): replace live safehouse room / observability endpoint with placeholders
- **#7707**: fix(daemon): spawn a detached post-exit verifier on the auto-update drain-and-restart path
- **#7871**: fix(ci): publish version bumps with repository App identity
- **#7885**: feat(ci): ratchet the pipefail + early-exit-consumer SIGPIPE class

## Proposed

Issues carrying `loom:curated`.

- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space *(curated)*
- **#6565**: Dogfood config: loom-repo curator starved 3d — runtime=codex admitted with no codex model configured (#5028 skip, DEBUG-silent) *(curated)*
- **#6650**: .loom/config.json commits a live Matrix room id and ingest URL — intentional, or move to the private overlay tier? *(curated)*
- **#6704**: Roster-driven role-runner shard assignment: reassign a dead host's slice within a bounded window (follow-up to #6374's static ring) *(curated)*
- **#6969**: auto_update drain-and-restart: one relaunch waited ~4 min for the watchdog instead of launchd (KeepAlive.SuccessfulExit) — single observation *(curated)*
- **#7657**: Champion: close a proposal whose central premise is verified false instead of escalating it as an operator decision *(curated)*
- **#7691**: [Phase B of #6704] Rank the role-runner ring from the live roster, generation-fenced, with bounded reassignment *(curated)*
- **#7705**: Version-bump commits are landing with Cargo.lock / mcp-loom/package-lock.json desynced from the bumped version *(curated)*
- **#7716**: [tracking] Budget agent-facing markdown by tokens *(curated)*
- **#7758**: Re-derive which bootstrap scripts must stay shell — auto_update.rs already duplicates loom-daemon-update.sh *(curated)*
- **#7765**: worktree.sh silently creates a main-HEAD branch shadowing an open cross-repo PR's branch name *(curated)*
- **#7789**: [tracking] 25 CI flake issues in 60 days, 22 closed: fix the mechanisms, not the instances *(curated)*
- **#7790**: Close the pipefail + early-exit-consumer SIGPIPE class: 4 flakes and 1 production wrong-answer from one idiom *(curated)*
- **#7791**: Retry-once-and-record for the shared shell suite: one flaky assertion currently blocks every concurrent PR *(curated)*
- **#7792**: tokens_pool: empty_pool_error_enumerates_per_token_exclusion_detail flakes on the 4h/session-limit-window hour boundary *(curated)*
- **#7793**: RCA: agent errors in this session cluster into 4 classes, 2 of which are the repo's own tracked defect classes *(curated)*
- **#7794**: Seven scripts read their own source to print --help; the tear-race has broken CI three times *(curated)*
- **#7795**: Guard ASK tier: which sites steer toward a safe alternative, and which are a bare 'are you sure?' that stalls headless runs *(curated)*
- **#7829**: Activate version-bump-on-merge: push via the loom-fleet-dispatch App token (GITHUB_TOKEN cannot bypass the main ruleset on a user-owned repo) *(curated)*
- **#7862**: flaky CI: assert_contains's 'printf | grep -q' races SIGPIPE under pipefail, reporting present substrings as absent *(curated)*

## Proposed (Architect / Hermit)

- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#7777**: ADR-0018 (draft): Rust owns behavior; shell exists only to reach it *(architect)*
- **#7854**: [Epic #6896] Phase 4: migrate docker-requiring callers onto the run-job seam *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam
- **#7810**: [epic] Retire shell: 10 sequential PRs, two foundational then update + agent execution

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 3 |
| Urgent | 3 |
| Ready (`loom:issue`) | 4 |
| In Progress (`loom:building`) | 4 |
| PRs awaiting review | 2 |
| Approved PRs awaiting merge | 4 |
| Curated | 22 |
| Architect / Hermit proposals | 5 |
| Active epics | 4 |
<!-- guide:plan-body:end -->
