# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#8016**: fix(guard): admit the exact mktemp-then-canonicalize chain in both same-command fast paths
- **#8207**: feat(pricing): deliver the model rate card as a resync-updatable defaults/pricing.json asset
- **#8220**: feat(tokens): prefer the account that last warmed this (repo, role) prompt cache (#8146)
- **#8227**: fix(release-fetch): refuse a published-but-unfetchable .sig instead of downgrading to checksum-only
- **#8258**: fix(provision-hooks): quote ${CLAUDE_PROJECT_DIR} in project-level hook entries
- **#8279**: fix(worktree): reconcile the dirty-marker filter with its shell twin (#8195)

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#8013**: test-guard-destructive-rm-scope.sh:338 '../ escaping the repo' assertion fails when the suite runs from a linked worktree (passes from the primary checkout)
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed
- **#8136**: Reconcile PR #8097's differential-corpus gaps with #8125's BotLoginNormalisation work
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it)
- **#8195**: Port worktree.sh to a daemon subcommand (1,812 lines; 26 fixes in 6 months, three of them data-loss classes)
- **#8221**: Guard: same-command mktemp fast paths count only bare `NAME=` assignments — `export`/`declare`/`read`/`printf -v` rebinding slips the ambiguity rule
- **#8248**: A green ratchet check goes stale when the baseline tightens under an in-flight PR — this is what broke main
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh
- **#8281**: bug(lease): publish and renew still fail on Bash 3.2 empty repo_args after fence fix

## In Progress

Issues currently being built (`loom:building`).

- **#8170**: sweep_registry::watchdog midbuild_* tests fail under the full parallel suite, pass 90/90 in isolation
- **#8217**: guard: an unquoted heredoc body's $( ) substitution bypasses rm-scope (extract_rm_targets analogue of #8035)
- **#8237**: Add a 'settled' allowlist category: shell we are deliberately keeping (47 scripts, 3,809 lines)
- **#8242**: [#8058 Phase 3] per-model-class observability: tokens health/status per-class counts, .ranking columns if the endpoint exposes them
- **#8250**: claim_reconciliation::reclaim_pr backfills loom:review-requested onto draft PRs (no isDraft check)
- **#8253**: dep-recheck-fingerprint: an already-MERGED PR's transient UNKNOWN mergeability still moves CONCLUSION_HASH (churns curator dep-recheck comments)
- **#8254**: dep-recheck-fingerprint: --block-reason is an uncanonicalized hash input, and the dep-recheck post is no longer serialized (gaps from 2am's retired premise-recheck.sh)
- **#8255**: curator.md Priority-2 fallback query ignores autonomous.workFinder.extraSkipLabels — repo-local non-work labels (2am's `journal`) surface as curation candidates
- **#8259**: Curator dep-recheck heartbeat spams issues held by champion:ac-hold (18 daily 'nothing changed' comments on gf180-pll#242) — a human-attestation gate has nothing to re-check
- **#8262**: loom-daemon tests: LOOM_SOCKET_PATH-mutating tests serialize under mismatched keys; one spawns a real heartbeat writer with no #[serial] (Rust half of #8077)
- **#8263**: gh api call sites pass an unsupported --repo flag, silently disabling five LOOM_REPO-host probes
- **#8265**: Judge accepts a 'TDD: yes' claim on path presence alone — require the test to fail at the merge-base
- **#8266**: Adopt a role-prompt authoring discipline: we ratchet prompt bytes but never verify prompt behaviour
- **#8277**: Thread the in-flight model into LOOM_TERMINAL_RESULT so #8058's class-scoped health marks have a producer

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#8314**: feat(guard): per-role tool-restriction allowlist enforced at the harness (#8256)
- **#8316**: fix(guard): scan an unquoted heredoc body's substitution spans for rm targets (#8217)

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#8016**: fix(guard): admit the exact mktemp-then-canonicalize chain in both same-command fast paths
- **#8207**: feat(pricing): deliver the model rate card as a resync-updatable defaults/pricing.json asset
- **#8220**: feat(tokens): prefer the account that last warmed this (repo, role) prompt cache (#8146)
- **#8227**: fix(release-fetch): refuse a published-but-unfetchable .sig instead of downgrading to checksum-only
- **#8258**: fix(provision-hooks): quote ${CLAUDE_PROJECT_DIR} in project-level hook entries
- **#8279**: fix(worktree): reconcile the dirty-marker filter with its shell twin (#8195)

## Proposed

Issues carrying `loom:curated`.

- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space *(curated)*
- **#7855**: robb-pro daemon stopped heartbeating and answering IPC for 25 min while still logging; the watchdog CONFIRMED the wedge twice and never restarted it *(curated)*
- **#7947**: [#4196 Phase 3b] Operator-agent persona: natural-language intent over the typed daemon ChatOps surface *(curated)*
- **#7972**: work_finder re-claims #7893 in a loop: 14 claims, 55 label events, 4 hours, zero PRs *(curated)*
- **#7986**: Guard: same-command mktemp safe-path denies the routine `VAR=$(cd "$VAR" && pwd -P)` realpath-canonicalization reassignment *(curated)*
- **#8001**: [Part of #7708] Broadcast the pool-exhaustion hold to peers via the peer-claim room *(curated)*
- **#8005**: Generalise #7818's credential-staging guard from two hardcoded gh-config paths to the credential-bearing class (.loom/tokens/, accounts.env, claude-config/) *(curated)*
- **#8013**: test-guard-destructive-rm-scope.sh:338 '../ escaping the repo' assertion fails when the suite runs from a linked worktree (passes from the primary checkout) *(curated)*
- **#8026**: peer_coordination DEGRADED can be a false positive during a fleet-wide dispatch lull (advertise is dispatch-gated, not periodic) *(curated)*
- **#8052**: telemetry: per-role × model token consumption report + weekly-limit calibration (activity.db token tables are empty, `loom-daemon stats` crashes) *(curated)*
- **#8055**: experiment: fleet-wide repo-stratified model A/B — assign arms, write/remove overlays, cover role-runner ticks, stamp the arm explicitly in outcome records *(curated)*
- **#8058**: token pool: per-model-class exhaustion state — an Opus ceiling bad-marks the whole account and starves Sonnet work *(curated)*
- **#8087**: Port loom-daemon-start.sh to a daemon subcommand (1,184 lines; preserve the FLAGS-OFF contract across start) *(curated)*
- **#8088**: Port loom-daemon-update.sh to a daemon subcommand (1,733 lines; the binary must replace itself) *(curated)*
- **#8097**: Differential corpus covers 2 of 7 separator chars, omits comments entirely, and its generator is not committed *(curated)*
- **#8103**: Repo settings: main has no required status checks, so CI cannot block a merge (and stale branches never re-run) *(curated)*
- **#8136**: Reconcile PR #8097's differential-corpus gaps with #8125's BotLoginNormalisation work *(curated)*
- **#8146**: Token selection: prefer the account that last warmed this (repo, role) prompt cache (65% vs 1.4% hit rate) *(curated)*
- **#8170**: sweep_registry::watchdog midbuild_* tests fail under the full parallel suite, pass 90/90 in isolation *(curated)*
- **#8177**: pricing: deliver the model rate card as a resync-updatable defaults/pricing.json asset (ask 2 of #8060) *(curated)*
- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)*
- **#8195**: Port worktree.sh to a daemon subcommand (1,812 lines; 26 fixes in 6 months, three of them data-loss classes) *(curated)*
- **#8197**: release-fetch: a .sig download failure is indistinguishable from an unsigned release, silently downgrading to checksum-only *(curated)*
- **#8217**: guard: an unquoted heredoc body's $( ) substitution bypasses rm-scope (extract_rm_targets analogue of #8035) *(curated)*
- **#8221**: Guard: same-command mktemp fast paths count only bare `NAME=` assignments — `export`/`declare`/`read`/`printf -v` rebinding slips the ambiguity rule *(curated)*
- **#8237**: Add a 'settled' allowlist category: shell we are deliberately keeping (47 scripts, 3,809 lines) *(curated)*
- **#8242**: [#8058 Phase 3] per-model-class observability: tokens health/status per-class counts, .ranking columns if the endpoint exposes them *(curated)*
- **#8248**: A green ratchet check goes stale when the baseline tightens under an in-flight PR — this is what broke main *(curated)*
- **#8250**: claim_reconciliation::reclaim_pr backfills loom:review-requested onto draft PRs (no isDraft check) *(curated)*
- **#8253**: dep-recheck-fingerprint: an already-MERGED PR's transient UNKNOWN mergeability still moves CONCLUSION_HASH (churns curator dep-recheck comments) *(curated)*
- **#8254**: dep-recheck-fingerprint: --block-reason is an uncanonicalized hash input, and the dep-recheck post is no longer serialized (gaps from 2am's retired premise-recheck.sh) *(curated)*
- **#8255**: curator.md Priority-2 fallback query ignores autonomous.workFinder.extraSkipLabels — repo-local non-work labels (2am's `journal`) surface as curation candidates *(curated)*
- **#8256**: security: per-role tool-restriction allowlist enforced at the harness (roles/*.json field + guard-hook backstop), so a persuaded read-only role cannot reach ssh/aws/gh secret/~/.ssh *(curated)*
- **#8257**: dashboard: add an ephemeral_compute record type (running-now + elastic-spend views, leak detection, hostless ingest) with a D1 migration *(curated)*
- **#8259**: Curator dep-recheck heartbeat spams issues held by champion:ac-hold (18 daily 'nothing changed' comments on gf180-pll#242) — a human-attestation gate has nothing to re-check *(curated)*
- **#8262**: loom-daemon tests: LOOM_SOCKET_PATH-mutating tests serialize under mismatched keys; one spawns a real heartbeat writer with no #[serial] (Rust half of #8077) *(curated)*
- **#8263**: gh api call sites pass an unsupported --repo flag, silently disabling five LOOM_REPO-host probes *(curated)*
- **#8265**: Judge accepts a 'TDD: yes' claim on path presence alone — require the test to fail at the merge-base *(curated)*
- **#8266**: Adopt a role-prompt authoring discipline: we ratchet prompt bytes but never verify prompt behaviour *(curated)*
- **#8277**: Thread the in-flight model into LOOM_TERMINAL_RESULT so #8058's class-scoped health marks have a producer *(curated)*
- **#8281**: bug(lease): publish and renew still fail on Bash 3.2 empty repo_args after fence fix *(curated)*
- **#8292**: test: cover the positive direction of champion-issue-promo's bot-vs-human un-park attribution (#8245 follow-up) *(curated)*
- **#8309**: Curator: state that an autonomous filing is not operator approval (#8269 point 3) *(curated)*

## Proposed (Architect / Hermit)

- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam
- **#7810**: [epic] Retire shell: 10 sequential PRs, two foundational then update + agent execution

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 6 |
| Urgent | 3 |
| Ready (`loom:issue`) | 10 |
| In Progress (`loom:building`) | 14 |
| PRs awaiting review | 2 |
| Approved PRs awaiting merge | 6 |
| Curated | 44 |
| Architect / Hermit proposals | 2 |
| Active epics | 4 |
<!-- guide:plan-body:end -->
