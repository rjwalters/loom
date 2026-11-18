# Comparing role model/provider/effort mixes

Model, provider and reasoning effort per role are an experiment question (#13956,
operator ruling 2026-11-17): Loom does not pick a best mix, it makes mixes
**comparable**. This is the read-out for that (#14067). It reuses the
`sweep.outcome` journal — no new store.

```bash
loom-daemon sweep-outcomes summary --group-by mix --since 14d --exclude-spawn-deaths
loom-daemon sweep-outcomes summary --group-by mix --json     # same rows, machine-readable
```

## What a "mix" is

`--group-by mix` derives the label from fields every `sweep.outcome` record already
carries, so history groups too:

```text
curator=claude-opus-5 builder=claude-sonnet-5 judge=claude-opus-5 | claude/- | effort=high
```

| Part | Source | Absent means |
|---|---|---|
| `curator=` / `builder=` / `judge=` | The model with the most tokens in that role's phases (`phase_durations[].tokens_by_model`) — what ran, not what was dispatched | Role not observed in this sweep (never guessed) |
| `model=` | Dispatched `model`, only when no phase carries per-model usage | `default` |
| `runtime/provider` | `runtime` / `provider` (launch record), then `config.runtime` | `-` for that half |
| `effort=` | Sweep-level `effort` | `default` (the runtime's own) |

`doctor` is deliberately **not** in the key: it runs only when a review bounced, so
keying on it would split one mix into "needed a Doctor" and "did not". That outcome
is in the bounce columns. A sweep that died before a role ran has fewer `role=`
parts and forms its own row; `--exclude-spawn-deaths` removes the common case. For a
stamped Builder-model comparison, `--group-by arm` (`sweep.modelExperimentArms`) is
the alternative; arms force only the Builder's model, so they cannot express a
Curator/Judge mix.

## Columns

`SWEEPS`, `MERGED` (forge merge join; `n/a` under `--no-merge-join` or an
unreachable forge, never `0`), `WTOK_USD` (cost-weighted tokens, at the rate card the
report names — provisional), `MERGE/USD`, `JDG1%` / `JDG_N` (first-pass Judge
approval over judged sweeps), `REJ` (Judge `fail` verdicts), `DR_CYC` (completed
Doctor cycles), `BNC_N` (sweeps whose PR timeline was read — the denominator for
`REJ`; unread is unknown, not zero), `DOCTOR%`. `DR_CYC` is summed over every
sweep carrying `doctor_cycles`, which is `BNC_N` plus any pre-#8222 sweep that has
the cycle count but no Judge verdicts; on a recent window the two sets coincide.

## What is queryable per role step (inventory, #14067 AC1)

Counts are from one host's real journal (435 `sweep.outcome` records, 2026-11-17).

| Field | Where | Coverage |
|---|---|---|
| Model per role | `phase_durations[].tokens_by_model` | 263 / 435 — only sweeps with attributable transcripts |
| Model (dispatched) | `model` | 434 / 435 |
| Reasoning effort | `effort` — **sweep-level only**, not per role | 65 / 435 |
| Runtime | `config.runtime` (`runtime` for native-harness launches) | 435 / 435 |
| Provider | `provider` — launch record, native harnesses only | **0 / 435** (a Claude launch writes none) |
| Outcome / merged | `result`, `disposition`, `pr_number` + forge merge join | 179 / 435 opened a PR |
| Bounces | `judge_verdicts`, `doctor_cycles` | 165 / 435 (PR timeline read) |
| Cost | `tokens_by_model` | 434 / 435 |
| Mix label | not stamped; derived above | — |

**Not recorded** (so not in the mix key): per-role reasoning effort, and a provider
for Claude launches. They need a launch-time stamp per phase; until then the key
carries the sweep-level effort and a `-` provider, never a fabricated value. Role
ticks (`role_tick.outcome`) carry `role`, `model`, `effort`, `provider` and
`tokens_by_model` per tick, but are a separate journal and are not joined here.

## Recipe: compare two mixes

1. Pick a window that contains both mixes and nothing else that changed (same repo,
   same issue-size distribution, no config change mid-window).
2. `loom-daemon sweep-outcomes summary --group-by mix --since <window> --exclude-spawn-deaths`.
3. Read the two rows side by side. Trust a row only if `SWEEPS` and `BNC_N` are
   large enough that a handful of sweeps would not flip it; treat `JDG1%` with a
   small `JDG_N` as noise.
4. Compare **cost per merge** (`MERGE/USD`, or `WTOK_USD` ÷ `MERGED`), not cost
   alone: a cheap mix that bounces twice per PR can cost more per merged PR.
5. Check `REJ` ÷ `BNC_N` and `DR_CYC` ÷ `BNC_N` for rework, and `DOCTOR%` for how
   often the Doctor was needed. `DR_CYC` ÷ `BNC_N` is only exact when the window
   holds no pre-#8222 sweeps (those carry `doctor_cycles` without Judge verdicts
   and inflate the ratio), so keep `--since` after that change.

### The opposite-mix hypothesis

Mix A is **strong Curator/Judge, cheap Builder** (e.g. `curator=opus builder=sonnet
judge=opus`); mix B is the reverse (`curator=sonnet builder=opus judge=sonnet`).
The hypothesis A is testing is that a strong reviewer catches what a cheap builder
misses at lower total cost. It holds when A shows a **higher `REJ`/`BNC_N`** (the
Judge catches more) but an equal-or-better `MERGE/USD` than B. It fails when A's
extra bounces cost more in Doctor cycles than the cheaper Builder saved, or when A's
`JDG1%` is no better than B's. Run both on comparable issues. Assignment is the
operator's configuration (per-role models, see [`model-selection.md`](model-selection.md));
the Builder-only experiment arms in
[`model-cost-experiment.md`](model-cost-experiment.md) do not assign the other roles.
