# Loom Orchestration - Repository Guide

This repository uses **Loom** for AI-powered development orchestration.

**Loom Version**: 0.15.0
**Installation Date**: 2026-04-21

> **This file is the operating core** — only what an agent must know to act
> correctly *right now*. Reference detail lives in `.loom/docs/*`; completed-migration
> history in `docs/migration/` + ADRs. **New subsystem detail goes to `.loom/docs/`
> with a one-line pointer here, not inline** — the CI budget check
> (`scripts/check-claude-md-budget.sh`) enforces this so every agent's context does
> not regrow unchecked.

## What is Loom?

Loom is a CLI + daemon for AI-powered development orchestration. It coordinates AI
development workers using git worktrees and a forge (GitHub or Gitea) as the
coordination layer, via manual roles, continuous autonomous orchestration (the
Rust `loom-daemon` binary), and GitHub Actions cron schedules.

**Loom Repository**: https://github.com/rjwalters/loom

## Orchestration Architecture

Loom decomposes development into three coordination tiers, with the forge (GitHub
/ Gitea) as the shared state.

| Tier | Entry point | Purpose | Mode |
|------|-------------|---------|------|
| Tier 3 | Human | Oversight — approve proposals, handle edge cases | Observer |
| Tier 2 | `loom-daemon` (MCP) + GH Actions cron | Multi-issue dispatch + scheduled support roles | Continuous / cron |
| Tier 1 | `/loom:sweep <issue>` | Single-issue lifecycle (Curator → Merge) | Per-issue |
| Tier 0 | `/loom:builder`, `/loom:judge`, etc. | Task execution — single focused work units | Per-task |

## Usage Modes

### 1. Manual Orchestration Mode (MOM)

Open Claude Code in this repo and use slash commands (`/loom:builder`,
`/loom:judge`, `/loom:curator`, …) — each terminal acts as a specialized agent.

### 2. Single-issue lifecycle: `/loom:sweep <issue>`

Run a complete Curator → Builder → Judge → Doctor → Merge lifecycle on one issue:

```bash
/loom:sweep 123
claude -p "/loom:sweep 123" --dangerously-skip-permissions   # from a script
```

**PR-set mode (Mode C, #3384)**: `/loom:sweep --prs 456 789` drives Judge / Doctor
→ Judge / Merge from an existing open-PR set without re-running Curator or Builder.
Checkpoints (#3373) under `.loom/sweep-checkpoint/issue-<N>.json` survive crashes,
so restarting `/loom:sweep N` resumes from the last completed phase.

### 3. Daemon Mode (`loom-daemon` + MCP tools)

The Rust `loom-daemon` binary is the Tier 2 dispatch backend, driven over MCP:
`mcp__loom__dispatch_sweep` (dispatch), `list_sweeps` / `get_sweep_status`
(observe), `subscribe_to_events` (events), `cancel_sweep`. **By default it is not a
work generator** — work arrives only via `dispatch_sweep` and the cron workflows;
the autonomous work finder, epic supervisor, and role runner (all opt-in,
default-off) let it generate its own work when enabled.

### 4. Scheduled Support Roles

Run the periodic support roles (Champion, Curator, Judge, Auditor, Guide) via the
daemon-native role runner (`autonomous.roleRunner.enabled=true`, preferred — same
rotated token pool as sweeps), or via GitHub Actions cron workflows under
`.github/workflows/loom-*.yml` (disabled by default; opt in with a `CLAUDE_API_KEY`
secret + uncommented `schedule:` lines — a single static key, no rotation).

The full MCP surface, event taxonomy, autonomous config, and role runner are in
[`.loom/docs/daemon-reference.md`](.loom/docs/daemon-reference.md);
Architect/Hermit cadence is out of scope (#3381).

## Agent Roles

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

Full role definitions: `.loom/roles/*.md`. The `loom.md` role file documents the
daemon-mode operator surface (observing the running `loom-daemon` via MCP tools).

## Label-Based Workflow

Agents coordinate through labels. See `.github/labels.yml` for full definitions
(the authoritative `Applied by:` field is on every label).

**Issue Lifecycle**:
```
(created) → loom:triage → loom:curating → loom:curated → loom:issue → loom:building → (closed)
           ↑ filer        ↑ Curator        ↑ Curator      ↑ human     ↑ Builder
                                                          (or Champion
                                                           in --merge mode)
```

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

**Epic Lifecycle**: `loom:epic` → Champion creates phased `loom:architect` +
`loom:epic-phase` issues.

> **Note on label cleanup**: Loom intentionally does **not** remove labels from
> closed issues or merged PRs (harmless — all agents filter by open state — and it
> saves gh API calls). Do not implement label cleanup on merge/close (see #2838).

### Issues Are Suggestions (Role Autonomy)

Filed issues are the *input queue*, not mandates. In autonomous mode the
**Curator, Builder, and Judge** may **close** or **rescope** an issue — with a
stated rationale — when building it is not the best outcome (obsolete, duplicate,
low value, wrong approach, better split/merged). Full guardrails live in each role
prompt's "Issues Are Suggestions" section (`.loom/roles/curator.md`, `builder.md`,
`judge.md`). In brief:

- **Comment the rationale BEFORE closing**, then `gh issue close <N> --reason "not
  planned"`. A closed issue leaves the queue automatically (the work-finder polls
  only *open* `loom:issue` items).
- **Rescope** instead of closing when the core is worth keeping: edit/split/relabel,
  and **remove `loom:issue`** if the labels no longer reflect an approved scope (drop
  back to `loom:triage`/`loom:curated`) so it is not re-dispatched with a stale scope.
- **Never close an issue that encodes a still-pending human decision** — route it to
  `loom:blocked` or `loom:operator-only` with a comment instead. Never invent labels.

## Git Worktree Workflow

Loom uses git worktrees to isolate agent work. **Issue Worktrees**
(`.loom/worktrees/issue-N`) hold issue-specific work for Builder agents.

```bash
gh issue edit 42 --remove-label "loom:issue" --add-label "loom:building"
./.loom/scripts/worktree.sh 42 && cd .loom/worktrees/issue-42
# ... work, commit ...
git push -u origin feature/issue-42
gh pr create --label "loom:review-requested"
```

**Rules**:

- Always use `./.loom/scripts/worktree.sh <issue-number>` (it writes a
  `.loom-managed` sentinel that authorizes cleanup).
- **Never run `git worktree` directly** (the helper prevents nested worktrees) — to
  remove one managed worktree on demand use `./.loom/scripts/worktree.sh remove
  <issue-number>` (`loom-clean` is the bulk path).
- Loom-managed worktrees (with the `.loom-managed` sentinel) are auto-removed when
  their PR merges; user-provisioned worktrees are never removed — set
  `LOOM_PRESERVE_WORKTREE=1` to disable cleanup for a session.

### Merging PRs

**Never use `gh pr merge`** — always use `./.loom/scripts/merge-pr.sh <PR_NUMBER>`
instead (`--auto` to queue until checks pass, `--dry-run` to preview). `gh pr
merge` attempts a local checkout that fails when the PR branch is linked to a
worktree; the script merges via the forge API directly and handles worktree
cleanup automatically.

## Development Workflow

### Sweep Lifecycle (MANDATORY)

When implementing issues — whether manually, via `/loom:sweep`, or by spawning
subagents — **all stages of the lifecycle must be executed in order**. Do not skip
stages.

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

**When spawning subagents**: each must run the full lifecycle, not just the builder
phase — creating a PR labeled `loom:review-requested` is only the Builder stage;
the work is not complete until reviewed and merged. **`/loom:sweep` handles all
stages automatically** — prefer it over manual orchestration to avoid skipping any.

### Builder Workflow

1. Find issue: `gh issue list --label="loom:issue"`
2. Claim: `gh issue edit 42 --remove-label "loom:issue" --add-label "loom:building"`
3. Create worktree: `./.loom/scripts/worktree.sh 42 && cd .loom/worktrees/issue-42`
4. Implement, test, commit
5. Create PR: `git push -u origin feature/issue-42 && gh pr create --label "loom:review-requested" --body "Closes #42"`

### Judge Workflow

Find `gh pr list --label="loom:review-requested"`, review, then coordinate via
**labels** (approve → `loom:pr`; changes → `loom:changes-requested`) plus a `gh pr
comment`. Use `gh pr comment`, **not** `gh pr review --approve` — GitHub's API
blocks self-review and Loom agents often create and review the same PR.

### Curator Workflow

Find unlabeled issues (use `-label:` search terms, **not** `--label` — gh ANDs
`--label` values with no negation syntax, so a negated `--label` always returns
empty), enhance with technical detail, then `gh issue edit 42 --add-label
"loom:curated"`.

### Overnight / long-running orchestration

`/loom:sweep` warns (advisory) via `check-host-sleep.sh` when the host can sleep;
after a `git pull` that updates `defaults/`, `./.loom/scripts/resync-installed.sh`
refreshes stale installed `.loom/hooks/` + `.loom/scripts/` copies. Details:
[`.loom/docs/troubleshooting.md` → Overnight / long-running orchestration](.loom/docs/troubleshooting.md).

## Configuration

Configuration lives in `.loom/config.json` (committed for team sharing): a
`terminals` array (per-agent role/model), plus the optional blocks below.

- **Daemon configuration (Tier 2)** — the `autonomous` block, start/stop wrappers,
  self-update, epic supervisor: [`.loom/docs/daemon-reference.md`](.loom/docs/daemon-reference.md)
  §Operability (precedence **env > config > default**; daemon defaults FLAGS-OFF).
- **Post-Builder quality gate (`buildGate`)** — [`.loom/docs/build-gate.md`](.loom/docs/build-gate.md).
- **Custom roles** — add `.loom/roles/<name>.md` (and optional `<name>.json`).
- **Branch rulesets & repository settings** — set at install time or via
  `./scripts/install/setup-branch-protection.sh` / `setup-repository-settings.sh`.
- **Guard hooks** — `PreToolUse` guards block/ask on destructive commands and
  confine Edit/Write to a builder's worktree; category toggles (`guards.sqlDdl`,
  `cloudCli`, `reversibleGh`, `rmScope`, `forceScope`, `readOnlyFastPath`,
  `decisionLog`, `worktreeIsolation`, each with an `LOOM_*` env override) let a
  repo opt out. Catalog: [`defaults/CLAUDE.md` → "Custom Guard Hooks"](defaults/CLAUDE.md).
- **MCP hooks** — the unified `mcp-loom` server; run `./scripts/setup-mcp.sh` to
  generate `.mcp.json`. See the mcp-loom README.

### Multi-Account Token Pool (operating summary)

For Pro/Max plans, Loom rotates among multiple Claude OAuth accounts so one weekly
limit does not stall the pipeline. Provision `.loom/tokens/` with `loom-tokens
bootstrap` (or `import-from-monitor --force` on a claude-monitor host) +
`loom-tokens check --ranking`. Agents spawn through `.loom/scripts/spawn-claude.sh`
(never `claude` directly), which selects a token (ranking → allowlist → random); a
missing/exhausted pool exits `78` (`EX_CONFIG`). Full reference:
[`.loom/docs/token-pool.md`](.loom/docs/token-pool.md).

## Forge Authentication & Releasing

- **GitHub** — Loom uses the `gh` CLI (the `gh auth login` credential; scope to
  one repo with `export GH_TOKEN=…`). See [`.loom/docs/github-authentication.md`](.loom/docs/github-authentication.md).
- **Gitea** — set `GITEA_TOKEN` or `FORGE_TOKEN` (repository read/write). See
  [`.loom/docs/forge-authentication.md`](.loom/docs/forge-authentication.md).
- **Releasing** — `scripts/version.sh` keeps all 5 version-bearing files in sync
  (including this `CLAUDE.md`'s `**Loom Version**` line); releases are driven by
  `/repo:release` from [rjwalters/repo](https://github.com/rjwalters/repo). The
  release workflow triggers on GitHub Release creation, not tag push.

## Troubleshooting

See [`.loom/docs/troubleshooting.md`](.loom/docs/troubleshooting.md) for stale
worktrees, stuck agents, daemon registry/event-bus/reaper issues, host-sleep and
`.loom/` resync procedures, and common fixes. Quick fixes: `loom-clean --force`
(stale worktrees/branches), `loom-recover-orphans --recover` (orphaned
`loom:building` issues), `gh label sync --file .github/labels.yml` (re-sync
labels), `mcp__loom__cancel_sweep --sweep_id <id>` (cancel a running sweep).

## Migration History

The v0.10.0 shepherd/daemon deprecation (epic #3372) deleted the Python shepherd
and daemon brains and `/shepherd`; epic #3449 rebuilt the daemon surface as the
Rust `loom-daemon` binary; v0.11.0 removed `spawn-loop.sh` (use
`mcp__loom__dispatch_sweep`). Complete history — phase table, removed-entry-point
map, downstream-consumer guidance —
[`docs/migration/v0.10.0-shepherd-deprecation.md`](docs/migration/v0.10.0-shepherd-deprecation.md),
[ADR-0009](docs/adr/0009-shepherd-deprecation.md).

## Resources

- **Repository**: https://github.com/rjwalters/loom · **Roles**: `.loom/roles/*.md`
  · **Labels**: `.github/labels.yml` · **Scripts**: `.loom/scripts/`
- **Docs**: [daemon-reference](.loom/docs/daemon-reference.md) ·
  [token-pool](.loom/docs/token-pool.md) ·
  [troubleshooting](.loom/docs/troubleshooting.md) ·
  [build-gate](.loom/docs/build-gate.md) ·
  [forge-auth](.loom/docs/forge-authentication.md) /
  [github-auth](.loom/docs/github-authentication.md)

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
