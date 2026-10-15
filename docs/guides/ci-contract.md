# CI Contract

Which checks must be green for a change to merge into `main`, where each one
runs, and which layer is the single authority for it. Written for #11710; the
evidence is the 30-day inventory posted on that issue.

**Measured at** `origin/main` @ `b80690ecc` (2026-10-15): 17 files in
`.github/workflows/`, of which `ci.yml` has 26 top-level jobs. Job timings come
from the 150 most recent `CI` runs (since 2026-10-12); per-workflow counts come
from the most recent 1000 runs per workflow, which the Actions API caps. Counts
drift, so re-measure before acting on a number.

## Layers and authority

| Layer | Runs | Authority for |
|-------|------|---------------|
| Local pre-PR gate (`buildGate`, #11690) | Builder worktree, before the PR exists | Compiles and passes the fast tier. Not a merge gate. |
| PR CI (`ci.yml`) | Every PR, every push to `main`, merge group | Everything that must be green to merge. One authority per suite. |
| Daily (`ci-daily.yml`) | Schedule, 08:00 UTC | Clean unpartitioned build, flake detection, release-profile builds, upcoming toolchain. Never blocks; `report` files one `[ci-daily]` issue per red job. |
| Judge | After the PR is open | Reads CI state, reviews the diff. Does not re-run the suites. |

Each test suite has exactly one authority in this table. A suite that appears in
two layers is a duplicate (see Cut rules) unless the second layer deliberately
undoes an optimisation of the first, as `ci-daily` does (see its header
comment).

## The required set

**Today** (ruleset `main`, id 8809610, as of 2026-10-15): `Structural Checks`,
`Daemon Checks`, `Shell Syntax (macos-latest)`. `merge-pr.sh` reads these
contexts and nothing else. The other 23 `ci.yml` jobs (Rust unit tests, native
port suites, installer tests, ...) are gated only by `CI Result`, which is *not*
required. A plain merge past a red unit-test shard is therefore possible.

**Target**: one context, `CI Result`.

| Required check | Runs | Authority for | Why |
|----------------|------|---------------|-----|
| `CI Result` | PR CI, `if: always()`, also on `merge_group` | Aggregates all 26 `ci.yml` jobs, treating a path-filter skip as success | The only context that exists on every event and covers every job. The three current contexts are a subset of what it already aggregates, so they stop being separate rules. |

Why one context rather than a longer list: `ci.yml` is path-filtered, so most
jobs are legitimately skipped on a given PR. A skipped job never reports a
required context and would block every merge. `CI Result` is the one job that
understands the skips (`scripts/ci-result-gate.sh`).

**The ruleset edit is an operator step.** It changes repository settings and
needs admin rights; the Builder of this document did not make it. The command
replaces only the `required_status_checks` rule and leaves every other rule
and bypass actor as it is:

```bash
gh api repos/rjwalters/loom/rulesets/8809610 \
  | jq '{name,target,enforcement,conditions,bypass_actors,
         rules: (.rules | map(if .type=="required_status_checks"
           then .parameters.required_status_checks = [{"context":"CI Result"}]
           else . end))}' \
  | gh api -X PUT repos/rjwalters/loom/rulesets/8809610 --input -
```

Verify with
`gh api repos/rjwalters/loom/rulesets/8809610 --jq '.rules[]|select(.type=="required_status_checks")'`.
Roll back by running the same command with the three current contexts. A job
that is renamed or removed must have its ruleset entry updated in the same
change, or every merge blocks on a context that never reports.

Until #12137 lands (merge helper and Champion criterion 6 disagree on a
required check that goes pending to failed mid-wait), the required set is
exactly the ruleset's contexts.

## Not in the contract

These run but are not required, and each says why it is advisory:

- **Role workflows** (`loom-auditor`, `loom-champion`, `loom-curator`,
  `loom-guide`, `loom-judge`): `workflow_dispatch` only; scheduled role runners,
  not PR checks. The daemon role runner is the real dispatcher.
- **Scheduled maintenance** (`ci-daily`, `codeql`, `harness-pins`,
  `fork-drift`, `version-bump-on-merge`): they act on `main` after the merge.
- **Push-to-`main` reporting** (`security`, `loc`, `shell-lint`, `ci-images`):
  `security` and `shell-lint` also run on a path-filtered PR, so a red result is
  visible to a reviewer but is not a merge gate.
- **`release.yml`** reads `CI`'s conclusion on `main` through `workflow_run`;
  it is a consumer of the contract, not part of it. `hosted-build-gate.yml` is a
  reusable workflow called from it.

## Cut rules

A check is cut, folded or demoted by these triggers, so that a proposal to
remove one is decidable:

1. A check that nothing reads (not required, not read by `merge-pr.sh`, Judge
   or release) is cut or made explicitly advisory.
2. A check that duplicates another layer's authority is folded into that
   layer.
3. A check red on more than half of its last-30-day runs for a non-regression
   cause (flake, infrastructure, timeout) is fixed or made advisory. It does
   not stay always-red.

Cuts are one issue each, filed from the inventory. This document does not edit
workflows.

## Always-red dispositions

Against rule 3, over the inventory window:

| Check | Window result | Disposition | Follow-up |
|-------|---------------|-------------|-----------|
| `CI Daily` → Flake Detection (shell suites, macos-latest) | 2 completed (both red), 15 cancelled by timeout, 0 green | Advisory until #12043 lands, then fix | #12198 (`loom:triage`); existing report #12019 |
| `Security Scan` | 152 of 1000 runs red, all before #11688 closed; the latest runs on `main` are green | No change: fixed | #11688 |

No `ci.yml` job is red on more than half of its runs. `Structural Checks`
(20 of 136) and `Rust Unit Tests` (3/5: 13 of 101) are the noisiest; they stay
required and are reported in the inventory.

## Related

- [`.loom/docs/ci-principles.md`](../../.loom/docs/ci-principles.md): the rules
  the workflows follow (never cancel a distinct commit's run, daily reporting).
- [CI/CD Integration Guide](ci-cd-setup.md): consumer-side Loom CI setup.
- #11690 (pre-PR gate), #11703 (fleet configuration precedence), #12137 (merge
  helper alignment).
