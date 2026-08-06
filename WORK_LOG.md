# Work Log

Chronological record of completed work in this repository, maintained by the Guide role.

Entries are grouped by date, newest first. Each entry references the merged PR or closed issue.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

### 2026-08-06
- **PR #5541**: fix(guide): stop dropping out-of-order-closed issues from WORK_LOG.md
- **Issue #5539** (closed): Guide's WORK_LOG.md closed-issue watermark misses out-of-order-closed issues (mirrors #5516, PR side already fixed)
- **PR #5537**: fix(orphan-recovery): never reset loom:building while a linked PR is open
- **PR #5534**: feat(install): add --dry-run and a root VERSION file
- **Issue #5517** (closed): Installer contract: empty VERSION file, and install.sh has no --dry-run
- **PR #5533**: fix(safehouse): loudify unresolved-socket failure + add drift check (#5523)
- **Issue #5523** (closed): #5457 left safehouse socket resolution with no default — every sweep silently stopped narrating, froze the public pulse for 11h
- **PR #5531**: fix(guide): stop dropping out-of-order-merged PRs from WORK_LOG.md
- **Issue #5516** (closed): Guide WORK_LOG.md watermark misses out-of-order-merged PRs (number > last_pr assumes merge order == number order)
- **PR #5525**: feat(fleet): add fleet roll with a measured process-vs-build verdict
- **Issue #5510** (closed): resync-installed.sh inside the loom repo modifies tracked files at a clean checkout — is that supported?
- **PR #5524**: docs(resync): clarify same-commit drift in loom repo is expected
- **Issue #5515** (closed): Guard: extract_write_targets() misreads bash arithmetic >/>= comparisons as redirection, manufacturing phantom write targets
- **PR #5521**: fix(guard): stop misreading arithmetic/test >, >=, <, <= as redirection
- **Issue #5508** (closed): Role-runner spawned sessions (Judge/Champion/etc.) inherit the daemon's own GH_CONFIG_DIR instead of the per-owner one — 404s on every non-default-owner repo
- **PR #5522**: fix(daemon): route role-runner children through per-owner GH_CONFIG_DIR
- **Issue #5502** (closed): Model "a human is needed" as a first-class state (loom:operator), not a comment marker
- **PR #5519**: feat(labels,champion): add loom:operator state, wire into merge-risk hold
- **Issue #5501** (closed): live_state_sandbox guards state paths but not launchd/systemd labels — a sandboxed test can stop the real daemon
- **PR #5507**: fix: guard test sandbox supervisor identity, not just state paths
- **Issue #5499** (closed): Codex: a roleModels pin that a ChatGPT-plan seat cannot serve fails as RECOVERABLE and retries forever
- **PR #5509**: fix(codex): drop a pinned model on ChatGPT-plan seats, classify the 400 as FATAL
- **PR #5503**: fix(gitignore): converge the managed block on .loom/.install.lock (#4940)

- **Issue #5504** (closed): loom-daemon fleet has no roll subcommand — and a roll needs a measured verdict, not --version
- **PR #5462**: chore(deps): bump docker/setup-buildx-action from 3 to 4
- **PR #5461**: chore(deps): bump actions/download-artifact from 7 to 8
- **PR #5460**: chore(deps): bump docker/login-action from 3 to 4
- **PR #5459**: chore(deps): bump docker/build-push-action from 6 to 7
- **PR #5233**: fix(guard): exclude heredoc redirection tokens from tee/cp/mv/sed write-target scan

### 2026-08-05

- **Issue #5495** (closed): check-ci-status.sh falsely reports 'success' while the main CI workflow is still queued/pending
- **PR #5500**: fix(scripts): fold GitHub Actions workflow-run state into check-ci-status.sh pending detection
- **Issue #5497** (closed): CI failing on main: dangling link to onidle-phase3-finding.md in daemon-reference.md
- **PR #5498**: fix(docs): add missing symlink for onidle-phase3-finding.md
- **Issue #5489** (closed): [Epic #5038 Phase 3] Activate onIdle scheduling for auditor and guide roles
- **PR #5494**: feat(daemon-docs): add onIdle verification script + Epic #5038 Phase 3 finding
- **Issue #5488** (closed): [Epic #5038 Phase 2] Add CI gates for repo hygiene: dangling links, gitignore drift, README/doc accuracy
- **PR #5493**: ci: add dangling-link and gitignore-convergence CI gates
- **PR #5484**: fix(daemon): scope the autonomy teardown like the kill it accompanies (#5131)
- **PR #5482**: fix(daemon): wire guards/quarantine/watchdog/worktree_ops gh calls through per-owner GH_CONFIG_DIR
- **PR #5481**: fix: exclude root node_modules and .mcp.json symlinks from worktree git status
- **Issue #5474** (closed): worktree.sh: root node_modules symlink (and .mcp.json) never get _append_worktree_exclude — untracked noise in every worktree
- **PR #5480**: ci: wire check-cas-recheck-consistency.sh into installer-tests (#4607)
- **PR #5479**: fix(daemon): never let unresolved host identity self-match in peer claims
- **PR #5478**: test: tie internal drain-exit(0) to watchdog kickstart recovery in one integration test
- **PR #5475**: docs: operator-session lane — skip Curator for mechanically-verifiable trivial changes
- **PR #5476**: docs(security): state plainly that CODEOWNERS does not gate merges here
- **PR #5472**: fix: revert the codeowners-enforcement probe line from ci.yml
- **PR #5471**: test: probe code-owner review enforcement (do not merge)
- **PR #5470**: chore(security): require code-owner review for .github/workflows/
- **Issue #5467** (closed): loom-daemon status does not report the RUNNING daemon's build — a failed restart is invisible
- **Issue #5455** (closed): Judge fallback queue livelocks on unlabeled PRs — 199 evaluations of a 2-line Dependabot bump (#4972)
- **PR #5463**: fix(judge): bound fallback-queue evaluations with a per-PR lifetime cap and bot exclusion
- **Issue #5457** (closed): Committed .loom/config.json hardcodes /Users/... macOS paths — Linux hosts must patch it, which then blocks git pull
- **PR #5464**: fix(safehouse): resolve socket per-host instead of a committed foreign path
- **Issue #5454** (closed): Guide's doc-maintenance phase self-perpetuates: own merged PRs feed the next cycle's WORK_LOG diff, generating a new PR every ~15-30min
- **PR #5465**: fix(guide): exclude the doc-maintenance phase's own PRs from the WORK_LOG diff
- **PR #5458**: chore(deps): bump clap from 4.6.4 to 4.6.5 in the all-dependencies group
- **PR #5456**: chore(deps): label Dependabot PRs into the Loom workflow (#5455)
- **Issue #5453** (closed): Guide's WORK_LOG update logs its own docs-maintenance PRs, causing perpetual per-cycle PR churn
- **PR #5451**: docs: Guide document maintenance update
- **PR #5450**: docs: Guide document maintenance update
- **PR #5449**: docs: Guide document maintenance update
- **PR #5448**: docs: Guide document maintenance update
- **PR #5447**: docs: Guide document maintenance update
- **PR #5446**: docs: Guide document maintenance update
- **PR #5445**: docs: Guide document maintenance update
- **PR #5444**: docs: Guide document maintenance update
- **PR #5443**: fix(tests): isolate tier-1 config in test-github-app-token.sh unconfigured cases
- **PR #5442**: fix(daemon): persist actual work_finder/health_gate into the autonomy marker so the nohup tier stops false-flagging every bare restart
- **PR #5439**: docs: Guide document maintenance update
- **Issue #5441** (closed): test-github-app-token.sh: 'unconfigured' case leaks host's private-defaults config tier (tier 1)
- **Issue #5437** (closed): loom-daemon-start.sh: nohup-tier downgrade-refusal false-positives on every bare restart after any prior start (#5426 regression)
- **PR #5438**: fix: avoid bare apostrophes in heredoc-in-$(...) bodies (bash 3.2 parser bug)
- **PR #5434**: docs: Guide document maintenance update
- **PR #5435**: fix(daemon): state confirmed FLAGS-OFF explicitly on a pid-file/nohup restart
- **Issue #5433** (closed): loom-daemon-start.sh fails to parse under bash 3.2 (stock macOS /bin/bash) — apostrophe in heredoc breaks command substitution
- **Issue #5436** (closed): loom-daemon-start.sh fails to parse under macOS stock bash 3.2 (unquoted heredoc + apostrophe)
- **Issue #5429** (closed): test-loom-daemon-update.sh: intermittent CI failures unrelated to the PR under review
- **PR #5432**: docs: Guide document maintenance update
- **PR #5430**: docs: Guide document maintenance update
- **PR #5428**: docs: Guide document maintenance update
- **PR #5427**: fix(install): probe pnpm runnability in install-loom.sh, not just presence
- **PR #5423**: fix(guide): give Document Maintenance a managed worktree to write in
- **PR #5425**: docs: Guide document maintenance update
- **PR #5424**: feat(fleet): refuse a non-Linux add-worker target after a uname probe
- **PR #5422**: feat(daemon): flag checkpoint reads as stale when the issue is closed
- **PR #5421**: docs: Guide document maintenance update
- **PR #5419**: fix(ci): bump Node from EOL v20 to Active LTS v24, state supported version
- **PR #5418**: docs: catch up WORK_LOG/WORK_PLAN high-water marks after Guide's 5.5-month dispatch gap
- **PR #5417**: fix(daemon): report real installation_id + warn on cross-owner managed repos
- **PR #5416**: fix(packaging): add missing defaults/roles symlink for comment-body-literal-path.md
- **Issue #5406** (closed): CI pins Node 20, which is EOL — and no engines/.nvmrc states a supported version
- **Issue #5413** (closed): Guide document-maintenance phase silently stopped landing PRs since 2026-02-26 (WORK_LOG high-water mark stuck at #3028)

- **Issue #5431** (closed): Wire remaining daemon gh call sites (guards/quarantine/watchdog/worktree_ops) through per-owner GH_CONFIG_DIR
- **Issue #5390** (closed): auto-update drain exits 0 for a launchd relaunch that never comes (the #4011 failure mode)
- **Issue #5440** (closed): Guard: tee/sed/cp/mv write-target scan misparses heredoc opener as a bogus target, causing false worktree-confinement DENY
- **Issue #5353** (closed): Operator-session lane: let session tools skip Curator for mechanically-verifiable trivial changes
- **Issue #5393** (closed): install.sh and loom-daemon assume a login-shell PATH — false 'missing dependency' over ssh
- **PR #5469**: fix(install): probe non-login install roots for deps; document loom-daemon over ssh
- **Issue #5381** (closed): Daemon dispatch degraded: GitHub App not installed on 2AMLogic/marketing and 2AMLogic/2am (persistent 404s)
- **Issue #5338** (closed): loom-worker-2 reports 0 registered repos despite 8 workspaces on disk
- **Issue #5401** (closed): Daemon mints one installation token for its workspace root's owner — cross-owner repos are unreachable
- **PR #5420**: fix(daemon): mint a GitHub App token per managed-repo owner so cross-owner repos are reachable
- **Issue #5409** (closed): #4693 recurred: plain start on the RECOVERY path silently downgraded autonomy again (~1h idle)
- **PR #5426**: fix(daemon): refuse a real start that would silently downgrade autonomy
- **Issue #5411** (closed): scripts/install-loom.sh has the same presence-only pnpm check as install.sh (#5394)
- **Issue #5395** (closed): fleet add-worker is Linux-only with no platform check — Mac hosts have no encoded onboarding
- **Issue #5403** (closed): checkpoint: a closed issue's .loom-checkpoint persists indefinitely in the primary checkout and still returns a recovery_path
- **Issue #5394** (closed): install.sh checks for pnpm but not that it can run — corepack floats to a pnpm that needs Node 22+
- **PR #5412**: fix(install): probe that pnpm can actually run, not just that it exists
- **Issue #5402** (closed): docs: comment-body-literal-path.md link is broken in all 7 .loom/roles/ copies (resolves only in .claude/commands/loom/)
- **Issue #5337** (closed): Unreadable ingestKeyFile reports as state=disabled/endpoint=null — a config error is indistinguishable from telemetry never being turned on
- **PR #5349**: fix(observability): add a Misconfigured export state distinct from Disabled
- **Issue #5352** (closed): Fleet dashboard: surface per-host watchdog/protection state
- **PR #5366**: feat(dashboard): surface per-host watchdog/protection state fleet-wide
- **Issue #5357** (closed): telemetry(sweep.outcome): capture work output (tokens, lines changed) per sweep
- **Issue #5340** (closed): daemon drain can never converge on a busy host, and --force-after-timeout is unavailable on the stale binaries that need it
- **PR #5362**: fix(daemon): gate explicit dispatch_sweep on active drain, make timeout refusal actionable
- **Issue #5356** (closed): telemetry(host.health): add worktree_root_total_gb so disk can be shown as a percentage
- **PR #5373**: feat(telemetry): add worktree_root_total_gb to host.health for disk-percentage rendering
- **PR #5370**: fix(guard): fully quote-strip cd argument classification for write-confinement
- **Issue #5341** (closed): loom-daemon --version reports the on-disk binary, so a stale running daemon misreports itself as current
- **PR #5368**: fix(daemon): report running daemon's build over IPC, not just the on-disk binary
- **Issue #5336** (closed): Provisioning copies a host-specific absolute ingestKeyFile path across hosts — macOS path landed on a Linux worker, telemetry silently off for a day
- **PR #5354**: fix(observability): resolve ingestKeyFile per-host instead of committing a foreign path
- **Issue #5342** (closed): PrSet sweep dispatch was never implemented and epic #3449 closed — the changes-requested backlog is undispatchable by the daemon
- **PR #5367**: feat(daemon): implement PrSet sweep dispatch (#5342)
- **Issue #5345** (closed): Delegated daemon administration (daemon.delegatedTo) — gate workspace/tokens admin CLI commands
- **PR #5359**: feat(daemon): gate workspace/tokens admin CLI commands behind daemon.delegatedTo
- **Issue #5343** (closed): loom-worker-2 runs armed but with no watchdog timer provisioned — nothing detects a daemon death
- **PR #5365**: fix(daemon): self-heal missing watchdog provisioning + fix fleet add-worker root cause
- **Issue #5355** (closed): Dashboard: per-host CPU/disk/throughput trend charts on the host detail view
- **PR #5364**: feat(dashboard): add per-host CPU/disk/throughput trend charts to host detail
- **Issue #5328** (closed): guard-loom-workflow: commit-message heredoc masked for 'git commit -m $(cat <<EOF)' but not 'git commit -F - <<EOF' — blocks commits quoting the redirect phrase
- **PR #5333**: fix(guard): mask git commit -F -/--file=- heredoc bodies in gh-pr-merge-redirect check
- **Issue #5351** (closed): guard confinement tier masks interpreter-fed heredoc bodies (extract_write_targets uses plain mask_heredoc_bodies)
- **PR #5361**: fix(guard): interpreter-aware heredoc masking in write-confinement tier (#5351)
- **Issue #5344** (closed): loom-daemon-start.sh silently narrows the systemd unit, dropping operator env keys on an unattended re-render
- **PR #5360**: fix(daemon): preserve installed env keys on loom-daemon-start.sh re-render
- **Issue #5350** (closed): test-spawn-codex.sh fails 2/214 when run from inside a linked worktree (harness assumes $(pwd) is the workspace)
- **PR #5358**: fix(tests): pin LOOM_WORKSPACE in test-spawn-codex.sh so worktree runs agree with main-checkout runs

### 2026-08-05 — Historical gap notice

**Guide's Document Maintenance phase produced no `docs/guide-update-*` PR
between 2026-02-26 and 2026-08-05** (see #5413) — roughly 5.5 months and
~2,400 PRs / closed issues of drift, leaving the high-water mark below
stuck at PR #3028. Root cause: this repo's own `.loom/config.json` →
`autonomous.roleRunner.roles` is a strict allowlist (not an additive
default over `DEFAULT_ROLES`), and `guide` was never added to it — so the
Guide role never dispatched through any path (interval `roles` or
`onIdle`), independently diagnosed and fixed by #5392 / PR #5407 (merged
2026-08-05). This fully accounts for the observed silence — the "Document
Maintenance is last in a long shared prompt and gets budget-starved by
earlier phases" hypothesis floated on #5413 is not needed to explain it
and was not separately confirmed; a future recurrence with `guide`
confirmed dispatching (per `role_runner: enabled (...)` startup logging)
but Document Maintenance still not landing a PR would be the signal to
revisit that hypothesis.

**Catch-up strategy** (per #5413's own suggested options): reset the
high-water mark to current `main` rather than backfilling all ~2,400
entries individually — a literal per-PR list for a gap this size doesn't
match this phase's "append what's new since last tick" design and isn't a
good use of a single triage cycle's context budget. Below is a
representative snapshot of the most recent activity (not exhaustive);
the entries recorded here set the new high-water mark so the phase
resumes normal incremental operation on its next successful tick.

- **PR #5415**: feat(daemon): re-provision a missing watchdog onto an already-running host
- **PR #5414**: fix(install): mark merged .claude/settings.json as preserved, not diverged
- **PR #5410**: feat(watchdog): auto-recover a dead daemon under bounded retries + a circuit breaker
- **PR #5408**: fix(daemon): give a standalone loom-daemon a working init recovery path
- **PR #5407**: fix(config): add guide to roleRunner.roles allowlist
- **PR #5404**: docs(guard): brief dispatched agents about ambient LOOM_FORCE_SCOPE/LOOM_GUARD_DECISION_LOG env pollution
- **PR #5400**: fix(install): insert blank-line separator on CLAUDE.md/AGENTS.md marker reinstall
- **PR #5399**: fix(install): dedup + bound settings.json.loom-backup-* retention
- **PR #5398**: fix(install): self-heal + diagnose provision-daemon.sh shim install failures
- **PR #5383**: fix(observability): fix response-before-record race in MockSink test helper
- **PR #5380**: fix(guard): exclude single-angle '<' stdin redirects from tee/sed/cp/mv write-target scan
- **PR #5379**: docs(auditor): extract CI's nextest command instead of guessing cargo test
- **PR #5376**: fix(guard): apply strip_cd_quoting() to parse_force_ops/resolve_stash_cwd cd classification
- **PR #5375**: feat(telemetry): capture tokens_in/out and lines_added/deleted per sweep.outcome
- **PR #5374**: fix(champion): sticky-hold precheck's HOLD_BODY selection matches comments that merely quote the marker, not just genuine holds
- **Issue #5405** (closed): Nothing re-provisions the watchdog timer onto an already-running host: #5343's self-heal only fires when loom-daemon-start.sh is re-run
- **Issue #5396** (closed): Installer reports .claude/settings.json as 'unexpected divergence' when Repo Skills co-owns it
- **Issue #5392** (closed): autonomous.roleRunner.roles silently drops new DEFAULT_ROLES — auditor and guide never dispatch
- **Issue #5391** (closed): The watchdog detects daemon death and never recovers it — 252 divergences, 1h40m outage
- **Issue #5389** (closed): loom-daemon init has no defaults payload — the recovery path the dispatch warning names cannot run
- **Issue #5388** (closed): Sweep dispatcher exports LOOM_FORCE_SCOPE / LOOM_GUARD_DECISION_LOG into the agent environment, corrupting managed repos' guard suites
- **Issue #5387** (closed): install.sh writes a new timestamped settings.json.loom-backup-* on every run
- **Issue #5386** (closed): provision-daemon.sh warns on every install: cannot install loom-clean / loom-recover-orphans / loom-claim shims
- **Issue #5384** (closed): install.sh appends the orchestration marker block to CLAUDE.md with no separating newline
- **Issue #5382** (closed): Flaky test: observability::exporter::tests::kill_and_revive_round_trip_still_talks_to_the_same_exporter
- **Issue #5378** (closed): Auditor Capability Request: local test validation should use cargo-nextest to match CI, not plain cargo test
- **Issue #5372** (closed): guard: parse_force_ops()/resolve_stash_cwd() cd-tracking still uses naive unstripped ^/ classification
- **Issue #5371** (closed): fix(champion): sticky-hold precheck's HOLD_BODY selection matches comments that merely quote the marker, not just genuine holds
- **Issue #5369** (closed): fix(guard): single-angle '<' stdin redirect is still scanned as a write target (cp/mv false-ALLOW confinement escape)
- **Issue #5363** (closed): guard: partially-quoted absolute cd argument still misclassified as relative (residual #4933/#4926 shape)

### 2026-02-23

- **PR #3028**: build(deps): bump the all-dependencies group with 3 updates
- **PR #3027**: build(deps-dev): bump the dev-dependencies group with 4 updates
- **PR #3021**: fix: skip worktree-escape dirty check when repo root is not on default branch
- **PR #3020**: fix: time-bound debug log checks in startup monitor to prevent stale-session misclassification
- **PR #3019**: fix: exempt force-mode failures from systematic failure detection
- **PR #3018**: fix: classify worktree branch-conflict as infrastructure failure
- **PR #3017**: fix: validate and reinitialize corrupted agent config dir before each spawn
- **PR #3016**: fix: detect and auto-recover when feature branch is checked out in main worktree
- **PR #3015**: fix: classify builder failures by log inspection before falling through to unknown
- **PR #3014**: fix: tag rate-limit pre-check PhaseResult with api_rate_limited flag
- **PR #3013**: fix: validate get_pr_for_issue body search via closingIssuesReferences
- **PR #3012**: fix: add repo_root and main-branch guards to _commit_prior_uncommitted_work
- **PR #3011**: fix: classify thinking stall exhaustion as builder_thinking_stall error class
- **PR #3010**: perf: run orphan recovery in background thread during daemon startup
- **PR #3008**: fix: skip body search in get_pr_for_issue when state='merged'
- **PR #3007**: fix: queue spawn_shepherd signals when all shepherd slots are full
- **PR #3006**: fix: re-queue spawn_shepherd signals dropped during daemon startup
- **PR #3005**: fix: use git diff origin/main instead of file existence in dirty-main recovery
- **PR #3004**: fix: exempt newly-spawned shepherds from tmux-session-missing stall check
- **PR #3003**: fix: scope systematic failure escalation to per-issue failure count
- **PR #3001**: fix: skip stale branch cleanup in force mode when PR awaiting review
- **PR #3000**: feat: write full Loom guide to .loom/CLAUDE.md, inject short pointer into root CLAUDE.md
- **PR #2999**: fix: clear stale checkpoint before thinking stall retry
- **PR #2998**: fix: fall back to GitHub mergeable check when rebase phase has no worktree
- **PR #2997**: test: add unit test for api_propagation_race in builder validation loop
- **PR #2996**: test: fix rev-parse mock handling and add branch guard tests for builder commit methods
- **PR #2995**: fix: skip recovery PR when builder commits only .no-changes-needed marker
- **PR #2994**: test: add TestFastMode class covering render_agents_table and output_fast
- **PR #2993**: fix: replace head -n -1 with sed $d in daemon script --help handlers
- **Issue #2971** (closed): bug: false 'builder escaped worktree' warning when working branch has staged changes
- **Issue #2973** (closed): perf: orphan recovery at daemon startup is sequential and slow — blocks signal processing for minutes
- **Issue #2970** (closed): bug: spawn signal silently dropped when all shepherd slots full — should persist and retry
- **Issue #2968** (closed): bug: spawn_shepherd signal dropped during daemon startup (no daemon state loaded)
- **Issue #2969** (closed): bug: STALL-L2 race condition kills shepherds that just started (23s lifetime)
- **Issue #2992** (closed): Install Loom context into .loom/CLAUDE.md instead of appending to root
- **Issue #2991** (closed): Add tests for branch guard in _commit_prior_uncommitted_work and _commit_interrupted_work
- **Issue #2990** (closed): Add unit tests for loom-status --fast mode (render_agents_table, output_fast)
- **Issue #2989** (closed): validate_phase creates recovery PR when builder only commits .no-changes-needed marker
- **Issue #2987** (closed): Add unit test for API propagation race fix in builder validation loop
- **Issue #2972** (closed): bug: duplicate PRs created when shepherd fails validation after builder succeeds

### 2026-02-22

- **PR #3022**: fix: add keyword-search fallback to prevent duplicate PRs in direct completion
- **PR #2985**: feat: add tests for pr-body.md happy path in builder and validate_phase
- **PR #2984**: feat: bug: thinking-stall retry budget (1 retry) too small for systematic stalls — no escalation path
- **PR #2983**: docs(judge): add MCP failure detection and environment health check
- **PR #2982**: docs: document --permission-mode bypassPermissions silently disabling hooks
- **PR #2980**: fix: block cd-to-main-repo worktree escapes in guard-destructive hook
- **PR #2979**: fix: return SUCCESS when PR with loom:review-requested exists during validation loop
- **PR #2975**: feat: add --fast mode to loom-status for rich agent table display
- **PR #3002**: test: add coverage for thinking-stall retry hint injection and escalating backoff
- **PR #2988**: feat: bug: builder checkpoint commits land on local main branch
- **Issue #2963** (closed): Bug: --help flag fails on macOS in start-daemon.sh and stop-daemon.sh (head -n -1 not supported by BSD head)

### 2026-02-19

- **PR #2962**: refactor: shepherd skill becomes signal-writer + observer
- **PR #2960**: fix: filter spinner artifacts from thinking stall diagnostic snippets
- **PR #2959**: test: add unit tests for pr-body.md pre-written body paths in builder and validate_phase
- **PR #2957**: feat: add tests for builder_thinking_stall_timeout config and parameter threading
- **PR #2956**: feat: add CommandPoller IPC and signal-writer /loom skill
- **PR #2953**: feat: detect prior committed checkpoint and skip builder invocation
- **PR #2948**: fix: verify PR state before skipping builder to guard against GitHub API eventual consistency
- **PR #2941**: fix: tee shepherd output to log file for reliable capture with 2>&1 redirect
- **PR #2938**: fix: update dirty-main warning to show --force/--merge instead of just --merge
- **PR #2934**: fix: stop /judge after one PR when invoked manually
- **PR #2904**: fix: recover builder phase when PR exists after thinking stall (exit 11/13/14)
- **PR #2903**: feat: increase builder thinking stall timeout to 360s for complex tasks
- **PR #2902**: fix: prevent false-positive thinking stall detection and symlink destruction
- **PR #2899**: fix: clarify systematic failure comment reflects global detection not per-issue count
- **PR #2891**: feat: shepherd comments on GitHub issue when abandoning due to non-retryable failure
- **PR #2889**: fix: use pre-written PR body from builder to avoid boilerplate descriptions
- **PR #2888**: feat: add recovery guidance to dirty-main warning
- **Issue #2892** (closed): bug: builder 100% thinking-stall rate on issue #2811
- **Issue #2927** (closed): friction: shepherd output invisible when captured via Claude Code Bash tool
- **Issue #2923** (closed): friction: shepherd should detect prior committed checkpoint and offer resume
- **Issue #2950** (closed): refactor: shepherd skill becomes signal-writer + observer
- **Issue #2951** (closed): refactor: extract standalone loom daemon process
- **Issue #2908** (closed): bug: builder-skip check accepts closed PRs after stale branch cleanup
- **Issue #2925** (closed): friction: shepherd stdout/stderr invisible when invoked via shell 2>&1 redirect
- **Issue #2931** (closed): Add unit tests for pre-written PR body (.loom/pr-body.md) reading
- **Issue #2929** (closed): Add unit tests for .loom/pr-body.md pre-written body logic
- **Issue #2922** (closed): bug: thinking stall snippet contains garbled/corrupted characters
- **Issue #2932** (closed): Add unit tests for pr-body.md pre-written body paths
- **Issue #2933** (closed): Add tests for builder_thinking_stall_timeout config and parameter threading
- **Issue #2913** (closed): enhancement: detect near-limit API usage warnings (95-99%)
- **Issue #2901** (closed): shepherd: --force alias shows 'implied by --merge' in dirty-main warning
- **Issue #2837** (closed): Dirty-main warning gives no recovery guidance for orphaned builder work
- **Issue #2851** (closed): shepherd: thinking stall (exit 14) should recover if PR already exists
- **Issue #2853** (closed): Builder thinking stall threshold (180s) too aggressive for complex tasks
- **Issue #2835** (closed): bug: MCP pre-flight smoke test passes but in-session MCP fails at runtime
- **Issue #2858** (closed): systematic failure comment incorrectly says 'this issue' failed N times
- **Issue #2839** (closed): feat: shepherd should comment on GitHub issue when abandoning
- **Issue #2811** (closed): bug: builder never creates its own PR (100% recovery rate)
- **Issue #2898** (closed): loom-shepherd wrapper produces no output when invoked via Claude Code Bash tool

### 2026-02-18

- **PR #2900**: fix: distinguish --allow-dirty-main specified vs implied by --merge in warning
- **PR #2871**: fix: skip systematic failure counter when PR already exists for issue
- **PR #2881**: fix: detect 100% weekly rate limit and classify as rate_limit_abort
- **PR #2875**: fix: retry thinking stalls once before classifying as permanent failure
- **PR #2879**: fix: use python3 instead of bare python in judge pytest commands
- **PR #2877**: fix: surface git exit-128 failures in judge diagnostics and use gh pr diff
- **PR #2876**: feat: surface builder thinking content in thinking stall errors
- **PR #2872**: fix: checkpoint stale worktree before builder retry
- **PR #2880**: feat: thinking stall post-mortem detection lacks minimum duration gate
- **PR #2867**: feat: log prior failure count at shepherd start for observability
- **PR #2864**: fix: suppress 'Killed: 9 sleep' messages from output monitor cleanup
- **PR #2869**: feat: clear issue-specific failures at force-mode shepherd startup
- **PR #2868**: fix: skip Doctor for pre-existing test failures in unmodified files
- **PR #2865**: fix: add pre-claim closed-issue check in shepherd main()
- **PR #2863**: docs: add worktree-aware checkout to judge prompt
- **PR #2862**: fix: route shepherd output through stdout to eliminate Bash tool duplication
- **PR #2861**: feat: skip test verification when builder PR already has loom:review-requested
- **PR #2857**: fix: delete own progress file on shepherd exit
- **PR #2847**: fix: write periodic shepherd heartbeats during worker polling
- **PR #2846**: feat: pass structured judge feedback context to doctor phase
- **PR #2845**: fix: set LC_ALL=C on tr invocations that process TUI output
- **PR #2843**: docs: note intentional policy of not removing labels on merge/close
- **PR #2825**: fix: classify small-log short-duration sessions as ghost
- **PR #2820**: fix: ensure shepherd logging is visible when invoked non-interactively
- **PR #2819**: fix: check MCP failure before thinking stall to prevent non-retryable misclassification
- **PR #2821**: fix: apply TUI noise filtering in real-time pipe-pane stream mode
- **PR #2818**: feat: builder zero-output failures should surface post-mortem in validation error message
- **PR #2817**: feat: builder scope creep: uncommitted work leaks to main worktree
- **PR #2816**: docs: add double-prefix anti-pattern and issue title prefix mapping to builder guides
- **PR #2815**: fix: strip global MCP plugins from agent config to prevent ghost sessions
- **PR #2792**: fix: improve recovery PR quality when builder exits without completing git workflow
- **PR #2787**: feat: add RATE_LIMIT_ABORT exit code for CLI usage limits
- **PR #2795**: feat: detect extended thinking without tool calls as degraded session
- **PR #2788**: fix: eliminate polling delay and reduce log noise in startup monitor
- **PR #2789**: feat: MCP status bar noise misclassifies builder failures as MCP failures
- **PR #2780**: feat: add post-mortem diagnostics for zero-output CLI sessions
- **PR #2786**: fix: distinguish rate-limited builder exit from unknown recovery path
- **PR #2770**: fix: shepherd stderr output lost when invoked non-interactively
- **PR #2760**: feat: add /epic skill for interactive epic creation
- **PR #2761**: fix: preserve builder exit code via sidecar file when agent-wait returns 0
- **PR #2759**: docs: add comprehensive /loom help command with sub-topic navigation
- **PR #2758**: feat: retry builder once on worktree escape with main cleanup
- **PR #2755**: fix: add diagnostic logging and MCP_PREFLIGHT_FAILED sentinel to wrapper pre-flight
- **PR #2754**: docs: add test failure diagnostic patterns to Doctor role instructions
- **PR #2753**: feat: add WORKTREE_ESCAPE exit code to prevent cross-issue escalation
- **PR #2750**: fix: detect and remove wrong-issue Closes keywords in PR body validation
- **PR #2747**: docs: add scope discipline sections to builder and doctor roles
- **PR #2749**: fix: completion phase uses canonical branch name, not diag branch
- **PR #2748**: fix: wrapper exits with code 7 instead of 1 when MCP failures exhaust retries
- **PR #2745**: feat: rebase onto main before Doctor when failing tests are in unmodified files
- **PR #2740**: fix: remove shepherd reflection phase entirely
- **PR #2741**: fix: skip baseline tests in builder validation when builder produced zero artifacts
- **PR #2739**: fix: increase stuck_max_retries default from 1 to 2
- **PR #2726**: fix: startup monitor tolerates global plugin failures when project MCPs connected
- **Issue #2827** (closed): Shepherd warning shows 'allow-dirty-main specified' when implied by --merge
- **Issue #2854** (closed): shepherd: systematic failure counter increments even when builder created a PR
- **Issue #2859** (closed): bug: builder does not detect or report Claude weekly rate limit exhaustion
- **Issue #2823** (closed): bug: thinking stall classified as non-retryable, blocks on first occurrence
- **Issue #2832** (closed): bug: judge uses 'python' instead of 'python3'
- **Issue #2828** (closed): bug: judge encounters git exit status 128 during PR review
- **Issue #2855** (closed): Surface builder thinking content in thinking stall error message
- **Issue #2849** (closed): Builder: clean or checkpoint worktree before retry
- **Issue #2833** (closed): bug: thinking stall post-mortem detection lacks minimum duration gate
- **Issue #2824** (closed): feat: log prior failure count at shepherd start for observability
- **Issue #2834** (closed): bug: output monitor 'Killed: 9 sleep 5' printed to agent logs
- **Issue #2822** (closed): bug: force/merge mode shepherd doesn't reset prior failure count
- **Issue #2809** (closed): bug: pre-existing test failures on main cause unnecessary Doctor intervention
- **Issue #2830** (closed): Shepherd claims issue before checking if it is already closed
- **Issue #2829** (closed): bug: judge fails gh pr checkout when builder worktree still exists
- **Issue #2840** (closed): bug: loom-shepherd.sh output appears duplicated when stderr is redirected
- **Issue #2813** (closed): bug: stale shepherd progress files never cleaned up for manual runs
- **Issue #2810** (closed): Orphan recovery script interferes with active shepherd sessions
- **Issue #2848** (closed): Builder: recover uncommitted work from worktree when thinking stall occurs
- **Issue #2831** (closed): Shepherd fallback comment reports 'unexpected failure' for known failure types
- **Issue #2812** (closed): Add max-doctor-cycles limit before automatic builder restart
- **Issue #2856** (closed): merge mode (-m) should clear systematic failure state for the target issue
- **Issue #2852** (closed): Merge mode (-m) override should reset systematic failure counter
- **Issue #2860** (closed): thinking stall should allow 1 retry before marking as non-retryable failure
- **Issue #2850** (closed): Retry builder thinking stall at least once before classifying as failure
- **Issue #2826** (closed): Builder thinking stall should retry once before failing
- **Issue #2838** (closed): bug: merge-pr.sh does not remove loom:pr label after successful merge
- **Issue #2814** (closed): friction: MCP global plugin failures add ~5-8s latency per shepherd phase
- **Issue #2794** (closed): bug: shepherd output invisible when invoked from Claude Code Bash tool
- **Issue #2791** (closed): bug: rate limit 'Stop and wait' interstitial not detected as degraded session
- **Issue #2790** (closed): Investigate MCP server startup failure adding ~5s latency per agent phase
- **Issue #2785** (closed): fix: agent logs are ~90% terminal rendering noise
- **Issue #2778** (closed): Systematic failure detector should exempt infrastructure failures
- **Issue #2779** (closed): Fallback handler classifies MCP failures as builder_unknown_failure
- **Issue #2777** (closed): MCP failure detection gated on non-zero exit code misses CLI-exits-0 case
- **Issue #2771** (closed): Clean up stale shepherd progress files from manual runs
- **Issue #2769** (closed): Clippy warnings on main branch

### 2026-02-15

- **PR #2293**: feat: wire run_warnings and add failure-path reflection coverage
- **PR #2285**: feat: per-agent CLAUDE_CONFIG_DIR isolation for concurrent session stability
- **PR #2278**: fix: stop gitignoring .loom/config.json so it stays tracked across reinstalls
- **PR #2277**: docs: add build verification guidance to builder role definitions
- **PR #2275**: feat: shepherd post-run reflection phase
- **PR #2274**: fix: mark issues as blocked instead of closing when builder finds no changes
- **PR #2273**: feat: kill orphaned claude processes during terminal/session lifecycle
- **PR #2272**: Add installer integration test suite
- **PR #2266**: fix: use remove_dir_all for robust worktree directory cleanup
- **PR #2265**: fix: filter build artifacts from builder diagnostics
- **PR #2263**: fix: install hooks and CLI wrapper in quick install mode
- **PR #2259**: docs: Guide triage cycle — update WORK_LOG and WORK_PLAN
- **PR #2256**: fix: prevent app crash when daemon is unavailable on launch
- **PR #2255**: feat: add dual-mode GitHub API layer with REST fallback
- **PR #2254**: docs: update WORK_LOG and WORK_PLAN for installer bug triage
- **PR #2252**: fix: ensure main working tree is clean after loom install
- **PR #2251**: fix: prevent unsafe worktree removal during merge and agent destroy
- **Issue #2276** (closed): Reflection phase: wire run_warnings and add failure-path coverage
- **Issue #2271** (closed): Shepherd: post-run self-reflection and upstream issue creation
- **Issue #2269** (closed): Builder should never close issues — use loom:blocked instead
- **Issue #2268** (closed): Feature: Kill orphaned claude processes during terminal/session lifecycle
- **Issue #2267** (closed): PR #2266 may not fix the actual uninstall failure described in #2246
- **Issue #2261** (closed): Add installer integration test suite for install/reinstall/uninstall paths
- **Issue #2260** (closed): Remove unused analytics pipeline (~5500 LOC)
- **Issue #2258** (closed): Builder generates non-compilable Rust code (holds MutexGuard across .await)
- **Issue #2257** (closed): Builder gets stuck at planning checkpoint without progressing
- **Issue #2253** (closed): App crashes on launch when daemon is unavailable
- **Issue #2249** (closed): Fresh install doesn't include loom CLI script or hooks directory
- **Issue #2248** (closed): Fresh install gitignores .loom/config.json but previous install tracked it
- **Issue #2246** (closed): Reinstall uninstall step fails on non-empty worktree directories
- **Issue #2245** (closed): Loom reinstall leaves working tree in broken state with uncommitted deletions
- **Issue #2243** (closed): Worktree cleanup breaks shell when CWD is inside deleted worktree

### 2026-02-13

- **PR #2242**: chore: bump version to v0.2.2
- **PR #2241**: fix: strip CLAUDECODE env var to prevent nested session guard
- **PR #2239**: Add uv sync to shepherd worktree dependency setup
- **Issue #2240** (closed): Shepherd fails: Claude Code v2.1.39 nested session guard blocks subprocess spawning
- **Issue #2238** (closed): Shepherd worktree missing Python venv — uv sync not run

### 2026-02-12

- **PR #2237**: feat: reject epic/tracking issues before builder phase
- **PR #2235**: fix: use same test command for baseline comparison in scoped tests
- **Issue #2236** (closed): Shepherd should reject epic/tracking issues before builder phase
- **Issue #2234** (closed): Scoped test baseline uses wrong command (auto-detects instead of using scoped command)

### 2026-02-11

- **PR #2233**: docs: rewrite contributing guide for AI-developed project
- **PR #2232**: test: improve test coverage for src-tauri Rust backend
- **PR #2231**: fix: resolve pipe-pane log capture issues with trailing CR and buffering
- **PR #2229**: feat: add unit tests for loom-daemon core modules
- **PR #2228**: fix: push ghloc badge to separate branch to avoid ruleset conflict
- **PR #2224**: chore: add ghloc lines-of-code badge to README
- **PR #2223**: chore: bump version to 0.2.1 and fix release workflow
- **PR #2222**: feat: improve /imagine bootstrapper with planning artifacts and starter issues
- **PR #2221**: docs: Guide document maintenance update
- **PR #2220**: fix: update time crate to 0.3.47 to resolve RUSTSEC-2026-0009
- **Issue #2230** (closed): Fix pipe-pane log capture: buffering + trailing CR bugs drop all output
- **Issue #2227** (closed): Improve test coverage for TypeScript frontend modules
- **Issue #2226** (closed): Improve test coverage for src-tauri Rust backend
- **Issue #2225** (closed): Improve test coverage for loom-daemon core modules

### 2026-02-10

- **PR #2219**: feat: extend guard-destructive hook with system and infrastructure patterns
- **PR #2218**: feat: detect and surface human-input-needed blockers in daemon
- **PR #2217**: fix: remove dead A/B testing module (1,341 LOC)
- **PR #2215**: fix: copy guard-destructive.sh hook to target repos during install
- **PR #2214**: feat: two-tier startup detection and diagnostic capture for stalled shepherds
- **PR #2213**: docs: Guide document maintenance update
- **PR #2212**: feat: two-tier heartbeat grace period for faster stale shepherd detection
- **PR #2211**: feat: capture terminal scrollback before killing stuck sessions
- **PR #2210**: feat: classify budget-exhausted shepherds and trigger architect decomposition
- **PR #2209**: feat: add -t/--timeout-min flag for time-bounded daemon runs
- **PR #2208**: fix: remove incorrect 100x scaling of usage API utilization
- **PR #2206**: feat: replace SQLite usage checking with direct Anthropic OAuth API
- **PR #2195**: Issue #2194: write loom-source-path to target repo root
- **PR #2193**: feat: detect editable pip installs before worktree cleanup
- **PR #2191**: Fix label sync script logic
- **PR #2190**: Bump the all-dependencies group with 6 updates
- **PR #2189**: Bump the production-dependencies group with 2 updates
- **PR #2188**: Bump the dev-dependencies group with 4 updates
- **Issue #2216** (closed): Add prompt hook to prevent agents from restarting servers/infrastructure
- **Issue #2207** (closed): Add -t/--timeout-min flag to /loom for time-bounded daemon runs
- **Issue #2205** (closed): Daemon should report when stalled waiting on human input
- **Issue #2203** (closed): Champion should be able to promote or close any open issue
- **Issue #2202** (closed): Remove orphaned ab_testing.rs backend (1,341 LOC dead code)
- **Issue #2201** (closed): Daemon needs strategy for issues that exceed single-session context budget
- **Issue #2200** (closed): Installer doesn't copy guard-destructive.sh hook to target repo
- **Issue #2199** (closed): Daemon should capture shepherd output on kill for post-mortem debugging
- **Issue #2198** (closed): Shepherd spawns without writing progress file -- silent failure mode
- **Issue #2197** (closed): Stale heartbeat detection too slow -- 8+ minutes to reclaim stuck shepherd
- **Issue #2196** (closed): Daemon should auto-resolve contradictory labels
- **Issue #2194** (closed): Installation does not create .loom/loom-source-path file
- **Issue #2192** (closed): loom-daemon breaks after worktree cleanup: editable install points to deleted path

### 2026-02-06

- **PR #2187**: Add tmux liveness detection for support role completion
- **PR #2186**: Add idempotency checks to Loom installation pipeline
- **PR #2185**: Add retry_blocked_issues action handler for blocked issue recovery
- **PR #2184**: Increase shepherd no-progress grace period from 300s to 600s
- **PR #2183**: Revert shepherd issue labels during daemon graceful shutdown
- **PR #2182**: Terminate child tmux sessions during daemon graceful shutdown
- **PR #2175**: Add spinning issue detection: auto-escalate after N review cycles
- **PR #2174**: Add .loom/issue-failures.json to .gitignore
- **PR #2172**: Add persistent cross-session failure tracking for daemon issues
- **PR #2171**: Detect shepherds stuck without progress files
- **PR #2167**: Fix judge/shepherd review workflow mismatch causing systematic judge_exhausted
- **PR #2166**: Fix PreToolUse hook error infinite retry loop
- **PR #2165**: Prevent .loom runtime files from triggering dirty-repo check
- **PR #2164**: Add heartbeat grace period for newly spawned shepherds
- **PR #2163**: Add contradictory label detection and exclusion group enforcement
- **PR #2162**: Extend pipe-pane sed filter to strip CR, BS, and bare escapes
- **PR #2155**: Downgrade ci_failing to info-level to prevent spurious stall escalation
- **PR #2154**: Clear systematic failure state on L3 pool restart
- **PR #2153**: Reset stall counter after L3 pool restart
- **PR #2149**: Guard against missing .loom/scripts when branch predates Loom install
- **PR #2148**: Add escalating stall recovery to daemon iteration loop
- **PR #2146**: Dispatch targeted doctor/judge agents for orphaned PRs
- **PR #2145**: Fix stale shepherd count in iteration summary and spawning decisions
- **PR #2141**: Add create_pr to direct completion mechanical steps
- **PR #2140**: Handle exit code 6 (instant-exit) in judge phase
- **PR #2137**: Fix shepherd Judge phase silent failure with 0s session duration
- **PR #2136**: Add test suite for log_filter module
- **PR #2133**: Filter .loom/ runtime files from shepherd dirty-repo check
- **PR #2132**: Replace sed ANSI stripping with Python log filter for cleaner agent logs
- **PR #2131**: Fix daemon runtime files missing from .gitignore template
- **Issue #2181** (closed): Support roles have no completion mechanism and run indefinitely
- **Issue #2180** (closed): Loom installation creates redundant 'Install Loom' PRs on every reinstall
- **Issue #2179** (closed): Blocked issues are retried endlessly without escalation or backoff
- **Issue #2178** (closed): Shepherds frequently stall with no progress file within grace period
- **Issue #2177** (closed): Daemon shutdown does not revert GitHub labels on in-progress issues
- **Issue #2176** (closed): Daemon shutdown does not kill child tmux sessions
- **Issue #2173** (closed): Add .loom/issue-failures.json to .gitignore
- **Issue #2170** (closed): Spinning issue detection: auto-escalate after N shepherd cycles
- **Issue #2169** (closed): Cross-session failure tracking and exponential backoff
- **Issue #2168** (closed): Stuck shepherd detection: heartbeat-based liveness checks
- **Issue #2161** (closed): Contradictory label state allowed on same PR
- **Issue #2160** (closed): Stale heartbeat detection too aggressive
- **Issue #2159** (closed): Repeated PreToolUse hook errors block shepherd progress
- **Issue #2158** (closed): Shepherds error on uncommitted .loom/ files in main repo
- **Issue #2157** (closed): Judge/Shepherd review workflow mismatch causes systematic judge_exhausted
- **Issue #2156** (closed): Logs unreadable: pipe-pane ANSI stripping missing or broken
- **Issue #2152** (closed): Systematic failure suppresses spawning even after L3 pool restart
- **Issue #2151** (closed): L3 pool restart does not reset stall counter
- **Issue #2150** (closed): ci_failing warning creates unrecoverable stall loop
- **Issue #2147** (closed): Shepherd judge phase crashes when branch predates Loom installation
- **Issue #2144** (closed): Daemon 'stalled' health status persists without corrective action
- **Issue #2143** (closed): Daemon reports stale shepherd count due to timing gap
- **Issue #2142** (closed): Daemon should dispatch targeted agents for orphaned PRs
- **Issue #2139** (closed): Judge CLI sessions exit immediately (0s duration)
- **Issue #2138** (closed): Builder completion phase fails to create PR
- **Issue #2135** (closed): Shepherd Judge phase fails silently with 0s session duration
- **Issue #2134** (closed): Add unit tests for loom_tools.log_filter module
- **Issue #2130** (closed): Shepherd tmux logs are raw terminal output
- **Issue #2129** (closed): Shepherd dirty-repo check too strict for .loom/ runtime files
- **Issue #2128** (closed): Daemon runtime files not included in .gitignore template

### 2026-02-05

- **PR #2127**: Fix --clean install leaving staged deletions in main
- **PR #2125**: Fix security audit failures: update bytes and MCP SDK
- **PR #2124**: Fix E2E terminal-management tests with proper mock terminal data
- **PR #2123**: Fix default mode not auto-approving past approval gate
- **PR #2122**: Add transient API error recovery for autonomous agents
- **PR #2121**: Add test failure analysis tooling for shepherd block rate investigation
- **PR #2120**: Add PreToolUse hook to block destructive agent commands
- **PR #2112**: Fix daemon self-sabotage: gitignore runtime files and fix empty args
- **Issue #2126** (closed): Clean reinstall leaves target repo in dirty state
- **Issue #2119** (closed): Implement PreToolUse hooks to block destructive agent commands
- **Issue #2118** (closed): Implement API error recovery for shepherd/daemon orchestration
- **Issue #2116** (closed): Shepherd: default mode should auto-promote past approval gate
- **Issue #2109** (closed): Daemon runtime files missing from .gitignore
- **Issue #2105** (closed): E2E terminal-management tests failing on main
- **Issue #2100** (closed): Investigate shepherd test failure patterns (20% block rate)

### 2026-02-04

- **PR #2117**: Add scoped test execution to Judge role
- **Issue #2114** (closed): Judge: scope test execution to changed files

### 2026-02-03

- **PR #2115**: Add approval phase timeout and heartbeat reporting
- **PR #2113**: Fix daemon-shepherd approval deadlock
- **PR #2108**: Add Python daemon implementation (daemon_v2) for deterministic orchestration
- **PR #2107**: Fix CI: workflow YAML syntax and E2E test mocks
- **PR #2106**: Fix scoped test detection for nested pyproject.toml
- **PR #2104**: Shepherd: Structured builder checkpoints to detect partial progress
- **PR #2103**: Add WIP commit preservation when builder exits with uncommitted changes
- **PR #2102**: Remove dead multi-terminal and prediction code (Phase 7)
- **Issue #2111** (closed): Approval phase has no timeout or heartbeat reporting
- **Issue #2110** (closed): Shepherd approval gate deadlocks daemon-spawned shepherds
- **Issue #2099** (closed): Scoped test detection misses nested pyproject.toml
- **Issue #2056** (closed): Shepherd: Structured builder checkpoints

### 2026-02-02

- **PR #2101**: Add stale worktree recovery to builder phase
- **PR #2098**: Extend name-based test comparison to line-based fallback path
- **PR #2097**: Add graceful 'no changes needed' pathway to shepherd
- **PR #2094**: Add output-based test ecosystem detection for umbrella commands
- **PR #2093**: Add scoped test verification to builder phase
- **PR #2092**: Add CI-aware validation to doctor phase
- **PR #2091**: Add diagnostics to 'no PR created' shepherd failure
- **PR #2090**: Update shepherd CLI tests to use ShepherdExitCode enum values
- **PR #2089**: Prefer loom-tools source over installed CLI in development
- **PR #2087**: Fix race condition in judge phase fallback approval
- **PR #2086**: Prevent curator from closing issues during curation
- **PR #2081**: Document judge_retry and phase_completed milestone events
- **PR #2080**: Phase 6: Rewrite main.ts for single-session analytics-first model
- **PR #2079**: Distinguish doctor failure modes for better label state recovery
- **PR #2078**: Improve builder completion retry prompting with diagnostic context
- **PR #2077**: Add granular exit codes for shepherd partial success states
- **PR #2076**: Fix PR creation gap after doctor test-fix loop
- **PR #2075**: Bump clap from 4.5.54 to 4.5.56
- **PR #2074**: Bump the dev-dependencies group with 2 updates
- **Issue #2096** (closed): Shepherd: Handle 'no changes needed' gracefully
- **Issue #2095** (closed): Builder: Verify problem exists before attempting fix
- **Issue #2088** (closed): Fix shepherd CLI tests for granular exit codes
- **Issue #2085** (closed): Detect stale loom-tools installation
- **Issue #2084** (closed): Curator agent should not close issues
- **Issue #2083** (closed): Shepherd fallback label application has race condition
- **Issue #2082** (closed): Doctor validation fails when CI is still pending
- **Issue #2068** (closed): Distinguish doctor failure modes
- **Issue #2067** (closed): Increase builder completion retry limit
- **Issue #2066** (closed): Extend name-based test comparison to line-based fallback
- **Issue #2065** (closed): Add diagnostics to 'no PR created' shepherd failure
- **Issue #2064** (closed): Fix PR creation gap after doctor test-fix loop
- **Issue #2045** (closed): Shepherd: Granular exit codes for partial success states
- **Issue #2044** (closed): Shepherd: Scoped test execution based on changed files
- **Issue #2031** (closed): Fix unused variable warning in loom-daemon scaffolding.rs

### 2026-02-01

- **PR #2073**: Judge: rename review terminology to judge/evaluate
- **PR #2071**: Add supplemental Python test verification when pipeline short-circuits
- **PR #2070**: Add post-worktree hook to pre-build loom-daemon binary
- **PR #2069**: Document repository-scoped GitHub token setup
- **PR #2063**: Improve builder completion phase with targeted retry
- **PR #2062**: DRY up pipe-pane log capture: strip ANSI escape sequences
- **PR #2061**: Add file-based analytics dashboard (Phase 5)
- **PR #2057**: Add shepherd pre-flight baseline health check
- **PR #2054**: Add name-based test comparison to reduce false positive regressions
- **PR #2053**: Enable Doctor to fix builder test failures
- **PR #2052**: Add atomic label state transitions
- **PR #2051**: Fix test path for relocated loom CLI wrapper
- **PR #2050**: Fix completion phase shell command parsing broken by newlines
- **PR #2042**: Document worktree deletion dangers
- **PR #2041**: Symlink node_modules from main workspace to worktrees
- **PR #2040**: Add builder completion retry phase for incomplete work
- **PR #2039**: Increase shepherd timeouts to prevent premature agent termination
- **PR #2038**: Implement input logging layer for terminal analytics
- **PR #2037**: Strip ANSI escape sequences from builder logs
- **PR #2035**: Remove auto-recovery from shepherd, add phase contracts
- **PR #2034**: Remove remaining AGENTS.md references
- **PR #2033**: Issue #1978: Auto-recovered PR
- **PR #2032**: Issue #2025: Auto-recovered PR
- **PR #2030**: Issue #1956: Auto-recovered PR
- **PR #2029**: Issue #1979: Auto-recovered PR
- **PR #1912**: Add diagnostic output on judge validation failure
- **Issue #2072** (closed): Judge: rename 'review' terminology to reduce API anchoring
- **Issue #2060** (closed): DRY up pipe-pane log capture
- **Issue #2058** (closed): Document repository-scoped GitHub token setup
- **Issue #2055** (closed): Shepherd: Improve builder completion phase
- **Issue #2049** (closed): Fix missing defaults/loom CLI wrapper
- **Issue #2048** (closed): Shepherd: Atomic label state transitions
- **Issue #2047** (closed): Shepherd: Pre-flight baseline health check
- **Issue #2046** (closed): Shepherd: Enable Doctor to fix test failures
- **Issue #2043** (closed): Shepherd: Compare specific test names
- **Issue #2036** (closed): Document: Agents must not delete worktrees directly
- **Issue #2026** (closed): Clean up remaining AGENTS.md references
- **Issue #2025** (closed): Move ./loom CLI wrapper into .loom/ folder
- **Issue #1908** (closed): Shepherd: judge worker silently fails without submitting review

### 2026-01-31

- **PR #1913**: Add force-mode fallback for changes-requested detection in judge phase
- **PR #1911**: Add judge retry mechanism in shepherd orchestrator
- **PR #1907**: Extract shared tmux session utilities to common/tmux_session.py
- **PR #1906**: Preserve worktree on builder test failure instead of cleaning up
- **PR #1905**: Extract stuck_detection.py formatting into dedicated stuck_formatting.py module
- **PR #1904**: Reduce CI minutes on pull requests
- **PR #1903**: Port detect-systematic-failure.sh and record-blocked-reason.sh to Python
- **PR #1892**: Add backwards-compatible clean.sh wrapper in defaults/scripts/
- **PR #1890**: Add Python loom-tools tests to CI via uv
- **PR #1889**: Issue #1884: Auto-recovered PR
- **PR #1877**: Add --clean flag to uninstall for clean reinstall
- **PR #1876**: Fix install PR creation failing silently on error
- **PR #1874**: Consolidate CuratorPhase validation to use validate_phase module
- **PR #1873**: Add loom-tools toolchain validation at daemon startup
- **PR #1872**: Add shared loom-tools.sh helper for consistent CLI error handling
- **PR #1871**: Unify GitHub label operations in labels.py
- **PR #1870**: Add generic gh_list() for GitHub CLI queries
- **PR #1869**: Add centralized environment variable parsing utilities
- **PR #1868**: Centralize path constants and naming conventions in common/paths.py
- **PR #1867**: Add phase result helpers to BasePhase
- **PR #1866**: Add SerializableMixin for automatic dataclass serialization
- **PR #1865**: Add centralized JSON I/O utilities to loom-tools
- **PR #1864**: Add phase_completed milestone event type
- **PR #1863**: Increase API rate limit threshold from 90% to 99%
- **PR #1853**: Fix flaky integration tests due to tmux server state
- **PR #1852**: Add worktree cleanup when builder phase fails
- **PR #1846**: Fix loom-shepherd ModuleNotFoundError on PEP 668 systems
- **PR #1845**: Add worktree safety checks to prevent destroying active sessions
- **PR #1844**: Fix shell stub scripts for non-interactive environments
- **PR #1843**: Fix daemon-cleanup.sh startup hang with many stale progress files
- **PR #1842**: Add CI health awareness to loom-tools snapshot
- **PR #1841**: Fix stale recommended_actions by reordering iteration sequence
- **PR #1840**: Fix state schema mismatch: support tmux_session for support roles
- **PR #1839**: Add --force flag to loom-clean calls in daemon_cleanup.py
- **PR #1836**: Fix daemon state rotation with Python fallback and better error logging
- **PR #1835**: Clean managed directories on reinstall to remove stale files
- **PR #1834**: Port health-check.sh proactive monitoring to loom-tools Python
- **PR #1832**: Add configurable champion auto-merge size limit
- **PR #1831**: Fix jq null handling in session-reflection.sh
- **PR #1830**: Port validate-phase.sh to loom-tools Python module
- **PR #1829**: Fix parent loop docs: Task() not Skill() for iteration spawning
- **PR #1828**: Port daemon-cleanup.sh to loom-tools Python
- **PR #1827**: Add check:ci:lite script excluding Tauri build for worktree verification
- **PR #1826**: Port loom-status.sh to loom-tools Python
- **PR #1824**: Port agent-metrics.sh to loom-tools Python
- **PR #1823**: Port report-milestone.sh to loom-tools Python module
- **PR #1822**: Port orphaned shepherd recovery from shell to Python module
- **PR #1821**: Port validate-daemon-state.sh to Python (loom-validate-state)
- **PR #1820**: Port agent-wait.sh to loom-tools Python module
- **Issue #1910** (closed): Shepherd: fallback approval detection should also detect changes-requested
- **Issue #1909** (closed): Shepherd: judge validation failure skips doctor loop entirely
- **Issue #1900** (closed): Reduce CI minutes on pull requests
- **Issue #1894** (closed): Phase 1: Config v3 & State Simplification for Single-Session Model
- **Issue #1891** (closed): Shepherd: preserve worktree on builder test failure instead of cleaning up
- **Issue #1887** (closed): Increase frontend test coverage thresholds
- **Issue #1886** (closed): Extract stuck_detection.py formatting into dedicated module
- **Issue #1885** (closed): loom-tools Python tests fail to import
- **Issue #1884** (closed): check:ci:lite fails on main: coverage thresholds exceed actual coverage
- **Issue #1882** (closed): Port detect-systematic-failure.sh to Python
- **Issue #1880** (closed): Extract shared tmux session utilities to common/tmux_session.py
- **Issue #1879** (closed): Remove unused prediction.ts module (658 LOC dead code)
- **Issue #1878** (closed): Add backwards-compatible clean.sh wrapper
- **Issue #1875** (closed): Clean reinstall preserves stale scripts
- **Issue #1862** (closed): loom-tools: Consolidate phase validation logic
- **Issue #1861** (closed): loom-tools: Add phase result helper to BasePhase
- **Issue #1860** (closed): loom-tools: Centralize environment variable parsing utilities
- **Issue #1859** (closed): loom-tools: Create generic gh_list() for GitHub CLI queries
- **Issue #1858** (closed): loom-tools: Unify GitHub label operations in labels.py
- **Issue #1857** (closed): loom-tools: Centralize path constants and naming conventions
- **Issue #1856** (closed): loom-tools: Create serialization mixin for dataclass models
- **Issue #1855** (closed): loom-tools: Create centralized JSON I/O utilities
- **Issue #1854** (closed): Increase API rate limit thresholds to 99%
- **Issue #1851** (closed): Refactor loom-tools: DRY opportunities and code consolidation
- **Issue #1850** (closed): Shepherd fails silently when loom-tools CLI commands not installed
- **Issue #1849** (closed): Milestone reporting shows 'Unknown event phase_completed' error
- **Issue #1848** (closed): Flaky integration tests fail on main due to tmux server state
- **Issue #1847** (closed): Builder phase should clean up worktree on failure
- **Issue #1838** (closed): Daemon should be resilient to missing/broken loom-tools commands
