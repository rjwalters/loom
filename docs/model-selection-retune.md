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
