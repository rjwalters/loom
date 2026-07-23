# Loom Orchestration - Repository Guide

This repository uses **Loom** for AI-powered development orchestration.

**Loom Version**: 0.13.0
**Installation Date**: 2026-04-21

## What is Loom?

Loom is a CLI + daemon for AI-powered development orchestration. It coordinates AI development workers using git worktrees and a forge (GitHub or Gitea) as the coordination layer. It supports manual coordination (Manual Orchestration Mode), continuous autonomous orchestration via the Rust `loom-daemon` binary (MCP-level dispatch + pub/sub + monitoring), and GitHub Actions cron schedules for periodic support roles.

**Loom Repository**: https://github.com/rjwalters/loom

## Orchestration Architecture

Loom decomposes development into three coordination tiers, with the forge (GitHub / Gitea) as the shared state.

```
┌─────────────────────────────────────────────────────────────────┐
│                    Tier 3: Human Observer                       │
│  - Watches system health, intervenes on blocked work            │
│  - Overrides Champion on controversial proposals                │
└─────────────────────────────────────────────────────────────────┘
                              │ observes/intervenes
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│              Tier 2: loom-daemon + GitHub Actions cron          │
│  loom-daemon (Rust) — MCP dispatch_sweep, list_sweeps,          │
│    get_sweep_status, cancel_sweep, event bus (pub/sub)          │
│  .github/workflows/loom-*.yml — periodic support roles          │
└─────────────────────────────────────────────────────────────────┘
                              │ spawns/triggers
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Tier 1: /loom:sweep <issue>                  │
│  Single-issue lifecycle: Curator → Builder → Judge → Doctor →   │
│  Merge. One detached process per issue. Mode C: PR-set sweeps.  │
└─────────────────────────────────────────────────────────────────┘
                              │ dispatches
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Worker Roles                                 │
│  Curator, Builder, Judge, Doctor, etc.                          │
│  - Execute single tasks (curate issue, build feature, review)   │
└─────────────────────────────────────────────────────────────────┘
```

| Tier | Entry point | Purpose | Mode |
|------|-------------|---------|------|
| Tier 3 | Human | Oversight — approve proposals, handle edge cases | Observer |
| Tier 2 | `loom-daemon` (MCP) + GH Actions cron | Multi-issue dispatch + scheduled support roles | Continuous / cron |
| Tier 1 | `/loom:sweep <issue>` | Single-issue lifecycle (Curator → Merge) | Per-issue |
| Tier 0 | `/builder`, `/judge`, etc. | Task execution — single focused work units | Per-task |

**Use `/loom:sweep <issue>`** when you have a specific issue to implement (interactively or from a script). The skill probes for a running `loom-daemon` + multi-account token pool (Stage -1) and delegates dispatch when both are available; otherwise it falls through to in-process subagent dispatch.
**Use `mcp__loom__dispatch_sweep`** from a Claude Code session to enqueue work directly against the running `loom-daemon` for autonomous multi-account batches.
**Enable the GitHub Actions cron workflows** under `.github/workflows/loom-*.yml` for periodic Champion / Curator / Judge / Auditor / Guide ticks.

## Usage Modes

### 1. Manual Orchestration Mode (MOM)

Use Claude Code terminals with specialized roles for hands-on development:

1. Open Claude Code in this repository
2. Use slash commands: `/builder`, `/judge`, `/curator`, etc.
3. Each terminal acts as a specialized agent

### 2. Single-issue lifecycle: `/loom:sweep <issue>`

Run a complete Curator → Builder → Judge → Doctor → Merge lifecycle on one issue:

```text
/loom:sweep 123
```

Or from a script:

```bash
claude -p "/loom:sweep 123" --dangerously-skip-permissions
```

Sweep also has a **PR-set mode (Mode C, #3384)** that drives Judge / Doctor → Judge / Merge from an existing open-PR set without re-running Curator or Builder:

```text
/loom:sweep --prs 456 789
```

Checkpoints (#3373) under `.loom/sweep-checkpoint/issue-<N>.json` survive crashes — restarting `/loom:sweep N` resumes from the last completed phase.

### 3. Daemon Mode (`loom-daemon` + MCP tools)

The Rust `loom-daemon` binary is the Tier 2 dispatch backend. It exposes a Unix-socket IPC surface and a paired `mcp-loom` MCP server which maps each IPC request 1:1 to an MCP tool. An operator running `/loom:sweep` in a Claude Code session — or any MCP client — interacts with the daemon via:

| MCP tool | Purpose | Phase |
|----------|---------|-------|
| `mcp__loom__dispatch_sweep` | Dispatch a sweep for an issue (multi-account token rotation via `spawn-claude.sh`) | A (#3452) |
| `mcp__loom__list_sweeps` | Enumerate running sweeps in the in-memory registry | A (#3452) |
| `mcp__loom__publish_event` | Publish a sweep-lifecycle event on the in-memory bus | B (#3453) |
| `mcp__loom__subscribe_to_events` | Stream topic-filtered events to a subscriber | B (#3453) |
| `mcp__loom__get_sweep_status` | Inspect a running sweep's state | C (#3455) |
| `mcp__loom__tail_sweep_log` | Tail the per-sweep log file | C (#3455) |
| `mcp__loom__cancel_sweep` | Cancel a running sweep (SIGTERM → grace → SIGKILL) | C (#3455) |
| `mcp__loom__tail_event_bus` | Tail the event bus without subscribing to a topic | C (#3455) |

**Event taxonomy (frozen for v0.10.0)**: `sweep.issue.{N}.phase`, `sweep.issue.{N}.blocker`, `sweep.issue.{N}.exited`, `sweep.issue.{N}.crashed`, `sweep.global.dispatch`, `sweep.global.completed`. New topics require a follow-up issue.

**`/loom:sweep` backend detection (Stage -1, Phase D #3454)**: the skill probes whether the daemon is reachable (a Ping over the IPC socket with a 500ms timeout) AND whether a multi-account token pool exists (`.loom/tokens/` contains ≥ 2 `ACCOUNT_KEY_*` entries). **Strict AND** — either probe failing falls through to in-process subagent dispatch (the existing Mode A/B/C lifecycle). Mode C (`--prs`) always uses subagent dispatch; the daemon does not handle PR-set dispatch in v0.10.0. The `--no-daemon` flag forces subagent dispatch unconditionally.

The daemon itself is **not** a work generator. It does not poll the forge for `loom:issue` items; it does not maintain a `shepherd-N` pool; it does not drive support roles on cron. Those responsibilities live in `mcp__loom__dispatch_sweep` (operator-driven enqueue) and the GitHub Actions cron workflows.

For full surface documentation — IPC request/response variants, event-bus internals, registry behaviour, reaper semantics — see [`.loom/docs/daemon-reference.md`](.loom/docs/daemon-reference.md).

> **Note**: `/loom:sweep` also supports a **PR-set mode** (Mode C, #3384) via `--prs <pr-number-list>` or NL phrases like "all open `loom:pr`" — drives Judge / Doctor → Judge / Merge from an existing open-PR set without re-running Curator or Builder. Mode C always uses subagent dispatch (see Stage -1 above).

> **Legacy spawn loop (removed)**: `defaults/scripts/spawn-loop.sh` (Phase 1, #3374) was **removed in v0.11.0**. Use `mcp__loom__dispatch_sweep` against `loom-daemon` instead. See [`docs/migration/v0.10.0-shepherd-deprecation.md`](docs/migration/v0.10.0-shepherd-deprecation.md).

### 4. Scheduled Support Roles (opt-in)

GitHub Actions workflows under `.github/workflows/loom-*.yml` run the periodic support roles (Champion, Curator, Judge, Auditor, Guide) on cron schedules (#3375). Each workflow checks out the repo, installs the Claude CLI, and runs `claude -p "/<role>" --dangerously-skip-permissions` for one tick of work — no Loom-side state file, no long-running process.

| Workflow | Role | Schedule (commented) |
|----------|------|----------------------|
| `loom-champion.yml` | `/champion` | `*/10 * * * *` |
| `loom-curator.yml`  | `/curator`  | `*/5 * * * *`  |
| `loom-judge.yml`    | `/judge`    | `*/5 * * * *`  |
| `loom-auditor.yml`  | `/auditor`  | `*/10 * * * *` |
| `loom-guide.yml`    | `/guide`    | `*/15 * * * *` |

**Disabled by default.** Every shipped workflow has its `schedule:` block commented out so forks don't burn Actions minutes accidentally. To opt in on a fork:

1. Add a `CLAUDE_API_KEY` repository secret (Settings -> Secrets and variables -> Actions). Workflows run on a single API key — token rotation is for per-task spawns only; scheduled support roles are predictable load that doesn't benefit from rotation.
2. Uncomment the `schedule:` / `- cron:` lines in each `.github/workflows/loom-*.yml` you want to enable.
3. Optionally trigger a run via `workflow_dispatch` (the Actions UI's "Run workflow" button) to smoke-test before the next scheduled tick.

Architect and Hermit cadence (work-generation triggers) is intentionally out of scope here — see follow-up #3381 (Phase 2d).

## Agent Roles

### Worker Roles

| Role | File | Purpose | Mode |
|------|------|---------|------|
| Builder | `builder.md` | Implement features and fixes | Manual |
| Judge | `judge.md` | Evaluate pull requests | Cron 5min (GH Actions) |
| Champion | `champion.md` | Evaluate proposals, auto-merge PRs | Cron 10min (GH Actions) |
| Curator | `curator.md` | Enhance and organize issues | Cron 5min (GH Actions) |
| Architect | `architect.md` | Create architectural proposals | Manual (cadence #3381) |
| Hermit | `hermit.md` | Identify simplification opportunities | Manual (cadence #3381) |
| Doctor | `doctor.md` | Fix bugs and address PR feedback | Manual |
| Guide | `guide.md` | Prioritize and triage issues | Cron 15min (GH Actions) |
| Driver | `driver.md` | Direct command execution | Manual |
| Auditor | `auditor.md` | Validate main branch build and runtime | Cron 10min (GH Actions) |

Full role definitions: `.loom/roles/*.md`.

> **Note**: the historical `shepherd.md` (single-issue orchestrator) role file was removed in v0.10.0 along with the `/shepherd` slash command — see [the migration guide](docs/migration/v0.10.0-shepherd-deprecation.md). Its orchestration responsibilities moved to `/loom:sweep` (Tier 1) and the `loom-daemon` + GH Actions cron (Tier 2). The `loom.md` role file is preserved and documents the daemon-mode operator surface: a Claude Code session that observes the running `loom-daemon` via MCP tools (`mcp__loom__list_sweeps`, `mcp__loom__get_sweep_status`, `mcp__loom__subscribe_to_events`) and dispatches new work via `mcp__loom__dispatch_sweep`. The historical Python brain (`loom_tools/daemon_v2/`) is gone; the MCP-level surface is the supported coordination point. The worker-role markdown files above are unchanged.

## Label-Based Workflow

Agents coordinate through GitHub labels. See `.github/labels.yml` for full definitions.

### Label Flow

**Issue Lifecycle**:
```
(created) → loom:triage → loom:curating → loom:curated → loom:issue → loom:building → (closed)
           ↑ filer        ↑ Curator        ↑ Curator      ↑ human     ↑ Builder
                                                          (or Champion
                                                           in --merge mode)
```

See `.github/labels.yml` for the authoritative `Applied by:` field on every label.

**PR Lifecycle**:
```
(created) → loom:review-requested → loom:pr → (auto-merged)
           ↑ Builder                ↑ Judge    ↑ Champion
```

**Proposal Lifecycle**:
```
(created) → loom:architect/loom:hermit/loom:auditor → (evaluated) → loom:issue
           ↑ Architect/Hermit/Auditor                 ↑ Champion    ↑ Ready for Builder
```

**Epic Lifecycle**: `loom:epic` → Champion creates phased `loom:architect` + `loom:epic-phase` issues.

> **Note on label cleanup**: Loom intentionally does **not** remove labels from closed issues or merged PRs (e.g., `loom:pr` remains on merged PRs). Labels on closed/merged items are harmless — all agents filter by open state — and skipping post-close label removal saves gh API calls. Do not implement label cleanup on merge/close (see issue #2838).

## Git Worktree Workflow

Loom uses git worktrees to isolate agent work.

**Issue Worktrees** (`.loom/worktrees/issue-N`): Issue-specific work for Builder agents.

### Creating Worktrees

```bash
# Claim issue and create worktree
gh issue edit 42 --remove-label "loom:issue" --add-label "loom:building"
./.loom/scripts/worktree.sh 42
cd .loom/worktrees/issue-42

# Work, commit, push, create PR
git push -u origin feature/issue-42
gh pr create --label "loom:review-requested"
```

### Best Practices

- Always use `./.loom/scripts/worktree.sh <issue-number>` (it writes a `.loom-managed` sentinel that authorizes cleanup)
- Never run `git worktree` directly (helper prevents nested worktrees) — to remove one managed worktree on demand, use `./.loom/scripts/worktree.sh remove <issue-number>` (sentinel-honoring, idempotent, deletes the local branch; `loom-clean` remains the bulk path)
- Loom-managed worktrees (under `.loom/worktrees/` with the `.loom-managed` sentinel) are auto-removed when their PR merges. User-provisioned worktrees at other paths are never removed by Loom — set `LOOM_PRESERVE_WORKTREE=1` to disable cleanup globally for a session.

### Merging PRs

**Never use `gh pr merge`** -- always use `./.loom/scripts/merge-pr.sh <PR_NUMBER>` instead. The `gh pr merge` command attempts a local checkout which fails when the PR branch is linked to a worktree. The merge script merges via the forge API directly and handles worktree cleanup automatically.

```bash
./.loom/scripts/merge-pr.sh <PR_NUMBER>         # Standard merge with worktree cleanup
./.loom/scripts/merge-pr.sh <PR_NUMBER> --auto   # Enable auto-merge instead of immediate merge (queues until checks pass; on CLEAN PRs falls back to immediate merge)
./.loom/scripts/merge-pr.sh <PR_NUMBER> --dry-run # Preview without merging
```

## Development Workflow

### Sweep Lifecycle (MANDATORY)

When implementing issues — whether manually, via `/loom:sweep`, or by spawning subagents — **all stages of the lifecycle must be executed in order**. Do not skip stages.

```
Curator → Builder → Judge → Doctor (if needed) → Merge
```

| Stage | What happens | Skip allowed? |
|-------|-------------|---------------|
| **Curator** | Enrich the issue with technical details, acceptance criteria, scope | No |
| **Builder** | Implement, test, commit, create PR | No |
| **Judge** | Review the PR, approve or request changes | No |
| **Doctor** | Fix issues from judge feedback | Only if judge approves |
| **Merge** | Champion auto-merges approved PRs | No |

**When spawning subagents to handle an issue**: each subagent must run the full lifecycle, not just the builder phase. If parallelizing multiple issues, each agent must independently execute Curator → Builder → Judge → Doctor → Merge. Simply creating a PR and labeling it `loom:review-requested` is only the Builder stage — the work is not complete until the PR has been reviewed and merged.

**When using `/loom:sweep`**: the skill handles all stages automatically. Prefer `/loom:sweep <issue>` over manual orchestration to avoid accidentally skipping stages.

### Builder Workflow

1. Find issue: `gh issue list --label="loom:issue"`
2. Claim: `gh issue edit 42 --remove-label "loom:issue" --add-label "loom:building"`
3. Create worktree: `./.loom/scripts/worktree.sh 42 && cd .loom/worktrees/issue-42`
4. Implement, test, commit
5. Create PR: `git push -u origin feature/issue-42 && gh pr create --label "loom:review-requested" --body "Closes #42"`

### Judge Workflow

1. Find PR: `gh pr list --label="loom:review-requested"`
2. Review: `gh pr checkout 123`
3. Approve: `gh pr comment 123 --body "LGTM! Approved." && gh pr edit 123 --remove-label "loom:review-requested" --add-label "loom:pr"`
4. Or request changes: `gh pr comment 123 --body "Changes needed: ..." && gh pr edit 123 --remove-label "loom:review-requested" --add-label "loom:changes-requested"`

**Note**: Use `gh pr comment` instead of `gh pr review --approve` — GitHub's API prevents self-review, and Loom agents often create and review the same PR. Labels are the coordination mechanism.

### Curator Workflow

1. Find unlabeled issues: `gh issue list --search "-label:loom:issue -label:loom:building -label:loom:architect -label:loom:hermit -label:loom:curated -label:loom:curating" --state open` (gh ANDs `--label` values and has no `!`/`,` negation syntax, so a `--label="!loom:issue,..."` filter matches a literal label no issue carries and always returns empty; use `-label:` search terms instead)
2. Enhance issue with technical details
3. Mark curated: `gh issue edit 42 --add-label "loom:curated"`

### Overnight / long-running orchestration: keep the host awake (#3350)

`/loom:sweep` automatically runs `./.loom/scripts/check-host-sleep.sh` at startup and warns when the host can sleep. This is **advisory only** — Loom never blocks on it. Heed the warning before walking away from a long run.

- **macOS:** user-idle sleep assertions (Amphetamine, `caffeinate -dimsu`, etc.) do **not** reliably defeat Maintenance Sleep on Apple Silicon. Use `sudo pmset -c sleep 0` for AC-only sleep disable, or flip your sleep manager's "allow system sleep when display is off" toggle to OFF.
- **systemd Linux:** wrap the session in `systemd-inhibit --what=idle:sleep --who=loom --why=loom -- <cmd>`.

Manual invocation: `./.loom/scripts/check-host-sleep.sh` (or `--quiet` for stderr-only output).

### Keeping installed `.loom/` copies fresh after a pull (#3770 detect → #3777 resync)

The installed `.loom/hooks/` and `.loom/scripts/` copies the harness actually executes are synced from `defaults/` **at install time**. A `git pull` that merges a hook/script fix updates `defaults/` but **not** the installed copies — so a session can run stale hooks/scripts indefinitely (the incident: a merged `guard-destructive.sh` fix kept prompting until hand-copied).

This is a **detect → fix** pair:

- **Detect (#3770)** — `/loom:sweep` runs `./.loom/scripts/check-main-freshness.sh` at startup. When local `main` is behind `origin/main` it prints a non-blocking warning and flags any installed file that differs from its `defaults/` counterpart. Advisory only; it never pulls, merges, or resets.
- **Fix (#3777)** — `./.loom/scripts/resync-installed.sh` refreshes the installed `.loom/hooks/*` and `.loom/scripts/*` from `defaults/`. Idempotent (a no-op when in sync), reports per-file `updated`/`created`/`unchanged`/`skipped`, and only ever touches files that exist in `defaults/` (repo-specific hooks with no `defaults/` counterpart are left alone).

The intended flow is **"freshness warning says you're stale → run resync"**:

```bash
git merge --ff-only origin/main             # bring defaults/ current
./.loom/scripts/resync-installed.sh --dry-run   # preview what would change
./.loom/scripts/resync-installed.sh             # apply
```

`--dry-run` makes no changes and exits `2` when drift is detected (so it doubles as a check). To pin an intentional per-repo customization so resync never overwrites it, list its relative path (e.g. `hooks/guard-destructive.sh`) — one per line — in `.loom/resync-ignore`; matching files are reported `skipped`. A full `loom-daemon init` / installer run already performs the equivalent recursive copy, so a normal reinstall keeps the copies current too.

## Configuration

### Workspace Configuration

Configuration stored in `.loom/config.json` (committed to git for team sharing):

```json
{
  "nextAgentNumber": 3,
  "terminals": [
    {
      "id": "terminal-1",
      "name": "Builder",
      "role": "builder",
      "roleConfig": {
        "workerType": "claude",
        "roleFile": "builder.md",
        "targetInterval": 0,
        "intervalPrompt": ""
      }
    }
  ]
}
```

### Daemon Configuration (Tier 2)

The Rust `loom-daemon` binary is the load-bearing Tier 2 dispatch backend. It is a single long-lived process that holds the sweep registry, the event bus, and the reaper task in memory — there is no on-disk state file the operator needs to touch. See [`.loom/docs/daemon-reference.md`](.loom/docs/daemon-reference.md) for the full surface and [`docs/migration/v0.10.0-shepherd-deprecation.md`](docs/migration/v0.10.0-shepherd-deprecation.md) for the migration narrative away from the legacy Python brain.

**Autonomous mode (config + start/stop, #3813)**: the daemon's autonomous work finder (#3810) and reactive main-health gate (#3812) can be enabled and tuned entirely from committed config — an `autonomous` block in `.loom/config.json` — with env vars still overriding for a single run (precedence **env > config > default**; an absent block is byte-for-byte the pre-#3813 env-only behavior):

```json
{
  "autonomous": {
    "workFinder": { "enabled": true, "intervalSecs": 60, "maxConcurrent": 5 },
    "mainHealthGate": { "enabled": true }
  }
}
```

Start/stop the **raw daemon process** (distinct from the tmux `loom start|stop` pool) with dedicated wrappers that run the advisory host-sleep check, write a PID file (`.loom/.daemon.pid`), surface the singleton-guard refusal, and shut down cleanly on **SIGTERM** (not just Ctrl-C):

```bash
./.loom/scripts/cli/loom-daemon-start.sh              # work finder + health gate, backgrounded
./.loom/scripts/cli/loom-daemon-start.sh --from-config # enable strictly per .loom/config.json
./.loom/scripts/cli/loom-daemon-stop.sh               # SIGTERM → grace → SIGKILL
```

A clean stop leaves in-flight `/loom:sweep` children **running** (they survive a daemon restart by design; use `mcp__loom__cancel_sweep` to actively cancel). The full config table, start/stop flags, and a scripted end-to-end acceptance playbook are in [`.loom/docs/daemon-reference.md`](.loom/docs/daemon-reference.md) §Operability and [`docs/autonomous-mode-e2e.md`](docs/autonomous-mode-e2e.md).

**Per-sweep logs** live at `.loom/logs/sweep-issue-<N>.log` and are tailable via `mcp__loom__tail_sweep_log`.

**Sweep checkpoints** (`.loom/sweep-checkpoint/issue-<N>.json`, gitignored): when a sweep child crashes mid-flight, the checkpoint records the last completed phase. On the next dispatch the sweep skill resumes from that phase. The exact schema is owned by the sweep skill (#3373).

**Scheduled support roles** run as separate GitHub Actions cron jobs — see `.github/workflows/loom-*.yml`. They have no persistent state on the Loom side; each tick is a fresh `claude -p "/<role>" --dangerously-skip-permissions` invocation.

> **Legacy spawn-loop state (obsolete)**: the v0.9.x state file `.loom/spawn-loop-state.json` was written by `spawn-loop.sh`, which was **removed in v0.11.0**. Nothing writes it anymore. Operators who need to observe running sweeps should call `mcp__loom__list_sweeps` against the daemon instead.

### Custom Roles

Create custom roles by adding files to `.loom/roles/`:

```bash
cat > .loom/roles/my-role.md <<EOF
# My Custom Role
You are a specialist in {{workspace}}.
## Your Role
...
EOF
```

### Branch Rulesets

Loom works best with a GitHub ruleset enabled on the default branch. During installation:

```bash
./scripts/install-loom.sh /path/to/repo  # Interactive, prompts for ruleset
./scripts/install-loom.sh --yes /path/to/repo  # Non-interactive, skip ruleset
```

Manual configuration: `./scripts/install/setup-branch-protection.sh /path/to/repo main`

### Repository Settings

Configure merge settings during installation or manually:

```bash
./scripts/install/setup-repository-settings.sh /path/to/repo
./scripts/install/setup-repository-settings.sh /path/to/repo --dry-run  # Preview
```

Settings applied: squash merge only (no merge commits/rebase), delete branches on merge, auto-merge enabled.

### Multi-Account Token Pool

For environments that rotate among multiple Claude OAuth accounts, Loom can bootstrap a per-account token pool at `.loom/tokens/` from numbered `ACCOUNT_EMAIL_N` / `ACCOUNT_KEY_N` / `ACCOUNT_TOKEN_FILE_N` triples:

```env
ACCOUNT_EMAIL_1=user1@example.com
ACCOUNT_KEY_1=sk-ant-oat01-...
ACCOUNT_TOKEN_FILE_1=user1.token
```

Run `loom-tokens bootstrap` to materialize the pool:

```bash
loom-tokens bootstrap            # Idempotent — only writes new/missing tokens.
loom-tokens bootstrap --dry-run  # Preview + print the effective merged account set.
loom-tokens bootstrap --force    # Overwrite on-disk tokens that have drifted from source.
```

Each account becomes `.loom/tokens/<file>.token` (mode `0600`). An `index.json` manifest is written alongside with sha256 fingerprints (8 chars) for drift detection plus each account's `source` (home/repo) — **no secret material is stored in the manifest**. Numbering gaps are allowed; partial triples are skipped with a warning.

`.loom/tokens/` is gitignored. The pool is consumed by external rotation logic (e.g. a `claude-wrapper.sh` that picks the least-used token); only the bootstrap step is provided here.

#### Account sources: claude-monitor-first + per-repo (#3695, #3698, #3704)

Rather than re-declaring the same account triples in every repo's `.env`, declare them **once** in the shared claude-monitor master and let each workspace add or override on top of it. Sources are merged by account email in precedence order:

| Source | Default location | Override |
|--------|------------------|----------|
| **claude-monitor master** (primary) | `~/.claude-monitor/accounts.env` | `LOOM_CLAUDE_MONITOR_DIR` env var (directory) |
| **Repo-local** | `<repo>/.loom/accounts.env` if present, else legacy `<repo>/.env` | `--env <path>` on `bootstrap` |
| **Home master** (opt-in only, #3704) | *no default location* — read **only** when explicitly pointed at | `LOOM_ACCOUNTS_ENV` env var (a path enables it, `""` disables); `--home-env <path>` / `--no-home` on `bootstrap` |

**Default resolution is claude-monitor → repo `.env`.** The `~/.loom/accounts.env` home master is **no longer auto-read** (#3704 retired the default location): it is consulted only when an operator opts in via `LOOM_ACCOUNTS_ENV=<path>` (conventionally `~/.loom/accounts.env`) or `--home-env <path>`. This retires the default *location*, not the *capability*.

`loom-tokens bootstrap` reads the available sources and **merges them by account email** (`ACCOUNT_EMAIL`), with the higher-precedence source winning:

- An email present **only in a lower-precedence source** is inherited into the pool.
- An email present **only in a higher-precedence source** is added.
- An email present in **both** → the higher-precedence entry overrides (e.g. to rotate a key or repoint the token file).

To *exclude* an inherited account from one repo, pin the subset you want with `loom-tokens pin` — the merge only ever adds/overrides, never subtracts. The effective merged set (and where each account came from) is printed by `bootstrap` and `bootstrap --dry-run`. A repo with only a legacy `.env` and no other source behaves exactly as before.

> **Secrets**: `~/.claude-monitor/accounts.env`, the opt-in `~/.loom/accounts.env`, and the repo-local `.loom/accounts.env` all hold raw OAuth keys. The repo-local file and `.loom/tokens/` are gitignored (installer- and `loom-daemon init`–managed); keep any home-level master `0600` and outside any repo. A repo can rely entirely on the claude-monitor master with no local account file at all.

#### Account health probe + ranking

Once bootstrapped, `loom-tokens check` probes each account for current rate-limit headers and (optionally) writes a JSON ranking that the spawn-time selector can consume:

```bash
loom-tokens check                  # Probe + print human table
loom-tokens check --ranking        # Probe + write .loom/tokens/.ranking atomically
loom-tokens check --json           # Emit full JSON report to stdout
./.loom/scripts/probe-tokens.sh    # Cron-friendly wrapper for periodic invocation
```

The probe sends a minimal `POST /v1/messages` request (1 input, 1 output token) and parses rate-limit response headers. The header parser matches by **suffix** (`-5h-utilization`, `-7d-utilization`, `-7d-reset`) so future renames of the `anthropic-ratelimit-tokens-*` prefix still work; the full header set is logged on the first probe of each run.

Status assignment: `available` (utilizations < 95%), `exhausted` (`7d_utilization >= 0.95`), `rate_limited` (current 429), `blocked` (401 auth failure or token listed in `.bad_tokens`). Probe failures (network, timeout, 5xx) are logged and skipped — one bad account does not abort the run.

OAuth tokens shaped `sk-ant-oat01-*` are sent with `Authorization: Bearer` + `anthropic-beta: oauth-2025-04-20`; plain API keys use `x-api-key`.

Cron example (probe every 10 minutes):

```cron
*/10 * * * * cd /path/to/repo && ./.loom/scripts/probe-tokens.sh --ranking >> .loom/logs/probe-tokens.log 2>&1
```

See `.loom/docs/troubleshooting.md` for detailed troubleshooting including:
- Cleaning up stale worktrees and branches
- Stuck agent detection and intervention
- Daemon troubleshooting (registry, event bus, reaper)
- Common issues and solutions

**Quick fixes**:

```bash
loom-clean --force                       # Clean stale worktrees/branches
loom-recover-orphans --recover           # Recover orphaned loom:building issues
gh label sync --file .github/labels.yml  # Re-sync labels (GitHub only)
# Cancel a running sweep: mcp__loom__cancel_sweep --sweep_id <id>
```

## Custom Guard Hooks

Loom ships Bash `PreToolUse` guard hooks (`defaults/hooks/guard-destructive.sh`) that block or ask on destructive commands. Several category toggles let a repo opt out of checks that are a category error for it — see `defaults/CLAUDE.md` → "Custom Guard Hooks" for the full catalog (`guards.sqlDdl`, `guards.cloudCli`, `guards.rmScope`, `guards.forceScope`). The read-only fast-path toggle is documented below.

### Read-Only Fast-Path Guard Toggle (`guards.readOnlyFastPath` / `LOOM_GUARD_READONLY_FASTPATH`)

`guard-destructive.sh` fires before **every** Bash tool call. In Bash-dense sessions almost every call is obviously read-only (`git status`, `ls`, `grep`, `aws … describe*`, `gh … list`), yet each otherwise runs the full deny/ask gauntlet (~37 `grep`/`awk`/`sed` forks plus a `git rev-parse`, ~179ms) before allowing. The read-only fast path (issue #3687) short-circuits that case to a **silent** `allow` (exit 0, zero output, no logging) via one bash-builtin structural test (zero forks) plus one lazy `jq` config read, running before the repo-root `git rev-parse` and every deny/ask array.

The fast path is **on by default**, resolved highest-precedence first:

1. **`LOOM_GUARD_READONLY_FASTPATH` env var** — `0`/`false`/`no` disables (full-path checking restored byte-for-byte); `1`/`true`/`yes` forces on. Overrides config.
2. **`.loom/config.json` → `guards.readOnlyFastPath`** — default `true` when absent; set `false` to disable.
3. **Default** — `true`.

**Security**: the fast path is a guard bypass by construction, so admission is purely structural. A command is fast-pathed only when it contains **none** of `;` `&` `|` `<` `>` backtick `$(` newline (excludes chaining/piping/redirection/substitution) **and** its first token exactly matches the built-in allowlist: `git status|log|diff|show` (bare, no `git -C …`), `ls`, `grep`, `rg`, `jq`, `wc`, `head`, `tail`, `test`, `[`, `[[` (any args), `find` (any args **except** a dangerous action-primary — `-delete`/`-exec`/`-execdir`/`-ok`/`-okdir`/`-fls`/`-fprint`/`-fprint0`/`-fprintf` — which routes it to the full path, #3772), `gh <noun> view|list`, `aws <service> describe*|get*|list*`, `aws s3 ls`. Wrappers (`bash -c`, `eval`, `sudo …`, `env …`) are excluded automatically because their first token isn't allowlisted. So `git status && git push --force origin main` takes the full path and is still denied.

**`cat` and `ssh` are deliberately excluded**: `cat` has an existing `.ssh`/`.aws/credentials` ASK carve-out a blanket fast-path would skip, and `ssh` wraps an opaque remote payload the catastrophic scan still covers.

**Optional** `guards.readOnlyFastPathExtra` is an extend-only array of **literal first-word commands** added to the allowlist without hand-editing the installer-managed `.claude/settings.json`:

```json
{ "guards": { "readOnlyFastPath": true, "readOnlyFastPathExtra": ["psql"] } }
```

> **Note**: `jq`/`wc` (and `head`/`tail`/`test`/`find`) are part of the built-in default allowlist as of #3772 — `readOnlyFastPathExtra` is now only for a genuinely-custom bare read-only word.

> **Warning**: each entry is a full-generality bypass for that command word (all arguments) — only add bare, argument-independent read-only utilities, never scripts or anything that could wrap a mutating call.

Disabling the fast path never weakens any deny/ask rule; a missing/malformed `.loom/config.json` falls through to fast-path-ON.

## MCP Hooks

Loom provides a unified MCP server (`mcp-loom`) for programmatic control. See the mcp-loom package README for full tool documentation.

**Key tools**: `list_terminals`, `create_terminal`, `send_terminal_input`, `get_agent_metrics`, `trigger_start`, `stop_engine`

**Setup**:
```bash
./scripts/setup-mcp.sh  # Generates .mcp.json
```

**Agent metrics** for self-aware behavior:
```bash
./.loom/scripts/agent-metrics.sh --role builder  # Check your effectiveness
mcp__loom__get_agent_metrics --command summary --period week
```

## Token Rotation (Multi-Account Claude Code)

For Pro/Max plans, Loom supports rotating between multiple Claude Code OAuth tokens. This spreads load across accounts and recovers automatically when a single token hits its weekly limit.

### Setup

1. Declare account credentials in a default source — the shared claude-monitor master `~/.claude-monitor/accounts.env` (primary) or per-repo in `<repo>/.loom/accounts.env` (falls back to legacy `<repo>/.env`). The `~/.loom/accounts.env` home master is **opt-in only** since #3704 (no longer auto-read); point `LOOM_ACCOUNTS_ENV=~/.loom/accounts.env` (or `--home-env <path>`) at it to enable:
   ```env
   ACCOUNT_EMAIL_1=account-one@example.com
   ACCOUNT_KEY_1=sk-ant-oat01-...
   ACCOUNT_TOKEN_FILE_1=account-one.token
   ACCOUNT_EMAIL_2=account-two@example.com
   ACCOUNT_KEY_2=sk-ant-oat01-...
   ACCOUNT_TOKEN_FILE_2=account-two.token
   ```
   The claude-monitor, repo-local, and (opt-in) home sources are **merged by email**, with the higher-precedence source overriding/adding (see "Account sources: claude-monitor-first + per-repo" above). Keep any home-level master `0600` and outside any repo.
2. Run `loom-tokens bootstrap` to materialize the merged set into per-account `.token` files in `.loom/tokens/` (mode 0600, parent dir 0700). See issues #3234, #3695.
3. Spawn agents through `.loom/scripts/spawn-claude.sh` instead of invoking `claude` directly. The wrapper selects a token using a 3-tier algorithm (ranking → allowlist → random), exports `CLAUDE_CODE_OAUTH_TOKEN`, then `exec`s `claude` (or pass `--use-wrapper` to layer on top of `claude-wrapper.sh` for retry behavior).

### Selection algorithm (`loom_tools.tokens.select`)

Three tiers, falling through to the next when the current tier yields nothing:

1. **Ranking** — `.loom/tokens/.ranking` (pipe-delimited `name|status`, refreshed every <10 min). Picks the first non-`exhausted`/non-`blocked` token.
2. **Allowlist** — `.loom/tokens/.allowlist` (one name per line). Random pick from allowed accounts.
3. **Random** — uniform pick from all `*.token` files.

Tokens marked bad in `.loom/tokens/.bad_tokens` are skipped at every tier.

### Bad-token tracking (`loom_tools.tokens.bad_tokens`)

When a token returns `TOKEN_EXPIRED` or `TOKEN_EXHAUSTED`, callers append an entry to `.loom/tokens/.bad_tokens`. Writes are guarded with a `mkdir`-based lock (POSIX-atomic, macOS-compatible — `flock` is **not** used because it isn't available on stock macOS). Reads use word-boundary regex so `agent-1` and `agent-10` don't collide.

### Error classification (`.loom/scripts/lib/classify-error.sh`)

The `classify_error <output> <exit_code>` function returns one of `SUCCESS`, `TIMEOUT`, `CWD_DELETED`, `TOKEN_EXPIRED`, `TOKEN_EXHAUSTED`, `RECOVERABLE`. Critical fix from #3233: exit code is checked **before** output substring matching — clean exits (`exit_code == 0`) always return `SUCCESS` regardless of stdout content. The previous lean-genius implementation returned `RECOVERABLE` for clean exits whose stdout contained substrings like `500` or `rate limit`.

### Worktree handling

When invoked from a worktree, `spawn-claude.sh` resolves the canonical repo root via `git rev-parse --git-common-dir` and locates `.loom/tokens/` there — never in the worktree's path. This avoids each worktree maintaining its own bad-tokens list.

### Hard-fail on missing pool

`spawn-claude.sh` exits `78` (`EX_CONFIG`) with a message instructing the user to run `loom-tokens bootstrap` when `.loom/tokens/` is absent or all tokens are bad. It does **not** silently fall back to keychain — that path belongs in `loom-daemon` (#3236), and only when token rotation has not been configured at all.

### Operator CLI (`loom-tokens pin/unpin/unblock`)

Operators can restrict the rotation pool to a subset of accounts (an "allowlist") and manually un-blacklist accounts marked bad. Auto-recovery prevents pin-induced lockouts.

```bash
loom-tokens pin agent-3 agent-7   # Set allowlist to exactly these
loom-tokens pin add agent-2       # Append (idempotent)
loom-tokens pin remove agent-3    # Remove
loom-tokens pin status            # Show current allowlist
loom-tokens unpin                 # Delete allowlist (back to full pool)

loom-tokens unblock agent-1       # Remove one entry from .bad_tokens
loom-tokens unblock --all         # Clear .bad_tokens entirely
```

**Validation**: `pin` accepts only exact bootstrapped account names — substring/fuzzy matches are rejected. The allowlist is sorted, deduplicated, and `mkdir`-lock guarded so concurrent operator commands don't drop entries.

**Reason-aware bad-token TTL**: bad-tokens entries with reason `auth` (401) ignore `LOOM_TOKENS_BAD_TTL` (default 21600s = 6h) and persist until `loom-tokens unblock`. Other reasons expire automatically.

**Auto-unpin** (`failure_counts`): the wrapper tracks consecutive `TOKEN_EXHAUSTED` failures per account in `.loom/tokens/.failure_counts` (JSON). When **every** account in the allowlist hits the threshold (default 5), the wrapper auto-clears `.allowlist` and `.failure_counts` with a loud stderr log line. Operators can re-pin afterwards. The threshold is `>= 5`, so a 6th failure does not silently exceed; it still triggers (idempotent at-or-above).

Counters are reset on:
- a successful spawn for that account, or
- any operator allowlist mutation (`pin`, `unpin`, `add`, `remove`).

**Empty-pool guard**: if the selector finds the allowlist minus `.bad_tokens` is empty, `spawn-claude.sh` exits `78` (`EX_CONFIG`) with operator instructions. It refuses to silently auto-clear `.bad_tokens` — that masks real auth problems (lean-genius failure mode 3).

### Tests

```bash
PYTHONPATH=loom-tools/src python3 -m pytest loom-tools/tests/tokens/ -v
bash .loom/scripts/tests/test-spawn-claude.sh
```

## Forge Authentication

### GitHub

Loom uses the `gh` CLI for all GitHub operations. By default it uses the credential from `gh auth login`, which has access to all repositories. To scope access to a single repository, create a fine-grained PAT and set `export GH_TOKEN=github_pat_xxx` before running Loom.

See `.loom/docs/github-authentication.md` for the detailed setup guide, required token permissions per role, and troubleshooting.

### Gitea

For Gitea repositories, Loom uses the Gitea API with token authentication. Set `GITEA_TOKEN` or `FORGE_TOKEN` environment variable with an API token created at `<your-gitea-instance>/user/settings/applications`. The token needs repository read/write permissions (issues, pull requests, labels).

See `.loom/docs/forge-authentication.md` for the complete authentication guide covering both GitHub and Gitea.

## Releasing

Use `scripts/version.sh` to manage versions across all packages:

```bash
./scripts/version.sh              # Show current version
./scripts/version.sh check        # Verify all files are in sync
./scripts/version.sh bump patch   # Bump patch (minor, major also supported)
./scripts/version.sh bump patch --tag  # Bump + commit + tag
./scripts/version.sh set 1.0.0 --tag   # Set explicit version + commit + tag
```

**Full release flow** — releases are driven by `/repo:release` from
[rjwalters/repo](https://github.com/rjwalters/repo) (install repo for the
command). `/repo:release` detects and honors `scripts/version.sh` as its
first-priority version tool, so the underlying mechanics are unchanged:
```bash
./scripts/version.sh bump patch --tag
git push origin main --tags
gh release create vX.Y.Z --title "vX.Y.Z" --notes "Release notes..."
```

The script updates all 5 version-bearing files (`package.json`, `mcp-loom/package.json`, 2 `Cargo.toml` files (`loom-daemon`, `loom-api`), `CLAUDE.md`) plus `Cargo.lock`. The GitHub Actions release workflow (`.github/workflows/release.yml`) triggers on GitHub Release creation (`release: types: [created]`), NOT on tag push. You must create a GitHub Release via `gh release create` to trigger the build.

## Migration: v0.10.0 shepherd/daemon deprecation

The orchestration-architecture migration (epic #3372) deleted the shepherd brain (`loom-tools/src/loom_tools/shepherd/`), the Python daemon brain (`loom-tools/src/loom_tools/daemon_v2/`), and the `/shepherd` slash command. The replacement is a two-surface architecture: `/loom:sweep` for in-session subagent dispatch (Tier 1) and the Rust `loom-daemon` binary for multi-account MCP-level dispatch (Tier 2). Epic #3449 rebuilt the daemon surface in phases A–D, all shipped on main. The completed phases:

| Phase | Issue | What shipped | Status |
|-------|-------|-----------|--------|
| Phase 1 | #3374 | Minimal multi-account spawn loop (legacy — deprecated in Phase E of #3449) | shipped (deprecated in Phase E, removed in v0.11.0) |
| Phase 2a | #3375 | GitHub Actions workflows for support roles | shipped (disabled by default) |
| Phase 2b | #3376 | Soft-deprecation warnings on deprecated entry points | shipped |
| Phase 3 | #3378 | Deletion of shepherd brain, Python daemon brain, `/shepherd` skill | shipped |
| Phase 4 | #3382 | Coordinated downstream sphere-install migration | shipped |
| #3449 Phase A | #3452 | `loom-daemon`: `dispatch_sweep`, `list_sweeps`, sweep registry, reaper task | shipped |
| #3449 Phase B | #3453 | `loom-daemon`: event bus (tokio broadcast), 6 frozen topics, `publish_event`, `subscribe_to_events` IPC | shipped |
| #3449 Phase C | #3455 | MCP tools: `get_sweep_status`, `tail_sweep_log`, `subscribe_to_events`, `publish_event`, `cancel_sweep`, `tail_event_bus`; daemon-reference.md rewrite | shipped |
| #3449 Phase D | #3454 | `/loom:sweep` Stage -1 backend detection (strict-AND daemon + pool probe) | shipped |
| #3449 Phase E | #3456 | `spawn-loop.sh` deprecation warning + doc-fiction rewrite | shipped |

**v1.0.0 is intentionally unscheduled.** Loom remains pre-1.0 while the architecture settles.

**Removed entry points** (no longer present in v0.10.0+):

| Removed | Replacement |
|---------|-------------|
| `loom-daemon` Python CLI | Rust `loom-daemon` binary + `mcp__loom__dispatch_sweep` (+ GitHub Actions for support roles) |
| `loom-shepherd` CLI / `/shepherd` slash command | `/loom:sweep <issue>` for the same per-issue lifecycle |
| `defaults/scripts/spawn-loop.sh` (removed in v0.11.0) | `mcp__loom__dispatch_sweep` against `loom-daemon` |

Full migration narrative and per-CLI replacement table: [`docs/migration/v0.10.0-shepherd-deprecation.md`](docs/migration/v0.10.0-shepherd-deprecation.md).

See also: [ADR-0009](docs/adr/0009-shepherd-deprecation.md) — records the architectural decision to delete the shepherd and daemon Python brains.

## Resources

- **Main Repository**: https://github.com/rjwalters/loom
- **Role Definitions**: `.loom/roles/*.md`
- **Label Definitions**: `.github/labels.yml`
- **Troubleshooting**: `.loom/docs/troubleshooting.md`
- **Daemon Reference**: `.loom/docs/daemon-reference.md`
- **GitHub Authentication**: `.loom/docs/github-authentication.md`
- **Forge Authentication** (GitHub + Gitea): `.loom/docs/forge-authentication.md`
- **Scripts**: `.loom/scripts/`

---

**Generated by Loom Installation Process**
Last updated: 2026-04-21

<!-- BEGIN REPO-SKILLS -->
This repository has [Repo Skills](https://github.com/rjwalters/repo) v0.4.0 installed —
general repository hygiene and environment commands invoked as `/repo:<command>`. Run
`/repo:help` for the command list, or see `.claude/skills/repo/SKILL.md` for the full
guide. Hygiene commands are report-first: they present findings and wait before changing
anything. Managed by `install.sh` — edit outside the markers only.
<!-- END REPO-SKILLS -->
