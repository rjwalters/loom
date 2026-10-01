# Forge contract — identity, outcomes, evidence, profiles

Status: reviewed contract for the hosted-first Gitea qualification
(#9779, phase 1 of epic #9769). Runtime counterpart:
`loom-daemon/src/forge_contract.rs` (types + tests). Builds on the
operation inventory (#9777, `defaults/forge/`) and the write-scope
permission probe (#9699). Authority invariants live in ADR-0014 and
ADR-0021 and are restated, not redefined, here.

## 1. Request identity

Every forge request carries five identity components, resolved per
request and threaded explicitly — never global ambient state:

| Component | Type | Example |
|---|---|---|
| `provider` | `Provider` | `gitea` |
| `instance_origin` | origin URL, scheme + host, lowercased | `https://git.example.com` |
| `repository` | stable `RepositoryRef` (below) | origin + `owner/repo` |
| `credential` | principal **reference**, never a secret | `credential-ref:gitea-token@git.example.com` |
| `profile` | `Profile` from the inventory | `required-coordination` |

### Object identity vs display values

Issue and PR numbers are **display values scoped to one
(origin, owner, repo)**; URLs are display strings. Stable object
identity is `(provider, instance_origin, owner, repo, kind, number)`.
Two origins that agree on owner, repo, number *and* actor login are
different objects with no collision:

| | Forge A | Forge B |
|---|---|---|
| origin | `https://git.acme.dev` | `https://gitea.cloud.dev` |
| owner/repo | `team/widgets` | `team/widgets` |
| issue | #7 | #7 |
| reporter | `rob` | `rob` |

`issue.create` on both returns `number = 7`; the two objects are
distinct because `instance_origin` differs. Any code path that keys a
cache, a claim, a verdict anchor or an event on `(owner, repo, number)`
alone is a bug: cross-origin collisions MUST fail closed (refuse and
report) rather than deduplicate into the wrong object.

### Credential resolution

Credentials resolve per request from explicit references (env var name,
config key, credential-file path). Concurrent requests to different
origins MUST NOT mutate process-global environment variables: switching
`GITEA_TOKEN` between threads is a cross-request credential leak.
GitHub App installation tokens keep their existing mint/read path
(`forge_identity.rs`); a Gitea token never mints through it.

**The quota-sharing boundary is the (provider, credential principal)
pair.** Requests under one principal reference share one rate-limit
budget and one breaker (`rate_limit_breaker.rs`); two different
principals to the same provider do NOT share budget. Concurrent
repositories sharing a credential MUST share its breaker state;
repositories with their own credentials are isolated. Any design that
varies the credential per request must scope the breaker key to that
credential reference.

## 2. Outcome taxonomy — fail closed

Six outcomes, no success-biased coercions:

| Outcome | Meaning | Merge-gate reading |
|---|---|---|
| `Unsupported` | The provider (at its observed version) has no such capability | the operation cannot have run |
| `Unknown` | The provider could not be asked, or answered unintelligibly | **pending**, never resolved |
| `InsufficientPermission` | Authenticated, denied | definitive about the **attempt**: no partial write happened and this credential cannot succeed — the work it stood for stays pending until the credential is fixed (`ForgeOutcome::is_definitive`) |
| `ConflictHeadChanged` | Optimistic concurrency lost (head SHA moved) | retry after rebase; not a forge fault |
| `PartialPagination` | A list read ended before exhaustion | the list is unusable for decisions — same reading as `Unknown`: pending, not resolved; a full re-**read** (not a blind write retry) is the recovery |
| `Transient` | Rate limit / 429 / 5xx / timeout | retry with backoff; still unresolved until answered |

Examples that fix the semantics:

- **Empty ≠ failed check.** `GET .../statuses` returning `[]` means
  "no statuses exist"; a 500 or a truncated page means "the answer is
  unknown". `check-ci-status` coerced the second into the first before
  #9879 — that coercion is the bug class this taxonomy bans. An empty
  collection may only be asserted when the read completed exhaustively.
- **Unknown ≠ resolved review.** A review thread that the API reports
  as neither resolved nor unresolved (absent, or a body the adapter
  cannot parse) is `Unknown`: a verdict anchored on it is stale, not
  approved.
- **Missing permission ≠ absent object.** A 404 on a repository the
  credential cannot see is `InsufficientPermission` when the
  credential-reference is known-good for other origins, and `Unknown`
  when visibility itself cannot be established — never "does not
  exist".

## 3. Evidence levels

Three, in the manifest (`disposition`, `test_id`) and in every
qualification report:

1. **Platform** — the forge capability exists and passes live tests
   against the observed build.
2. **Adapter** — Loom's adapter implements the operation (code + tests
   exist and pass against the live forge).
3. **Installed caller** — actual installed callers (role scripts,
   daemon passes, prompts) invoke the adapter path for real work.

A platform probe alone does not certify fleet readiness: GO requires
level 3 for every `required-*` row, per #9777's coverage accounting.

## 4. Profiles

Required profiles are #9777's own (`required-coordination`,
`required-ci-landing`, `fleet-bootstrap`, `delivery`). Optional
exclusions carry an `exclusion_reason` and an explicit preflight
behavior (what the caller checks before skipping, and what it reports).
No implicit waivers for: claims, identity/trust, reviews, branch
protection, guarded merge.

## 5. Observed target build + requalification

| | Version | Observed |
|---|---|---|
| Self-managed target | Gitea **28.0.0** (docker `gitea/gitea:28`) | `gitea-1` (2am#1793), 2026-10-01 |
| Hosted profile | Gitea Cloud / **1.24.x floor** (manifest `providers.gitea`) | per #9788's setup |
| Runner/CLI | Gitea's own REST via `gitea_api` (curl); `tea` only if a transport needs it — no CLI dependency for its own sake | — |

Requalification triggers: a major-version bump on the hosted tenant, a
manifest `version_support` floor raise, an adapter behavior change, or
a failed probe re-run. Version before/after is recorded in every run's
receipt (the probe runner emits them; #9789).

## 6. Authority + fidelity boundary

One primary forge owns claims, labels and merges (ADR-0014); an event
feed prompts fresh reads, never carries state (ADR-0021). A future
GitHub mirror is **non-dispatchable** — nothing reads it for
coordination. Fidelity boundary for that deferred publisher: native
Git replication does not reproduce issue/review history, and ordinary
GitHub APIs cannot freely assign historical authors, timestamps or
issue numbers. The mirror is therefore a lossy publication, accepted
as such; its design is deferred (#9769) and must not delay GO/NO-GO.
