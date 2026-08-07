# Token pool provider identity (#5605)

**Status:** design accepted; implementation deferred to the follow-up issues in
§9. No behavior changes ship with this document.
**Verified against:** `origin/main` @ `a354d10a`, 2026-08-07. Every file:line
reference below was read at that commit — re-check them before implementing.
**Related:** #5604 (the narrow stop-gap: filter the import to Anthropic rows),
epic #4167 Phase 4 / #4489 (provider-aware account management), #5028 /
`runtimes.roles` (per-role runtime binding).

## 1. Problem

`.loom/tokens/` is keyed by **email**. claude-monitor is keyed by
**(provider, account)**. Two operator emails already exist under both
`anthropic` and `openai` on the live `robb-studio` host, so the import
collapses two distinct upstream accounts into one pool slot and which one wins
is row-order dependent (`monitor_db.rs:268-316` — `by_email.insert(...)`
overwrites unconditionally, highest `c.id` last-write-wins). The losing
Anthropic account becomes invisible; the symptom looks like ordinary rate-limit
exhaustion, so it cost a Max account at 8% weekly utilisation for weeks before
anyone compared token file byte counts.

The narrow fix (#5604) filters the import to `provider = 'anthropic'`. That is
correct as a stop-gap and should ship independently, but it makes every
non-Anthropic credential permanently unreachable — which contradicts
`runtimes.roles` (a Codex Judge is already configurable today) and the stated
intent to add kimi and qwen on the same operator emails. The pool has to become
provider-aware rather than provider-filtered.

## 2. What already exists (three layers, one missing dimension)

| Layer | Provider-aware today? | Where |
|---|---|---|
| **Selection API** | **Yes** — `AccountId { provider, name }`, `AccountProvider { Claude, Codex }`, `account_inventory()` / `select_account()` dispatch on provider | `account_registry.rs:17-28`, `:449-496` |
| **Health / cooldown state** | **Yes** — `AccountHealth { provider, name, … }` in `.loom/account-health.json`, provider-scoped capacity and round-robin cursors | `health.rs:64-93`, `:152-154`, `:345-465` |
| **Claude credential storage** | **No** — `Account { email, key, file, source, index }`, `index.json` rows carry `email`/`name`/`file`/`source`/`key_fingerprint`, no provider, no upstream id | `bootstrap.rs:217-224`, `:433-463` |
| **Import (claude-monitor)** | **No** — SQL selects `a.email` only, dedup by lowercased email | `monitor_db.rs:232-236`, `:268-316` |
| **Rate-limit probe / ranking** | **No** — probes every `*.token` against Anthropic's `/v1/messages`; the monitor-sourced path joins on email | `check.rs:330-365`, `:494-580`; `monitor.rs:117-147`, `:244-296` |

So this is **not** a green-field identity design. The `(provider, …)` shape
exists above and beside the Claude pool; the Claude pool's own storage and
import path are the hole. The work is to push the dimension down, not to invent
a type.

Note also which surfaces are *cheap* to change: `index.json`'s only functional
in-tree reader is `monitor.rs::load_index_email_map` (`monitor.rs:117-147`);
everything else that mentions `index.json` in `loom-daemon/` is a doc comment,
a test fixture, or the `tokens.rs` pool-shape guard listing filenames. The
Python conformance requirement recorded in `bootstrap.rs:33-41` is **historical**
— `loom-tools` was deleted in epic #4081 Phase 4 (#4557, ADR-0013), so there is
no second implementation to stay byte-compatible with. That comment should be
corrected by the implementer rather than treated as a live constraint.

## 3. The collision path the filed issue does not name

#5604 attributes the misleading `exhausted / 5h=None / 7d=1.0` reading to the
Anthropic probe being unable to read headers from an OpenAI token. **Reading the
code, the probe cannot produce that output**, and the real mechanism survives
#5604's import filter. This is the most load-bearing finding in this design.

- A JWT does not start with `sk-ant-oat`, so `build_headers` (`check.rs:467-480`)
  sends it as `x-api-key`. Anthropic answers 401, and `probe_account`
  (`check.rs:537-541`) maps 401 to `blocked` with `error: "auth_401"` — never
  `exhausted`. `exhausted` is only reachable from `status_from_utilization`
  (`check.rs:484-490`), which needs a real `anthropic-ratelimit-*-7d-utilization`
  header ≥ `EXHAUSTED_THRESHOLD` (0.95).
- The default ranking source is `auto` (`check.rs:704-720`), which prefers
  claude-monitor's `ranking.json` when it is fresh and never probes at all.
  `build_monitor_accounts` (`monitor.rs:244-296`) joins `ranking.json` rows to
  Loom account names **by lowercased email** via `load_index_email_map`, and when
  two rows resolve to the same Loom name it deliberately keeps **the more severe
  status** (`monitor.rs:284-289`, added by #4873 for a different reason).

So an `openai` row at 100% weekly usage and an `anthropic` row at 8%, sharing an
email, both resolve to the same Loom name; the OpenAI row wins the severity
merge; the pool reports `exhausted`, `7d=1.0`, `5h=None` (no 5h window upstream),
and the *OpenAI* weekly reset — exactly the reported reading, including the
detail that made it look legitimate.

**Consequence for scope:** filtering the *import* does not fix this. The
`ranking.json` join is a second, independent email-keyed identity assumption,
and it poisons the status of an Anthropic account that was imported perfectly.
Both must be fixed, which is why §9 splits them into separate issues rather than
folding the probe work into the storage change.

*(Confidence: the code path is verified by reading; the exact `ranking.json`
row set on the live host is not. If claude-monitor turns out to publish only
Anthropic rows there, this path is latent rather than active — the fix is
unchanged either way, since the join must not be email-only once the pool can
hold more than one provider.)*

## 4. Design decisions

### D1 — Two keys with distinct jobs, not one

The issue title asks for identity to become `(provider, account_id)`. This
design **partially deviates**, deliberately:

| Key | Shape | Job | Stability |
|---|---|---|---|
| **Selection key** | `AccountId { provider, name }` (unchanged) | Everything operator- and state-facing: `.ranking` rows, `.allowlist`, `.bad_tokens`, `.failure_counts`, `account-health.json` records, `tokens pin/unblock <name>`, `LOOM_ACCOUNT_NAME` | Stable across re-imports; renaming resets pool state, so it must not churn |
| **Upstream key** | `upstream_id: Option<String>`, provider-scoped | Import-time dedup and the join to claude-monitor's own records | Opaque; may be absent for sources that have no upstream |

Rationale for not folding `account_id` into `AccountId`:

1. The collision is an **import-time dedup** bug. The dedup key is exactly where
   the upstream id is needed, and that is not the same place as the selection key.
2. `account-health.json` already serializes `(provider, name)` records with
   `deny_unknown_fields` (`health.rs:64-78`, `:95-103`). Widening `AccountId`
   forces a schema migration of that file for no functional gain.
3. `name` is the operator handle in every CLI surface and in the shell-consumed
   `.ranking` format. A uuid is not a usable substitute there.

`upstream_id` is still **carried through import → storage → selection** (AC 2):
it is written to `index.json`, resolved by the selector, and exposed in
`tokens select --json` output plus a new `LOOM_ACCOUNT_UPSTREAM_ID` observability
variable alongside the existing `LOOM_ACCOUNT_PROVIDER` / `LOOM_ACCOUNT_NAME`
(`account_registry.rs:96-112`), so a dispatched sweep can be correlated back to
the exact upstream account.

### D2 — `upstream_id` is a namespaced string, always derivable

`Option<String>` in the struct, but every writer produces one, using a
documented namespace prefix so derivations from different sources can never
collide:

| Prefix | Source | Example |
|---|---|---|
| `monitor:` | claude-monitor's provider-native account id | `monitor:0f3c…` |
| `monitor-pk:` | fallback — claude-monitor's `accounts.id` integer PK, when no provider-native id column exists | `monitor-pk:14` |
| `email:` | env-triple sources (`accounts.env`, repo `.env`), which have no upstream id at all | `email:alice@example.com` (lowercased) |

`email:` keeps today's behavior *exactly* for the env-triple paths: identity
there remains email-derived, because that is genuinely all those sources carry.
The dedup key becomes `(provider, upstream_id)` everywhere, which is a total
function, and which for env sources reduces to today's `(claude, email)`.

**Open question the implementer must close first** (inherited from #5604's
"Suspected cause (unverified)"): claude-monitor's `accounts` schema. Run
`PRAGMA table_info(accounts);` against a live or fixture `usage.db` and confirm
(a) the provider column's name and value vocabulary, and (b) whether a
provider-native stable id column exists. The in-repo test double
(`monitor_db.rs`, `seed_usage_db`) creates only
`CREATE TABLE accounts (id INTEGER PRIMARY KEY, email TEXT)`, so no existing
test constrains this. If (b) is absent, use `monitor-pk:` and say so in the PR.

### D3 — Where `provider` is recorded at the storage layer

- `bootstrap::Account` gains `provider: AccountProvider` and
  `upstream_id: Option<String>` (`bootstrap.rs:217-224`).
- `bootstrap::ManifestRow` gains the same two fields (`bootstrap.rs:433-463`),
  serialized as `"provider"` and `"upstream_id"`.
- `INDEX_VERSION` 2 → 3 (`bootstrap.rs:60`). The writer always emits 3; readers
  accept 2 **and** 3, backfilling a v2 row as
  `provider = "claude"`, `upstream_id = "email:<lowercased email>"`.

The backfill rule means **no migration step and no `tokens migrate` command**: a
pre-existing v2 manifest reads correctly, and the next `bootstrap` /
`import-from-monitor` rewrites it as v3. Both fields are additive, so a stale
reader that ignores unknown keys is unaffected.

### D4 — One Claude pool; other providers are recorded, not materialized

The judgment call the `complex` marker exists for. Three options were considered
(§8 records the rejected two). **Chosen: keep `.loom/tokens/` as the Claude
credential-file backend, and keep every other provider behind its own backend
under the shared `AccountId` front** — i.e. extend the split that
`account_registry.rs:449-457` already implements, rather than unify storage.

Concretely, `import-from-monitor` changes from *filter* to *record*:

| Row provider | `.token` file written | `index.json` row |
|---|---|---|
| `anthropic` | yes, into `.loom/tokens/` | `provider: "claude"`, `materialized: true` |
| anything else | **no** | `provider: "<provider>"`, `materialized: false`, no `key_fingerprint`, plus a `Warning` surfaced by `print_monitor_import` |

This is #5604's "skip + warn" upgraded to "record + warn + don't materialize".
The credential never enters an Anthropic pool slot (the defect is
unrepresentable), *and* the operator gains the visible signal the issue says is
missing today — `index.json` now answers "why is this email not in the pool?"
without comparing file sizes.

When a future provider gains a credential backend (kimi/qwen are expected to be
API-key shaped), it flips to materialized via its own backend and its own
storage location. The extension point is a small `ProviderAdapter`:

```rust
trait ProviderAdapter {
    fn provider(&self) -> AccountProvider;
    fn credential_kind(&self) -> CredentialKind;              // account_registry.rs:30-35
    fn validate_credential_shape(&self, secret: &str) -> Result<()>;  // D7
    fn probe(&self, name: &str, secret: &str) -> AccountResult;       // D6
}
```

Only `Claude` implements all four today. `Codex` keeps its directory-based
backend and implements `credential_kind` only. Everything else resolves to "no
adapter", which is a *reported* state, never a silent one.

**Why not per-provider subdirectories under `.loom/tokens/`:** `.ranking`,
`.allowlist`, `.bad_tokens` and `.failure_counts` are flat, name-keyed text
files consumed by shell (`claude-wrapper.sh`, `spawn-claude.sh`) as well as by
Rust. Making the pool directory multi-provider forces all four formats to grow a
provider dimension, with a migration for each, to serve zero present consumer —
`health.rs` already provides a provider-keyed state store for exactly the
non-Claude case.

### D5 — Collision-proof name derivation

`derive_token_filename` (`bootstrap.rs:105-132`) stays as-is for the common case,
so the 17 existing token files keep their names and their pool state. When two
entries in one import batch derive the same stem:

1. The **`claude`** entry keeps the bare stem (`rjwalters-gmail`).
2. Every other provider's entry gets `-<provider>` appended
   (`rjwalters-gmail-openai`).
3. Two entries of the **same** provider colliding on a stem (the genuine
   `a.jones@x` vs `ajones@x` case) remains a hard `DuplicateFile` error
   (`monitor_db.rs:474-485`) — unchanged, deterministic, loud.

Rule 1 is what makes the outcome order-independent, which is the property the
issue's "resolution appears order-dependent" wrinkle is asking for. Under D4 the
non-claude entry has no file at all today; the suffix rule is what makes the
`name` field unique in the manifest, and it is already correct for the day a
second provider becomes materializable.

### D6 — Provider-dispatched rate-limit probing

Two separate paths, both currently email/Anthropic-assuming:

**(a) `--source probe`.** `discover_tokens` (`check.rs:330-365`) walks `*.token`
and returns `(name, token)`. It gains a provider resolution step against
`index.json`: a file with no manifest row is treated as `claude` (fail-open to
today's behavior for hand-provisioned pools). Dispatch then goes through
`ProviderAdapter::probe`:

- `claude` → the existing Anthropic probe, byte-identical.
- any provider with no probe adapter → `AccountResult { status: "unsupported",
  error: Some("no_probe_adapter:<provider>") }`, with **all** utilization and
  reset fields `None`. Never `exhausted`, never a fabricated number (AC 4).

`status_rank` (`check.rs:584-594`) gains `"unsupported" => 6` (worse than
`skipped`) as defense-in-depth for any sort path that sees it. Crucially,
`unsupported` rows are **omitted from `.ranking`** — that file's contract is
"Claude accounts the Claude selector may pick", and it is read by
`select::try_ranking` and by the daemon's healthy-count reader. They *are*
present in `--json` and in the human table, which is where an operator-visible
signal belongs. This keeps the shell-consumed format's semantics and the
capacity counter untouched.

**(b) `--source auto|monitor` (the default, and the actual bug from §3).**
`load_index_email_map` (`monitor.rs:117-147`) must emit rows for **claude-provider
manifest entries only**, and the join in `build_monitor_accounts`
(`monitor.rs:244-296`) must prefer `upstream_id` when the `ranking.json` row
carries one, falling back to the (now claude-scoped) email map. An `openai` row
then matches nothing and is skipped instead of poisoning an Anthropic account's
status through the severity merge. The severity-merge rule itself
(`monitor.rs:284-289`) stays — it is correct for its original #4873 case (two
emails, one Loom account) and only misfired because the map was provider-blind.

### D7 — Per-provider credential shape validation on write

`ProviderAdapter::validate_credential_shape` is called from
`materialize_accounts` (`bootstrap.rs:485-579`) **before** the write, so both
`bootstrap` (env triples) and `import-from-monitor` are covered by one gate —
today a fix applied only to the importer would leave the env path open.

- `claude`: must start with `sk-ant-` (covers both `sk-ant-oat…` OAuth and
  `sk-ant-api…` keys). A JWT (`eyJ…`) is a hard error, not a warning, per
  #5604's acceptance criteria.
- Validation applies to **writes only**. A pre-existing wrong-shaped token on
  disk is never deleted by this change; `tokens check` reports it (see below).

`tokens check` additionally applies the same shape check at discovery, so a
legacy mis-bound file reports `blocked` with `error: "shape_mismatch"` instead of
the far less diagnostic `auth_401`.

### D8 — `tokens select --provider` and the runtime → provider chain

`tokens select --provider` **already exists** (`main.rs:1588-1591`), defaulting
to `claude`, with `spawn-codex.sh:603-609` passing `--provider codex` and
`spawn-claude.sh:512` relying on the default. The stamped claim in the issue
that this surface must be added is stale as of `a354d10a`; what is missing is
three narrower things:

1. **Parsing.** `cli/tokens.rs:558-594` compares two hardcoded strings. Give
   `AccountProvider` `FromStr` + `Display` and parse through it, so the valid
   vocabulary has one definition and the error message enumerates it. This is
   also what lets a new provider be added without touching the CLI arm.
2. **Enforcement in the Claude arm.** `select::select_token` skips any `.token`
   file whose manifest row says `provider != claude`; a file with no manifest
   row is treated as `claude` (fail-open, as in D6a). Under D4 no such file
   should exist, so this is an assertion, not the primary mechanism — but it is
   the assertion that makes the defect unrepresentable even if a stale pool
   directory survives an upgrade.
3. **Deriving the provider from the runtime, not from a new config axis.**
   `defaults/runtimes/<name>.json` gains an `"accountProvider"` field
   (`claude.json` → `"claude"`, `codex.json` → `"codex"`), and each
   `spawn-<runtime>.sh` passes that value to `tokens select --provider` instead
   of hardcoding it.

The resolved chain therefore is:

```
role  ──►  runtime                                        ──►  provider            ──►  pool
       LOOM_RUNTIME_<ROLE> > LOOM_RUNTIME >              runtime manifest        tokens select
       runtimes.roles.<role> > runtimes.default > claude  "accountProvider"       --provider <p>
```

**Provider is a property of the runtime, not a fourth thing operators
configure.** A Codex Judge (`runtimes.roles.judge = "codex"`) already implies
the Codex account provider; asking an operator to keep a second map in sync
would only create a new way to get a mismatch — precisely the failure #5001
fixed for the model axis (`LOOM_RUNTIME_JUDGE=codex` + a globally pinned Claude
model looping forever on HTTP 400). Manifest resolution already has a bundled
`include_str!` fallback (#5002), so a missing `accountProvider` on an un-resynced
install must default to `claude` rather than fail closed.

### D9 — Relationship to `account_registry.rs` (AC 5, explicit)

**Unify the identity type; do not unify the storage.**

- `AccountProvider` (`account_registry.rs:17-22`) becomes the single provider
  vocabulary for the whole crate — storage, import, probe, selection, health —
  gaining `FromStr`/`Display` and, when the runtimes land, new variants. The
  storage layer imports it rather than defining a parallel enum.
- `AccountId { provider, name }` stays the selection/state key, unchanged (D1),
  and `select_account`'s existing Claude arm (`account_registry.rs:467-482`)
  keeps delegating to `select::select_token` — it just now delegates to a
  provider-filtered selector.
- The two **storage backends** stay separate: `.loom/tokens/` +
  `index.json` for Claude, `codex_profile_root()` + `.loom/accounts.json` for
  Codex. They are genuinely different credential shapes (a secret string in a
  file vs. a profile directory the CLI owns), and the `CredentialKind` enum
  (`account_registry.rs:30-35`) already models that difference correctly.

The end state is one identity type, one provider vocabulary, one health store,
and N credential backends behind `ProviderAdapter` — not one storage
representation for all providers.

## 5. `index.json` v3 schema

```jsonc
{
  "version": 3,
  "generated_at": "2026-08-07T12:00:00Z",
  "accounts": [
    {
      "env_index": 1,
      "name": "rjwalters-gmail",              // selection key (with provider)
      "provider": "claude",                   // NEW
      "upstream_id": "monitor:0f3c…",         // NEW (namespaced, D2)
      "email": "…",                           // display / legacy join only
      "file": "rjwalters-gmail.token",
      "source": "monitor-db",
      "materialized": true,                   // NEW (D4)
      "key_fingerprint": "a1b2c3d4"           // omitted when !materialized
    },
    {
      "env_index": 2,
      "name": "rjwalters-gmail-openai",
      "provider": "openai",
      "upstream_id": "monitor:8891",
      "email": "…",
      "file": null,
      "source": "monitor-db",
      "materialized": false
    }
  ]
}
```

Compatibility: `version: 2` remains readable with the D3 backfill; the `drift` /
`env_fingerprint` pair (`bootstrap.rs:448-463`) is unchanged; no secret material
is added — `upstream_id` is an account identifier, not a credential.

## 6. Acceptance-criteria map

| #5605 acceptance criterion | Answered in |
|---|---|
| Where `provider` is recorded at the storage layer, round-tripping into `AccountId` | D3, D9, §5 |
| How `account_id` is carried import → storage → selection | D1, D2, §5 |
| `tokens select --provider` surface + `runtimes.roles` / `LOOM_RUNTIME_<ROLE>` binding | D8 (note: the flag already exists as of `a354d10a`) |
| `tokens check` provider-dispatched; non-Anthropic never `exhausted` | D6 (both the probe path **and** the monitor-join path from §3) |
| Explicit stance on unifying with `account_registry.rs` | D9 |
| Follow-up implementation issue(s) filed | §9 |
| #5604 not blocked on this | §7 |

## 7. Composition with #5604

#5604 ships first and independently. Its behavior (skip non-Anthropic rows,
warn, validate `sk-ant-` shape, deterministic provider preference) is the
degenerate case of D4 minus the recording. Phase 1 below keeps every one of its
acceptance criteria satisfied and upgrades "skip" to "record, don't
materialize". Neither issue blocks the other; if #5604 lands first, Phase 1 is a
smaller diff, and if it does not, Phase 1 subsumes it.

## 8. Rejected alternatives

- **Per-provider subdirectories in `.loom/tokens/`** — forces a provider
  dimension into four flat, shell-consumed state formats to serve no present
  consumer. See D4.
- **Fold `account_id` into `AccountId`** — migrates `account-health.json`, churns
  every operator-facing handle and `.ranking` row, for a key that is only needed
  at import-dedup time. See D1.
- **A separate `runtimes.roles` → provider config map** — a fourth precedence
  chain operators must keep consistent with the runtime binding; the mismatch
  failure mode is already documented (#5001). See D8.
- **Keep filtering to Anthropic permanently** (i.e. declare #5604 the end state)
  — leaves `runtimes.roles.judge = "codex"` unable to reach a credential the
  pool can see, and leaves the §3 `ranking.json` join bug unfixed.
- **A `tokens migrate` command for index.json v2 → v3** — unnecessary: the
  backfill rule in D3 makes v2 readable, and the next import rewrites the file.

## 9. Decomposition into implementation issues

Three Builder-sized PRs, in dependency order. Phase 1 is the only one that
touches the write path; 2 and 3 are independent of each other once 1 lands.

**Phase 1 — storage layer records `(provider, upstream_id)`**
`bootstrap.rs`, `monitor_db.rs`. `Account` + `ManifestRow` fields, `INDEX_VERSION`
3 with v2 backfill, SQL selects the provider (+ id) column, dedup on
`(provider, upstream_id)`, non-claude rows recorded unmaterialized with a
warning, the D5 name rule, `validate_credential_shape` in `materialize_accounts`,
and correcting the stale Python-conformance comment at `bootstrap.rs:33-41`.
Tests must include a provider-mixed `usage.db` fixture — the current
`seed_usage_db` has no provider column.

**Phase 2 — provider-dispatched probing and the monitor join**
`check.rs`, `monitor.rs`. `ProviderAdapter::probe` dispatch, `unsupported`
status (never `exhausted`), `unsupported` excluded from `.ranking` but present
in `--json`/table, `status_rank` entry, `load_index_email_map` scoped to claude
rows, `upstream_id`-preferring join. Regression test for §3: an `openai`
`ranking.json` row sharing an email with an Anthropic account must not change
that account's reported status.

**Phase 3 — selection and runtime→provider plumbing**
`account_registry.rs`, `select.rs`, `cli/tokens.rs`, `defaults/runtimes/*.json`,
`spawn-claude.sh`, `spawn-codex.sh`. `AccountProvider: FromStr + Display`,
`--provider` parsed through it, selector skips non-claude manifest rows
(fail-open when no row), `accountProvider` in runtime manifests defaulting to
`claude`, spawn adapters passing it through, `LOOM_ACCOUNT_UPSTREAM_ID` in the
identity env.

Documentation (`.loom/docs/token-pool.md` / `defaults/docs/token-pool.md`) is
updated **within** each phase, not as a fourth issue.
