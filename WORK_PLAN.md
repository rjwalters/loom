# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5543**: dashboard-deploy is pinned by a miniflare isolated-storage flake — the live Worker has not redeployed since 08-05T07:28Z

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5543**: dashboard-deploy is pinned by a miniflare isolated-storage flake — the live Worker has not redeployed since 08-05T07:28Z

## In Progress

Issues currently being built (`loom:building`).

- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5550**: fix(dashboard-deploy): patch miniflare isolated-storage flake, retry-once + failed-test-gate visibility
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision *(curated)*
- **#5546**: loom-daemon is DOWN on robb-pro and watchdog recovery is exhausted *(curated)*
- **#5543**: dashboard-deploy is pinned by a miniflare isolated-storage flake — the live Worker has not redeployed since 08-05T07:28Z *(curated)*
- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*

## Proposed (Architect / Hermit)

- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management

## Backlog Balance

| Tier | Count |
|------|-------|
| Urgent | 1 |
| Ready (`loom:issue`) | 1 |
| In Progress (`loom:building`) | 1 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 2 |
| Curated | 7 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-07, Guide triage cycle):** No new `loom:issue` ready work arrived; #5543 remains the sole ready/urgent issue (1 of a possible 3 `loom:urgent` slots used — no other ready-and-unclaimed issue exists to fill the remaining slots), and its fix (PR #5550, `loom:pr`) is already approved and queued for Champion auto-merge, so no priority change was needed. The only backlog-state delta since the prior pass is #5565 moving into `loom:building` (still correctly excluded from `loom:urgent` consideration per the Building-issue safety rule) plus a newly filed `loom:blocked` issue, #5567 (retire the mechanism-repo's `dashboard-deploy.yml` once the instance-side deploy at `2AMLogic/2am#76` goes green) — an external cross-repo sequencing gate with no parseable in-repo dependency, so it correctly stays blocked pending that (inaccessible-to-this-token) repo's status. `loom-recover-orphans --recover` found 0 orphans (queue's one `loom:building` issue, #5565, is within its grace period, not stale). Verified the 7 most-recently-merged non-docs PRs (#5561, #5554, #5553, #5551, #5541, #5537, #5534) all correctly closed their linked issues via GitHub's `closingIssuesReferences` — no orphaned closures found. Re-checked all 7 open `loom:blocked` issues (#5567, #5385, #5329, #4196, #4167, #4136, #3979) for parseable `Blocked by/Depends on/Requires #N` references — none found on any of them, so none qualify for the mechanical dependency-unblock path; #5385 in particular remains correctly blocked under the superseding-block gate (linked PR #5397 is still OPEN with `loom:changes-requested`, capped after exhausting the Doctor-cycle budget) even though its own body dependency language is stale. Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, `loom:operator-only`-gated for a live Codex canary requiring operator credentials — last activity 2026-08-06, not stale). WORK_LOG.md had no new merged-PR or closed-issue content to append this pass (latest entries, PR #5561 / Issue #5559, are still the most recent on both fronts).
