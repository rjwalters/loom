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

**None of the above are provisioned yet.** As of this writing (#9788) no
Gitea Cloud tenant, org, repo, or identity exists — this table names where
each credential will live once an operator completes the external signup
documented in
[`docs/research/gitea-cloud-qualification-runbook.md`](../docs/research/gitea-cloud-qualification-runbook.md)
Step 0. A builder picking up #9789/#9790/#9791 should check these env vars by
name before asking the operator anything beyond "is Step 0 done yet" — a
still-unresolved name here is the missing-credential case
[`.loom/docs/credentials.md`](docs/credentials.md) describes, not a reason
to fabricate a tenant.

If a task needs a credential not listed here, that is a **missing-credential**
case per `.loom/docs/credentials.md` — ask the operator for the name, shape,
and provisioning path only, never the value, then add the resulting row here.
