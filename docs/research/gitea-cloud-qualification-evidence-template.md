# Gitea Cloud qualification — evidence template (issue #9788)

Copy this template's tables into a dated evidence record (or fill it in
place, per run) once [`gitea-cloud-qualification-runbook.md`](gitea-cloud-qualification-runbook.md)
Step 0 is complete and Steps 1–4 have been run against a live tenant. Every
cell starts `UNKNOWN` — leave it that way rather than guessing. A field that
was not measured stays `UNKNOWN`; it does not default to "pass", "none", or
"unlimited" (see the issue's "do not invent unlimited quotas").

If a server build or edition cannot be pinned (a provider-controlled hosted
SaaS may upgrade without notice), capture it **before and after every
qualification run**, and invalidate affected results if it changed between
the two readings.

## Run identity

| Field | Value |
|---|---|
| Run namespace (`GITEA_QUAL_RUN_NS`) | UNKNOWN |
| Run date | UNKNOWN |
| Operator who completed Step 0 | UNKNOWN |
| Builder/agent who ran Steps 1–4 | UNKNOWN |

## Tenant and plan

| Field | Value |
|---|---|
| Provider | UNKNOWN — must be Gitea Cloud per the epic; record if a substitute was used and why |
| Instance origin | UNKNOWN |
| Server build/version (before run) | UNKNOWN |
| Server build/version (after run) | UNKNOWN |
| Edition (Cloud / Enterprise / other) | UNKNOWN |
| Selected plan/tier | UNKNOWN |
| Update policy (provider-controlled? pinned?) | UNKNOWN |
| Region (if exposed) | UNKNOWN |
| Trial expiry date | UNKNOWN |
| Monthly/trial cost | UNKNOWN |
| Cleanup/renewal owner | UNKNOWN |
| Export mechanism before teardown | UNKNOWN |

## Identities

| Identity | Login reference | Verified permission level | Notes |
|---|---|---|---|
| Administrator | `GITEA_QUAL_ADMIN_LOGIN` | UNKNOWN | used only for setup + protection tests |
| Writer | `GITEA_QUAL_WRITER_LOGIN` | UNKNOWN | ordinary automation identity |
| Read-only | `GITEA_QUAL_READONLY_LOGIN` | UNKNOWN | must reject mutation |
| Untrusted reviewer | `GITEA_QUAL_REVIEWER_LOGIN` | UNKNOWN | outside-collaborator / fork trust boundary |

## Disposable resources created this run

| Resource | Name | Created | Cleaned up |
|---|---|---|---|
| Org | UNKNOWN | UNKNOWN | n/a — org is reused, never deleted |
| Source repo | `${GITEA_QUAL_RUN_NS}-source` | UNKNOWN | UNKNOWN |
| Isolation repo | `${GITEA_QUAL_RUN_NS}-isolated` | UNKNOWN | UNKNOWN |
| Fork of source (writer identity) | UNKNOWN | UNKNOWN | UNKNOWN |

## Smoke test results

A result is one of `PASS`, `FAIL`, `UNSUPPORTED`, `BLOCKED` (plan does not
offer the capability), or `UNKNOWN` (not executed). `PASS` requires an
observed response, not an assumed one.

| Check | Identity used | Result | Evidence (response code / excerpt, no secrets) |
|---|---|---|---|
| Authenticate (`GET /user`) | admin | UNKNOWN | |
| Authenticate (`GET /user`) | writer | UNKNOWN | |
| Authenticate (`GET /user`) | read-only | UNKNOWN | |
| Authenticate (`GET /user`) | untrusted reviewer | UNKNOWN | |
| Clone over HTTPS with token | writer | UNKNOWN | |
| Push a commit | writer | UNKNOWN | |
| Create issue | writer | UNKNOWN | |
| Comment on issue | writer | UNKNOWN | |
| Mutating call rejected | read-only | UNKNOWN | expect 403/404, not 200 |
| Create PR / fork visible upstream | writer | UNKNOWN | |
| Review/comment from outside collaborator | untrusted reviewer | UNKNOWN | must not be treated as a trusted collaborator by any Loom guard |
| Actions available on plan | admin | UNKNOWN | |
| Runner registration or managed runner access | admin | UNKNOWN | |
| CI result obtainable via API | writer | UNKNOWN | |
| Branch protection configurable | admin | UNKNOWN | |
| Revoked/expired credential rejected | (any) | UNKNOWN | |

## Explicit unknowns / blockers

Record anything from the runbook's "Explicit unknowns" section that is still
unresolved after this run, plus any newly discovered one. A missing required
capability is a **blocker**, not an empty success — name it here rather than
leaving the matrix silent.

| Item | Status | Notes |
|---|---|---|
| Automation / bot policy | UNKNOWN | |
| Measured request rate limits | UNKNOWN | |
| Runner concurrency / included minutes | UNKNOWN | |
| Bot/service-account seat allowance | UNKNOWN | |
| Storage / artifact / export allowances | UNKNOWN | |
| Org team/user creation via API vs. web-UI-only | UNKNOWN | |

## Reproducibility

| Field | Value |
|---|---|
| Second run on fresh disposable resources performed? | UNKNOWN |
| Results reproduced? | UNKNOWN |
| Divergences (if any) | UNKNOWN |
