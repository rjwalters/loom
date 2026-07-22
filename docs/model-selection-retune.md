# Model-Selection Retune: The Cheap-First Gating Procedure

**Status: measurement-gated. No `suggestedModel` default has been flipped.**

This document defines the *decision criterion* and the *measurement procedure* that
must be satisfied **before** any role's `suggestedModel` default is changed from
`opus` to `sonnet` (the "cheap-first" retune sketched in #3702's D5). It exists so
that the eventual flip is **data-driven, reproducible, and one role at a time** —
never a blind batch edit made on sight of the idea.

Source issues: #3702 (D5 sketch), #3703 (opt-in escalation ladder — already
shipped), #3482 / PR #3485 (the `--by-model` measurement plumbing — already
shipped), #3706 (this document).

> **Hard rule.** Do **not** edit any `suggestedModel` value in
> `defaults/roles/*.json`, nor the model values in the `defaults/CLAUDE.md`
> "Suggested models by role" table, until the inequality below is demonstrated
> **for that specific role** over a representative sample. The flip is a separate,
> data-gated follow-up PR. This document changes no default.

---

## 1. What is actually up for retune

On `origin/main`, only the `opus` roles are candidates for the cheap-first move, and
two of them are protected by design:

| Role | Current `suggestedModel` | Retune candidate? |
|------|--------------------------|-------------------|
| Builder | `opus` | **Yes — the single real candidate** |
| Judge | `opus` | No — review needs the stronger model; stays `opus` |
| Architect | `opus` | No — system design stays `opus` |
| Doctor | `sonnet` | Already cheap |
| Curator, Guide, Champion, Hermit, Driver, Auditor | `sonnet` | Already cheap |

The D5 sketch mentioned "Builder/Doctor → sonnet", but Doctor already ships as
`sonnet`. **The only default the D5 inequality could move is
`defaults/roles/builder.json`: `opus → sonnet`.** State this plainly so nobody
"flips the batch."

---

## 2. The decision criterion (inequality)

The naive flip is attractive but can *raise* total cost: a cheaper first attempt is
only a win if Sonnet's first-attempt Judge-rejection rate is low enough that the
occasional extra escalated Doctor cycle does not dominate the per-attempt savings.
The opt-in escalation ladder (#3703) means a Sonnet first attempt that gets rejected
by Judge escalates the rejection-triggered Doctor to `opus` (`ladder[1]`), so the
cost of the "sonnet-first" strategy is a *probability-weighted* sum, not a single
attempt.

Define, per role **R**, over a representative sample:

- `cost_sonnet_attempt` — expected cost of one Sonnet first attempt for R.
- `cost_opus_attempt` — expected cost of one Opus first attempt for R.
- `cost_doctor_cycle(escalated)` — expected cost of the rejection-triggered,
  escalated (`opus`) Doctor cycle plus the re-review it triggers.
- `P(judge_reject | sonnet)` — probability a Sonnet first attempt is rejected by
  Judge for R.
- `merge_rate(strategy)` — fraction of sweeps that reach a merged PR under a
  strategy (the quality gate).

Then:

```
cost(sonnet_first) = cost_sonnet_attempt
                   + P(judge_reject | sonnet) · cost_doctor_cycle(escalated)

cost(opus_first)   = cost_opus_attempt
```

**Flip R's default `opus → sonnet` only when BOTH hold:**

```
(1)  cost(sonnet_first) < cost(opus_first)                       # strictly cheaper
(2)  merge_rate(sonnet_first) >= merge_rate(opus_first) − ε      # quality floor
```

`ε` is a small quality-tolerance band (suggested starting point: `ε = 0.02`, i.e. a
sonnet-first strategy may cost at most 2 percentage points of merge rate). Both
conditions are mandatory: cost condition (1) alone is not sufficient — a cheaper
strategy that merges materially less often is not a win. If either condition fails,
**keep the `opus` default.**

This inequality is per role. Demonstrating it for Builder says nothing about any
other role; each candidate is evaluated independently.

---

## 3. The gating procedure (how to collect the data)

The measurement plumbing already ships as `agent-metrics.sh --by-model` (#3482 /
PR #3485). Do **not** re-implement it. Collect the two terms of the inequality with:

```bash
# Per-(role, model) first-attempt success/merge signal and average cost.
.loom/scripts/agent-metrics.sh effectiveness --by-model
.loom/scripts/agent-metrics.sh effectiveness --by-model --role builder

# Per-(issue, model) realized cost, to attribute spend to the model that produced it.
.loom/scripts/agent-metrics.sh costs --by-model

# JSON for scripted aggregation across a sample.
.loom/scripts/agent-metrics.sh effectiveness --by-model --format json
```

(The `loom-agent-metrics …` console entry point and the `mcp__loom__get_agent_metrics`
MCP tool are equivalent surfaces onto the same `loom_tools.agent_metrics` module.)

### Columns to read

`effectiveness --by-model` renders, per `(Role, Model)` pair:

```
Role         Model                     Prompts    Success       Rate   Avg Cost   Avg Time
```

- **Rate** — first-attempt success rate for that `(role, model)` pair. For Builder
  this is the empirical proxy for `1 − P(judge_reject | sonnet)` when reading the
  `sonnet` row.
- **Avg Cost** — average per-prompt cost for that pair, feeding
  `cost_sonnet_attempt` / `cost_opus_attempt`.

`costs --by-model` renders, per `(Issue, Model)` pair:

```
Issue    Model                     Prompts         Cost       Tokens
```

Use it to attribute realized spend (including escalated Doctor cycles) to the model
that incurred it, so `cost_doctor_cycle(escalated)` is measured rather than assumed.

### The `model`-populated prerequisite

The `model` dimension is only meaningful when the sampled sweeps actually **recorded
a model**. Per #3482, the sweep checkpoint carries an optional `model` field
(`sweep-checkpoint.sh write <issue> <phase> --model <M>`) which flows to
`resource_usage.model` in the activity DB. Rows recorded before per-model
attribution — or spawns that inherited the session/CLI default — group under the
literal **`default`** bucket (`_model_expr` renders `NULL`/empty as `'default'`, and
degrades to a constant `'default'` on legacy DBs that lack the `model` column).

**A sample dominated by the `default` bucket cannot decide the inequality** — you
cannot compare `sonnet` vs `opus` if neither is attributed. Before trusting the
numbers, confirm that the `sonnet` and `opus` rows (not just `default`) are
populated for the role under test.

### Minimum representative sample

Do not flip on a handful of sweeps. Before the inequality is trusted for a role,
require **at least ~30 model-attributed first attempts per model arm** for that role
(so `sonnet` and `opus` each have ≥ ~30 attributed prompts), drawn from a mix of
issue complexities rather than a single easy batch. This is a floor, not a target;
more is better, and a wider spread of issue difficulty makes `merge_rate` more
trustworthy. Accumulating this sample is an **out-of-band, over-time activity** — it
is deliberately **not** a Builder deliverable and must not be fabricated.

### Runbook: accruing the observe-mode sample

The `agent-metrics.sh --by-model` plumbing above reads the **activity DB**. A
parallel, purpose-built collector — the `/loom:sweep` model-cost experiment
(#3725/#3728) — writes a durable per-phase **outcome-chain** to
`.loom/stats/sweep-model-stats.jsonl` (arm / model / attempt / Judge verdict /
Doctor-cycle count / complexity) and harvests it into exactly the per-arm §2
inequality inputs. This runbook is the copy-pasteable procedure for accruing that
sample. See `defaults/CLAUDE.md` § "Model-Cost Experiment (canary A/B, #3725)" for
the full behavior contract.

> **This runbook is the *tooling*. The multi-day accrual run itself — turning on a
> mode against real sweeps, waiting for the ≥~30-per-arm floor, harvesting a real
> sample, and re-evaluating the §2 inequality — is a DEFERRED operator action, not
> a Builder PR deliverable. Do not fabricate stats rows or a harvest report.**

**Step 1 — choose a collection mode.** Tri-state `sweep.modelExperiment` /
`LOOM_MODEL_EXPERIMENT` (env-over-config, string-valued like `guards.rmScope`):

| Mode | What it collects | Fills both arms? |
|------|------------------|------------------|
| `observe` | Passive per-phase outcome-chain; **no model forcing, `arm=null`**. Safe anywhere. | **Only if the workspace itself already runs a mix of Builder models** (e.g. a `roleConfig.model` pin, or `sonnet` first attempts that escalate to `opus`). A stock `opus`-default workspace in `observe` only ever produces Arm A (opus) rows. |
| `experiment` | Active A/B: Builder **forced** to the per-issue arm's model, complexity bump suppressed. **Canary-only.** | **Yes — deterministically balances A (opus) and B (sonnet).** This is the reliable way to fill both arms. |

Because `observe` never forces a model, a default `opus` workspace cannot generate
Arm B (sonnet) rows by observation alone — that is precisely why `experiment`
(canary) mode exists. Harvest attributes an `observe` issue to an arm from its
**observed Builder model** (opus⇒A, sonnet⇒B); rows whose Builder ran neither stay
in the `?` bucket. So `observe` is the safe, always-on collector, but a *balanced*
two-arm sample in a reasonable window generally needs `experiment` on a canary.

**Step 2 — turn the mode on.** Per-invocation (env wins over config):

```bash
# Passive observe on any sweep (no behavior change; just records the outcome-chain):
LOOM_MODEL_EXPERIMENT=observe claude -p "/loom:sweep 123" --dangerously-skip-permissions

# Active A/B on a CANARY (must confirm the canary via an UNCOMMITTED signal, else
# it loudly downgrades to observe):
LOOM_MODEL_EXPERIMENT=experiment LOOM_MODEL_EXPERIMENT_CANARY=1 \
  claude -p "/loom:sweep 123" --dangerously-skip-permissions
```

Or durably for a multi-day window via committed config (safe: the *mode* may live
in config; it is inert without the uncommitted canary confirmation):

```json
// .loom/config.json
{ "sweep": { "modelExperiment": "observe" } }
```

The canary confirmation must be **uncommitted** by design (#3731): either the
`LOOM_MODEL_EXPERIMENT_CANARY=1` env var or the gitignored `.loom/CANARY` sentinel
file (`touch .loom/CANARY`). The committed `sweep.modelExperimentCanary` flag is no
longer accepted, and a git-tracked `.loom/CANARY` is refused.

**Step 3 — no daemon-side config is needed.** `LOOM_MODEL_EXPERIMENT`,
`LOOM_MODEL_EXPERIMENT_CANARY`, and `LOOM_TRANSCRIPT_ARCHIVE` propagate to
daemon-dispatched detached sweep children (#3732), so a sweep launched via
`mcp__loom__dispatch_sweep` inherits the mode from the daemon's environment — set
the env (or the committed `sweep.modelExperiment`) once and every dispatched child
records.

**Step 4 — harvest periodically.** The harvest reader is reachable through the same
`agent-metrics.sh` surface as `--by-model`, and directly via `sweep-experiment.sh`:

```bash
# Per-arm inequality inputs (first-pass rate, Doctor cycles, merge rate, cost):
./.loom/scripts/agent-metrics.sh --model-experiment --archive-dir "$LOOM_TRANSCRIPT_ARCHIVE"
# Equivalent direct entry point + JSON for scripted aggregation:
./.loom/scripts/sweep-experiment.sh harvest --archive-dir "$LOOM_TRANSCRIPT_ARCHIVE" --format json
```

Passing `--archive-dir` (the #3726 transcript archive) upgrades cost from the
best-effort sweep-aggregate estimate to **exact** per-role token cost — each
harvested record carries a `token_fidelity` tag (`transcript` | `sweep-aggregate-log`
| `none`) naming the source. Over a multi-day run, harvest on a cron so usage is
extracted before `~/.claude/projects` is pruned (mirrors the `probe-tokens.sh` /
`archive-transcripts.sh` cron pattern):

```cron
# Harvest every 30 minutes into a log:
*/30 * * * * cd /path/to/repo && ./.loom/scripts/agent-metrics.sh --model-experiment \
  --archive-dir "$LOOM_TRANSCRIPT_ARCHIVE" >> .loom/logs/model-experiment-harvest.log 2>&1
```

**Step 5 — read `n_issues` against the floor.** The harvest prints one row per arm:

```
  arm  model    issues  1st-pass  cycles  merge      cost$    $/issue
  A    opus         31      87%    0.10    97%    ...       ...
  B    sonnet       33      74%    0.55    96%    ...       ...
```

The sample is representative enough to evaluate the §2 inequality only when **both**
arm A and arm B show `issues` (i.e. `n_issues`) ≥ ~30 (the "Minimum representative
sample" floor above). Until then, `harvest` still runs cleanly — an empty or
single-arm store reports `records: 0` / one arm and never crashes — but the
`cost(sonnet_first) < cost(opus_first)` decision stays **unevaluable**, and the
default remains `opus` (the Hard Rule at the top).

**Step 6 (DEFERRED — operator, not this tooling PR).** Once both arms clear the
floor, evaluate the §2 inequality against the harvested numbers and record a new
dated entry in the Status log below — either another "keep opus" decision citing the
now-non-empty sample, or, if conditions (1) and (2) both hold, open the separate
gated flip PR per §5.

---

## 4. Plumbing verification (result)

Verified on `origin/main` at the time this document was authored (#3706):

- **Unit coverage is present and green.** `loom-tools/tests/test_agent_metrics.py`
  contains the "Per-model dimension tests (#3482, Phase 3a)" block
  (`class TestByModel`) asserting that `effectiveness --by-model` groups by
  `(role, model)`, that `NULL`/empty models fall under `default`, and that omitting
  `--by-model` preserves the pre-#3482 output shape. Run:

  ```bash
  PYTHONPATH=loom-tools/src python3 -m pytest loom-tools/tests/test_agent_metrics.py -q
  # 61 passed
  ```

- **The CLI surface exists.** `defaults/scripts/agent-metrics.sh` documents and
  forwards `effectiveness [--by-model]` / `costs [--by-model]` to
  `loom_tools.agent_metrics`; `get_effectiveness(..., by_model=True)` and
  `get_costs(..., by_model=True)` add the model dimension via `_model_expr`, and the
  text formatters add a `Model` column only when a row carries a model.

- **Empty-DB behavior (observed).** Against a fresh activity DB with no recorded
  activity, the live commands surface a plain error rather than a per-model table,
  e.g. `Failed to get metrics: no such table: quality_metrics` (effectiveness) /
  `no such table: prompt_github` (costs). This is the *no-activity* case, distinct
  from the *legacy-schema* case the unit tests cover (a populated DB whose
  `resource_usage` table lacks a `model` column, which degrades cleanly to the
  `default` bucket). No crash or traceback; the command exits with a logged error.
  **Practical consequence:** run the gating commands against an activity DB that has
  recorded real sweeps — an empty DB has nothing to decide the inequality with.

No metrics-code gap was found that blocks the gating procedure; no new metrics code
is introduced by this document. If a future gap appears (e.g. a graceful
"no data yet" message instead of the raw missing-table error), prefer a dedicated
follow-up issue over expanding the retune scope.

### Status log

Dated re-evaluations of the gate. Each entry records that the measurement plumbing
was re-run, what it reported, and the resulting flip / no-flip decision. **No entry
here changes a default** unless it also cites a qualifying sample per Section 3.

- **2026-07-22 (#3718) — keep `opus`, no data yet.** Re-verified the full §3 gating
  path against `origin/main`. All four by-model commands still surface the documented
  *no-activity* case, not a defect:

  ```text
  $ .loom/scripts/agent-metrics.sh effectiveness --by-model
  [ERROR] Failed to get metrics: no such table: quality_metrics
  $ .loom/scripts/agent-metrics.sh effectiveness --by-model --role builder
  [ERROR] Failed to get metrics: no such table: quality_metrics
  $ .loom/scripts/agent-metrics.sh costs --by-model
  [ERROR] Failed to get metrics: no such table: prompt_github
  $ .loom/scripts/agent-metrics.sh effectiveness --by-model --format json
  [ERROR] Failed to get metrics: no such table: quality_metrics
  ```

  The #3725/#3728 model-cost experiment harvest entry point
  (`agent-metrics.sh --model-experiment`) resolves cleanly and reports zero records
  (exit 0, `records: 0`, empty per-arm table) — the expected empty state, again not a
  defect. Unit coverage stays green: `pytest loom-tools/tests/test_agent_metrics.py`
  → 61 passed.

  **Representative sample?** No. The gate requires ≥~30 model-attributed first
  attempts per model arm for the Builder role (§3, "Minimum representative sample");
  the DB has zero attributed rows for *any* arm (neither `sonnet` nor `opus`, not even
  the `default` bucket). The `cost(sonnet_first) < cost(opus_first)` inequality (§2)
  therefore cannot be evaluated.

  **Decision:** `defaults/roles/builder.json` `suggestedModel` remains `opus`. No
  default changed, preserving the Hard Rule at the top of this document. Accruing the
  sample is out-of-band, over-time work (a multi-day `LOOM_MODEL_EXPERIMENT=observe`
  or canary `experiment` campaign per `defaults/CLAUDE.md` § "Model-Cost Experiment"),
  not a Builder code change. `observe` mode is the suggested follow-up mechanism for
  generating the `sonnet` vs `opus` data points a passive `opus`-only default can
  never produce on its own.

- **2026-07-22 (#3750) — runbook + observe→harvest wiring shipped; still no data,
  accrual DEFERRED.** Delivered the operator tooling to *make* the sample
  collectable, not the sample itself:

  - Added the "Runbook: accruing the observe-mode sample" procedure to §3 (turn on
    `observe`/`experiment`, confirm the uncommitted canary, harvest on a cron via
    `agent-metrics.sh --model-experiment`, read `n_issues` against the ≥~30-per-arm
    floor).
  - Fixed a genuine observe→harvest gap: a pure `observe`-mode store (every record
    `arm=null`) previously collapsed into a single `?` bucket with `model=null`,
    dropping each row's real Builder model — so the opus-vs-sonnet split was
    unreadable. Harvest now infers an arm-null issue's arm from its observed Builder
    model (opus⇒A, sonnet⇒B); explicit `experiment` arms are unchanged
    (`loom-tools/src/loom_tools/sweep_experiment.py`, covered by new tests in
    `loom-tools/tests/test_sweep_experiment.py`).

  **Representative sample?** Still **no.** This entry ships tooling only and cites no
  numbers — no real accrual run was performed and none was fabricated. The §2
  inequality remains unevaluable until an operator accrues ≥~30 model-attributed
  first attempts on **both** arms.

  **Decision:** `defaults/roles/builder.json` `suggestedModel` remains `opus`. No
  default changed. The multi-day accrual + harvest + §2 re-evaluation + any
  opus→sonnet flip is the **deferred operator action** tracked by #3750, not closed
  by its Builder PR.

---

## 5. The flip is a separate, gated follow-up

When — and only when — conditions (1) and (2) both hold for a specific role over a
sample meeting Section 3's prerequisites:

1. Open a **separate** PR that flips *only that one role's* `suggestedModel`
   (Builder first), and updates the corresponding row in the `defaults/CLAUDE.md`
   "Suggested models by role" table.
2. Cite the measured `cost(sonnet_first)`, `cost(opus_first)`,
   `merge_rate` values and the sample size in the PR body.
3. Repeat per role. **Never** flip more than one role per PR, and never flip a role
   whose inequality has not been demonstrated.

Until then, the opt-in mechanism from #3703 (the `model@effort` escalation ladder,
complexity marker, and refusal fallback) lets an individual workspace exercise
cheap-first behavior per-config without touching any shipped default.
