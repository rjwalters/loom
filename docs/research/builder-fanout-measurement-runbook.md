# Builder fan-out (best-of-N) measurement runbook (issue #4776)

**Audience:** an operator deciding whether Loom should build an opt-in
`/loom:sweep --fanout N` mode — N independent Builder attempts on one issue, each
in its own worktree, with Judge picking the winning PR.

**What this document is:** the measurement/design step #4776 was scoped to, and
the mirror image of `docs/research/judge-fanout-measurement-runbook.md` on the
generation side. It selects a corpus of historically-hard issues from this repo's
own record, works one of them through what N=3 would actually require, prices the
fan-out against this machine's live dispatch cap, and ends in a proceed/defer
call.

**What this document is NOT:** it changes no code. `dispatch_sweep`,
`defaults/scripts/worktree.sh`, `defaults/.claude/commands/loom/sweep.md`, and
`defaults/roles/judge.md` are read-only references here, per #4776's non-goals.

> **Do not fabricate numbers.** Same invariant the Judge-side runbook holds
> (`judge-fanout-measurement-runbook.md`, and
> `dynamic-workflows-evaluation.md` before it): if a cell was not measured, it is
> **blank** and says so. Several columns below are blank on purpose — see
> "What could not be measured".

**Origin:** `.loom/docs/survey-orca-2026-07-31.md` idea 1 (verdict: *adapt*),
filed from #4775.

---

## Step 1 — Select the corpus (3–5 historically-hard issues)

### Selection method and its provenance

Two candidate signals were tried. Only one survived.

**Signal A (used): `attempt >= 2` in this host's sweep checkpoints.** Each
`.loom/sweep-checkpoint/issue-<N>.json` carries an `attempt` field on
`doctor-done` / `judge-rejected` phases — the live Doctor-cycle counter.
Scanning all 297 checkpoint files on this host yielded exactly **five** issues
with `attempt >= 2`. That is the corpus.

```bash
cd .loom/sweep-checkpoint && for f in *.json; do
  jq -r --arg f "$f" 'select(.attempt != null and .attempt >= 2)
    | "\($f)\t\(.phase)\tattempt=\(.attempt)\tpr=\(.pr_number)"' "$f"
done
```

*Provenance caveat:* checkpoint files are **overwritten per issue** and are
**host-local**, so `attempt` is a lower bound on the last sweep only, and issues
worked from another host are invisible. This is a floor, not a census.

**Signal B (rejected as a proxy, kept as a finding): merged PRs still carrying
`loom:changes-requested`.** Repo-wide this returns only 6 PRs (#4575, #4560,
#3869, #2594, #2490, #2445). Reading the three recent ones showed the label is
**not** a reliable "Judge bounced the first attempt" marker — every one is
contaminated by a concurrent-Judge race:

- **#3869** — `Changes Requested` at `09:05:18Z`, `Approved!` at `09:05:22Z`.
  Four seconds apart, two Judge dispatches on one PR.
- **#4575** — approved and auto-merged at `05:53:42Z`/`05:54:52Z`; a second
  Judge's `Changes Requested` landed at `05:55:11Z` and self-annotated as "moot
  … a race between two Judge dispatches on the same PR" (the real finding was
  re-filed as #4577).
- **#4560** — carried `loom:pr` **and** `loom:changes-requested` simultaneously;
  a Judge posted an explicit label-state correction at `05:30:42Z` describing
  the collision.

This matters beyond corpus selection: **any future best-of-N measurement that
scores "did Judge reject attempt K" from labels will over-count**, because
Loom's own concurrent Judge dispatches already produce contradictory verdicts on
a single PR. A fan-out measurement has to read verdict *comments*, not labels.

### The corpus

| Issue | Winning PR | Doctor cycles (checkpoint `attempt`) | Judge changes-requested rounds (comment-counted) | PR open → merged | Diff size | Bounce-cause class | Outcome |
|---|---|---|---|---|---|---|---|
| [#4405](https://github.com/rjwalters/loom/issues/4405) — review branches leak into the main checkout | [#4437](https://github.com/rjwalters/loom/pull/4437) | 3 (`judge-rejected`) | 3 | 7h 14m | 3 files, +652/−60 | **Sequentially-revealed defects** in one approach | merged |
| [#3786](https://github.com/rjwalters/loom/issues/3786) — phantom labels in role prompts | [#3854](https://github.com/rjwalters/loom/pull/3854) | 3 (`doctor-done`) | 1 | 37m | 29 files, +625/−404 | **Stale base / semantic merge conflict** (main moved twice) | merged |
| [#4492](https://github.com/rjwalters/loom/issues/4492) — secure Codex account lifecycle CLI | [#4519](https://github.com/rjwalters/loom/pull/4519) | 2 (+1 explicit grace cycle) | 2 | 34h 45m | 12 files, +1957/−22 | **Depth** — each review found distinct defects in paths the *previous fix* introduced | merged |
| [#4393](https://github.com/rjwalters/loom/issues/4393) — dashboard phase 3 | [#4457](https://github.com/rjwalters/loom/pull/4457) | 2 (`doctor-done`) | 1 (content **lost**) | 5h 19m | 4 files, +1650/−35 | **External/infrastructure** — the sweep died mid-Judge, and the replacement review was destroyed by the `--body @path` bug (#4523) | merged |
| [#4230](https://github.com/rjwalters/loom/issues/4230) — user-scoped mcp-loom registration | [#4264](https://github.com/rjwalters/loom/pull/4264) | 2 (`doctor-done`) | **0** (approved on first review) | 14m 31s | 10 files, +688/−119 | **Sibling-PR merge race** — #4263 landed ~1 min earlier and conflicted in `scripts/loom` | merged |

All five have a known ground-truth outcome: **merged**. That is the answer key —
in every case a single Builder attempt plus iteration eventually produced a PR
the Judge approved.

### What the corpus says about the *cause* of difficulty

This is the load-bearing read, because best-of-N only helps one of these classes.

- **#4230, #3786 — attempt-quality-independent (2/5).** #4230's first review was
  a clean approve; its extra cycle was purely a textual conflict with a sibling
  PR that merged one minute earlier ("Judge's approval on substance stands —
  this was a textual merge conflict only"). #3786's blocking review opened
  "The implementation itself is solid … **But** the branch is 8 commits behind
  `main`". Three parallel attempts would each have been cut from the same base
  and would each have gone stale identically. Fan-out cannot help here, and by
  lengthening the window between base-cut and merge it makes this class
  *marginally worse*.
- **#4393 — infrastructure (1/5).** The claiming sweep "died with no Judge
  output", its re-dispatch "crashed immediately (`Unknown command: /loom:sweep`)",
  and the eventual review was posted with `--body @path`, which posts the literal
  string and destroyed the review. The one substantive defect (missing CORS
  headers) was caught by a **Curator cross-reference**, not by the Builder attempt
  at all. Running three Builders would have changed nothing about any of it.
- **#4405, #4492 — approach-coupled and *sequentially revealed* (2/5).** These
  are the only rows where "the Builder produced a weaker artifact" is even
  arguably the cause, and both have the same shape: each defect only became
  visible **after the previous fix was executed**. On #4405 the chain was
  (1) a Shellcheck SC2034 warning, then (2) `_maybe_delete_local_branch`'s three
  transitive helpers missing from the `awk` extraction — reproduced live as
  `_find_worktree_by_branch: command not found` — then (3) a pre-existing
  `set -e` abort in a loop the PR never touched, which defeated the PR's primary
  deliverable in this repo. On #4492 the Judge's own words: the extra cycle was
  granted "because the second review uncovered **distinct defects in newly added
  recovery/rollback paths**". Defect 2 could not exist until defect 1 was fixed
  and the script actually ran. **Parallel independent attempts do not substitute
  for an execute-verify-iterate loop**; three attempts that each stop at their
  first plausibly-passing state would each ship defect 1.

**Score: at most 2 of 5 measured hard issues are in a class best-of-N could
address, and in both of those the winning artifact was reached by iteration, not
by picking a luckier first draft.**

---

## Step 2 — Two accidental N=2 fan-outs already in the record

The strongest evidence here is that Loom has **already run this experiment
twice, by accident**, via concurrent dispatch on the same work item. Both are in
the corpus.

**#4385 / PR #4560 — two concurrent Builder sessions on one issue.** The second
session's comment: *"Two `/loom:sweep 4385` sessions ran concurrently **against
this worktree**. I audited the `#[serial]` surface independently, before reading
this PR body, and reached the same classification."* Two facts fall out:

1. **The two attempts converged.** The independent audit reproduced the first
   attempt's classification (and refined one count: 372 `#[serial]` attributes,
   up from the Curator's 298). It did not produce a materially different or
   better candidate to choose between — there was nothing for a Judge to pick.
2. **They shared one worktree.** Not two isolated ones. See Step 3 for why that
   is structural, not an accident of that particular run.

**#4405 / PR #4437 — two concurrent Doctors on one grace cycle.** *"I was
dispatched on the same grace cycle and reached the same fix independently. A
concurrent Doctor pushed `18a3e73c` while I was testing, so I reset my work
rather than force-pushing over it."* Again convergence, plus a real marginal
gain: the second agent recorded three non-blocking observations the winner
missed (discarded `git branch -D` stderr; two unguarded `_extract_shell_fn`
command substitutions with the same `set -e` shape). It judged them "none is
worth another review cycle" and did not push.

That marginal gain is the honest strongest argument *for* fan-out — a second
independent attempt does find real, if minor, extra things. It is also the
honest ceiling observed so far: **two natural N=2 trials, two convergences, zero
cases where a Judge would have had a materially better candidate to pick.**

---

## Step 3 — What N=3 would actually require (worked against #4405)

Take the hardest corpus row, #4405, and walk the operation end to end.

### 3a. Worktree isolation — the "just reuse `worktree.sh`" premise is false

`#4776`'s proposal assumes N attempts can reuse the existing isolation primitive
at `.loom/worktrees/issue-N-attempt-{1..N}`. Verified against
`defaults/scripts/worktree.sh` on `main`, they cannot:

- **The path is derived from the bare issue number only.**
  `WORKTREE_PATH="$WORKTREE_ROOT_DIR/issue-$ISSUE_NUMBER"`. There is no attempt
  suffix, and the first positional argument after the issue number sets
  `CUSTOM_BRANCH` (`BRANCH_NAME="feature/$CUSTOM_BRANCH"`) — it changes the
  *branch*, never the *path*. Attempt 2 invoked for issue 4405 finds an existing
  `.loom/worktrees/issue-4405` and takes the "worktree already exists" branch —
  reusing (or stale-resetting) attempt 1's tree. That is the exact collision
  observed live on #4385 above.
- **The default branch collides too.** `BRANCH_NAME="feature/issue-$ISSUE_NUMBER"`,
  so three attempts would push to one remote branch unless every caller passes a
  distinct `CUSTOM_BRANCH`.
- **Worktree creation is serialized repo-globally anyway.** The lock is
  `worktree-add` (`_worktree_lock_path` ignores its issue argument by design —
  `git worktree add` mutates `.git/config`). Cheap, but N attempts do not
  parallelize *creation*.
- **Cleanup is keyed on the same convention.** `merge-pr.sh` discovers
  `.loom/worktrees/issue-N` + the `.loom-managed` sentinel (whose body records
  `# Issue:` / `# Branch:`) to auto-remove a worktree on merge. Three
  differently-named attempt worktrees and their branches would be invisible to
  that path, so N−1 losers would be **left dangling** — the precise outcome
  #4776 says must not happen.

Net: fan-out needs changes to `worktree.sh` (attempt-suffixed paths + branches),
`merge-pr.sh` (loser disposal), and `cleanup-branches.sh` / `loom-clean` (which
map `feature/issue-<N>` branches back to issues one-to-one). "Reuse, don't
reinvent" is not available at N>1.

### 3b. What a Judge would need to compare N candidate PRs

Today `defaults/roles/judge.md` (1766 lines) reviews **one** PR against **one**
issue and emits a binary verdict expressed as labels: approve → `loom:pr`,
reject → `loom:changes-requested`. There is no ranking surface, no "candidate"
concept, and no notion of PRs being alternatives. A best-of-N Judge would need
all of:

1. **A comparative verdict shape** — rank N candidates and approve exactly one.
   Today's approve/reject verdict does not express "correct, but #2 is better".
2. **A dedupe guard in Champion.** This is a **safety blocker, not a nicety**.
   Champion's merge path has duplicate detection, but every instance of it is
   scoped to *follow-on issue creation* — `champion.md` § "Duplicate Prevention"
   ("searches for existing issues with 'Follow-on from PR #N' in the title"),
   and Stage 5 "Duplicate Detection" in `champion-pr-merge.md` /
   `champion-reference.md`. Nothing in the merge criteria asks whether **another
   open PR closes the same issue**; the nearest neighbour,
   `champion-reference.md`'s "another PR merged first causing conflicts", is a
   conflict-recovery scenario, not mutual exclusion. Three
   fan-out PRs would each carry `loom:review-requested`; a *different* Judge
   dispatch could independently approve a loser (which is exactly what the
   concurrent-Judge races in Step 1's Signal B demonstrate happens routinely),
   and Champion would merge two or three near-duplicate implementations of one
   issue. Fan-out cannot ship before Champion knows that N PRs closing the same
   issue are mutually exclusive.
3. **A loser-disposal path** — close N−1 PRs, delete their branches, remove
   their worktrees, and leave the issue's labels coherent.
4. **≈N× review cost for the winning decision.** On #4405 the winning diff was
   3 files/+652/−60; the Judge's actual work was not reading it but *executing*
   it (reproducing `_find_worktree_by_branch: command not found` end-to-end,
   running `shellcheck 0.10.0` with CI's exact invocation, running 12
   `test-merge-pr-*.sh` suites). Comparing three candidates means doing that
   three times. Judge cost is where fan-out's expense actually concentrates, and
   the #4776 proposal does not account for it.

### 3c. What the operator sequence would look like

```
dispatch --fanout 3 4405
  ├── attempt 1 → .loom/worktrees/issue-4405-attempt-1 → feature/issue-4405-a1 → PR X
  ├── attempt 2 → .loom/worktrees/issue-4405-attempt-2 → feature/issue-4405-a2 → PR Y
  └── attempt 3 → .loom/worktrees/issue-4405-attempt-3 → feature/issue-4405-a3 → PR Z
                        (3 admission slots held, 3 distinct tokens)
  → Judge(compare X,Y,Z) → approve one, close two, delete two branches,
                           remove two worktrees, then the normal Doctor loop
```

Every arrow above except "the normal Doctor loop" is new mechanism.

---

## Step 4 — Token / concurrency cost of an N=3 fan-out

Measured live on this host, `loom-daemon status`, 2026-07-31:

```
Dynamic concurrency cap: 16  (the number dispatch uses)
  = min(healthy 8 × per-token 3 = 24, disk headroom 78, configured max 16)
Token pool: 12 accounts, 7/12 healthy on live probe
            (dispatch still using a stale .ranking that says 8)
Managed repos: 11        In-flight at observation: 9
```

Effective config: `autonomous.workFinder.maxConcurrent = 16`,
`autonomous.perTokenConcurrency = 3`, `autonomous.workFinder.maxAdmissionsPerTick`
unset ⇒ default **3** (`.loom/docs/daemon-reference.md`).

| Axis | Formula | N=1 today | N=3 fan-out | Share consumed by one issue |
|---|---|---|---|---|
| Admission slots | `min(token, disk, maxConcurrent)` = 16 | 1 slot | 3 slots | **18.75% of the whole machine**, across an 11-repo fleet |
| Token axis | `healthy × perTokenConcurrency` | 1 of 24 (ranking) / 1 of 21 (live 7 healthy) | 3 of 24 / 3 of 21 | 12.5% / **14.3%** |
| Per-tick ramp (`maxAdmissionsPerTick` = 3) | new sweeps admitted per tick | 1 of 3 | **3 of 3** | one fan-out issue **saturates an entire tick's admission budget** |

Three consequences worth stating plainly:

1. **The ramp cap is the sharp edge, not `maxConcurrent`.** An N=3 fan-out is
   dispatched as one unit but eats the whole per-tick admission budget, so every
   *other* repo in the fleet is delayed by at least one full tick per fan-out.
   At the current observed state (9 in flight, cap 16, "the limiter is work
   availability, not tokens/disk/CPU") there is nominal headroom — but headroom
   at the cap axis does not buy headroom at the ramp axis.
2. **N attempts must land on N distinct tokens** or the fan-out defeats its own
   premise (three attempts on one account share a utilization window and rate-
   limit together). At 7 healthy accounts that is fine for N=3 today, and
   degrades exactly when the pool degrades.
3. **Judge cost scales with N too** (Step 3b), and the Judge phase is the one
   the corpus shows doing the expensive work — executing candidates, not reading
   them. A "3× cost" framing understates it; the honest framing is
   *3× Builder + ~3× Judge − 1 Doctor loop*.

### What could not be measured (blank on purpose)

- **Absolute tokens per Builder phase.** Loom does not persist per-phase token
  accounting in a queryable place on this host — `.loom/` has no
  `sweep-model-stats.jsonl`, and the per-subagent transcript path
  (`~/.claude/projects/-Users-rwalters-GitHub-loom/subagents/`) is **empty**,
  because sweeps run as detached processes rather than in-session subagents. No
  dollar or token figure is estimated here.
- **Builder-phase wall clock per corpus issue.** Checkpoints record a single
  overwritten phase, and the surviving sweep logs
  (`sweep-issue-{3786,4230,4393}.log`) carry no phase timings. The one latency
  column in the corpus table is *PR open → merged*, which is forge-measurable
  and honest, and it includes non-Builder waits (e.g. #4492's 34h includes an
  operator size-waiver wait).
- **Whether a *real* N=3 on a corpus issue beats N=1.** Not run. Running it
  requires the mechanism from Step 3, which does not exist. Everything above is
  an analysis of what the mechanism would cost and what the historical record
  suggests it would buy — not a trial.

---

## Step 5 — Recommendation: **DEFER** (do not build dispatch-level fan-out yet)

**Defer.** The measurement does not support proceeding to a dispatch-level
experiment, on three grounded counts. First, the corpus does not contain the
failure mode fan-out fixes: of five measured historically-hard issues, three
bounced for reasons entirely independent of attempt quality (a sibling-PR merge
race one minute wide, an 8-commit-stale base, and an infrastructure chain in
which the sweep died and the replacement review was destroyed by the
`--body @path` bug), and the remaining two bounced on defects that were
*sequentially revealed* — each visible only after the previous fix was executed
— which parallel independent attempts do not shorten, because all N stop at
their own first plausibly-passing state. Second, Loom has already run this
experiment twice by accident (two concurrent Builders on #4385, two concurrent
Doctors on #4405) and both trials **converged**: the second attempt reproduced
the first's answer, yielding a Judge nothing meaningful to choose between, with a
marginal gain (three non-blocking observations on #4405) the finder itself judged
not worth a review cycle. Third, the cost side is worse than #4776 assumed on
both axes: the "reuse `worktree.sh`" premise is false — path *and* branch are
both derived from the bare issue number, so N attempts collide and N−1 losers
would be invisible to `merge-pr.sh`'s sentinel-keyed cleanup — and Champion's
merge criteria never ask whether another open PR closes the same issue (its
duplicate detection is scoped entirely to follow-on issue creation), so given
the concurrent-Judge races
already documented on #3869/#4560/#4575, fan-out today would risk merging two or
three near-duplicate implementations of one issue. Meanwhile one N=3 fan-out
consumes 18.75% of this machine's entire dispatch cap and 100% of a tick's
admission ramp, for a single issue, across an 11-repo fleet.

### What would have to be true to revisit

Revisit if any of these change — otherwise leave the single-Builder default
alone:

1. **A corpus emerges whose dominant bounce cause is "the Builder picked a
   structurally worse approach", not staleness/races/infrastructure.** Re-run
   Step 1's checkpoint scan; the query is three lines and the answer is cheap.
2. **The convergence result reverses.** If a future accidental (or deliberate,
   read-only) N=2 produces two *materially different* candidate approaches
   rather than the same answer twice, the premise gains its missing evidence.
3. **Champion learns that N PRs closing one issue are mutually exclusive**, and
   `worktree.sh` / `merge-pr.sh` / `cleanup-branches.sh` learn attempt-suffixed
   paths and branches. These are prerequisites for *any* fan-out, and each is
   independently defensible — the Champion dedupe guard especially, since the
   concurrent-Judge races that make it necessary already happen at N=1.

### Follow-up filing

**None filed.** #4776's acceptance criteria call for a follow-up issue only *if*
this doc recommends proceeding; it recommends deferring, so filing a
`/loom:sweep --fanout N` implementation issue would put work in the queue this
measurement does not justify. The three revisit conditions above are recorded
here rather than filed, following the precedent in
`dynamic-workflows-evaluation.md` § "Recommended follow-ups (to be filed by a
human/Curator — NOT filed here)". A human or Curator who disagrees with the
defer call has, above, the exact queries and the exact list of surfaces that
would need to change.

---

## References (read-only)

- `docs/research/judge-fanout-measurement-runbook.md` — the Judge-side
  precedent this document mirrors, and the source of the "do not fabricate
  numbers" invariant.
- `docs/research/dynamic-workflows-evaluation.md` — keep/defer/reject verdict
  shape and the do-not-file-follow-ups-autonomously precedent.
- `.loom/docs/survey-orca-2026-07-31.md` § "Idea 1 — Fan-out / best-of-N
  dispatch" — the *adapt* verdict that produced #4776.
- `.loom/docs/token-pool.md`, `.loom/docs/daemon-reference.md` §"dynamic cap" /
  §"Per-tick admission (ramp) cap" — the concurrency model priced in Step 4.
- `defaults/scripts/worktree.sh` — `WORKTREE_PATH` / `BRANCH_NAME` derivation
  and the repo-global `worktree-add` lock (Step 3a).
- `defaults/scripts/merge-pr.sh`, `defaults/scripts/cleanup-branches.sh` —
  sentinel-keyed worktree/branch cleanup that N−1 losers would evade.
- `defaults/roles/judge.md`, `defaults/roles/champion.md`,
  `defaults/.claude/commands/loom/champion-pr-merge.md` — today's single-PR
  verdict surface, and duplicate detection scoped to follow-on issues only.
- Corpus: issues #4405, #3786, #4492, #4393, #4230; PRs #4437, #3854, #4519,
  #4457, #4264; race-artifact PRs #3869, #4560, #4575.
