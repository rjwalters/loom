# Merge-Queue CI Qualification

**Issue #10257** (Phase C of #9978). With GitHub's merge queue, Loom would
enqueue a Judge-approved PR rather than merging it directly. That
only protects `main` if every suite Loom relies on validates the **combined
merge-group tree**, meaning the base plus every PR queued ahead of this one. A
workflow can list `merge_group` as a trigger and still skip the suites that
matter on it. This document covers the read-only tooling that proves a
repository's workflows are qualified, the prerequisites for enabling queue mode,
and how to roll back.

**Status:** queue execution is compiled on (#12323) but opt-in via
`champion.mergeMode=queue`. Phases A and B, #10255 and #10256, provide the queue
controls and the Champion handoff; the `merge_group` check workflow is the
optional template `defaults/optional/github-workflows/merge-group-check.yml`.
`2AMLogic/loom` is now organization-owned, so it is eligible for the pilot
once the prerequisites below pass (#14159). As of 2026-11-17 its default branch
requires `CI Result` and has **no** active `merge_queue` rule: it is in direct
mode and stays there until the cutover. Its `ci.yml` carries a `merge_group`
trigger and `.github/workflows/merge-group-check.yml` is installed (pinned
`LOOM_VERSION`); both are dormant because GitHub only emits `merge_group` on a
queue-enabled branch. The queue desired state is recorded, unapplied, in
`docs/merge-queue-pilot/desired-state.json` (merge method MERGE, required
contexts `CI Result` + `loom/merge-authorization`, one build at a time, 40 min
check timeout).

## `loom-daemon merge-group-ci`

Both verbs are read-only. Neither verb enqueues, edits a ruleset, or changes
branch protection, and neither writes to any repository.

| Verb | Exit 0 | Exit 1 | Exit 2 |
|---|---|---|---|
| `audit` | every relied-on suite and required context covers `merge_group` | a finding | could not run |
| `eligibility` | the repository may pilot queue mode | a named prerequisite failed | could not run |

```bash
loom-daemon merge-group-ci audit                     # this checkout; required = .loom/config.json
loom-daemon merge-group-ci audit --required "Gate" --workflow ci.yml --json
loom-daemon merge-group-ci eligibility --repo acme/widgets --root ../widgets
loom-daemon merge-group-ci eligibility --facts facts.json   # offline
```

### What `audit` checks

The audit relies on a workflow when it hosts a required context or runs on
every pull request, meaning a `pull_request` trigger with no `paths:` filter. A
path-filtered PR workflow is listed as advisory: a skipped required check never
reports, so such a workflow cannot be a required gate. The audit evaluates each
job and step of a relied-on workflow under synthetic `pull_request`, `push` and
`merge_group` contexts with a small GitHub-expression evaluator. A value the
evaluator cannot know statically, such as `matrix`, `steps`, or a `needs`
output of a job that actually ran, counts as **unknown**, and an unknown
counts as uncovered.

| Code | Meaning |
|---|---|
| `MISSING_MERGE_GROUP_TRIGGER` | relied-on workflow has no `merge_group` trigger, or its `types:` excludes `checks_requested` |
| `PR_ONLY_CONDITION` | a job's `if:` is false on `merge_group` (`github.event_name == 'pull_request'`, …) |
| `SKIPPED_DEPENDENCY` | a job `needs:` a job skipped on `merge_group` and has no status function (`!cancelled() && …`) |
| `PATH_FILTER_SKIP` | a job/step runs on `merge_group` only if a path-filter job's output says so |
| `UNDETERMINED_CONDITION` | the condition cannot be decided on `merge_group` |
| `PR_ONLY_STEP` | a step of a covered job runs on PRs but is skipped on `merge_group` |
| `NON_MERGE_GROUP_CHECKOUT` | `actions/checkout` `ref:` resolves to something other than the merge-group commit |
| `GROUP_CHECK_WITHOUT_CHECKOUT` | a job runs `forge merge-queue group-check` with no `actions/checkout` step, so `write_scope` refuses the status write and `loom/merge-authorization` never posts |
| `CANCELLING_CONCURRENCY` | `cancel-in-progress` is (or may be) true on `merge_group` |
| `SHARED_CONCURRENCY_GROUP` | the group is not unique to the merge-group commit, so a pending run can be superseded |
| `MISSING_REQUIRED_SUITE` | a required context matches no job name (matrix names are expanded) |
| `REQUIRED_SUITE_UNCOVERED` | a required context's job is not covered on `merge_group` |
| `UNPARSEABLE_WORKFLOW` | a workflow could not be read, so it cannot count as covered |

The output records each workflow's and job's `permissions:` for review, but
permissions do not produce findings.

The shipped `merge-group-check.yml` template needs `statuses: write`,
`pull-requests: read`, `checks: read` and `contents: read`; without the reads
`group-check` fails with `Resource not accessible by integration`. It also
triggers on `workflow_run` (`completed`, `gh-readonly-queue/**`) so the `pending`
status posted at `checks_requested` is re-evaluated when CI finishes; set
`workflows:` to the name of your CI workflow. This is why the job needs no wait
step; a copy that polls for CI holds a runner for the whole check duration and
should drop the wait and keep this trigger. Its job-level `if:` admits a
`workflow_run` only from a same-repo `merge_group` CI run. Keep that gate: the
branch filter alone admits fork PRs (see "Trust boundary" below).

A job that exists only to trim PR runs, such as `ci.yml`'s `changes`
path-filter job, declares itself with a marker comment inside the job block:

```yaml
  changes:
    # merge-group-audit: pr-only -- path groups exist only to trim PR runs
```

The marker takes the job out of the relied-on set. The audit never reports a
marked job as covered, and it still audits a marked job that hosts a required
context.

### What `eligibility` checks

`eligibility` gathers the repository's facts with `GET` reads only, through the
counted gh facade, or loads them from `--facts`. It reads `repos/{nwo}` for the
owner type and permissions, and `repos/{nwo}/rules/branches/{branch}` for the
effective active rules. It then runs `audit` against the branch's **actual**
required checks. Every prerequisite fails closed: a fact the credential cannot
see becomes a named `*_UNKNOWN` failure, never a pass.

| Prerequisite | Failure codes |
|---|---|
| Organization-owned repository | `OWNER_NOT_ORGANIZATION`, `OWNER_TYPE_UNKNOWN` |
| Credential can push (needed to enqueue) | `INSUFFICIENT_PERMISSION`, `PERMISSION_UNKNOWN` |
| Active `merge_queue` rule on the branch | `NO_MERGE_QUEUE_RULE`, `MERGE_QUEUE_UNKNOWN` |
| Required status checks exist | `NO_REQUIRED_CHECKS` |
| Each required check covers `merge_group` | `MISSING_REQUIRED_SUITE`, `REQUIRED_SUITE_UNCOVERED` |
| Writer App installation has `statuses: write` | `WRITER_CANNOT_POST_STATUSES` |
| No other audit finding | `WORKFLOW_NOT_QUALIFIED` |

A GitHub App installation token (`ghs_…`, the fleet's usual credential) gets
an all-`false` `permissions` object, even on a repository it can read. The
check reads that as `PERMISSION_UNKNOWN`, not as a denial. To get a real
answer, run `eligibility` with a credential whose permissions are visible, or
record `can_push` in a `--facts` file.

`WRITER_CANNOT_POST_STATUSES` is read from the writer App's installation
(`GET /orgs/{org}/installations`, matched by app id or slug), never from
`repos/{nwo}.permissions`. It fires when that installation's `statuses` is
`read` or absent. It is not evaluated (no failure) when no writer App is
configured for the owner or the installation list is unreadable.

The `--facts` file has the shape
`{"repository","owner_type","branch","can_push","writer_statuses","rules":[{"type","ruleset_id","parameters"}]}`
(`writer_statuses` is `read`, `write` or `none`).
A missing or `null` field means "unknown".

## Handoff is idempotent and refuses a writer that cannot post

`forge merge-queue step` / `handoff` posts the `loom/merge-authorization`
status on the approved head, so the writer needs `statuses: write`. Two guards
run before the first grant write or revoke (#12579):

- A PR already queued at the approved head under a live grant for that same head
  reports `LOOM-MERGE-QUEUE-HANDED-OFF` ("was already in the queue") and changes
  nothing: no revoke, no re-grant, no status write. A credential that cannot
  post statuses therefore cannot dequeue a PR another identity queued.
- Otherwise, a writer App whose installation has `statuses` below `write` is
  refused with `[WRITER_CANNOT_POST_STATUSES]` (exit 3) before any grant is
  written. Nothing is revoked and an existing queue entry is left alone. Fix the
  App's permission; Loom does not change it.

## How `ci.yml` is qualified

- `on.merge_group: types: [checks_requested]` is added. The `push` and
  `pull_request` triggers are unchanged.
- Every path-filtered job's condition admits `merge_group` unconditionally:
  `github.event_name == 'push' || github.event_name == 'merge_group' ||
  needs.changes.outputs.<group> == 'true'`. A merge group therefore runs the
  full suite. `changes` stays PR-only and carries the marker.
- The image jobs (`worker-base-image` and the three image smokes) are the one
  exception on `push` (#10825, [ci-principles](ci-principles.md) rule 3).
  Their condition puts `github.event_name == 'merge_group' ||` first, so a
  merge group still runs them unconditionally, while a push runs them only
  when the `changes` job finds an image input or fails. `changes` never runs
  on a merge group (it carries the `pr-only` audit marker), so the audit does
  not count it as a relied-on suite.
- The concurrency group for a merge-group run is keyed on
  `merge_group.head_sha`, so it can never share a group with a PR run, with
  `main`, or with another merge group. `cancel-in-progress` stays true only
  for `pull_request`, so a started merge-group run is never cancelled
  ([ci-principles](ci-principles.md) rule 2).
- The PR-only steps that guard the merged tree also run on `merge_group`, over
  `merge_group.base_sha..head_sha`. These are the version-bearing-file check
  and the secret scan, both in `Lint`.
- No checkout that runs on `merge_group` sets `ref:`, so every such checkout
  lands on `github.sha`, which is the merge-group head. (`Detect Changes`
  checks out the PR base, and does not run on `merge_group`.)
- Regression tests in `loom-daemon/src/merge_group_ci/tests.rs` audit this
  repository's own workflows against `.loom/config.json`'s required checks.
  They also assert that every job still runs on `push` and may still run on a
  PR. The exceptions are named explicitly: `changes` (PR and push only), and
  the four image jobs, which must not be `Skipped` on push and must `Run` on
  `merge_group`.

## Two contexts, two meanings

- `CI Result` is the **test verdict**: the aggregate job in `ci.yml` over the
  complete merge-group tree. It is a CI job, so `merge-group-ci audit` can match
  it by job name.
- `loom/merge-authorization` is an **authorization** context: a commit status
  posted by `forge merge-queue group-check` (`merge-group-check.yml`) once the
  Judge/Champion grant for every PR head in the group is live and the required
  CI checks are green. It says "Loom still authorizes this group", never "tests
  passed"; a revocation or head change turns it `failure`. Because it is a
  status and not a job, `audit --required "loom/merge-authorization"` reports
  `MISSING_REQUIRED_SUITE` by design. Audit the two contexts separately:

```bash
loom-daemon merge-group-ci audit --required "CI Result"        # QUALIFIED expected
loom-daemon merge-group-ci audit -v --required "CI Result"     # also lists merge-group-check.yml
```

The audit covers every workflow in the checkout, so a QUALIFIED result also
proves `merge-group-check.yml` has no `GROUP_CHECK_WITHOUT_CHECKOUT`,
`CANCELLING_CONCURRENCY` or `SHARED_CONCURRENCY_GROUP` finding. (Do not pass
`--workflow merge-group-check.yml` with `--required "CI Result"`: that
restricts the audit to a file that does not host the job.)

**Trust boundary.** The `workflow_run` `branches: ['gh-readonly-queue/**']`
filter is **not** a security control. It matches the triggering CI run's head
branch, and a fork PR author can name a fork branch `gh-readonly-queue/x`. Its
`CI` run would then fire `merge-group-check.yml` in base-repo, default-branch
context, with the `statuses: write` token. The job checks out that run's head,
and `group-check` runs `forge.mergeQueue.statusCommand` from the checked-out
`.loom/config.json` (`status_command::resolve`). Even with no queue, an empty
group list still reaches `post_status`. What keeps untrusted code out is the
**job-level `if:`**: it accepts a `merge_group` event, or a `workflow_run`
whose triggering run had `event == 'merge_group'` and
`head_repository.full_name == github.repository`. Merge groups exist only in
the base repository, so a fork PR's CI run is skipped. A skipped run posts
nothing, and nothing authorizes nothing (fail-closed). Keep the gate in every
copy of the workflow, including copies made from
`defaults/optional/github-workflows/merge-group-check.yml`.

Two residual limits are known and accepted for the pilot:

- On a genuine merge-group run, `.loom/config.json` comes from the group-head
  checkout, so it includes queued PRs' edits. Those PRs are Judge-approved, but
  a queued diff can still change the `statusCommand` that the writer runs.
  Reading `.loom/` from `merge_group.base_sha` would close this, but the
  `merge-group-ci` audit currently flags any checkout whose `ref:` is not the
  merge-group head (`NON_MERGE_GROUP_CHECKOUT`). Changing that needs an audit
  rule change, which is out of scope here.
- The install step checks the binary's `.sha256`, which ships in the same
  release. That is an integrity check only. The published `.sig`/`.pem` are not
  verified, so the pinned `LOOM_VERSION` is the protection.

Otherwise the workflow uses a sparse `.loom/` checkout of the merge-group
commit. It holds `statuses: write`, `pull-requests: read`, `checks: read` and
`contents: read`, and uses a non-cancelling per-head concurrency group.
`workflow_run.workflows` names this repo's CI workflow (`CI`), which
re-evaluates the `pending` status when CI completes.

## Enablement prerequisites (all required, in order)

1. Phases A and B (#10255, #10256) have landed: the queue controls are present
   and the Champion handoff preserves the safety invariant.
2. `loom-daemon merge-group-ci eligibility --repo <org>/<repo>` exits 0 for the
   pilot repository (see "Pilot cutover" below for the full ordered procedure,
   including broker/App enrollment and the publisher version-bump path). That
   requires an organization owner, push permission, an active `merge_queue`
   rule, required checks, and a clean audit.
3. **Operator authorization.** Adding a `merge_queue` rule, changing required
   checks, or transferring a repository are protected settings changes. No
   agent makes them, and nothing in this tooling attempts them.
4. Record the direct-mode baseline before the switch. The baseline covers
   approval-to-merge median and p90, re-date pushes, head-induced Judge
   re-reviews, CI runs and runner-minutes per merged PR, and red-main
   incidents.

## The authorization context is excluded from the CI verdict gate (#12526)

`loom/merge-authorization` is a required context on the queue repo, but
Champion posts it only AFTER the Judge approves. `forge wait-checks` (and so
`post-verdict.sh` and the CI-gate recovery) therefore ignores it when deciding
`GREEN`: absent or `pending` it does not hold the wait, or the Judge would
wait for a status that only its own approval can cause and time out. A posted
`failure` (a revocation) still counts as failing in both default and
`--required-only` modes. `merge-pr.sh`'s zero-row settle
(`merge_pr/zero_checks.rs`) is unchanged: it runs post-authorization and does
not use the verdict path.

## A queue drop caused by lost infrastructure is retried once (#12573)

GitHub reports a runner lost mid-job as `failed_checks`, which Loom first
classified as `check-failure` and routed to Doctor (`loom:pr` →
`loom:changes-requested`), though the PR's code was green. When the drop is
`check-failure`, the reconcile pass reads the check runs on the removed group
commit (`beforeCommit.oid` of the removal event, readable after the group ref
is deleted) and reclassifies it as `dropped-infra` only if:

- at least one run failed, and every failed run ended `cancelled` or
  `timed_out`, and
- none of those runs' jobs has a step that ended `failure` (a test or build
  that ran and failed is a real failure), and
- no earlier `dropped-infra` revoke marker covers the same head.

`dropped-infra` takes the `timeout` route: labels unchanged, `loom:pr` kept,
and the next Champion pass re-enqueues with a fresh authorization. The CLI
prints `LOOM-MERGE-QUEUE-DROPPED reason=dropped-infra`. The one-retry bound is
per head: the second infra drop on the same head, a mixed set, an in-progress
run, an unreadable or empty check-run read, or a missing group commit all stay
`check-failure` and go to Doctor as before (fail closed).

The revoke comment says why: a `dropped-infra` comment names the cancelled or
timed-out job(s); a `check-failure` drop whose group had cancelled runs, or
whose refinement was skipped, names the guard that declined (failed step, real
failure, run still in progress, unreadable read, no group commit, retry spent).

## Pilot cutover (2AMLogic/loom)

Every step is a read except step 6, which is an operator action. Do not set
`champion.mergeMode: queue` or add the `merge_queue` rule before steps 1-5 pass.

1. **Baseline (direct mode, one week or 20 merges).** Record: approval-to-merge
   time (median/p90, `loom:pr` label to merge), CI runs and runner-minutes per
   merged PR, queue ejections (zero in direct mode; the comparison column), and
   red-main incidents (a failed `CI` push run on `main`). Example reads:
   `gh run list --workflow CI --branch main --limit 100 --json conclusion,createdAt,updatedAt,event`
   and `gh pr list --state merged --limit 50 --json number,mergedAt,labels`.
2. **Audit:** the two commands under "Two contexts" above.
3. **Eligibility / preflight (live, read-only):**
   ```bash
   loom-daemon merge-group-ci eligibility --repo 2AMLogic/loom --json
   loom-daemon forge merge-queue preflight --repo 2AMLogic/loom
   loom-daemon forge merge-queue mode        # must still print direct
   ```
   `NO_MERGE_QUEUE_RULE` is expected before step 6 and is the only failure
   allowed. The daemon on the host must include the `merge-group-ci` and
   `forge merge-queue` subcommands. `merge-group-ci` has shipped since
   v0.19.695, `forge merge-queue preflight` since v0.19.761, `group-check`
   since v0.19.879, and the `statusCommand` route since v0.19.1185. Running the
   v0.19.1311 that `merge-group-check.yml` pins as `LOOM_VERSION` keeps the
   host and the workflow on one version.
4. **Broker / App enrollment (external, must be verified, not assumed).** The
   writer identity that posts `loom/merge-authorization` needs `statuses: write`
   on this repository. If the writer App's grant is read-only or too broad,
   enroll the repository with the external status poster
   (`forge.mergeQueue.statusCommand`, see
   [merge-queue-authorization](merge-queue-authorization.md)) and confirm its
   `--dry-run` probe passes (`eligibility` reports `facts.status_route`). The
   broker allowlist and App installation are outside this repository.
5. **Publisher version-bump / direct-push path (external, must be verified).**
   `version-bump-on-merge.yml` posts the bump to the fleet publisher, which
   commits to `main` directly. Under a `merge_queue` rule that direct commit must
   still be accepted (publisher identity as a bypass actor limited to the
   version-bump commit, or a queue-routed bump), or version bumps and releases
   stall. Verify before step 6, and expect each bump to move the base under
   queued entries (measure the rebuild churn).
6. **Operator applies** `docs/merge-queue-pilot/desired-state.json`: the ruleset
   `merge_queue` rule (merge method MERGE, required contexts `CI Result` and
   `loom/merge-authorization`), then sets `champion.mergeMode: queue`. Re-run
   step 3; `eligibility` must now exit 0.
7. **Pilot week** compared to the step 1 baseline.

## Rollback to direct mode

Direct mode is always available. To roll back:

1. Set the mode back to direct (remove `champion.mergeMode` or set it to
   `direct`, or `LOOM_MERGE_MODE=direct`) and confirm:
   `loom-daemon forge merge-queue mode`. The Champion then returns to
   `merge-pr.sh`.
2. Drain or dequeue in-flight entries:
   `loom-daemon forge merge-queue status --pr <N>` and
   `loom-daemon forge merge-queue dequeue --pr <N>` per queued PR.
3. Have the operator disable or remove the `merge_queue` rule (a protected
   settings change); keep `CI Result` required.
4. Leave the `merge_group` trigger and `merge-group-check.yml` in place. They
   are inert without a queue and keep the workflows audited.

## #9990 tier split (fast PR / full integration): evaluation

#9990 proposed a fast PR tier (`pull_request`) and a full integration tier
(`merge_group`). The data below is from this repository's `CI` workflow, read
on 2026-10-04 with the Actions API. It covers the last 200 completed runs per
event and the job lists of the last 30 successful runs per event.

| Event | Wall time p50 / p90 (success) | Jobs per run (median) | Jobs skipped (median) | Runner-minutes per run (median / p90) |
|---|---|---|---|---|
| `pull_request` | 7.3 / 11.4 min | 26 | 4 | 70.0 / 73.2 |
| `push` (main) | 8.3 / 13.9 min | 26 | 1 (`changes`) | 85.9 / 89.1 |

The findings:

- **The PR tier is already the "fast" tier.** Path filtering skips a median of
  4 of 26 jobs and saves about 16 runner-minutes (about 18%) per PR run. The
  wall-time difference against a full run is about 1 minute at p50. Trimming
  PR suites further would buy little time and would delay failure discovery
  from the PR to the queue, where a failure ejects the entry and costs a queue
  cycle. **No PR suite is removed.** Every job that runs on a PR today still
  runs there.
- **The integration tier is the full suite.** `merge_group` runs exactly what
  `push` runs, so the combined tree gets the same coverage `main` does.
- **Queue limits do not combine builds.** Each queue entry gets its own
  `merge_group` run on its own temporary branch. Queue settings such as
  `max_entries_to_build` and grouping bound concurrency and what merges
  together. They do not reduce Loom's per-PR run count, and this design does
  not assume they do.
- **Expected cost shape.** Per merged PR, queue mode adds one full
  `merge_group` run of about 86 runner-minutes, and in exchange tests the
  combined tree that direct mode merges untested when `main` moves under an
  approved PR (#12063 removed direct mode's re-date reruns). Whether
  that nets out positive is the pilot's question. GitHub fast-forwards `main`
  to the merge-group head, so the `push` run on `main` re-tests an identical
  SHA. Removing that `push` run would be a separate, evidence-gated change and
  is **not** made here: `main` verification stays (rule 2).
- **Interaction to watch in the pilot:** `version-bump-on-merge.yml` lands
  its bump commit directly on `main` (through the fleet publisher, not a PR), at most once per `RELEASE_MIN_INTERVAL` (24 h by
  default, #11174; see [`release-cadence.md`](release-cadence.md)). In a
  queue-enabled repository, each such push moves the base under every queued
  entry. The pilot repository must
  either route that bump through the queue or measure the rebuild churn it
  causes.

## Still open: the live pilot (out of band)

Merging this tooling does not complete #10257. The remaining acceptance
criteria need an eligible organization-owned pilot repository and explicit
operator authorization. Evidence for each item is recorded with Loom's
`loom:ac-verified sha=<head>` convention.

- [ ] Select the pilot repository and record its queue, ruleset and check
  prerequisites, using `eligibility --json` as the record.
- [ ] Live: individually green but mutually incompatible PRs fail
  combined-tree validation and never reach `main`.
- [ ] Live: queued approval revocation, a head change, and a hold each confirm
  the Phase B safety invariant, recorded with timestamps and SHAs.
- [ ] One pilot week compared against the direct-mode baseline. Report the
  metrics listed under the prerequisites above, plus removals by reason and
  the study's limitations.

## Related

- [ci-principles](ci-principles.md): the started-run and check-evidence rules
  merge-group runs inherit (rules 2 and 6).
- [hyperparameters](hyperparameters.md): the pointer to these prerequisites
  and the rollback steps.
- #9978 (parent), #10255 (Phase A), #10256 (Phase B), #9990 (the original
  proposal).
