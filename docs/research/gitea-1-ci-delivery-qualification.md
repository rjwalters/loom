# gitea-1 CI and delivery qualification (#9790, epic #9769 phase 1)

Status: **runbook and disposition ledger only. No live evidence has been
collected.** The builder host had none of the `GITEA_QUAL_*` variables set
(names in `.loom/credentials.md`), no loopback/SSH-tunnel access to gitea-1
was established, and no isolated qualification runner was confirmed. Every row
below is therefore `UNRESOLVED`; nothing here counts as a pass for #9792.
A second builder pass (2026-10-06) found the same state. That host had no
`GITEA_QUAL_*` variables and no SSM read access to `/gitea/*`. That pass added
only locally verifiable pieces: a real CI fixture and a static
GitHub-dependency audit (see "Local evidence" below).

## Scope

Qualify self-hosted gitea-1 (2AMLogic/2am#1793) with an existing isolated
runner. Never run jobs on the forge host; do not deploy a new AWS runner.
Platform, adapter and caller-integration evidence stay separate: direct Gitea
probes do not certify production Loom callers.

## Prerequisites to unblock the live run

1. `GITEA_QUAL_INSTANCE_URL`, `_ORG`, `_RUN_NS` and the admin / writer /
   read-only / reviewer login+token pairs exported in the session (never in a
   checkout, log or artifact).
2. Reachability of the loopback-only instance (run on the host or via SSH tunnel).
3. An existing isolated Actions runner registered to the qualification org
   (record version, architecture, executor image digests, action revisions).

## Work list (from `loom-daemon forge-inventory probe-manifest --profile required-ci-landing --profile delivery --json`, base `a1714b187d9c`)

| Test ID | Operation | Profile | Evidence class | Disposition |
|---|---|---|---|---|
| `forge-probe::ci-landing::automerge-disable` | automerge.disable | required-ci-landing | caller-integration | UNRESOLVED |
| `forge-probe::ci-landing::ci-check-runs-for-sha` | ci.check-runs-for-sha | required-ci-landing | adapter | UNRESOLVED |
| `forge-probe::ci-landing::ci-stale-check-freshness` | ci.stale-check-freshness | required-ci-landing | caller-integration | UNRESOLVED |
| `forge-probe::ci-landing::ci-workflow-runs-for-sha` | ci.workflow-runs-for-sha | required-ci-landing | adapter | UNRESOLVED |
| `forge-probe::ci-landing::merge-expected-head-guarded` | merge.expected-head-guarded | required-ci-landing | platform | UNRESOLVED |
| `forge-probe::ci-landing::protection-read-effective-rules` | protection.read-effective-rules | required-ci-landing | adapter | UNRESOLVED |
| `forge-probe::ci-landing::branch-delete-after-merge` | branch.delete-after-merge | required-ci-landing | adapter | UNRESOLVED |
| `forge-probe::ci-landing::branch-update-from-base` | branch.update-from-base | required-ci-landing | platform | UNRESOLVED |
| `forge-probe::fleet-delivery::release-publish` | release.publish | delivery | platform | UNRESOLVED |
| `forge-probe::fleet-delivery::release-resolve-and-fetch` | release.resolve-and-fetch | delivery | adapter | UNRESOLVED |

Regenerate with the command above; update this ledger (supported /
unsupported / unresolved, with evidence link) rather than adding prose claims.

## Negative-result matrix (each must NOT yield a green merge gate)

Record exact head SHA, run/job ids, timestamps and sanitized output per row.

| Case | Expected | Result |
|---|---|---|
| Absent checks (no run yet) | merge rejected / not-green | UNRESOLVED |
| Queued job, no check objects yet | not-green | UNRESOLVED |
| Explicit failure | not-green | UNRESOLVED |
| Cancelled | not-green | UNRESOLVED |
| Skipped | not-green unless explicitly allowed | UNRESOLVED |
| Unavailable / 5xx / inaccessible | unknown, never green | UNRESOLVED |
| Later page fails (> 1 page of runs/jobs/statuses) | incomplete, never green | UNRESOLVED |
| Stale-head result (success on prior SHA) | not-green for current head | UNRESOLVED |
| Admin bypass | recorded separately from ordinary behavior | UNRESOLVED |

## Local evidence (verifiable without gitea-1)

Neither item below is live evidence. Neither counts as a pass for any work-list
row or negative-matrix case above.

### Fixture: real build, test and artifact work

`defaults/forge/qualification/ci-fixture/` is a dependency-free Rust crate (a
version-ordering library and a CLI). It ships with
`.gitea/workflows/qual-ci.yml`, which runs `build → test → package` with
`needs:`, uploads a SHA-named test log and a SHA-named binary with its
`sha256`, smoke-runs the packaged binary before uploading it, and includes a
`skip-probe` job whose only role is a genuine `skipped` result. It has no
success stub. Four static properties are enforced by a test:

- Every job has `runs-on: loom-qual`, so only the isolated runner can pick it
  up.
- Actions are pinned to the same SHAs the production workflows pin.
- There are no `gh`/forge-API calls.
- It sets `cancel-in-progress: false`.

`loom-daemon/src/forge_inventory/tests/qualification_fixture.rs` copies the
fixture to a tempdir and runs `cargo test`, which passes. It then mutates
`compare` to order versions lexically, reruns `cargo test`, and asserts that
the run fails and names `numeric_not_lexical_ordering`. This is the local half
of test-plan item 1. Whether the same fixture runs on gitea-1 (push/PR
triggers, `needs:`, artifact upload with these action revisions, rerun and
cancel) is still `UNRESOLVED`.

### Static GitHub delivery-dependency audit

```bash
loom-daemon forge-inventory workflow-deps          # text
loom-daemon forge-inventory workflow-deps --json   # per-reference file:line evidence
```

This command lists, per plane, what tracked workflows fetch from GitHub at run
time. It keeps forge API coordination (`gh`, `api.github.com`) on its own
plane, so neither list can hide the other. The figures below are from this
PR's build base (`1fecbabd9c9f`), 16 workflow files:

| Plane | Refs | Distinct | What zero-GitHub would take (estimate) |
|---|---|---|---|
| action-source | 134 | 19 actions | mirror each action at its pinned SHA on the forge; `DEFAULT_ACTIONS_URL=self` or absolute `uses:` URLs |
| package-registry (GHCR) | 23 | 8 image refs | move `loom-worker*` push/pull to the forge's OCI registry (release.yml, ci.yml, ci-daily.yml) |
| release-download | 2 | 2 URLs | mirror shellcheck and lychee archives, or bake them into the runner image |
| forge-api | 41 | 41 lines | counted by the operation inventory (`forge-inventory report`), not here |
| github-other | 2 | 1 | OIDC issuer (`token.actions.githubusercontent.com`), reviewed individually |

This is a lexical scan (`static_only: true`). It cannot see what an action
downloads internally (for example `dtolnay/rust-toolchain` → rustup, or
`taiki-e/install-action` → its tool sources) or what a runner image bakes in.
It is therefore **not** a network-independence claim. Measuring those
dependencies during a run on gitea-1 is still `UNRESOLVED`.

## Remaining live steps

- Copy `defaults/forge/qualification/ci-fixture/` into a namespaced disposable
  repo (`$GITEA_QUAL_RUN_NS`-prefixed) and register the isolated runner with
  the `loom-qual` label. Push it, then push a commit that applies the same
  lexical-ordering mutation the local test uses. Record both runs' head SHAs
  and results, and show that the results differ.
- Push and PR triggers, required contexts, multi-job `needs`, permissions,
  concurrency, rerun attempts, log and artifact download tied to the SHA.
- Branch protection with real results; guarded merge only on current-head success.
- Audit workflow actions for GitHub / GHCR / Releases fetches; the target is zero
  required GitHub API coordination, without claiming total network independence.
- Delivery: release create/assets/download, checksum/signature, authenticated
  update resolution, post-merge version ordering.
- Measure concurrency, wall time, request/write volume and runner plus host cost
  for a declared workload (reuse #9778's reporting boundary).
- Report unsupported required behavior to #9792.
