# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

_None._

## Urgent

Issues flagged as highest priority (`loom:urgent`).

_None._

## Ready

Human-approved issues ready for implementation (`loom:issue`).

_None._

## In Progress

Issues currently being built (`loom:building`).

_None._

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

_None._

## Proposed

Issues carrying `loom:curated`.

- **#6204**: Role-runner intervals all resolve to a uniform 1800s with nothing configured; docs document per-role 5–15 min built-ins *(curated)*
- **#6203**: workFinder.maxConcurrent config edit silently requires a daemon restart — docs imply the cap recomputes from live inputs *(curated)*
- **#6202**: resync-installed.sh is unusable on any clone that did not run install.sh — the documented freshness→resync remediation fails on first use *(curated)*
- **#6201**: Curator role silently dead on a workspace for 9 days: codex spawn 400s despite suggestedWorkerType=claude, RECOVERABLE failure never retried, no health surface *(curated)*
- **#6200**: Installer-written .claude/README.md asserts no per-project .mcp.json exists — goes stale when a later tool registers one *(curated)*
- **#6199**: loom:building is never cleared when an issue closes — 20 stale claims on one consumer repo *(curated)*
- **#6198**: warn-operator-gated: vocabulary misses "Operator task — requires human action", the most common operator-task phrasing *(curated)*
- **#6197**: /loom:sweep's operator-gate scan is phrase-based and cannot catch decision-shaped acceptance criteria *(curated)*
- **#6196**: Consumer AGENTS.md is 100% managed block — no room for repo-authored guidance *(curated)*
- **#6195**: Root CLAUDE.md is outside verify-install.sh's checksum set — marker damage is undetected *(curated)*
- **#6194**: test-verify-install-scope.sh ships to consumer repos but cannot run there *(curated)*
- **#6192**: Sweep build steps have no timeout — a wedged build volume accumulated 5 concurrent hung cargo builds for one sweep, plus orphans past sweep exit *(curated)*
- **#6191**: loom-daemon health exits 1 with all-unknown when its 2s IPC budget times out under sweep load — busy is indistinguishable from degraded *(curated)*
- **#6190**: Vendored .loom/.claude docs ship operator-specific private-repo references into every consumer repo *(curated)*
- **#6189**: check-main-clean.sh --quarantine posts the operator's machine hostname in public issue comments *(curated)*
- **#6177**: Flaky suite: config_resolver and git_utils::diff_stat fail under parallel test runs, pass in isolation (2 then 3 failures on the same commit) *(curated)*
- **#6175**: Stop-guard blocks every turn-end while a /loop ScheduleWakeup continuation timer is armed *(curated)*
- **#6173**: resync to 0.18.45 warns EPHEMERAL_PATTERNS on the newly-shipped biome.jsonc payload files *(curated)*
- **#6171**: workspace add is hot-applied but the per-owner App credential is not — a newly registered repo 404s silently until the daemon restarts *(curated)*
- **#6169**: CI settle-polls false-settle on empty gh pr checks output — mandate a row-count guard *(curated)*
- **#6168**: guard-background-subagents prescribes blocking TaskOutput, but TaskOutput on local_agent tasks dumps raw JSONL into orchestrator context *(curated)*
- **#6167**: recover-orphaned-shepherds: also reclaim stale PR-side loom:reviewing claims from dead Judges *(curated)*
- **#6163**: role_runner DEFAULT_ROLES warning names no workspace and fired 10,002 times in one day — undiagnosable and actively misleading *(curated)*
- **#6162**: An abandoned stash pop left spawn-claude.sh non-parsing in the live installed surface — and a resync would have shipped it to 25 workspaces *(curated)*
- **#6161**: Check-in on #6018: after the next fleet resync, verify the ownership classifier neither deleted repo-owned files nor over-preserved stale ones *(curated)*
- **#6160**: loom-daemon-update.sh cannot install a binary it just built when cargo target-dir is redirected — silent no-op that reports a build *(curated)*
- **#6159**: Check-in on #5999: has any live worktree been lost to a reinstall, and is the rm -rf fallback dead code? *(curated)*
- **#6158**: Efficacy review: after ~100 PRs, verify #6148's live-process gate still reclaims disk (over-detection fails silently) *(curated)*
- **#6157**: Duplicate builds when the peer-claim channel dies: stale-claim recovery becomes a duplication engine — needs fail-visible degradation *(curated)*
- **#6156**: Efficacy review: after ~300 merges, verify #6118's mergeable-recheck is behaving (and never merged a real conflict) *(curated)*
- **#6134**: Guide/Judge/Champion: fast-path review+merge for docs-only WORK_LOG/WORK_PLAN PRs *(curated)*
- **#6123**: Guard friction: worktree-write-confinement blocks gitignored build-artifact writes in the primary checkout *(curated)*
- **#6076**: Guard friction: stash-scope:main-checkout ASKs recur in headless runs despite a documented bypass toggle existing *(curated)*
- **#6062**: Pulse (2amlogic.com) narrates closed-unmerged duplicate efforts; should narrate on merge / dedup by merged-PR *(curated)*
- **#5897**: sweep skill: presumed-dead Builder Task can survive a session roll — verify task liveness before re-dispatch (duplicate-builder hazard) *(curated)*
- **#5660**: Vendored guard-destructive-generic.sh has drifted ~2,200 lines ahead of its upstream, and the single-marker capability probe makes partial reconciliation unsafe *(curated)*
- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
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
| Operator merge-risk holds | 0 |
| Urgent | 0 |
| Ready (`loom:issue`) | 0 |
| In Progress (`loom:building`) | 0 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 0 |
| Curated | 40 |
| Architect / Hermit proposals | 3 |
| Active epics | 3 |
<!-- guide:plan-body:end -->
