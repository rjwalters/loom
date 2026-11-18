# Model Selection Strategy

How Loom resolves each worker's model, the Judge-rejection escalation ladder, and
the suggested-model defaults by role. Retuning these defaults is measurement-gated
— see [`docs/model-selection-retune.md`](https://github.com/rjwalters/loom/blob/main/docs/model-selection-retune.md).

For *task-oriented* recipes that combine these keys with the runtime, credential-pool
and spend-bound axes ("put the expensive model on Judge", "minimize wall-clock"), see
[`configuring-resources.md`](configuring-resources.md).

### Model Selection Strategy

Model selection is a first-class orchestration concern (issue #3477, Phase 1). Each worker's model is resolved through a fixed precedence chain — highest first:

1. **Explicit dispatch param** — `mcp__loom__dispatch_sweep`'s optional `model` argument (daemon path), an explicit `--model` flag passed to `spawn-claude.sh` / `claude-wrapper.sh`, or an operator-requested model for an in-session sweep.
2. **Workspace override** — `.loom/config.json` → `terminals[].roleConfig.model` (optional). Pin exact IDs here (e.g., `claude-sonnet-4-6`) when your workspace needs deterministic cost/behavior.
3. **Role default** — `.loom/roles/<role>.json` → `suggestedModel` (ships as an alias). The `/loom:sweep` skill passes the resolved model to role subagents via the Task tool's `model` parameter.
4. **Session default** — when nothing above resolves, NO `--model` flag (and no Task `model` param) is emitted at all, and the worker inherits the parent session/CLI default. This is the zero-config behavior: nothing configured means nothing changes.

Tier 2's keys can also be set per-host without committing them: `config_resolver.rs` merges the committed `.loom-project/project.json` and then the ungitted, highest-precedence `.loom-local/local.json` overlay over `.loom/config.json` (see [`docs/design/config-resolution-tiers.md`](https://github.com/rjwalters/loom/blob/main/docs/design/config-resolution-tiers.md)) — so a one-host model override belongs in `.loom-local/local.json`, which **must stay gitignored**: unignored it is untracked dirt, and `check-main-clean.sh --quarantine` stashes untracked dirt between sweep waves, silently reverting the override (#8075).

The spawn plumbing also honors a `LOOM_MODEL` environment variable (`spawn-claude.sh`, `claude-wrapper.sh`): it is injected as `--model <value>` unless an explicit `--model` is already present in the args. Retries inside `claude-wrapper.sh` always reuse the same model — transport-level failures (token exhaustion, crashes, 5xx) are not quality signals and never change the model.

### Attended subagents (Task-tool spawns)

The chain above governs the **attended** path too — any session spawning a
`loom-<role>` subagent through the Agent/Task tool — via a deterministic
lookup instead of hand-reading config (issue #13956):

```bash
./.loom/scripts/resolve-role-model.sh <role> --json
# -> {"role":"judge","model":"claude-opus-5-5","source":"roleConfig.model","task_model":"opus"}
```

`source` names the tier that supplied the model (`param` | `roleConfig.model`
| `suggestedModel` | `inherit`), so an experiment can confirm what actually
ran. `model` is the real resolved value (a pinned ID stays a pinned ID);
**`task_model` is the value to pass** as the Task tool's `model` parameter —
the #4282 degradation to the nearest Task-passable alias, since that
parameter is alias-only and rejects a pinned ID (#14133). `task_model: null`
(nothing configured, or no alias for the model) means omit the parameter, and
the subagent inherits the parent session's model — the zero-config behavior is
byte-for-byte unchanged. The bare form prints just the model and exits 3 with
no output when nothing is configured; `--task-alias` (bare form only — a
no-op under `--json`) prints the alias and exits 3 when there is none. The shipped `defaults/.claude/agents/*.md` carry no
`model:` frontmatter (a test asserts it): a role's model comes from config at
spawn time, never from a hardcoded tier — the #13956 operator ruling declined
per-role pins in shipped files so per-role experiments stay possible.

**Reasoning effort has no attended-path knob.** The Task tool's interface
carries model only, so a `roleEfforts` setting remains a daemon-path setting
(the role runner resolves effort per role in `role_runner/model_resolution.rs`).
Configuring effort for attended subagents would require a harness change, not a
Loom one.

**Running the "opposite mix"** (the per-role experiment the ruling asks for):
pin models per role in `.loom/config.json` (committed) or
`.loom-local/local.json` (per-host, gitignored) — for example strong
Curator/Judge with a cheaper Builder:

```json
{
  "terminals": [
    { "id": "curator", "roleConfig": { "roleFile": "curator.md", "model": "claude-opus-5" } },
    { "id": "judge",   "roleConfig": { "roleFile": "judge.md",   "model": "claude-opus-5" } },
    { "id": "builder", "roleConfig": { "roleFile": "builder.md", "model": "claude-sonnet-4-6" } }
  ]
}
```

…and the reverse (cheap Curator/Judge, strong Builder) by swapping the
values. A tier-2 pin beats the role JSON's `suggestedModel` and is never
overridden by the escalation ladder (see "Precedence interaction" above).
Compare mixes on outcome telemetry — merged work, review bounces, cost via
`agent-metrics.sh --by-model` (#3482) and the sweep records' `models_used` /
`tokens_by_model` — not on impressions. The setting survives every
reinstall/resync untouched: config lives outside `defaults/`, and the
resolution happens at spawn time, so a config edit applies to the next spawn
with no resync. The per-mix read-out report is tracked separately (#14067).

**Escalation on Judge rejection (`sweep.escalation`, Phase 2, issue #3481)**:

The `/loom:sweep` orchestrator escalates one rung up a capability ladder when the Judge rejects a PR (`loom:changes-requested`) and a Doctor is dispatched to address the feedback. The escalation decision is made by the sweep orchestrator at Doctor-dispatch time — never by `claude-wrapper.sh` retries, never by worker self-assessment. Mode C (`--prs`) inherits the same rule for its Doctor phase (step C1b).

The ladder is configured in `.loom/config.json`:

```json
{
  "sweep": {
    "escalation": ["sonnet", "opus"]
  }
}
```

| Value | Behavior |
|-------|----------|
| Key absent | Default ladder `["sonnet", "opus"]` applies |
| `[]` or `false` | Escalation disabled entirely (pure Phase 1 behavior) |
| Non-empty array | As configured; rungs accept aliases or pinned IDs |

**Precedence interaction**: escalation replaces only tier 3 (`suggestedModel`) / tier 4 (session default) resolution for the rejection-triggered Doctor. Tier 1 (explicit dispatch param) and tier 2 (`roleConfig.model` workspace pin) always win — pins are never overridden. `ladder[0]` never overrides anything either: first attempts of every role use the unmodified precedence chain, and the ladder only fires on rejection (the rejection-triggered Doctor gets `ladder[1]`).

**Cap interaction**: escalation composes with — and does not extend — the configurable Doctor→Judge cycle cap (`sweep.max_doctor_cycles`, default 1; issue #3668). The ladder is consumed as `ladder[min(attempt - 1, len - 1)]`, so raising the cap above 1 (or granting the default-cap distinct-defect grace cycle) activates deeper rungs automatically. At the default cap of 1, a second Judge rejection blocks the PR rather than dispatching another Doctor — unless that second rejection is a demonstrably distinct defect from the first, in which case the orchestrator may grant one logged, single-use grace cycle (never on an operator-raised cap). The sweep checkpoint's optional `attempt` field (`sweep-checkpoint.sh write N doctor-done --attempt 2`) records the cycle count (2 = first Doctor cycle, 3 = second, …); absent means attempt 1, and legacy checkpoints without the field read cleanly.

**Suggested models by role** (`suggestedModel`, live as the role-default tier):

| Role | Model | Rationale |
|------|-------|-----------|
| Builder | `opus` | Complex implementation requires deep reasoning |
| Judge | `opus` | Code review needs thorough understanding |
| Curator | `sonnet` | Issue enhancement is structured |
| Doctor | `sonnet` | PR fixes are usually targeted and scoped |
| Architect | `opus` | System design requires sophisticated thinking |
| Hermit | `sonnet` | Code removal analysis is pattern-based |
| Champion | `sonnet` | Proposal evaluation has clear criteria |
| Guide | `sonnet` | Triage is systematic |
| Driver | `sonnet` | General-purpose default |

> **Retuning these defaults is measurement-gated.** Whether to flip a role's
> default `opus → sonnet` ("cheap-first") is decided by measured data, not edited
> blind — see [`docs/model-selection-retune.md`](https://github.com/rjwalters/loom/blob/main/docs/model-selection-retune.md)
> (upstream Loom repo) for the decision inequality and the `agent-metrics.sh
> --by-model` (#3482) gating procedure. Builder is the only real candidate; no
> default has been flipped.

**Valid model values**: aliases (`haiku`, `sonnet`, `opus`) or pinned model IDs (e.g., `claude-sonnet-4-6`).

- **haiku**: Fast, cheap - for simple status checks and monitoring
- **sonnet**: Balanced - for structured tasks with clear criteria
- **opus**: Most capable - for complex reasoning and implementation

**Aliases vs pinned IDs**: shipped role JSONs use aliases so defaults stay sensible across model releases with zero maintenance. The GitHub Actions cron workflows (`.github/workflows/loom-*.yml`) are the exception — they pin exact IDs because scheduled support roles are predictable, cost-sensitive load and a stale pin is visible and cheap to bump in the consuming repo.

Daemon sweep dispatch resolves implicit defaults **after runtime admission**. An
admitted Claude sweep keeps the cost-safe `sonnet` default (or its canary-gated
experiment arm); admitted native runtimes use their model profile without a
Claude default/experiment override. Explicit dispatch models still win, and
`autonomous.model` keeps its existing precedence and alias handling. An explicit
native model must match its profile or use `provider/model`; incompatible pins
remain errors. Claude experiments still outrank `autonomous.model` when enabled.

> **Logical-tier resolution (`sweep.modelAliases`, issue #3982).** A logical alias
> is not always current on the wire: the bare `opus` alias still resolves to a
> **previous-generation** model (`claude-opus-4-8`) while `sonnet`/`fable` resolve
> to the current generation, which would make the escalation ladder
> `sonnet → sonnet@xhigh → opus → fable` step *down* a generation at the `opus`
> rung. So every consumer keeps naming `opus` and a **single indirection point**
> maps the logical tier to the concrete ID the dispatch should use — the
> `/loom:sweep` skill via `./.loom/scripts/resolve-model.sh` and `loom-daemon`
> via `resolve_dispatch_model`. (Issue #4809:
> a **daemon-dispatched** single-issue sweep resolves through the sibling
> `resolve_autonomous_dispatch_model`, which inserts the model-cost A/B
> experiment's forced arm — when resolved-`experiment` mode confirms a canary —
> ahead of `resolve_dispatch_model`'s own config/default sub-tiers; an explicit
> dispatch `model` param still wins over both.) The shipped map
> pins only the stale tier (`opus → claude-opus-5`); `sonnet`/`fable` and pinned
> IDs pass through unchanged. Repoint or drop a pin per-repo with an additive
> `.loom/config.json` → `sweep.modelAliases` object (no code change):
>
> ```json
> { "sweep": { "modelAliases": { "opus": "claude-opus-5" } } }
> ```

**Workspace override example** (`.loom/config.json`):

```json
{
  "terminals": [
    {
      "id": "terminal-1",
      "name": "Builder",
      "role": "claude-code-worker",
      "roleConfig": {
        "workerType": "claude",
        "roleFile": "builder.md",
        "model": "claude-sonnet-4-6"
      }
    }
  ]
}
```

**Fast-harness default + per-role quality levels** (e.g. a cheap Gemini-Flash
default with Judge on a higher-quality model, including on native runtimes such
as Pi/OpenCode) is the runbook recipe in
[`configuring-resources.md`](configuring-resources.md).
