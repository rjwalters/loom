<!-- .loom/credentials.md — NAMES ONLY, never a credential value. See
     .loom/docs/credentials.md for the full convention (reference-by-name,
     lookup-before-ask, standard provisioning flow). -->

# Task Credential Manifest

Lists, per credential this repo's tasks need, where an agent finds it — an
env var name, an owner-store file path, or a provisioned-file path. Never a
value. Safe to commit: every row below is a placeholder name, not a secret.

| Purpose | Reference | Usage |
|---|---|---|
| Gitea Cloud qualification tenant origin (#9788/#9769) | env var `GITEA_QUAL_INSTANCE_URL` | non-secret, environment-specific; used to build API URLs for the hosted-qualification runbook |
| Gitea Cloud qualification org slug (#9788/#9769) | env var `GITEA_QUAL_ORG` | the disposable qualification org |
| Gitea Cloud qualification run namespace (#9788/#9769) | env var `GITEA_QUAL_RUN_NS` | prefix for disposable resources created by one provisioning run |
| Gitea Cloud qualification administrator login (#9788/#9769) | env var `GITEA_QUAL_ADMIN_LOGIN` | used only for setup + branch-protection tests |
| Gitea Cloud qualification administrator API token (#9788/#9769) | env var `GITEA_QUAL_ADMIN_TOKEN` | org/repo creation, protection config |
| Gitea Cloud qualification writer identity login (#9788/#9769) | env var `GITEA_QUAL_WRITER_LOGIN` | ordinary automation identity |
| Gitea Cloud qualification writer identity API token (#9788/#9769) | env var `GITEA_QUAL_WRITER_TOKEN` | push/issue/PR/CI smoke checks |
| Gitea Cloud qualification writer identity SSH key (#9788/#9769, optional) | provisioned file `~/.ssh/gitea_qual_writer` | `ssh -i <path>`; public half installed on the writer account |
| Gitea Cloud qualification read-only identity login (#9788/#9769) | env var `GITEA_QUAL_READONLY_LOGIN` | permission-boundary tests |
| Gitea Cloud qualification read-only identity API token (#9788/#9769) | env var `GITEA_QUAL_READONLY_TOKEN` | must be rejected on any mutating call |
| Gitea Cloud qualification untrusted-reviewer identity login (#9788/#9769) | env var `GITEA_QUAL_REVIEWER_LOGIN` | outside-collaborator / fork-review trust-boundary tests |
| Gitea Cloud qualification untrusted-reviewer identity API token (#9788/#9769) | env var `GITEA_QUAL_REVIEWER_TOKEN` | comments/reviews from this identity must never be treated as a trusted collaborator's |

**Resolved 2026-10-01** against the fleet's own self-managed instance: the
hosted trial runs on `gitea-1` (2am#1793 — Gitea 28.0.0, loopback-only; reach
it on the host or via an SSH forward, not a cloud tenant). Each name resolves
as follows — values live only in AWS SSM under `/gitea/*`, never in a repo or
chat:

| Env var | Resolves from |
|---|---|
| `GITEA_QUAL_INSTANCE_URL` | `http://127.0.0.1:3000` (non-secret; SSM `/gitea/gitea/api-url` records it) |
| `GITEA_QUAL_ORG` | `qual-org` (disposable repo: `qual-org/loomp-test`; `forge-ci` is a write collaborator) |
| `GITEA_QUAL_RUN_NS` | generated per run (`loomp-<unix-seconds>` default) |
| `GITEA_QUAL_ADMIN_LOGIN` | `loom-bot` |
| `GITEA_QUAL_ADMIN_TOKEN` | SSM `/gitea/loom-bot/admin-token` (admin-scoped; the plain `/gitea/loom-bot/token` predates it) |
| `GITEA_QUAL_WRITER_LOGIN` | `forge-ci` |
| `GITEA_QUAL_WRITER_TOKEN` | SSM `/gitea/forge-ci/token` |
| `GITEA_QUAL_READONLY_LOGIN` | `qual-readonly` |
| `GITEA_QUAL_READONLY_TOKEN` | SSM `/gitea/qual-readonly/token` |
| `GITEA_QUAL_REVIEWER_LOGIN` | `qual-reviewer` |
| `GITEA_QUAL_REVIEWER_TOKEN` | SSM `/gitea/qual-reviewer/token` |

Boundary evidence (2026-10-01, on-host): writer `POST /repos/qual-org/loomp-test/issues`
→ 201; read-only `POST` → 403. The writer SSH key row stays optional and
unprovisioned — git-over-SSH checks belong to a later slice.

If a task needs a credential not listed here, that is a **missing-credential**
case per `.loom/docs/credentials.md` — ask the operator for the name, shape,
and provisioning path only, never the value, then add the resulting row here.
