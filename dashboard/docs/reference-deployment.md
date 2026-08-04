# Recording your own instance's deployment details

This repository intentionally does not track any specific operator's live
deployment identity — Cloudflare account ID, D1 database ID, custom domain,
CI secret names, Cloudflare Access application layout, and the machine-local
paths where credentials live all belong in *your own* infrastructure repo,
not in this public mechanism repo. Once you stand up your own instance by
following [`deploy-runbook.md`](deploy-runbook.md) and
[`cloudflare-access.md`](cloudflare-access.md), keep a document with this
same shape — Worker identity, database identity, your local config-overlay
pattern, CI auto-deploy secrets (if any), Access applications, credential
file locations, and host enrollment status — in your own repo, so the next
person who needs to touch your instance (redeploy, rotate a credential, add
a host, debug an incident) does not have to rediscover it from scratch.
