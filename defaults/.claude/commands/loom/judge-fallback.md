# Judge: Fallback Queue (Review Only)

**Load when**: `loom-daemon pr-queue --role judge` serves a `mode=fallback` row (unlabeled / external-contributor PRs).

On-demand sub-skill of `judge.md` (split out in issue #13957): the core workflow stays in `judge.md`, which points here by the trigger above. Section references to the main workflow live in `judge.md`; existing split-outs `judge-reference.md` and `judge-ci-red.md` work the same way.

### Fallback Queue (Review Only)

`pr-queue --role judge` lists fallback beside labeled work for trusted interactive PRs (preference on) and, after labeled rows, unlabeled same-repo `feature/issue-<N>` PRs. Other shapes and forks wait for an empty labeled queue. Fallback review never adds labels or authorizes branch changes/automated landing.

**Incident this section guards against (#5455)**: PR #4972 — a 2-line
Dependabot `Cargo.lock` bump — accumulated **199 fallback-mode Judge
comments over 37 hours** without ever leaving the fallback queue. The
SHA-based dedup below (#5058) is correct in spec, but before #5455 the
*entire* decision (dedup, cap, structural exclusion) lived in prompt-embedded
bash an LLM had to faithfully re-derive on every pass; a fleet
propagation-lag window (some hosts still running the pre-#5058 prompt) let
~21h of comments through even after the dedup fix had merged. **The
skip/evaluate decision itself is now a real script,
`.loom/scripts/judge-fallback-guard.sh`** — correctness no longer depends on
the model re-deriving multi-step bash from prose each pass.

**Fallback search**:
```bash
# Find PRs without any loom: labels (cached — see "Cached Forge Reads")
"$GH_READ" pr list --state=open --limit 500 --json number,title,labels \
  --jq '.[] | select(([.labels[].name | select(startswith("loom:"))] | length) == 0) | "#\(.number) \(.title)"'
```

**Per-PR gate — `judge-fallback-guard.sh`**: for each candidate PR number
from the search above, run:

```bash
./.loom/scripts/judge-fallback-guard.sh <PR>
```

It prints `KEY=VALUE` lines and exits with a code that names the decision:

| Exit | `DECISION` | Meaning |
|------|------------|---------|
| `0`  | `EVALUATE` | No skip condition matched — proceed with fallback-mode review |
| `10` | `SKIP` | PR author is a bot (`is_bot: true` — Dependabot, Renovate, `github-actions[bot]`, …). Outside the Loom label workflow by construction: the fallback path never labels it, so it can never leave this query's result set through any Loom-side action. Skipped **permanently**, checked **before** the cap. |
| `11` | `SKIP` | **Lifetime cap reached** — `MARKER_COUNT` (total `loom:fallback-evaluated` markers across the PR's *entire* comment history, **not scoped to the current head SHA**) has reached `--cap` (default 20). See "Why the cap is per-PR-lifetime, not per-SHA" below. |
| `12` | `SKIP` | SHA dedup (#5058) — the most recent marker's SHA already equals the current head SHA; nothing changed since the last evaluation. |
| `1`  | — | Environment/`gh` error — treat exactly like any other fallback-queue `gh` failure (see the Pre-Iteration Environment Check above); **never** interpret this as "no work available". |

Also read `VELOCITY_ALERT` from the output — **independent of `DECISION`**: if
`VELOCITY_ALERT=1` (the PR's marker-comment count within the trailing
`--velocity-window-hours`, default 4h, meets or exceeds `--velocity-threshold`,
default 8), surface it loudly regardless of whether the PR was evaluated or
skipped — e.g. a fleet handoff (`.loom/scripts/fleet-send.sh --type handoff`)
or a one-line note in your own turn's output naming the PR and `VELOCITY_COUNT`.
This is a backstop distinct from the cap: it exists so a bug in the cap/dedup
logic itself is loud, not discovered 199 comments later.

**Why the cap is per-PR-lifetime, not per-SHA**: #4972's 199 comments
accumulated on a *single, unchanged* head SHA over 37 hours, so a per-SHA cap
alone would have bounded that specific incident — but a per-SHA-only cap does
not bound a PR that repeatedly force-pushes trivial commits, each reset
resetting a per-SHA counter back to zero. `judge-fallback-guard.sh` counts
markers across the PR's *entire* comment history regardless of how many times
the head SHA has changed, so the lifetime total only ever goes up. The
SHA-based dedup (exit `12`) still runs on top of the lifetime cap for its
original purpose (skip re-evaluation of an unchanged PR between ticks) — it
no longer has to be the *only* bound.

**IMPORTANT: Fallback mode behavior**:
- **DO evaluate the code** thoroughly with same standards as labeled PRs
- **DO provide feedback** via comments
- **DO NOT add workflow labels** (`loom:pr`, `loom:changes-requested`) to unlabeled PRs
- **DO NOT update PR labels** at all - these may be external contributor PRs outside the Loom workflow

**Queue selection (every pass):**
```bash
QUEUE=$(loom-daemon pr-queue --role judge) || exit 1
# Walk EVERY row in order; a held/claimed/pending row never ends the pass.
printf '%s\n' "$QUEUE" | jq -r '.[] | [.number, .mode, .origin, .priorityReason] | @tsv'
```

For each `workflow` row, run the normal claim, CI, evaluation and verdict
workflow above. For each `fallback` row, re-run `judge-fallback-guard.sh <PR>`
immediately before evaluation. The queue already uses this guard for discovery;
the fresh check closes the intervening head/comment race. Exit 10/11/12 means
continue to the next row, exit 1 is an environment failure. Surface its velocity
alert as described above. Evaluate in a PR worktree and post a comment containing
`<!-- loom:fallback-evaluated sha=<evaluated-head> -->`; **do not add Loom workflow
labels, change the contributor branch, or call the normal verdict-label path**.
The same restrictions apply to interactive agent PRs. If a preferred PR needs
a human response or cannot advance, continue to the next candidate.

**Benefits of fallback queue**:
- Maximizes Judge utilization during low-activity periods
- Provides proactive code evaluation on external contributor PRs
- Catches issues before they accumulate
- Respects external PRs by not adding workflow labels
- Bounded: a bot-authored PR (e.g. Dependabot) is excluded structurally, and
  any other PR the queue cannot advance is bounded by a per-PR-lifetime cap
  rather than re-evaluated indefinitely (#5455)
