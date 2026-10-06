<!-- .loom/credentials.md — NAMES ONLY, never a credential value. See
     .loom/docs/credentials.md for the full convention (reference-by-name,
     lookup-before-ask, standard provisioning flow). -->

# Task Credential Manifest

Lists, per credential this repo's tasks need, where an agent finds it — an
env var name, an owner-store file path, or a provisioned-file path. Never a
value. Safe to commit: every row below is a placeholder name, not a secret.

| Purpose | Reference | Usage |
|---|---|---|
| Cloudflare D1 `loom-fleet-telemetry`: sweep-facts rollup and `landed-size.sql` (#9934, #10066) | AWS SSM `/dev/cloudflare/api-token-joseph` (us-east-1) → env `CLOUDFLARE_API_TOKEN`; account env `CLOUDFLARE_ACCOUNT_ID=a7a402ccb9616532d8f4ee64447affe9` (2amlogic, non-secret) | inject into the one `npx wrangler@4 d1 execute loom-fleet-telemetry --remote …` command's env, never print; the token also carries Workers/Pages write (D1 access is account-wide), so use it for D1 only |
| Gitea qualification forge origin (#9769) | env var `GITEA_QUAL_INSTANCE_URL` | non-secret; base for API URLs (loopback-only — run on the host or through its SSH tunnel) |
| Gitea qualification org slug (#9769) | env var `GITEA_QUAL_ORG` | the qualification org |
| Gitea qualification run namespace (#9769) | env var `GITEA_QUAL_RUN_NS` | prefix for disposable resources created by one run |
| Gitea qualification administrator login (#9769) | env var `GITEA_QUAL_ADMIN_LOGIN` | used only for setup + branch-protection tests |
| Gitea qualification administrator API token (#9769) | env var `GITEA_QUAL_ADMIN_TOKEN` | org/repo creation, protection config |
| Gitea qualification writer identity login (#9769) | env var `GITEA_QUAL_WRITER_LOGIN` | ordinary automation identity |
| Gitea qualification writer identity API token (#9769) | env var `GITEA_QUAL_WRITER_TOKEN` | push/issue/PR/CI smoke checks |
| Gitea qualification writer identity SSH key (#9769, optional, unprovisioned) | provisioned file `~/.ssh/gitea_qual_writer` | `ssh -i <path>`; public half installed on the writer account |
| Gitea qualification read-only identity login (#9769) | env var `GITEA_QUAL_READONLY_LOGIN` | permission-boundary tests |
| Gitea qualification read-only identity API token (#9769) | env var `GITEA_QUAL_READONLY_TOKEN` | must be rejected on any mutating call |
| Gitea qualification untrusted-reviewer identity login (#9769) | env var `GITEA_QUAL_REVIEWER_LOGIN` | outside-collaborator / fork-review trust-boundary tests |
| Gitea qualification untrusted-reviewer identity API token (#9769) | env var `GITEA_QUAL_REVIEWER_TOKEN` | comments/reviews from this identity must never be treated as a trusted collaborator's |

**Where these resolve:** the self-hosted `gitea-1` forge (2AMLogic/2am#1793),
not a hosted tenant — the hosted trial was dropped (#9788, operator 2026-10-03).
Which login and which AWS SSM `/gitea/*` parameter backs each name is recorded
once, in 2AMLogic/2am `infra/aws/gitea/gitea.md` §Qualification identities —
Gitea infrastructure docs live in that repo, so the mapping is not repeated
here. Read values from SSM into the consumer's env (stdin or a 0600 file),
never onto argv.

If a task needs a credential not listed here, that is a **missing-credential**
case per `.loom/docs/credentials.md` — ask the operator for the name, shape,
and provisioning path only, never the value, then add the resulting row here.
