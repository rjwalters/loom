# `defaults/scripts/experiments/`

Off-by-default, isolated experiment artifacts. Nothing here is wired into the
production Loom path (sweep / judge / doctor / merge). These files exist to be
picked up deliberately by a human or a future top-level session — never
auto-loaded, never triggered by a normal sweep.

## Contents

| File | What it is |
|------|------------|
| `judge-fanout-workflow.js` | **Design sketch** (issue #3739) of a Claude Code Dynamic Workflow that reviews ONE PR via multi-dimension reviewer fan-out + a typed adversarial verify pass. NOT wired into `sweep.md`/`judge.md`; NOT in a discovered `workflows/` directory, so the CLI does not auto-load it. |
| `judge-fanout-experiment.sh` | Off-by-default gate/runner. No-op unless `LOOM_JUDGE_FANOUT_EXPERIMENT=1`. When enabled it syntax-checks the sketch and prints top-level-session invocation guidance — it never dispatches a live judge run, applies no labels, merges nothing, and creates no issues. |

Full context, the capability→need mapping, the substrate boundary, and the
keep/defer/reject verdicts live in
[`docs/research/dynamic-workflows-evaluation.md`](../../../docs/research/dynamic-workflows-evaluation.md).

## Flag contract

Follows the `LOOM_MODEL_EXPERIMENT` / `sweep.modelExperiment` precedent
(off by default, loud banner when on):

```bash
# Disabled (default) — no-op, exits 0:
./defaults/scripts/experiments/judge-fanout-experiment.sh

# Enabled — banner + syntax check + guidance (still NO live judge run):
LOOM_JUDGE_FANOUT_EXPERIMENT=1 ./defaults/scripts/experiments/judge-fanout-experiment.sh
```

## Guardrails (why this is safe to have in the tree)

- **One level deep (#3289):** the workflow makes direct `agent()`/`parallel()`
  calls and never calls `workflow()` — it adds no second nesting level.
- **Single-token / in-session:** all agents share the session's one OAuth token.
  This makes NO claim to multi-account rotation — that is a `loom-daemon` +
  `spawn-claude.sh` concern on the other side of the substrate boundary.
- **Read-only:** the workflow returns a verdict object; it merges nothing, applies
  no `loom:pr` / `loom:changes-requested` transitions, and creates no GitHub issues.
- **Deferred:** the runnable prototype + measured comparison are a named follow-up,
  not delivered here. See the evaluation doc → "What is deferred".
