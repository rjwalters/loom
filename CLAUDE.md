# Loom Orchestration - Repository Guide

This repository uses **Loom** for AI-powered development orchestration.

**Loom Version**: 0.9.1
**Installation Date**: 2026-04-21

## What is Loom?

Loom is a CLI tool for AI-powered development orchestration. It coordinates AI development workers using git worktrees and a forge (GitHub or Gitea) as the coordination layer. It supports manual coordination (Manual Orchestration Mode) and continuous autonomous orchestration via the spawn loop + GitHub Actions cron schedules.

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
│              Tier 2: Spawn loop + GitHub Actions cron           │
│  spawn-loop.sh — claims ready issues, spawns sweep children     │
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
| Tier 2 | `./.loom/scripts/spawn-loop.sh` + GH Actions cron | Multi-issue batch + scheduled support roles | Continuous / cron |
| Tier 1 | `/loom:sweep <issue>` | Single-issue lifecycle (Curator → Merge) | Per-issue |
| Tier 0 | `/builder`, `/judge`, etc. | Task execution — single focused work units | Per-task |

**Use `/loom:sweep <issue>`** when you have a specific issue to implement (interactively or from a script).
**Use `LOOM_USE_SPAWN_LOOP=1 ./.loom/scripts/spawn-loop.sh start`** for multi-issue autonomous batches.
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

### 3. Spawn-Loop Mode (opt-in)

A minimal multi-account `/loom:sweep` launcher (#3374). Polls `loom:issue`, atomically claims ready issues, and detaches `claude -p "/loom:sweep N"` per issue — each spawn picks its own OAuth token via `spawn-claude.sh`. No work generation, no support-role triggers, no pool-slot bookkeeping.

> **Note**: `/loom:sweep` also supports a **PR-set mode** (Mode C, #3384) via `--prs <pr-number-list>` or NL phrases like "all open `loom:pr`" — drives Judge / Doctor → Judge / Merge from an existing open-PR set without re-running Curator or Builder. The spawn loop is issue-keyed and does not invoke PR-set mode; operators use it directly via `claude -p "/loom:sweep --prs ..."`.

```bash
LOOM_USE_SPAWN_LOOP=1 ./.loom/scripts/spawn-loop.sh start  # opt-in gate is required
./.loom/scripts/spawn-loop.sh status
./.loom/scripts/spawn-loop.sh stop                          # or: touch .loom/stop-spawn-loop
```

State lives in `.loom/spawn-loop-state.json`, logs in `.loom/logs/spawn-loop.log`, claim locks under `.loom/locks/issue-<N>/`. Crashed children whose checkpoints (#3373) survive are re-queued on the next tick. If `daemon-loop.pid` is alive, the loop warns and proceeds (both will compete for `loom:issue` items — pick one). Overrides: `MAX_PARALLEL=3`, `POLL_INTERVAL=30`, `SHUTDOWN_GRACE_SEC=300`.

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

> **Note**: the historical `shepherd.md` (single-issue orchestrator) role file was removed in v0.10.0 along with the `/shepherd` slash command — see [the migration guide](docs/migration/v0.10.0-shepherd-deprecation.md). Its orchestration responsibilities moved to `/loom:sweep` (Tier 1) and the spawn loop + GH Actions cron (Tier 2). The `loom.md` role file is preserved and documents the daemon-mode operator surface (`./.loom/scripts/daemon.sh` + tmux + token-rotated separate Claude Code sessions); the Python brain it historically referenced (`loom_tools/daemon_v2/`) is removed in v0.10.0, but the shell-level daemon surface stays. The worker-role markdown files above are unchanged.

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
- Never run `git worktree` directly (helper prevents nested worktrees)
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

1. Find unlabeled issues: `gh issue list --label="!loom:issue,!loom:building,!loom:architect,!loom:hermit,!loom:curated,!loom:curating"`
2. Enhance issue with technical details
3. Mark curated: `gh issue edit 42 --add-label "loom:curated"`

### Overnight / long-running orchestration: keep the host awake (#3350)

`/loom:sweep` and the spawn loop automatically run `./.loom/scripts/check-host-sleep.sh` at startup and warn when the host can sleep. This is **advisory only** — Loom never blocks on it. Heed the warning before walking away from a long run.

- **macOS:** user-idle sleep assertions (Amphetamine, `caffeinate -dimsu`, etc.) do **not** reliably defeat Maintenance Sleep on Apple Silicon. Use `sudo pmset -c sleep 0` for AC-only sleep disable, or flip your sleep manager's "allow system sleep when display is off" toggle to OFF.
- **systemd Linux:** wrap the session in `systemd-inhibit --what=idle:sleep --who=loom --why=loom -- <cmd>`.

Manual invocation: `./.loom/scripts/check-host-sleep.sh` (or `--quiet` for stderr-only output).

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

### Spawn-Loop Configuration

The spawn loop replaces the historical Python daemon brain. The shell-level daemon surface (`./.loom/scripts/daemon.sh`) is preserved and re-implemented around the spawn loop + GitHub Actions cron + token-rotated tmux panes (see [`.loom/docs/daemon-reference.md`](.loom/docs/daemon-reference.md)). For the full migration narrative, see [`docs/migration/v0.10.0-shepherd-deprecation.md`](docs/migration/v0.10.0-shepherd-deprecation.md).

**State file** (`.loom/spawn-loop-state.json`, gitignored):

```json
{
  "started_at": "2026-06-04T10:00:00Z",
  "running": [
    {
      "issue": 123,
      "pid": 49281,
      "started_at": "2026-06-04T10:15:00Z",
      "token": "agent-3.token"
    }
  ]
}
```

That's the entire schema. Pipeline state, warnings, completed-issue history, and work-generation cooldowns are not tracked here — the forge is the source of truth for queue state, and the spawn loop is intentionally minimal (see `docs/migration/daemon-state-consumers.md` for the design rationale).

**Tunables (env)**:

| Variable | Default | Description |
|----------|---------|-------------|
| `MAX_PARALLEL` | 3 | Maximum concurrent `/loom:sweep` children |
| `POLL_INTERVAL` | 30 | Seconds between `loom:issue` polls |
| `SHUTDOWN_GRACE_SEC` | 300 | Seconds to wait for in-flight children at shutdown |
| `LOOM_USE_SPAWN_LOOP` | (unset) | Opt-in gate, required to start the loop |
| `LOOM_REPO` | (auto) | Override remote auto-detection (`owner/repo`) |

**Sweep checkpoints** (`.loom/sweep-checkpoint/issue-<N>.json`, gitignored):

When a sweep child crashes mid-flight, the checkpoint records the last completed phase. On the next spawn the sweep skill resumes from that phase. The exact schema is owned by the sweep skill (#3373) and is not part of the spawn loop's public surface.

**Scheduled support roles** run as separate GitHub Actions cron jobs — see `.github/workflows/loom-*.yml`. They have no persistent state on the Loom side; each tick is a fresh `claude -p "/<role>" --dangerously-skip-permissions` invocation.

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

For environments that rotate among multiple Claude OAuth accounts, Loom can bootstrap a per-account token pool at `.loom/tokens/` from numbered triples in `.env`:

```env
ACCOUNT_EMAIL_1=user1@example.com
ACCOUNT_KEY_1=sk-ant-oat01-...
ACCOUNT_TOKEN_FILE_1=user1.token
```

Run `loom-tokens bootstrap` to materialize the pool:

```bash
loom-tokens bootstrap            # Idempotent — only writes new/missing tokens.
loom-tokens bootstrap --dry-run  # Preview without writing.
loom-tokens bootstrap --force    # Overwrite on-disk tokens that have drifted from .env.
```

Each account becomes `.loom/tokens/<file>.token` (mode `0600`). An `index.json` manifest is written alongside with sha256 fingerprints (8 chars) for drift detection — **no secret material is stored in the manifest**. Numbering gaps are allowed; partial triples are skipped with a warning.

`.loom/tokens/` is gitignored. The pool is consumed by external rotation logic (e.g. a `claude-wrapper.sh` that picks the least-used token); only the bootstrap step is provided here.

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
- Spawn-loop troubleshooting
- Common issues and solutions

**Quick fixes**:

```bash
loom-clean --force                       # Clean stale worktrees/branches
./.loom/scripts/stale-building-check.sh --recover  # Recover stuck issues
gh label sync --file .github/labels.yml  # Re-sync labels (GitHub only)
touch .loom/stop-spawn-loop              # Graceful spawn-loop shutdown
```

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

1. Add account credentials to `.env` at the workspace root:
   ```env
   ACCOUNT_KEY_1=sk-ant-oat01-...
   ACCOUNT_TOKEN_FILE_1=robb-personal.token
   ACCOUNT_KEY_2=sk-ant-oat01-...
   ACCOUNT_TOKEN_FILE_2=robb-work.token
   ```
2. Run `loom-tokens bootstrap` to materialize per-account `.token` files into `.loom/tokens/` (mode 0600, parent dir 0700). See issue #3234.
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

**Full release flow** (use `/release` skill for guided process):
```bash
./scripts/version.sh bump patch --tag
git push origin main --tags
gh release create vX.Y.Z --title "vX.Y.Z" --notes "Release notes..."
```

The script updates all 5 version-bearing files (`package.json`, `mcp-loom/package.json`, 2 `Cargo.toml` files (`loom-daemon`, `loom-api`), `CLAUDE.md`) plus `Cargo.lock`. The GitHub Actions release workflow (`.github/workflows/release.yml`) triggers on GitHub Release creation (`release: types: [created]`), NOT on tag push. You must create a GitHub Release via `gh release create` to trigger the build.

## Migration: v0.10.0 shepherd/daemon deprecation (completed)

The orchestration-architecture migration (epic #3372) is complete as of v0.10.0. The shepherd brain (`loom-tools/src/loom_tools/shepherd/`), the Python daemon brain (`loom-tools/src/loom_tools/daemon_v2/`), and the `/shepherd` slash command were deleted and replaced by the spawn loop (#3374) + GitHub Actions workflows (#3375). The shell-level daemon surface (`./.loom/scripts/daemon.sh`) is preserved, re-implemented as a tmux session launcher around the spawn loop. The completed phases:

| Phase | Issue | What shipped | Status |
|-------|-------|-----------|--------|
| Phase 1 | #3374 | Minimal multi-account spawn loop (`./.loom/scripts/spawn-loop.sh`) | shipped |
| Phase 2a | #3375 | GitHub Actions workflows for support roles | shipped (disabled by default) |
| Phase 2b | #3376 | Soft-deprecation warnings on deprecated entry points | shipped |
| Phase 3 | #3378 | Deletion of shepherd brain, Python daemon brain, `/shepherd` skill; `daemon.sh` re-implemented | shipped (v0.10.0) |
| Phase 4 | #3382 | Coordinated downstream sphere-install migration | shipped |

**v1.0.0 is intentionally unscheduled.** Loom remains pre-1.0 while the architecture settles.

**Removed entry points** (no longer present in v0.10.0+):

| Removed | Replacement |
|---------|-------------|
| `loom-daemon` Python CLI | `./.loom/scripts/daemon.sh` (preserved) or `./.loom/scripts/spawn-loop.sh` (headless) + GitHub Actions schedules |
| `loom-shepherd` CLI / `/shepherd` slash command | `/loom:sweep <issue>` for the same per-issue lifecycle |

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
