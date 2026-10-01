# Gitea Cloud qualification runbook (issue #9788, epic #9769 phase 1)

**Audience:** an operator who can complete an external account signup — the
one action no agent in this fleet can perform (no inbox, no browser, no
payment method, no CAPTCHA solver). Everything in this runbook that does
*not* require that signup has already been done; what remains is listed in
[Step 0](#step-0--the-one-external-action-only-an-operator-can-do).

**What this is for:** provisioning the disposable Gitea Cloud tenant, org,
repos and test identities that the sibling hosted-qualification issues
(#9789, #9790, #9791) need to run live capability probes against. This issue
provisions the test surface; it does not run those probes and does not
certify Gitea compatibility — see the epic (#9769) and the decision gate
(#9792) for that.

**Do not fabricate a tenant.** No credential, account, org, repo, or test
result in this runbook is real until an operator completes Step 0 and a
builder runs Steps 1–4 against the live tenant. Every table below starts
empty or marked `UNKNOWN` for exactly that reason — see
[`gitea-cloud-qualification-evidence-template.md`](gitea-cloud-qualification-evidence-template.md).

---

## Step 0 — the one external action only an operator can do

1. Decide whether an existing hosted Gitea tenant is already available for
   reuse. If one exists, skip to recording its details (provider, instance
   origin, server build/version, edition, plan, update policy, region, trial
   expiry) in the evidence template and go to
   [Step 1](#step-1--credential-reference-naming-convention).
2. Otherwise, sign up for the smallest suitable trial at
   [Gitea Cloud](https://about.gitea.com/products/cloud/) — this is the one
   official URL named by the epic; do not substitute Forgejo or a different
   shared-hosting provider. This step needs an email inbox, a web browser,
   and (if the chosen plan requires it) a payment method — none of which an
   agent has access to.
3. From that one signed-up account, create (or confirm you can create) **four
   distinct identities** that will be added to the qualification org with
   different permission levels:
   - **Administrator** — org owner / repo admin. Used only for setup and the
     explicit branch-protection tests in #9789/#9790.
   - **Writer** — ordinary automation user. Used for the push/issue/PR/CI
     smoke checks and by future hosted-trial probes.
   - **Read-only** — a collaborator with read access only. Used to verify a
     read-only credential is rejected on mutation.
   - **Untrusted reviewer** — a collaborator from *outside* the org (e.g. a
     fork contributor), used to test the trust boundary a hosted probe must
     not blur (see `loom-daemon/src/comment_trust.rs` for why Loom
     distinguishes a trusted collaborator's comment from an outside
     reviewer's).

   Gitea Cloud is a multi-tenant SaaS offering, not a self-hosted instance —
   confirm during signup whether its admin API can create new local user
   accounts at all. If it cannot (expected for a hosted SaaS plan), each of
   the four identities needs its **own** gitea.com account, added to the
   qualification org/repos at the matching permission level, rather than
   being created via the API in Step 2. Record whichever is actually true as
   evidence, not as an assumption.
4. For each of the four identities, generate a personal API token (**Settings
   → Applications → Manage Access Tokens**, per
   [`defaults/docs/forge-authentication.md`](../../defaults/docs/forge-authentication.md)) with
   repository read/write scope as appropriate for that identity's role.
5. Confirm (from the account/plan page) whether Actions and self-hosted/managed
   runner registration are available on the selected plan. If not, that is a
   **blocker to record**, not something to silently work around (per the
   issue's "record missing plan features as blockers, not empty successes").
6. **Store every token and login outside this checkout**, owner-only
   permissions or the machine credential store — never in a fixture,
   screenshot, shell argument, CI log, or issue/PR body. See
   [`defaults/docs/credential-storage.md`](../../defaults/docs/credential-storage.md).
7. Record the reference (env var name or owner-store path) for each
   credential in this repo's `./.loom/credentials.md` — this PR adds the rows
   with their names; **do not fill in values**, the operator (or whoever runs
   Step 1 with the live tenant) does that outside the repo.
8. Hand back only the *names* that are now resolvable (e.g. "`GITEA_QUAL_ADMIN_TOKEN`
   is exported" or "the file exists at `~/.config/gitea-qual/admin.token`") —
   never the token values themselves — so a builder picking up #9789 can
   verify by name per the lookup-before-ask convention in
   [`defaults/docs/credentials.md`](../../defaults/docs/credentials.md).

Everything below this point assumes Step 0 is complete. A builder who reaches
this runbook before Step 0 is done should stop at this line, verify by
checking the credential references below (not by asking "is it done yet" in
chat), and if they are still unresolved, treat it as a missing-credential
case per the normal convention — not a reason to invent a tenant.

---

## Step 1 — credential-reference naming convention

Every credential below is a **name**, not a value. None of these are set in
this repository; this table is the manifest a builder consults before
running any step below, mirrored into `./.loom/credentials.md`.

| Purpose | Reference | Usage |
|---|---|---|
| Gitea Cloud instance origin (non-secret, environment-specific) | env var `GITEA_QUAL_INSTANCE_URL` | e.g. `https://gitea.com` or a tenant-specific origin; read to build API URLs |
| Qualification org slug | env var `GITEA_QUAL_ORG` | the disposable org created in Step 2 |
| Unique run namespace | env var `GITEA_QUAL_RUN_NS` | prefix for every disposable resource this runbook creates, e.g. `loomq-20261001-a1b2` — timestamp + short random suffix, so repeated runs never collide and cleanup is scoped to exactly one run |
| Administrator identity login | env var `GITEA_QUAL_ADMIN_LOGIN` | used in audit/evidence notes, never printed with the token |
| Administrator identity API token | env var `GITEA_QUAL_ADMIN_TOKEN` | org/repo creation, protection config, admin-bypass tests |
| Writer identity login | env var `GITEA_QUAL_WRITER_LOGIN` | ordinary automation identity |
| Writer identity API token | env var `GITEA_QUAL_WRITER_TOKEN` | push/issue/PR/CI smoke checks |
| Writer identity SSH key (optional, for git-over-SSH checks) | provisioned file `~/.ssh/gitea_qual_writer` | `ssh -i <path>`; public half installed on the writer account, private half never leaves the owner's machine |
| Read-only identity login | env var `GITEA_QUAL_READONLY_LOGIN` | permission-boundary tests |
| Read-only identity API token | env var `GITEA_QUAL_READONLY_TOKEN` | must be rejected on any mutating call — that rejection is the test |
| Untrusted-reviewer identity login | env var `GITEA_QUAL_REVIEWER_LOGIN` | outside-collaborator / fork-review trust-boundary tests |
| Untrusted-reviewer identity API token | env var `GITEA_QUAL_REVIEWER_TOKEN` | comments/reviews from this identity must never be treated as a trusted collaborator's by any Loom guard |

A missing entry here when a later issue (#9789, #9790, #9791) needs it is a
**missing-credential case**, handled per
[`defaults/docs/credentials.md`](../../defaults/docs/credentials.md) — ask the
operator for the name and provisioning path only, never the value.

---

## Step 2 — idempotent setup (reference commands)

These are **reference commands for a human or a future builder to run once
the credentials above resolve** — they are not a tracked script in this repo
(see [`.loom/docs/shell-language-policy.md`](../../.loom/docs/shell-language-policy.md):
a one-off external-account provisioning flow is documentation, not new
product automation). Copy them into a local, untracked script if useful;
do not commit a copy with values filled in.

Every created resource is prefixed with `${GITEA_QUAL_RUN_NS}` so a second
run against the same org is safe (checks existence before creating) and
cleanup can be scoped to exactly the resources this run made.

```bash
# All calls assume the four GITEA_QUAL_*_TOKEN / *_LOGIN env vars above are
# already exported in-session by whoever is running this -- never paste a
# value here.
set -euo pipefail

api="${GITEA_QUAL_INSTANCE_URL%/}/api/v1"
ns="${GITEA_QUAL_RUN_NS:?set a unique run namespace first, e.g. loomq-$(date +%Y%m%d)-$RANDOM}"
org="${GITEA_QUAL_ORG:?set the qualification org slug}"

admin_auth=(-H "Authorization: token ${GITEA_QUAL_ADMIN_TOKEN}")

# 0. Verify the admin identity authenticates before doing anything else.
curl -sf "${admin_auth[@]}" "${api}/user" | jq -r '.login'

# 1. Reuse the org if it exists; create it only if this is the first run.
#    Never delete the org in cleanup -- it is reused across runs.
if ! curl -sf "${admin_auth[@]}" "${api}/orgs/${org}" >/dev/null 2>&1; then
  curl -sf -X POST "${admin_auth[@]}" -H "Content-Type: application/json" \
    -d "{\"username\": \"${org}\", \"visibility\": \"private\"}" \
    "${api}/orgs"
fi

# 2. Create the two disposable repos this run needs: a source repo (for the
#    fork scenario) and a separate repo for cross-repo isolation tests.
#    Both are idempotent -- a repo that already exists under this run's
#    namespace is reused, never recreated.
for repo in "${ns}-source" "${ns}-isolated"; do
  if ! curl -sf "${admin_auth[@]}" "${api}/repos/${org}/${repo}" >/dev/null 2>&1; then
    curl -sf -X POST "${admin_auth[@]}" -H "Content-Type: application/json" \
      -d "{\"name\": \"${repo}\", \"private\": true, \"auto_init\": true}" \
      "${api}/orgs/${org}/repos"
  fi
done

# 3. Add the writer and read-only identities to the org at the matching
#    permission level (requires org teams -- Gitea's default "Owners" team
#    cannot hold a read-only member; create a dedicated team per level).
#    This step is written generically because whether Gitea Cloud's hosted
#    plan supports team creation via API, versus requiring the web UI, is
#    itself one of the facts Step 0 records -- do not assume the API path
#    works until it has been tried against the live tenant.

# 4. Fork scenario: the writer identity forks the source repo (exercises
#    fork/upstream identity resolution used by the hosted probes).
curl -sf -X POST -H "Authorization: token ${GITEA_QUAL_WRITER_TOKEN}" \
  "${api}/repos/${org}/${ns}-source/forks" \
  -H "Content-Type: application/json" -d "{\"organization\": null}"

# 5. Smallest smoke checks -- auth, issue/comment, push, CI result.
curl -sf -H "Authorization: token ${GITEA_QUAL_WRITER_TOKEN}" "${api}/user" | jq -r '.login'

issue_id=$(curl -sf -X POST -H "Authorization: token ${GITEA_QUAL_WRITER_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"title": "qualification smoke test", "body": "created by the gitea-cloud-qualification runbook"}' \
  "${api}/repos/${org}/${ns}-source/issues" | jq -r '.number')

curl -sf -X POST -H "Authorization: token ${GITEA_QUAL_WRITER_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"body": "smoke comment"}' \
  "${api}/repos/${org}/${ns}-source/issues/${issue_id}/comments"

git clone "https://${GITEA_QUAL_WRITER_LOGIN}:${GITEA_QUAL_WRITER_TOKEN}@${GITEA_QUAL_INSTANCE_URL#https://}/${org}/${ns}-source.git" "/tmp/${ns}-source"
# ... commit + push inside /tmp/${ns}-source, then check CI:
curl -sf -H "Authorization: token ${GITEA_QUAL_WRITER_TOKEN}" \
  "${api}/repos/${org}/${ns}-source/actions/tasks" | jq '.workflow_runs // .'

# 6. Read-only identity must be rejected on a mutating call -- this failure
#    is the test passing, not an error to work around.
curl -s -o /dev/null -w '%{http_code}\n' -X POST \
  -H "Authorization: token ${GITEA_QUAL_READONLY_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"title": "should be rejected"}' \
  "${api}/repos/${org}/${ns}-source/issues"
```

## Step 3 — cleanup (scoped to one run)

```bash
set -euo pipefail
api="${GITEA_QUAL_INSTANCE_URL%/}/api/v1"
ns="${GITEA_QUAL_RUN_NS:?}"
org="${GITEA_QUAL_ORG:?}"
admin_auth=(-H "Authorization: token ${GITEA_QUAL_ADMIN_TOKEN}")

# Only delete repos whose name is prefixed with this run's namespace.
# Never delete the org, never delete a repo outside this prefix.
for repo in "${ns}-source" "${ns}-isolated"; do
  curl -sf -X DELETE "${admin_auth[@]}" "${api}/repos/${org}/${repo}" || true
done

# Remove the fork too, if the writer identity's fork landed under its own
# account rather than the org.
curl -sf -X DELETE -H "Authorization: token ${GITEA_QUAL_WRITER_TOKEN}" \
  "${api}/repos/${GITEA_QUAL_WRITER_LOGIN}/${ns}-source" || true

rm -rf "/tmp/${ns}-source"
```

Keep evidence/exports from a run until the GO/NO-GO decision (#9792) is
recorded — cleanup removes the *disposable resources*, not the receipt.

## Step 4 — record evidence

Fill in [`gitea-cloud-qualification-evidence-template.md`](gitea-cloud-qualification-evidence-template.md)
with the real values observed from the live tenant. Leave any field that was
not or could not be measured as `UNKNOWN` — an empty success is not a passing
result per the issue's acceptance criteria.

---

## Explicit unknowns (until a live tenant exists)

These cannot be known without Step 0 and are recorded as unknowns rather than
assumed, per the issue's "record automation policy ... as explicit unknowns
if not knowable":

- Automation / bot policy on the selected plan (is a token-driven CI/bot
  account within normal ToS for the chosen trial tier?).
- Measured request rate limits.
- Runner concurrency and included minutes.
- Bot/service-account seat allowance.
- Storage and artifact/export allowances, and what export mechanism exists
  if the tenant needs to be torn down.
- Whether the hosted plan's admin API can create org teams/users at all, or
  whether every identity needs its own independently-signed-up account (see
  Step 0.3).
- Trial expiry date and who owns renewal/cancellation.

## Related

- Epic: #9769 — "Hosted-first Gitea qualification, GO/NO-GO, and gated 2am AWS rollout"
- #9789 / #9790 / #9791 — the hosted probes this provisioning unblocks
- #9792 — the GO/NO-GO decision gate this evidence feeds
- [`defaults/docs/credentials.md`](../../defaults/docs/credentials.md) — reference-by-name convention
- [`defaults/docs/credential-storage.md`](../../defaults/docs/credential-storage.md) — storage policy
- [`defaults/docs/forge-authentication.md`](../../defaults/docs/forge-authentication.md) — existing Gitea token/auth support in Loom
