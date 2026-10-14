# gitea-1 CI and delivery qualification (#9790, epic #9769 phase 1)

Status: **runbook and disposition ledger only. No live evidence has been
collected.** The builder host had none of the `GITEA_QUAL_*` variables set
(names in `.loom/credentials.md`), no loopback/SSH-tunnel access to gitea-1
was established, and no isolated qualification runner was confirmed. Every row
below is therefore `UNRESOLVED`; nothing here counts as a pass for #9792.

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

## Remaining live steps

- Port a bounded real build/test/artifact fixture to a namespaced disposable
  repo (`$GITEA_QUAL_RUN_NS`-prefixed); no stub that always succeeds; show a
  failing test differs from the passing run.
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
