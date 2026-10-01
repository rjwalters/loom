# Hosted Gitea trial runbook — provisioning the qualification sandbox

Issue #9788 (phase 1 of epic #9769) — the PR-able slice: the exact
commands and the evidence checklist. The execution itself
(tenant purchase, credential minting) is operator-only; this runbook
makes it a short mechanical session instead of a design one.

Everything the tenant gives us is **evidence, not a promise**: a limit
that cannot be measured is recorded as an explicit unknown in the
receipt, never as an unlimited quota.

## 1. Provision the tenant (operator)

- Candidate: [Gitea Cloud](https://about.gitea.com/products/cloud/)
  (the manifest's `providers.gitea` target). Do not silently substitute
  Forgejo or another host — a substitution is a new decision.
- Smallest plan that exposes: REST API tokens, web UI, Actions
  (managed runner access or runner registration), repository
  protection configuration.
- Record in the receipt (§5): provider, instance origin, server build
  + version, edition, plan, update policy (who controls upgrades),
  region if exposed, trial expiry, cost/seat basis.
- **If signup requires an owner action or billing input that is
  absent, STOP and record the named provisioning blocker** on #9788.
  Do not invent quotas or reuse an unrelated tenant.

## 2. Identities + resources (run namespace)

Create everything under one run namespace (e.g. `loom-qual-<date>`) so
cleanup is total and unambiguous:

| Identity | Purpose | Used for |
|---|---|---|
| `qual-admin` | setup + protection tests only | config, protection rules |
| `qual-writer` | the automation writer | issues/labels/comments/PRs/merges |
| `qual-reader` | read-only | negative tests |
| `qual-outsider` | untrusted reviewer | comment-trust / marker tests |

Resources: one isolated qualification organization; **two** private
repos (`qual-repo-a`, `qual-repo-b`) — the second exists for isolation
tests; one fork of `qual-repo-a` (the fork/source-repo scenario).

Idempotency: every setup command is safe to re-run (create-if-missing,
never create-duplicate); a re-run after partial teardown converges.

## 3. Credentials — by reference, never by value

- Mint one token per identity, least scope (writer: repo
  issues/PRs/actions; reader: read; admin: admin).
- Store outside every checkout: owner-only file or the OS credential
  store (see `defaults/docs/credential-storage.md`,
  `defaults/docs/credentials.md`).
- Commit **references only** into the receipt — the credential
  *reference* convention (`credential-ref:<what>@<origin>`), never a
  token, in fixtures, screenshots, shell arguments, CI logs, Terraform
  state, or issues. The harness receives credentials by env-var name.
- Verify distinct permissions before any test: the reader's mutation
  attempts must be REJECTED (that rejection is evidence, and it is the
  negative half of acceptance criterion 2).

## 4. Smoke checks (the "second implementer" bar)

A second implementer, working only from this runbook + the credential
references, must complete in one sitting:

```sh
export GITEA_URL="<instance origin>"          # from the receipt
export GITEA_TOKEN="<reference - the writer's token, external store>"

# auth
curl -s -H "Authorization: token $GITEA_TOKEN" \
  "$GITEA_URL/api/v1/user" | jq .login        # expect: qual-writer

# clone + push (git transport)
git clone "https://$GITEA_URL/qual-org/qual-repo-a.git" && cd qual-repo-a
echo test > smoke.txt && git add . && git commit -m smoke && git push

# issue + comment (REST)
curl -s -X POST -H "Authorization: token $GITEA_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"title":"smoke","body":"#9788"}' "$GITEA_URL/api/v1/repos/qual-org/qual-repo-a/issues"
```

Plus an **actual CI result**: enable Actions on the repo, commit a
minimal workflow, register (or reach) a runner, and record the run's
real outcome — a missing plan feature is a **blocker**, never an empty
successful result.

## 5. The environment receipt

A sanitized receipt is committed through the normal Loom lifecycle
(the template lives with the probe runner, #9789): server build +
version/edition, origin, plan, automation policy, **measured** request
limits (a probe, not a doc quote), runner concurrency/minutes, bot
seats, storage/artifact allowances, export capabilities, expiry, and
every unmeasured commercial limit as an explicit `unknown`. If the
provider controls upgrades and a version cannot be pinned: capture the
version before/after each qualification run and invalidate affected
results on change (#9779 §5's requalification rule).

## 6. Cleanup + evidence retention

- Cleanup list = exactly the namespace's resources; **never** a bulk
  delete. Identities and repos created here die at cleanup; evidence
  (receipts, exports) survives until the GO/NO-GO decision is recorded
  (#9792) and then per the retention rule in #9788.
- Export before expiry: repo archives + issue/PR JSON + Actions run
  logs — the hosted tenant is disposable, the evidence is not.
