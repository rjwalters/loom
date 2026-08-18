# Loom Orchestration - Repository Guide

This repository uses **Loom** for AI-powered development orchestration.

**Loom Version**: 0.18.94
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

Run the periodic support roles (Champion, Curator, Judge, Doctor, Auditor, Guide,
Hermit) via the daemon-native role runner (`autonomous.roleRunner.enabled=true`,
preferred — same rotated token pool as sweeps), or via GitHub Actions cron
workflows under `.github/workflows/loom-*.yml` for the other five (disabled by
default; opt in with a `CLAUDE_API_KEY` secret + uncommented `schedule:` lines —
a single static key, no rotation; no `loom-doctor.yml`/`loom-hermit.yml` exist —
Doctor's and Hermit's standalone dispatch are role-runner-only, see #5272/#5601). Architect ships there too, `onIdle`-only, capped by `architectMaxProposals` (#5656).

The full MCP surface, event taxonomy, autonomous config, and role runner are in
[`.loom/docs/daemon-reference.md`](.loom/docs/daemon-reference.md);
interval-cadence Architect/Hermit work generation is out of scope (#3381).

## Agent Roles

Ten specialized roles (Builder, Judge, Champion, Curator, Architect, Hermit,
Doctor, Guide, Driver, Auditor) plus `loom` (the daemon-mode operator surface)
— purpose/cadence table: [`.loom/roles/README.md`](.loom/roles/README.md)
§"Available Roles". Full definitions: `.loom/roles/<name>.md`.

## Label-Based Workflow

Agents coordinate through labels — `.github/labels.yml` is authoritative (every
label documents its own `Applied by:` owner). State transitions:

- **Issue**: `loom:triage` (filer) → `loom:curating`/`loom:curated` (Curator) →
  `loom:issue` (human, or Champion in `--merge` mode) → `loom:building`
  (Builder) → closed.
- **PR**: `loom:review-requested` (Builder) → `loom:pr` (Judge) → auto-merged
  (Champion).
- **Proposal**: `loom:architect`/`loom:hermit`/`loom:auditor` (proposer) →
  evaluated (Champion) → `loom:issue` (ready for Builder).
- **Epic**: `loom:epic` → Champion creates phased `loom:architect` +
  `loom:epic-phase` issues.

`loom:operator` is the first-class "a human is needed" state (engine stops
acting, re-evaluable, unlike `loom:operator-only`) — wired at Champion's
merge-risk hold only so far: [`.loom/docs/label-state-machine.md`](.loom/docs/label-state-machine.md).

> **Note on label cleanup**: Loom intentionally does **not** remove labels from
> closed issues or merged PRs (harmless — all agents filter by open state — and it
> saves gh API calls). Do not implement label cleanup on merge/close (see #2838).

### Issues Are Suggestions (Role Autonomy)

Filed issues are the *input queue*, not mandates — this repo runs
autonomy-by-default. In autonomous mode **Curator, Builder, and Judge** may
**close** (rationale commented first, then `--reason "not planned"`) or
**rescope** (relabel back to `loom:triage`/`loom:curated` if the scope no
longer matches) an issue rather than build it as filed, when it is obsolete,
duplicate, low value, or the wrong approach. **Never** close an issue that
encodes a still-pending human decision — use `loom:blocked`/
`loom:operator-only` instead. Full guardrails live in each role prompt's own
"Issues Are Suggestions" section: `.loom/roles/curator.md`, `builder.md`,
`judge.md`.

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
- Loom-managed worktrees (with the `.loom-managed` sentinel) are auto-removed on
  merge AND by the daemon's periodic reaper (#4876, catches merges made on another
  host); user-provisioned worktrees are never removed — `LOOM_PRESERVE_WORKTREE=1`.
- `worktree.sh N` detects and skips a stale `origin/feature/issue-N` whose tip is
  already the head of a **merged** PR (e.g. a partial-increment slice's branch
  name reused by the next slice, #3667/#3599) instead of reusing it — see
  [`.loom/docs/troubleshooting.md`](.loom/docs/troubleshooting.md) (#5657).

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

**Operator-session lane (the one Curator-skip exemption)**: an operator driving
the session tools directly may skip Curator and label a new issue `loom:building`
at filing time — but **only** when *both* hold: (1) the acceptance criterion is
verifiable by a command, not by judgment, **and** (2) the diff is confined to
non-executing files (`.md`, `.txt`, and similar). Anything touching `.sh`, `.rs`,
`.ts`, a role prompt, `.github/labels.yml`, or `.loom/config.json` is out of the
lane, **unconditionally**. This is a predicate on the *change* (evaluated by
whoever files), not a config toggle, opt-out, or human-approval gate. **Judge and
Champion are unaffected** — both run unmodified; only Curator may be skipped.

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
refreshes stale installed `.loom/hooks|scripts|roles|docs|bin/` + `.claude/commands/loom/`
copies (and re-stamps install metadata). Details:
[`.loom/docs/troubleshooting.md` → Overnight / long-running orchestration](.loom/docs/troubleshooting.md).

## Configuration

Configuration lives in `.loom/config.json` (committed for team sharing): a
`terminals` array (per-agent role/model), plus the optional blocks below.

- **Daemon configuration (Tier 2)** — the `autonomous` block, start/stop wrappers,
  self-update, epic supervisor: [`.loom/docs/daemon-reference.md`](.loom/docs/daemon-reference.md)
  §Operability (precedence **env > config > default**; daemon defaults FLAGS-OFF).
- **Post-Builder quality gate (`buildGate`)** — [`.loom/docs/build-gate.md`](.loom/docs/build-gate.md).
- **Runtime dispatch (`runtimes`)** — `spawn-worker.sh` selects the worker runtime
  (`LOOM_RUNTIME` env > `runtimes.default` > `"claude"`), execing `spawn-<runtime>.sh`:
  [`.loom/docs/runtime-adapters.md`](.loom/docs/runtime-adapters.md).
- **Custom roles** — add `.loom/roles/<name>.md` (and optional `<name>.json`).
- **Branch rulesets & repository settings** — set at install time or via
  `./scripts/install/setup-branch-protection.sh` / `setup-repository-settings.sh`.
- **Guard hooks** — `PreToolUse` guards block/ask on destructive commands and
  confine Edit/Write to a builder's worktree; category toggles (`guards.sqlDdl`,
  `cloudCli`, `reversibleGh`, `rmScope`, `forceScope`, `readOnlyFastPath`,
  `decisionLog`, `worktreeIsolation`, `stashScope`, each with an `LOOM_*` env
  override) let a repo opt out — above an **ungated denial floor** no toggle can disable. Catalog: [`defaults/docs/guard-hooks.md`](defaults/docs/guard-hooks.md); forge text is untrusted input: [`defaults/docs/untrusted-external-content.md`](defaults/docs/untrusted-external-content.md).
- **MCP hooks** — the unified `mcp-loom` server is registered once per machine at
  user scope (`scripts/install-loom.sh`, refreshed by `loom update`); `setup-mcp.sh`
  is demoted to a bundle-rebuild/legacy-migration tool. See the mcp-loom README.
- **Fleet dashboard** (`loom-daemon serve`, opt-in, read-only, loopback by default): [`.loom/docs/daemon-reference.md`](.loom/docs/daemon-reference.md) §Fleet dashboard.
- **Fleet observability** (`observability` config block: daemon → Cloudflare backend → dashboard) — [`.loom/docs/observability.md`](.loom/docs/observability.md).

### Multi-Account Token Pool (operating summary)

For Pro/Max plans, Loom rotates among multiple Claude OAuth accounts so one weekly
limit does not stall the pipeline. Provision `.loom/tokens/` with `loom-daemon tokens
bootstrap` (or `import-from-monitor --force` on a claude-monitor host) +
`loom-daemon tokens check --ranking`. Agents spawn through `.loom/scripts/spawn-claude.sh`
(never `claude` directly), which selects a token (ranking → allowlist → random); a
missing/exhausted pool exits `78` (`EX_CONFIG`). Full reference:
[`.loom/docs/token-pool.md`](.loom/docs/token-pool.md).

## Forge Authentication & Releasing

- **GitHub** — Loom uses the `gh` CLI (the `gh auth login` credential; scope to
  one repo with `export GH_TOKEN=…`). Fleet rate-limit protections (breaker, ETag
  cache, App tokens — epic #4432) are `loom-daemon`-internal; hand-rolled parallel
  `claude-wrapper.sh` loops get none. **File new issues with
  `./.loom/scripts/create-issue.sh`, never a bare `gh issue create`** — that is
  GraphQL-backed and dies on GraphQL exhaustion while the independent REST pool
  sits idle; the script falls back to one REST POST with labels applied
  atomically (#5047). See [`.loom/docs/github-authentication.md`](.loom/docs/github-authentication.md).
- **Gitea** — set `GITEA_TOKEN` or `FORGE_TOKEN` (repository read/write). See
  [`.loom/docs/forge-authentication.md`](.loom/docs/forge-authentication.md).
- **Releasing** — `scripts/version.sh` keeps all 6 version-bearing files in sync
  (including this `CLAUDE.md`'s `**Loom Version**` line and the root `VERSION`
  file, #5517); releases are driven by
  `/repo:release` from [rjwalters/repo](https://github.com/rjwalters/repo). The
  release workflow triggers on GitHub Release creation, not tag push. The same
  workflow also publishes `ghcr.io/rjwalters/loom-worker:<version>` — a pinned
  sweep-execution-environment base image (daemon stays on the host; see
  [`docker/worker/README.md`](docker/worker/README.md) for the shape decision
  and `FROM` contract). **Cadence**: releases are cut at explicit fleet-rollable boundaries, not on every `VERSION` bump (`main` bumps `VERSION` on nearly every merge) — see [`.loom/docs/release-cadence.md`](.loom/docs/release-cadence.md) for the policy and the `--fetch`/`--check` artifact-gap visibility it documents (#6010).

## Troubleshooting

See [`.loom/docs/troubleshooting.md`](.loom/docs/troubleshooting.md) for stale
worktrees, stuck agents, daemon registry/event-bus/reaper issues, host-sleep and
`.loom/` resync procedures, quarantine safety, and common fixes. Quick fixes:
`loom-clean --force` (stale worktrees/branches), `loom-recover-orphans
--recover` (orphaned `loom:building` issues), `gh label sync --file
.github/labels.yml` (re-sync labels), `loom-daemon cancel --issue <N>` /
`mcp__loom__cancel_sweep` (cancel a running sweep — never hand-`kill` its
pids, #4980). **Branching does not protect uncommitted edits in the primary
clone from quarantine** — see [`.loom/docs/troubleshooting.md` →
Uncommitted work in the primary clone can be quarantined at any
time](.loom/docs/troubleshooting.md).

## Migration History

Completed-migration history (v0.10.0 shepherd/daemon deprecation, the Rust
`loom-daemon` rebuild, `spawn-loop.sh` removal in v0.11.0) lives in
[`docs/migration/v0.10.0-shepherd-deprecation.md`](docs/migration/v0.10.0-shepherd-deprecation.md)
and [ADR-0009](docs/adr/0009-shepherd-deprecation.md) — not inline here.

## Resources

- **Repository**: https://github.com/rjwalters/loom · **Roles**: `.loom/roles/*.md`
  · **Labels**: `.github/labels.yml` · **Scripts**: `.loom/scripts/`
- **Docs**: [daemon-reference](.loom/docs/daemon-reference.md) ·
  [token-pool](.loom/docs/token-pool.md) ·
  [troubleshooting](.loom/docs/troubleshooting.md) ·
  [build-gate](.loom/docs/build-gate.md) ·
  [forge-auth](.loom/docs/forge-authentication.md) /
  [github-auth](.loom/docs/github-authentication.md) ·
  [safehouse](.loom/docs/safehouse.md) ·
  [blame-issue](.loom/docs/blame-issue.md) · [fleet-config-lifecycle](.loom/docs/fleet-config-lifecycle.md)

---

**Generated by Loom Installation Process**
Last updated: 2026-04-21

