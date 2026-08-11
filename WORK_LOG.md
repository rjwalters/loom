# Work Log

Chronological record of completed work in this repository, maintained by the Guide role.

Entries are grouped by date, newest first. Each entry references the merged PR or closed issue.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

### 2026-08-11

- **Issue #5950** (closed): Something removed builder issue-5919's worktree/branch mid-session despite loom-daemon clean logging 'preserving'
- **PR #5957**: fix(daemon): gate aggressive worktree removal on issue-open state, add a removal ledger
- **Issue #5944** (closed): Watchdog reports [OK] for hours while status times out on IPC — a live-but-load-starved daemon never diverges, so #5790's fix does not reach it
- **PR #5956**: fix(watchdog): windowed/rate failure signal for intermittent IPC probes
- **Issue #5928** (closed): Vendored guard-destructive-generic.sh lacks Repo Skills #244's rm unresolved-var fail-closed branch
- **PR #5954**: fix(guard): fail closed on unresolved-var rm targets under guards.rmScope=repo
- **Issue #5930** (closed): Guide: hand-written WORK_PLAN.md "Operator Attention" narrative section bypasses the #5890 debounce, still spamming ~1 docs PR/hour
- **PR #5935**: fix(guide): fold WORK_PLAN's Operator Attention section into the debounced generated region
- **Issue #5922** (closed): install: install-loom.sh hardcodes target/release/, breaking installs when build.target-dir is redirected
- **PR #5948**: fix(install): resolve Cargo's real target dir instead of assuming target/
- **Issue #5927** (closed): quickstart/webapp: tsconfig includes a nonexistent workers/ dir and emits stray .js into the repo root
- **PR #5951**: fix(quickstart/webapp): fix tsconfig include and composite build artifacts
- **Issue #5826** (closed): loom:operator-decision is keyed on judgement difficulty, not authority — with "safe default when unsure" that re-creates the pile the sub-kinds were meant to drain
- **PR #5945**: Redefine loom:operator-decision on authority, add loom:operator-objective (#5826)
- **Issue #5919** (closed): The reaper can never reclaim build artifacts — reaper_clean_options hardcodes deep:false/worktrees_only:true, so every long-lived host leaks disk until a human intervenes
- **PR #5942**: feat(daemon): reclaim the primary checkout's build artifacts under disk pressure
- **Issue #5926** (closed): quickstart/webapp: npm install fails on the wrangler pin; toolchain ~18 months stale
- **PR #5949**: fix(quickstart/webapp): bump toolchain pins and type res.json() call sites

### 2026-08-10
- **Issue #5925** (closed): quickstart/webapp: biome.json has "root": false, breaking lint once the template is copied out
- **PR #5947**: fix(quickstart/webapp): make biome.json standalone-safe after template copy-out
- **Issue #5938** (closed): Guide's has_open_pr_labeled_loom_pr() always returns false — gh issue view --json closedByPullRequestsReferences lacks state/labels
- **PR #5943**: fix(guide): resolve has_open_pr_labeled_loom_pr via per-PR gh pr view lookup
- **Issue #5936** (closed): loom-fleet-dispatch repeatedly re-claims loom:building on issues with an already-open loom:operator-held PR, wasting dispatch cycles for days
- **Issue #5940** (closed): Guide: has_open_pr_labeled_loom_pr() (#5911) always returns false — gh issue view --json closedByPullRequestsReferences has no state/labels sub-fields
- **Issue #5924** (closed): quickstart/webapp: 12 tests fail out of the box — auth tests seed localStorage but use-auth.tsx is cookie-based
- **PR #5937**: Fix quickstarts/webapp auth tests: mock fetch instead of localStorage
- **Issue #5923** (closed): quickstart/webapp: Tailwind 4 theme never compiles — tailwind.config.ts is never referenced
- **PR #5933**: fix(quickstart/webapp): move Tailwind 4 theme from dead JS config into CSS
- **Issue #5916** (closed): Guard: canonical Repo Skills guard still false-denies gh --search + --jq pipe combo despite #5803 fix
- **PR #5918**: Add third dispatcher capability probe for search/jq masking (#5916)
- **Issue #5921** (closed): The peer-claim view is unobservable — no status line, no subcommand, and the re-advertise heartbeat is debug-only, so every duplicate dispatch is undiagnosable
- **PR #5932**: feat(daemon): surface the peer-claim view in status, counters, and a new subcommand
- **Issue #5911** (closed): Ready-pool keeps re-selecting issues whose PR is loom:pr + awaiting human merge (repeat sweep dispatch waste, seen on #5565)
- **PR #5914**: fix: skip ready-pool candidates whose PR already carries loom:pr
- **Issue #5912** (closed): github-app-token.sh: owner-only installation cache collides when one host runs two apps under the same GitHub account (silent fallback to ambient auth)
- **PR #5915**: Fix owner-only installation cache collision for two GitHub Apps sharing an owner
- **Issue #5907** (closed): Raise token-exhaustion probe threshold 95% -> 99%
- **PR #5909**: Raise token-exhaustion probe threshold 95% -> 99%
- **Issue #5902** (closed): Remove dead code: unused build_exclude_args/convert_glob_to_find in random-file.sh
- **PR #5905**: chore: remove dead code from random-file.sh
- **Issue #5896** (closed): sweep-run-registry: peer detection cannot distinguish a dead cleared-context run from a live peer when both share the orchestrator PID
- **PR #5901**: feat: add heartbeat freshness signal to sweep-run-registry peer detection
- **Issue #5898** (closed): check-duplicate.sh: titles beginning with '--' are consumed as options when passed positionally
- **PR #5900**: fix: honor "--" end-of-options separator in check-duplicate.sh argument parsing
- **Issue #5779** (closed): Guard force-op ask fires on heredoc/prose text, not just executed commands
- **PR #5781**: fix: mask single-quoted heredoc bodies before ASK-tier force-op/stash-scope scan
- **Issue #5890** (closed): Guide docs PR churn: WORK_PLAN regeneration has no hysteresis, spamming docs-only PRs during label flapping
- **PR #5892**: feat: debounce WORK_PLAN.md rewrites against rapid label-driven diffs
- **Issue #5874** (closed): VERSION does not bump on defaults/ prompt changes, so every currency check reports a fleet as current while its agents run different instructions
- **PR #5876**: feat(ci): detect and gate installed-surface drift independent of VERSION
- **Issue #5818** (closed): Require a stated category for loom:operator-only, and document operator-only <-> needs-capability friction routing
- **PR #5871**: docs: document loom:operator-only <-> loom:needs-capability routing convention
- **Issue #5865** (closed): champion-epic.md lacks the unrevised-proposal idempotency guard champion-issue-promo.md has
- **PR #5867**: docs(champion): add a body-hash idempotency guard to the epic rejection path
- **Issue #5851** (closed): Investigate a fleet-level cross-repo summary for multi-repo Loom hosts (adapt atomic-claude Realm)
- **PR #5863**: docs: decide the fleet-level cross-repo summary is already solved (no new artifact)
- **Issue #5859** (closed): Add a bounded rejection-review standing policy to Auditor's periodic tick
- **PR #5860**: docs(auditor): add bounded rejection-review standing policy

### 2026-08-09
- **Issue #5850** (closed): Investigate a human-gated retrospective pass mining Judge/Doctor patterns (adapt atomic-claude retrospective-learning)
- **PR #5858**: docs: decide against an automated Judge/Doctor retrospective mining pass
- **Issue #5849** (closed): Investigate a structural test-first checkpoint inside Builder (adapt atomic-claude maker/checker)
- **PR #5855**: docs(builder): add in-Builder test-first checkpoint (TDD line + Judge check)
- **Issue #5848** (closed): Evaluate a lightweight code-graph / blast-radius helper for Judge and Hermit (adapt atomic-claude code-intel)
- **PR #5857**: docs: recommend against a code-graph index for Judge/Hermit blast-radius queries
- **Issue #5847** (closed): Design a generated, dirty-marked repo knowledge digest (adapt atomic-claude wiki)
- **PR #5854**: docs: add design for a generated, dirty-marked repo knowledge digest
- **Issue #5844** (closed): Research: evaluate damusix/atomic-claude for ideas worth bringing into Loom
- **PR #5852**: docs: evaluate damusix/atomic-claude for ideas worth adopting into Loom
- **Issue #5819** (closed): Wire loom:operator-only sub-kind requirement into Curator/Builder/Doctor/Judge — not just Champion's two escalation paths
- **PR #5846**: feat(labels): require a loom:operator-only sub-kind in Curator/Builder/Doctor/Judge
- **Issue #5838** (closed): Guard catastrophic-tier deny fires on quoted prose (search queries, echo labels, jq filters), not just executed commands
- **PR #5840**: fix: mask echo/jq/check-duplicate.sh inert prose ahead of catastrophic-tier guard scan
- **Issue #5835** (closed): Guard false positive: gh-api-rawfield-body-literal-at catastrophic pattern fires on descriptive text, not just live invocations
- **PR #5837**: fix: mask quoted-string prose in gh-api-rawfield-body-literal-at guard check
- **Issue #5824** (closed): Guard ask:cat .ssh fires on reading ~/.ssh/config, not just private key material
- **PR #5832**: fix: narrow guard ask for cat .ssh/ to an allowlisted basename check
- **Issue #5823** (closed): Guard false positive: cloud-cli docker rm asks even for self-created ephemeral test containers
- **PR #5831**: fix: narrow cloud-cli docker rm ask to volume-destroying variant only
- **Issue #5817** (closed): Split loom:operator-only into a by-right label and a loom:needs-capability label with sweep-skip parity
- **PR #5829**: feat: add loom:needs-capability label with sweep-skip parity to loom:operator-only
- **PR #5828**: docs: fix self-contradictory config-resolution-tiers.md status line, audit Follow-ups
- **Issue #5822** (closed): docs: safehouse.md tells operators to configure host-local socket in .loom-local/local.json, but no call site reads that tier
- **PR #5809**: fix(guard): mask gh --search and jq --arg/--argjson quoted values before catastrophic/cloud-cli scans
- **PR #5807**: fix(champion): clear loom:operator when routing a stale PR to Doctor
- **Issue #5802** (closed): Champion stale-PR routing to Doctor never clears loom:operator, deadlocking the changes-requested queue
- **PR #5803**: fix(guard): mask gh --search and jq --arg/--argjson values from catastrophic/ask pattern scans
- **Issue #5797** (closed): Guard-decision proposal: catastrophic/cloud-cli patterns match substrings anywhere in the command line, including read-only search/filter arguments
- **PR #5805**: fix(daemon): make safehouse env-alone test hermetic against ambient LOOM_SAFEHOUSE_ROOM
- **Issue #5801** (closed): Test failure: safehouse test not hermetic against ambient LOOM_SAFEHOUSE_ROOM env var
- **PR #5799**: fix(daemon): enforce cross-host dispatch-collision back-off instead of only logging it
- **Issue #5789** (closed): Two hosts claimed and built the same issue four seconds apart — cross-host claim acquisition races
- **PR #5794**: fix(watchdog): sample load average on IPC divergence, stop masking it with a same-tick OK line
- **Issue #5790** (closed): Watchdog reports "[OK] daemon healthy" while loom-daemon status times out on IPC — an outage class it cannot see
- **PR #5792**: fix: skip workspace-add auto-init so migrate-consumer.sh's own fixes stick
- **Issue #5788** (closed): migrate-consumer.sh: workspace registration step clobbers its own prior migration work
- **Issue #5783** (closed): Guard ASK tier: backtick command substitution evades ASK_PATTERNS / stash-scope scan
- **PR #5786**: fix(guard): recognize backtick/no-space command substitution in ASK-tier stash/clean/read-tree checks
- **PR #5775**: fix(guard): exempt known-safe reset recovery targets from force-op:detached on a Loom-managed worktree
- **Issue #5772** (closed): Guard: force-op:detached ask-tier fires on own-worktree git -C $WT resets with no human to answer
- **Issue #5773** (closed): guard-background-subagents.sh keeps blocking stop after all subagents have completed
- **PR #5768**: chore(deps): Bump base64 from 0.23.0 to 0.23.1 in the all-dependencies group

### 2026-08-09 — Residual gap notice

The `work_log_has_pr()` / `work_log_has_issue()` presence checks (#5516,
#5539) surfaced ~383 additional merged PRs (159) and closed issues (224),
dated 2026-07-30 through 2026-08-04, that are still absent from this file.
These are not new out-of-order stragglers — they are the un-swept tail of
the same 2026-02-26–2026-08-05 outage documented in the
"2026-08-05 — Historical gap notice" below (#5413): the reset snapshot
taken there was explicitly a representative sample, not exhaustive, and
did not reach back far enough to cover this window. Confirmed this window
is a genuine gap (spot-checked several numbers, e.g. PR #4914 and #5180,
absent from the file entirely) and confirmed the file **is** fully current
for all activity from 2026-08-05 onward (checked the 30 most recent merged
PRs and 30 most recent closed issues — all already recorded).

Per the same reasoning as the original notice, a literal 383-entry backfill
is not a good use of a single triage cycle's budget. Leaving this window
undocumented rather than backfilling it; a future pass with a specific
need for that history should query the forge directly by date range
(`merged:2026-07-30..2026-08-04` / `closed:2026-07-30..2026-08-04`) rather
than trusting this file's coverage for that window.

### 2026-08-08
- **PR #5766**: docs(auditor): note docker/target arch mismatch fallback for worker-image-smoke
- **Issue #5765** (closed): Auditor Capability Request: local worker-image-smoke validation blocked by host/target arch mismatch (arm64 vs x86_64-unknown-linux-gnu)
- **PR #5763**: fix(daemon): remove per_token_concurrency, a disclaimed knob multiplying nothing
- **Issue #5743** (closed): Remove per_token_concurrency: since #5270 it only prints a disclaimed number, and it has now caused two wrong operator conclusions
- **PR #5760**: fix(guard): gate the stash CREATE, not the recovery — stash-scope:create-redirect
- **Issue #5754** (closed): Guard telemetry (#3898): stash-scope ask (32 hits, top trigger after gh-pr-merge-redirect) — steer callers to worktree.sh snapshot/stash-push instead of raw git stash
- **PR #5759**: feat(safehouse): emit per-model token totals alongside the flat tokens sum
- **Issue #5740** (closed): Completion envelope: emit per-model, per-counter token totals (single `tokens` sum overstates cost 7.7x)
- **PR #5756**: fix(scripts): route root clean.sh through loom-daemon clean, not dead loom-tools
- **Issue #5739** (closed): fix(scripts): root clean.sh routes to the retired loom-tools package and prints an impossible remediation
- **PR #5751**: fix(install): clean up 11 permanently-orphaned loom-tools shims
- **Issue #5738** (closed): chore(install): 11 dangling loom-* shims from the loom-tools retirement have no cleanup path (#5708 follow-up)
- **PR #5752**: fix(daemon): clean --safe --branches-only must gate on reachability, not just tracking-branch absence
- **Issue #5737** (closed): fix(daemon): clean --safe --branches-only conflates 'no remote tracking branch' with 'safe to delete'
- **PR #5750**: feat(check-ci-status): add --job filter for single-check-run lookups
- **Issue #5748** (closed): Auditor Capability Request: no docker on Auditor host, cannot validate loom-worker image locally
- **PR #5747**: fix(daemon): route --aggressive clean through the shared confirmation gate
- **Issue #5736** (closed): fix(daemon): clean --aggressive --worktrees-only destroys without confirmation while other modes prompt
- **PR #5745**: fix(daemon): --safe --force must not override the unreachable-HEAD skip in aggressive clean
- **Issue #5735** (closed): fix(daemon): clean --force overrides the 'would lose work' skip AND drops those decisions from the report
- **PR #5744**: fix(quarantine): anchor TTL manual-repark check to the specific cycle, symmetric recency
- **Issue #5725** (closed): quarantine release (TTL/reconciliation) recency-comparison re-fetches the newest marker, releasing a legitimate loom:blocked that predates a repeat re-quarantine
- **PR #5733**: fix: include stale and current SHA values in head-moved diagnostic
- **Issue #5714** (closed): Champion squash-merges a PR whose head moved during evaluation, silently landing a partial branch
- **PR #5730**: fix(champion): narrow critical-file 'migration' pattern to avoid docs/migration/ false positives
- **Issue #5723** (closed): champion: critical-file 'migration' pattern false-positives on docs/migration/*.md paths
- **PR #5731**: fix(hooks): bind loop item before $polled_ok_refs lookup in guard-background-subagents.sh
- **Issue #5721** (closed): guard-background-subagents.sh: `$polled_ok_refs | index(.)` rebinds `.`, so one terminal TaskOutput poll silently resolves every async Agent dispatch
- **PR #5695**: fix(daemon): auto-init workspaces missing /loom:sweep and surface it in status
- **Issue #5682** (closed): workspace add succeeds on a repo that can never be dispatched into, and status renders it identically to a healthy idle repo
- **PR #5727**: fix(daemon): add starvation escape hatch to the admission brake (#5715)
- **Issue #5715** (closed): Admission brake starves sweeps indefinitely when role-runner load alone exceeds the threshold (33h outage, no signal)
- **PR #5718**: chore(defaults): remove orphaned status.sh agent-status-file script
- **Issue #5710** (closed): chore(defaults): defaults/scripts/status.sh is orphaned — last survivor of the retired agent-status-file mechanism
- **PR #5716**: feat(daemon): two-condition auto-retirement classifier for quarantine stashes
- **Issue #5693** (closed): Auto-retire safe quarantine stashes (closed issue + installer-only/at-HEAD content) with back-test against current backlog
- **PR #5724**: feat(daemon): distinguish credit exhaustion in outcome telemetry; close chain pre-resolution as not worth it (#5697)
- **Issue #5697** (closed): sweep: pre-resolve a model fallback chain at dispatch, and tag credit exhaustion distinctly in daemon outcome telemetry
- **PR #5720**: fix(guard-background-subagents): accept <task-notification> as resolution for Agent/Task dispatches
- **Issue #5713** (closed): guard-background-subagents.sh: <task-notification> resolves background Bash but not Agent dispatches — permanent stop-block after async agents
- **PR #5717**: feat(champion): respect a startable subset instead of parking a whole issue, surface parked work in status
- **PR #5712**: docs(tests): add tests/README.md documenting the suites and the ci-wired.txt requirement
- **Issue #5711** (closed): docs(tests): add tests/README.md documenting the three suites and the ci-wired.txt registration requirement
- **Issue #5690** (closed): Quarantine stashes have no lifecycle: 148 accumulated across the fleet in 12 days, and exactly one held work that mattered
- **PR #5708**: fix(install): unlink dangling shim symlinks before write
- **Issue #5706** (closed): fix(install): shim install cannot repair a dangling loom-* symlink left by the loom-tools retirement (#5386 follow-up)
- **PR #5705**: docs(readme): correct the daemon work-generation claim and installed-surface tree
- **Issue #5704** (closed): docs(readme): daemon work-generation claim is false; installed-surface tree omits hooks/docs/bin
- **PR #5707**: chore: resync installed Loom surfaces (restores two stale guard hooks, incl. the #4767 confinement fix)
- **PR #5702**: feat(daemon): report per-repo quarantine-stash counts and oldest age in status
- **Issue #5692** (closed): loom-daemon: report fleet-wide per-repo quarantine-stash counts and oldest age
- **PR #5701**: feat(sweep): classify per-model credit exhaustion distinctly, add model-downgrade fallback
- **Issue #5687** (closed): sweep: in-session wave builders die en masse on model credit exhaustion — no automatic model-fallback retry
- **PR #5699**: fix(judge): bind review verdicts to the head SHA they were rendered against
- **Issue #5686** (closed): A review verdict survives a force-push: loom:changes-requested persists after the head SHA moves
- **PR #5698**: feat(quarantine): post a forge breadcrumb comment when check-main-clean.sh rescues main-worktree dirt
- **Issue #5691** (closed): Quarantine: reconcile installer/build-artifact stash volume without reintroducing the #4332 main-worktree-write blind spot
- **PR #5688**: feat(champion): defer instead of escalating a self-clearing dependency block, and self-heal stuck escalations
- **Issue #5664** (closed): champion escalates dependency-blocked proposals to operator-only and never un-escalates when the blocker clears
- **PR #5679**: feat(labels): distinguish loom:operator-only sub-kinds (blocked/mechanical/decision)
- **Issue #5671** (closed): loom:operator-only carries at least four meanings with no way to tell them apart
- **PR #5677**: feat(role-runner): add architect as an idle-addressable-only role with a per-repo proposal cap
- **Issue #5656** (closed): architect is not in DEFAULT_ROLES, so a repo whose backlog empties has no way to acquire new work
- **PR #5667**: fix(daemon): stop gating role-runner loop spawn on one workspace's own roleRunner.roles
- **PR #5670**: docs(builder,doctor): forbid ending the turn on a background build/CI monitor
- **Issue #5659** (closed): roles: judge/builder/doctor prompts need the in-turn CI-polling directive (background-monitor parking stalls interactive Task-tool dispatches)
- **PR #5669**: fix(merge-pr): name dirty paths and gate cross-host hypothesis in worktree data-loss guard
- **Issue #5658** (closed): merge-pr.sh: #5031 data-loss guard misattributes trivial lockfile churn to 'cross-host duplicate dispatch' — name the dirty paths instead
- **PR #5665**: fix(worktree): skip stale post-squash-merge remote branch on worktree creation
- **Issue #5657** (closed): worktree.sh: reuses stale remote branch after a partial-increment squash-merge — next slice's PR is CONFLICTING with zero CI runs
- **PR #5663**: feat(role-runner): log resolved role list + config source per repo per tick
- **Issue #5654** (closed): role_runner: doctor is never admitted on one host while hermit/auditor run despite identical 'will not be dispatched' warnings
- **PR #5653**: fix(tests): scrub ambient GH_CONFIG_DIR in unregistered-root no-op tests
- **Issue #5651** (closed): 3 nextest tests spuriously fail on hosts with ambient GH_CONFIG_DIR set (test isolation gap, #5431)
- **PR #5648**: feat(guide): stabilize loom:urgent selection with an incumbency rule and a forge-backed flip guard
- **Issue #5643** (closed): Guide: loom:urgent flaps on #5565 across independent triage ticks (7 flips in 2.5h), churning WORK_PLAN.md and spawning docs PRs

### 2026-08-07
- **PR #5647**: fix(dashboard): qualify sweep-count text and surface role-tick totals in fleet headline
- **Issue #5642** (closed): dashboard: a busy fleet reads as '0 active sweeps' — role ticks are not counted, and the data to fix it is already exported
- **PR #5644**: fix(daemon): hold main-health verdicts on a stale forge credential instead of halting every repo
- **Issue #5630** (closed): credential_preflight 20s exec timeout fails under host saturation, flipping the main-health gate to 22/22 red and halting dispatch host-wide
- **PR #5634**: fix(classify-error): widen TOKEN_EXHAUSTED regex to catch "monthly spend limit"
- **Issue #5631** (closed): claude-wrapper: 'monthly spend limit' isn't classified TOKEN_EXHAUSTED, so a capped account is retried instead of rotated
- **PR #5626**: fix(install): stop leaking installer's absolute path into install-metadata.json
- **Issue #5624** (closed): install.sh records the installer's absolute path in install-metadata.json, which consumers commit
- **PR #5620**: fix(daemon): stop reaper from resuming a clean-exit sweep that made no checkpoint progress
- **Issue #5614** (closed): Issue #5565 rapidly flapping between loom:issue and loom:building (~10 transitions in 7 minutes)
- **PR #5617**: fix(guide): add uncached pre-create recheck to close cross-host docs-PR race
- **Issue #5615** (closed): Guide's docs-guide-lock only serializes same-host ticks — cross-host role-runner races still open duplicate docs PRs
- **PR #5613**: fix(daemon): add hermit to role_runner DEFAULT_ROLES
- **Issue #5601** (closed): role_runner: hermit is not in DEFAULT_ROLES, so it can never be dispatched
- **PR #5611**: fix(tokens): filter non-Anthropic providers and validate token shape in import-from-monitor
- **Issue #5604** (closed): tokens import-from-monitor: email-only keying lets a non-Anthropic credential occupy an Anthropic pool slot
- **PR #5610**: docs: specify provider-aware token pool identity and decompose it into three issues
- **Issue #5605** (closed): design: token pool identity should be (provider, account_id), not email — blocks multi-runtime (codex/kimi/qwen)
- **Issue #5546** (closed): loom-daemon is DOWN on robb-pro and watchdog recovery is exhausted
- **PR #5597**: feat(daemon): add --expected-head-sha precondition to forge auto-merge
- **Issue #5589** (closed): loom-daemon forge auto-merge lacks the head-SHA precondition added to the shell merge path (#5579 follow-up)
- **PR #5594**: fix(champion): thread expected head SHA into merge API calls (#5579)
- **Issue #5579** (closed): Champion can squash-merge a PR while a session is still pushing to its branch, stranding commits invisibly
- **PR #5592**: feat: support an operator-supplied fleet roster via LOOM_FLEET_PATH
- **Issue #5576** (closed): The fleet family can only see hosts add-worker created — let it read an operator-supplied roster
- **PR #5588**: fix(dashboard): tolerate WAL sidecar files in miniflare isolated-storage teardown
- **Issue #5543** (closed): dashboard-deploy is pinned by a miniflare isolated-storage flake — the live Worker has not redeployed since 08-05T07:28Z
- **PR #5585**: fix(fleet): make fleet status per-host timeout configurable
- **Issue #5575** (closed): fleet status reports a BUSY worker as UNREACHABLE — the 8s per-host timeout is hardcoded
- **PR #5584**: fix(loom-daemon): stop fleet::add_worker tests shadowing on hosts with system loom-daemon
- **Issue #5577** (closed): cargo nextest: fleet::add_worker tests fail on hosts with loom-daemon installed system-wide (PATH stub shadowed)
- **PR #5583**: docs(observability): stop claiming reference-deployment.md holds operator identity
- **Issue #5578** (closed): observability.md points readers at identity content reference-deployment.md no longer carries
- **Issue #5582** (closed): fleet::add_worker tests execute the real loom-daemon binary on hosts where it's installed, instead of the test stub
- **Issue #5573** (closed): Guide's doc-maintenance Step 1 open-PR check is a TOCTOU race — two concurrent instances opened duplicate PRs #5571/#5572
- **PR #5580**: fix(guide): close TOCTOU race in doc-maintenance Step 1 open-PR check
- **Issue #5329** (closed): Retire dashboard-deploy.yml + remove 2AM Cloudflare secrets once 2AMLogic/2am-side deploy is green
- **Issue #5574** (closed): Guide doc-maintenance phase: concurrent ticks can race and open duplicate docs PRs
- **PR #5570**: ci: delete dashboard-deploy.yml — the instance owns its own deploy now
- **Issue #5567** (closed): dashboard-deploy.yml deploys one operator's instance from the mechanism repo — remove it now that the instance owns its deploy
- **PR #5561**: fix(resync): restamp .loom/CLAUDE.md's version header on resync
- **Issue #5559** (closed): resync-installed.sh never restamps the vendored CLAUDE.md version header (metadata 0.18.0 vs header 0.16.0)

### 2026-08-06
- **Issue #5038** (closed): Design: who owns continuous maintenance? Split by determinism and granularity, not topic — and why a janitor agent cannot own install repair
- **PR #5554**: fix(daemon): replace pgrep -f loom-daemon liveness checks with exact process-name matching
- **Issue #5548** (closed): pgrep -f loom-daemon is not a liveness check — leaked test fixtures named loom-daemon kept a dead daemon looking healthy for 66 minutes
- **PR #5553**: fix(mcp-loom): commit engines field in package-lock.json
- **Issue #5552** (closed): mcp-loom/package-lock.json missing committed engines field, causes npm install drift on every run
- **PR #5551**: feat(dashboard): add kind filter to history query API
- **Issue #5542** (closed): dashboard /public/history has no time filter or cursor and caps at 500 — a bounded window cannot be read to completion
- **Issue #5539** (closed): Guide's WORK_LOG.md closed-issue watermark misses out-of-order-closed issues (mirrors #5516, PR side already fixed)
- **PR #5541**: fix(guide): stop dropping out-of-order-closed issues from WORK_LOG.md
- **Issue #5511** (closed): loom-recover-orphans (or similar) reset loom:building -> loom:issue on #5501 despite an open, Closes-referencing PR
- **Issue #5232** (closed): Guard: tee heredoc delimiter misparsed as write target, false worktree-isolation DENY
- **Issue #4928** (closed): install.sh: no per-target lock; silent multi-minute cargo build reads as a dead install (two installers raced over one target)
- **PR #4940**: feat(install): serialize concurrent installs with a per-target PID lock
- **Issue #4889** (closed): worktree.sh remove can't delete squash-merged branches — uses git branch -d while merge-pr.sh has a squash-aware path
- **PR #4918**: fix(worktree): make worktree.sh remove squash-aware when deleting the attached branch
- **Issue #4767** (closed): Codex guard bridge: model-controlled `workdir` bypasses managed-worktree write confinement
- **PR #4770**: fix(codex-bridge): validate a model-chosen workdir before trusting it as GUARD_CWD
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

- **Issue #5266** (closed): Remaining stale Loom installs beyond #5184's eight — active tool repos (anvil, kicad-tools, claude-monitor, safehouse) still lack create-issue.sh
- **Issue #5131** (closed): something removed the live autonomy-desired marker on robb-studio while its daemon kept running — crash protection silently disarmed
- **Issue #5007** (closed): operator: provision additional Codex accounts + install/trust the managed pre-tool hook so the allocation can be used
- **Issue #4607** (closed): Wire defaults/scripts/check-cas-recheck-consistency.sh into .github/workflows/ci.yml's installer-tests job
- **Issue #5063** (closed): host_identity() is whatever `hostname` prints: three naming schemes across the fleet, $HOSTNAME makes it launch-context-dependent, and it drives peer-claim self-recognition
- **Issue #4702** (closed): Epic: Rich fleet observability dashboard with user-configurable hosting
- **Issue #4057** (closed): Provision a dedicated shared AWS CI runner for the project fleet (operator-only; gated on #4038)
- **Issue #4859** (closed): [Epic #4702] 2AM production deploy: dashboard.2amlogic.com cutover
- **Issue #4993** (closed): operator: mint Developer ID Application cert and provision signing secrets for release CI
- **Issue #4992** (closed): operator: enroll 2AM Logic in the Apple Developer Program (org account)
- **Issue #4996** (closed): operator: provision gf180 clones + workspaces on robb-pro to absorb sim-heavy load (18 cores mostly idle)
- **Issue #5062** (closed): loom-worker-1 telemetry ingest key is bound to ip-172-31-74-176 while filing under loom-worker-1 (~35h unactioned)
- **PR #4972**: chore(deps): bump libc from 0.2.186 to 0.2.189 in the all-dependencies group
- **PR #5132**: fix(daemon): make the restart primitive supervisor-aware and self-healing
- **Issue #4933** (closed): Bash-tool write-confinement is bypassed by quoting the `cd` argument
- **PR #4941**: fix(hooks): strip quotes before classifying a Bash cd argument as absolute
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

### 2026-08-04

- **PR #5310**: fix(daemon): surface cross-host collision count in WorkFinderTickSummary
- **Issue #5347** (closed): docs(readme): ADR range in docs/README.md is stale (says 0001–0013, ADR-0014 exists)
- **PR #5348**: docs(readme): correct ADR range to 0001–0014
- **Issue #5339** (closed): role runner: #5272's standalone Doctor is inert on any repo pinning autonomous.roleRunner.roles — loom's own config omits doctor
- **PR #5346**: fix(role-runner): warn when a pinned roles allowlist is missing a default role
- **Issue #5334** (closed): fleet add-worker: verify step races daemon startup (false-fail at 12/13 green), and an all-blocked token pool bootstraps silently
- **PR #5335**: fix(fleet): retry verify's daemon-status race and gate token-pool health
- **Issue #5327** (closed): skill-router: bare \bfix\b routes a prompt that explicitly declines Loom to /loom:doctor
- **PR #5332**: fix(skill-router): suppress AGENT_ROUTE on explicit Loom-decline prompts
- **Issue #5325** (closed): Publish a loom-worker OCI base image from the release workflow (decide: daemon-as-PID-1 vs sweep-execution-environment)
- **PR #5331**: feat(docker): publish a loom-worker OCI base image from the release workflow
- **Issue #5326** (closed): Extract 2AM instance content from dashboard/: relocate reference-deployment.md + retire dashboard-deploy.yml once the 2am-side deploy is green
- **PR #5330**: docs(dashboard): extract 2AM instance identity out of reference-deployment.md
- **Issue #5314** (closed): Headless hosts run untrusted workspaces — provisioning should set hasTrustDialogAccepted so permissions.allow isn't silently ignored
- **PR #5322**: feat(daemon): seed ~/.claude.json workspace trust at registration time
- **Issue #5305** (closed): Token-signal over-removal in #5304: false add-accounts advisory, unreachable status guidance, silenced ranking-divergence warning
- **PR #5312**: fix(daemon): re-scope token_bound to genuine starvation instead of a cross-axis cap comparison
- **Issue #5315** (closed): Worktree-isolation guard doesn't tilde-expand a chained cd (literal ~ mid-path in resolved target) and blocks gitignored runtime-state writes
- **PR #5321**: fix(guard): tilde/$HOME-expand cd arguments in all three cd-tracking blocks
- **Issue #5319** (closed): reaper.rs claim-timeline tests hardcode an absolute date, breaking CI on main permanently after 2026-08-04T15:59:59Z
- **Issue #5317** (closed): fix(daemon): hardcoded-date time bomb breaks reaper cross-host tests after 2026-08-04T16:00 UTC
- **PR #5320**: fix(tests): derive reaper cross-host-claim fixture timestamp from the clock (#5317)
- **Issue #5279** (closed): install.sh rewrites .claude/settings.json wholesale, silently dropping other tools' hooks (install order becomes load-bearing)
- **PR #5306**: fix(install): regenerate manifest after guard-hook wiring to stop spurious settings.json drift
- **Issue #5111** (closed): nothing bounds a sweep's CPU: one agent-written driver ran 8 concurrent sims on 8 cores with no overall wall-clock limit
- **PR #5318**: feat(spawn): enforce a per-sweep CPU quota via systemd --user scope
- **PR #5316**: feat(check-main-clean): surface outstanding quarantine stashes and stop creating empty ones
- **PR #5313**: feat(daemon): add install/host invariant self-check with repair-or-file
- **PR #5311**: fix(curator): make verified findings append-only across re-curation passes
- **Issue #5185** (closed): Outstanding loom-quarantine stashes have no operator-facing surface
- **PR #5309**: Surface outstanding loom-quarantine: stashes to the operator
- **Issue #5169** (closed): test-loom-dispatcher.sh: false failures from unisolated ambient env (LOOM_RUNTIME leak, Darwin uname branch mismatch)
- **PR #5308**: fix(tests): make test-loom-dispatcher.sh Test 16b OS-independent, add real-plist happy path
- **Issue #5068** (closed): observability: finish the robb-studio host rename — mint the key, decide on relabeling historical D1 rows, and document a host-rename procedure (robb-air is next)
- **Issue #5188** (closed): Test isolation gap: 'no resolvable binary' fixtures leak the host's real machine-level loom-daemon/safehoused install
- **Issue #5251** (closed): check-duplicate.sh reports flat ~100% similarity for unrelated issues/PRs
- **Issue #5302** (closed): daemon: wedged-sweep watchdog give-up is silent, and the live-vs-live dispatch race has no CAS (split from #5017)
- **PR #5307**: feat(daemon): surface watchdog give-up via a forge comment, not just a log line
- **PR #5301**: fix(daemon): verify claim ownership against the forge timeline before cancel/reap label-restore
- **Issue #5157** (closed): Guard false positive: worktree-write-confinement scans heredoc/nested test-string bodies as live redirects
- **PR #5299**: fix(guard): mask multi-line quoted string bodies in write-confinement scan
- **Issue #5270** (closed): Token axis caps at one sweep per healthy account — make it utilization-weighted so fresh accounts contribute multiple slots
- **PR #5304**: feat(daemon): remove token axis from admission, add RAM headroom + retuned CPU brake ("dumb mode")
- **Issue #5172** (closed): Guard false positive: merge-redirect guard denies gh api -f body=VAR referencing a heredoc that merely quotes the disallowed phrase
- **PR #5297**: fix(guard): close two-hop heredoc-variable indirection gap in merge-redirect check
- **Issue #5294** (closed): Resync regenerated .gitignore without .claude/worktrees/, reintroducing the #5267 gitlink hazard
- **PR #5303**: fix(scripts): stop .gitignore resync from trusting a stale loom-daemon binary
- **Issue #5282** (closed): loom-daemon cancel releases a loom:building claim owned by a DIFFERENT host's surviving sweep — destroys the cross-host mutex
- **Issue #5289** (closed): install.sh --quick reinstall: --confirm-reinstall staleness + possible silent CLAUDE.md in-block edit loss
- **PR #5298**: fix(install): stash root CLAUDE.md before --quick reinstall's chained uninstall
- **Issue #5158** (closed): Guard false positive: catastrophic curl-pipe-shell pattern fires on grep introspection of its own regex
- **PR #5300**: fix(guard): mask grep/rg positional pattern args in catastrophic curl-pipe scan
- **Issue #5264** (closed): docs: .claude/README.md ships Loom-repo-specific content into every consumer install
- **PR #5281**: docs: fix Loom-repo-specific content in consumer-installed .claude/README.md
- **Issue #5271** (closed): Orphaned test suites and scripts outside the CI-manifest partition: tests/install/, tests/hermit/, scripts/test-*.sh have no runner
- **Issue #5278** (closed): Widen check-ci-suite-manifest.sh partition guard to cover tests/install/, tests/hermit/, and scripts/test-*.sh
- **PR #5296**: Widen check-ci-suite-manifest.sh partition guard to cover scripts/test-*.sh
- **Issue #5159** (closed): Guard refinement: force-op:protected should allow lease-guarded force pushes after a verified HEAD-unchanged check
- **PR #5295**: docs(adr): decide forge coordination decoupling — local memo, safehouse as accelerator only
- **Issue #5272** (closed): No role owns loom:changes-requested once a sweep ends — Doctor is sweep-internal only, so rejected PRs park forever
- **PR #5291**: feat(daemon): give changes-requested PRs a standalone Doctor owner
- **Issue #5285** (closed): champion: Priority 2/3 discovery queries still surface already-promoted loom:issue/loom:building work
- **PR #5293**: fix(champion): exclude already-promoted loom:issue/loom:building from discovery queries
- **Issue #5277** (closed): Wire, document, or remove no-caller operator scripts under defaults/scripts/
- **PR #5292**: chore(scripts): resolve 4 no-caller operator scripts under defaults/scripts/
- **Issue #5276** (closed): Wire or delete orphaned scripts/test-*.sh suites (no CI/build-gate runner)
- **PR #5290**: chore(scripts): wire or delete orphaned scripts/test-*.sh suites
- **Issue #5265** (closed): install: reinstall refusal should point at resync-installed.sh as the non-destructive update path
- **PR #5288**: docs: point install.sh reinstall refusal at resync-installed.sh
- **Issue #5275** (closed): Wire or delete orphaned tests/install/ + tests/hermit/ test suites (no CI runner)
- **PR #5287**: fix(ci): wire tests/install/ + tests/hermit/ suites into shell-suite-tests
- **Issue #5269** (closed): Daemon ranking self-refresh and health read different token pools when the daemon's CWD is another repo (worker-1: 5h-stale machine pool)
- **PR #5283**: fix(daemon): surface per-repo token-pool ranking staleness in health/status
- **Issue #5268** (closed): Primary checkouts get parked on dead branches (closed-PR / never-PR'd) and primary-clone agents read stale surfaces indefinitely
- **PR #5284**: feat(daemon): reap a primary checkout parked on a dead branch back to its default branch
- **Issue #5267** (closed): Installer/resync don't manage a .claude/worktrees/ gitignore rule — git add -A commits an embedded-repo gitlink
- **PR #5280**: fix(gitignore): ignore .claude/worktrees/ so git add -A can't stage a gitlink
- **Issue #5263** (closed): Guard false positive: sql-ddl catastrophic pattern fires on grep introspecting a DDL keyword as a search pattern
- **PR #5274**: fix(guard): admit read-only search piped to a read-only sink on the fast path
- **Issue #5262** (closed): Subagent killed by a session cap leaves external mutexes held; stale-break by directory mtime never fires
- **PR #5273**: docs(troubleshooting): document killed-subagent external-lock gap and self-healing pattern
- **Issue #5260** (closed): Guard false positive: gh release delete pattern matches gh release delete-asset
- **PR #5261**: fix(guard): right-hand anchor gh release delete so it doesn't match delete-asset
- **Issue #5211** (closed): Champion blocks dependents on an epic's label state, not its delivered capability — and can deadlock on a cycle
- **PR #5220**: fix(champion): make epic-blocker check capability-aware, not label-aware
- **Issue #5243** (closed): guard-background-subagents.sh false-positives on synchronous Agent dispatches (inline tool_result completions not counted)
- **PR #5249**: fix(guard): count synchronous Agent/Task dispatches as completed in stop backstop
- **Issue #5236** (closed): dispatch_sweep: spawn_child failure leaks the claim lock (then #4556 guard permanently blocks retries as 'confirmed-live'); inner error chain never surfaced
- **PR #5259**: fix(daemon): release the claim lock/label/peer-claim when spawn_child fails
- **Issue #5252** (closed): role agents post 'gh api -f body=@FILE', sending the literal path as the comment body
- **PR #5258**: docs(roles): extract gh comment @path pitfall into a shared canonical doc
- **Issue #5240** (closed): install.sh targeting a linked git worktree stages deletions in the PRIMARY checkout (deleted 158 files from a live sibling worktree)
- **PR #5257**: fix(uninstall): don't redirect --local targets off a linked worktree
- **Issue #5245** (closed): test-spawn-codex.sh: 'no LOOM_ROLE' case leaks ambient LOOM_ROLE, fails inside Loom agent sessions
- **PR #5255**: fix(test): unset ambient LOOM_ROLE in test-spawn-codex.sh's run_preflight
- **Issue #5242** (closed): install.sh strips the target's .loom/config.json gitignore rule — host-local runtime config (worktree.root) becomes committable
- **PR #5253**: fix(gitignore): stop stripping a pre-existing .loom/config.json ignore rule
- **Issue #5235** (closed): Guard: ask-tier strip_literal_text has no positional-argument masking, unlike the #5155 catastrophic-tier fix
- **PR #5244**: fix(guard): mask check-duplicate.sh positional arguments in the ask-tier scan
- **Issue #5248** (closed): test-safehoused-service.sh: missing-binary preview test fails when a real safehoused binary is on PATH
- **PR #5250**: fix(tests): neutralize PATH resolution in safehoused-service missing-binary test
- **Issue #5247** (closed): Guard: markdown blockquote '>' inside a quoted heredoc body parsed as a redirect target, false worktree-isolation DENY
- **Issue #5246** (closed): Judge-filed follow-up issues state inferred root cause with the same weight as observed symptom (4 of 5 refuted on measurement)
- **PR #5254**: fix(judge,curator): separate observed evidence from inferred root cause in filed issues
- **Issue #5234** (closed): merge-pr.sh partial-increment detector matches 'Part of #N' in prose/backticks, reopening a deliberately-closed issue
- **PR #5239**: fix(guard): require structural anchor for Part of/Contributes to declarations in merge-pr.sh
- **Issue #5237** (closed): worktree-root.sh: fall back loudly when the resolved override root is unreadable (macOS TCC EPERM — stat succeeds, readdir fails)
- **PR #5241**: fix(worktree-root): fall back loudly when the resolved override root is unreadable
- **Issue #5238** (closed): recover-orphaned-shepherds.sh hard-requires pipx loom-tools and exits 0 when missing — /sweep all orphan recovery silently no-ops
- **Issue #5226** (closed): fix(guard): is_interpreter_opener() still fails OPEN on bare VAR= / sudo / timeout / quoted interpreter openers
- **PR #5230**: fix(guard): normalize wrapper, assignment and quoting shapes in interpreter-opener detection
- **Issue #5216** (closed): Guard: catastrophic rm-scope pattern false-positives on a backtick-quoted example inside a heredoc-wrapped gh pr comment body
- **PR #5227**: fix(guard): mask inert cat-heredoc bodies in text-carrying flag values
- **Issue #5184** (closed): all 8 non-loom managed repos are stuck on Loom 0.16.0 — they lack create-issue.sh, so canary agents cannot file issues while GraphQL is exhausted
- **Issue #5224** (closed): gh-cached: cache key omits repo/cwd, causing cross-repo data leakage on multi-repo hosts
- **PR #5229**: fix(gh-cached): scope the cache directory per repo to stop cross-repo leakage
- **PR #5228**: fix(resync): dereference symlinked source files in resync_tree's walk
- **Issue #5222** (closed): resync-installed.sh silently skips symlinked defaults/roles/*.md — consumer role prompts never refresh, while metadata claims current
- **Issue #5217** (closed): Guard: stash-scope:worktree-collision ask-tier blocks headless push/pop baseline-diff pattern with no human to answer
- **PR #5223**: feat(worktree): add per-issue stash-push/stash-pop clean-baseline pair
- **Issue #5213** (closed): Champion: detect dependency cycles between epics and their blocked dependents
- **PR #5225**: feat(champion): detect dependency cycles with a bounded cross-repo walk
- **Issue #5198** (closed): Guard regression: gh-api-rawfield-body-literal-at no longer denies a live invocation fed via an interpreter heredoc (bash <<EOF ... EOF)
- **PR #5205**: fix(guard): stop masking interpreter-fed heredoc bodies in gh-api-rawfield check
- **Issue #5214** (closed): Guard false positive: ask pattern for systemctl's restart subcommand fires on grep/jq introspection text
- **PR #5221**: fix(guard): segment-parse systemctl service-management ask patterns (#5214)
- **Issue #5210** (closed): dispatch_sweep: opaque 'failed to spawn sweep child' when workspace_root is not a registered workspace
- **PR #5218**: fix(daemon): reject unregistered dispatch_sweep workspace_root with a structured error
- **PR #5219**: feat: batch dispatch-wave bursts into one digest narration root
- **Issue #5208** (closed): recover-orphaned-shepherds.sh hard-fails without pipx loom-tools — no bash fallback for orphan recovery
- **PR #5215**: feat(sweep): surface orphan-recovery unavailability at the `all`-sentinel gate
- **Issue #5207** (closed): Mirror the quarantine-safety CLAUDE.md pointer into defaults/.loom/CLAUDE.md for consumer repos
- **PR #5212**: docs(claude-md): mirror quarantine-safety pointer into defaults/.loom/CLAUDE.md
- **Issue #5209** (closed): merge-pr.sh --auto aborts on GraphQL rate-limit instead of falling back to REST immediate merge
- **Issue #5196** (closed): CHANGELOG is not a running record: every release hand-reconstructs ~150 commits, and Unreleased doesn't survive a release
- **PR #5206**: feat(changelog): add a deterministic conventional-commit CHANGELOG generator
- **Issue #5194** (closed): Document the safe-edit pattern for managed repos — branching does not prevent quarantine
- **PR #5202**: docs(troubleshooting): document the safe-edit pattern for managed repos
- **Issue #5195** (closed): Curated issues should date-stamp volatile facts (counts, versions, "no bump needed") so Builders know what to re-verify
- **PR #5204**: docs(curator,builder): date-stamp volatile facts in curated issues
- **Issue #5177** (closed): squash-merged worktrees are never reaped ('HEAD unreachable'), so a small-disk worker silently decays to 1/6 capacity
- **PR #5189**: fix(clean): reap squash-merged worktrees and flag disk-bound dispatch
- **Issue #5191** (closed): test(daemon): adopt the live-state sandbox helper in test-loom-daemon-start.sh / test-loom-daemon-stop.sh
- **PR #5203**: test(daemon): adopt lib/live-state-sandbox.sh in the start + stop lifecycle suites
- **Issue #5187** (closed): Reclaim target/ inside a kept worktree without removing the whole worktree (AC3 of #5177)
- **PR #5201**: feat(worktree-reaper): reclaim build artifacts from kept worktrees
- **Issue #5193** (closed): version.sh misses mcp-loom/package-lock.json (stale at 0.15.0 for three releases) and 'version.sh check' reports a false all-clear
- **PR #5200**: fix(version): sync mcp-loom/package-lock.json in version.sh bump/check
- **Issue #5182** (closed): check-main-freshness.sh warns when behind origin but never when ahead — and ahead is the dangerous direction
- **PR #5197**: feat(scripts): warn when local main is ahead of origin, not just behind
- **Issue #5181** (closed): Guard false positive: gh-api-rawfield-body-literal-at fires on heredoc text that merely quotes the denied command
- **PR #5192**: fix(guard): scope gh-api-rawfield-body-literal-at to a heredoc-masked command copy
- **Issue #5179** (closed): test-loom-daemon-update.sh writes the LIVE host's daemon pid file — LOOM_PID_FILE is the one un-isolated state path, causing a false degraded liveness verdict
- **PR #5190**: test(daemon): sandbox every live daemon state path behind one helper + guard
- **Issue #5183** (closed): Test hermeticity: 4 shell suites false-fail on hosts with a real machine-level loom-daemon install
- **PR #5186**: test: isolate the machine-level daemon-bin fallback in 4 hermeticity fixtures
- **Issue #5175** (closed): curator.md Priority 2 fallback query doesn't exclude loom:operator-only
- **Issue #5173** (closed): Guard: stash-scope:main-checkout check doesn't thread a cd-prefix, unlike parse_force_ops (#5156)
- **Issue #5174** (closed): v0.18.0 release still missing loom-daemon-aarch64-unknown-linux-gnu asset (backfill or document, per #5167 follow-up)
- **Issue #5164** (closed): test(daemon): worktree_root env-seam leak makes worktree_reaper::test_unmerged_and_absent_pr_worktrees_are_never_reaped flaky under parallel threads
- **Issue #5167** (closed): release: aarch64-unknown-linux-gnu artifact fails to build — rust-toolchain.toml pin means 'targets:' is added to the wrong toolchain
- **Issue #5166** (closed): test-loom-dispatcher.sh not hermetic against inherited LOOM_RUNTIME/LOOM_WORKSPACE env
- **Issue #5128** (closed): Cut a release containing ac1917b6 so consumers can drop local account-health gitignore shims
- **Issue #5163** (closed): champion.md Priority 2/3 queue queries don't exclude loom:operator-only/loom:blocked
- **Issue #5135** (closed): orphan-process reaper cannot reap an orphan while a live sweep claims the same issue — the shape of #5110's own incident
- **Issue #5138** (closed): loom-daemon-update.sh has no --drain plumbing, so its default restart is the known-harmful one (#5084 telemetry loss, #5119 sweep kill)
- **Issue #5156** (closed): guard-destructive-generic.sh: force-op:protected false-asks on a worktree's own branch when a cd-prefix changes cwd mid-command
- **Issue #5155** (closed): Guard false positive: merge-redirect substring check still fires on positional (non-flag) script arguments

### 2026-08-03

- **Issue #5140** (closed): recover-orphans and loom-daemon-update.sh fail when CWD isn't the repo root — and recover-orphans reports 'No orphaned tasks found' after its query failed
- **Issue #5139** (closed): stale loom-* entry points are warned about on every run but never pruned — studio still carries 8 including loom-tokens
- **Issue #5137** (closed): /loom:sweep 'all' sentinel should flag operator-gated-but-unlabeled candidates at the confirmation gate
- **Issue #5145** (closed): Build/runtime failure on main: pnpm test:python references deleted loom-tools/ directory
- **Issue #5130** (closed): harden the orphan process reaper's fail-safes (daemon self-protection, live-claim probe, agent gate, age floor, freeze-first kill)
- **Issue #5133** (closed): test(daemon): observability::backfill env-seam leak makes default_backfill_state_path_is_under_loom_logs flaky under parallel test threads
- **Issue #5136** (closed): /loom:sweep daemon-dispatch path skips the pre-wave advisories (host-sleep, main-freshness) and transcript archival
- **Issue #5141** (closed): Build/runtime failure on main: pnpm check:ci fails — test:python targets deleted loom-tools/ directory
- **Issue #5110** (closed): an agent's background process tree survives the agent and nothing reaps it — an orphaned driver held worker-1 at load 65 for 5h52m, starving that host's own sweep
- **Issue #5134** (closed): Harden orphan-process reaper: self-blast-radius, watchdog-released-lock, min age, kill ordering
- **Issue #5123** (closed): Stranded loom:reviewing claim livelocks a PR: every later Judge correctly stands down, so a dead claimant blocks review forever
- **Issue #5119** (closed): loom-daemon restart on a busy systemd host SIGKILLs in-flight sweeps and leaves the daemon in failed state (Restart=on-success never fires)
- **Issue #5118** (closed): the autonomy-loss watchdog is non-functional fleet-wide: it reads a pid file neither host writes, LOOM_PID_FILE is ignored, and the file it does write is never refreshed
- **Issue #5083** (closed): health/status give no positive confirmation that telemetry is flowing — "exporting fine" and "silently never exported" both render as no observability section
- **Issue #5122** (closed): fix(guard): merge-redirect masking still bypassable via flag-captured cat-heredoc piped to a shell (regression from #5115)
- **Issue #5117** (closed): guard-destructive-generic.sh: mask_heredoc_bodies() residual blind spots (interpreter-fed heredocs; crafted false opener)
- **Issue #5109** (closed): Guard false-positive: loom:gh-pr-merge-redirect fires on the disallowed-CLI substring anywhere in a command, not just an actual invocation
- **Issue #5081** (closed): the "bootout kills in-flight sweeps" guidance looks stale post-#4982, and bootout+bootstrap (the only way to apply a plist env change) can leave the daemon down
- **Issue #5108** (closed): Build/runtime failure on main: mcp-loom config-resolver test points to retired loom-tools/ fixture path
- **Issue #5107** (closed): ci(dashboard-deploy): deploy failed at "Apply D1 migrations (idempotent)"
- **Issue #5084** (closed): sweeps adopted across a daemon restart never export sweep.completed/sweep.outcome — the data sits in the local journal while the backend silently under-counts
- **Issue #5087** (closed): guard-destructive-generic.sh: mask_heredoc_bodies() (#5000) fail-opens on unterminated/false heredoc opener, defeats write-confinement guard
- **Issue #5101** (closed): dashboard: SPA's fleet summary counts sweep-only 'unknown' hosts in its 'N hosts' headline, inflating the count in production
- **Issue #5082** (closed): dashboard: POST /admin/hosts 409s on a revoked host_id, so re-provisioning a retired identity requires hand-editing production D1
- **Issue #5078** (closed): dashboard: a host known only from orphaned sweep entries renders as a phantom fleet member, inflating host and active-sweep counts
- **Issue #5094** (closed): Role-doc idempotency/CAS checks use `echo "$VAR" | jq`, which zsh silently corrupts (false-negative marker matches)
- **Issue #5092** (closed): docs: document the host-rename procedure in observability.md
- **Issue #5076** (closed): observability: regression fixture — all-roles-failing must not read green anywhere (final AC of #5004)
- **Issue #5077** (closed): Role prompts still teach a bare gh issue create — route the 38 call sites through the #5070 helper
- **Issue #5086** (closed): guard-background-subagents.sh: async-subagent detector is a silent no-op — dispatch tool is named "Agent", not "Task"
- **Issue #5067** (closed): [Epic #4990] Phase 4: fleet add-worker consumes prebuilt artifacts (no Rust toolchain on workers)
- **Issue #5071** (closed): dashboard: revoking a host leaves its sweep: DO entries orphaned for up to 4h — a renamed/drained host double-counts live sweeps

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
