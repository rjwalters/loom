# `defaults/forge/` — the versioned forge operation inventory

The data behind Loom's forge **operation accounting** (issue #9777, phase 1 of
epic #9769 "hosted-first Gitea qualification"). Epic #9769 enumerated the
GitHub surface Loom consumes as prose tables. Prose cannot fail a build, so it
cannot answer the question the epic's GO/NO-GO gate (#9792) asks: *which*
operations must a replacement forge satisfy, who owns each one, what proves it,
and what is still unknown. This directory is that enumeration as data.

| File | Contents |
|---|---|
| `manifest.toml` | Header: schema version, source, verified base SHA, provider version floors, the owner-area table |
| `operations/*.toml` | One `[[operation]]` row per operation, grouped by qualification profile |
| `call-bypass-baseline.toml` | Generated debt ledger: every file still making a direct forge call without being a declared caller |

Field semantics live in `loom-daemon/src/forge_inventory/model.rs` — the
`include_str!` consumer of these files, so the schema and its documentation
cannot drift apart. Nothing here makes a forge call; the whole family runs with
no credential and no network.

## Commands

```bash
loom-daemon forge-inventory validate                 # is the manifest well-formed and honest?
loom-daemon forge-inventory validate --qualification # could this evidence support a GO? (#9792)
loom-daemon forge-inventory gate                     # any new unclassified direct forge call?
loom-daemon forge-inventory report                   # four-axis coverage view
loom-daemon forge-inventory probe-manifest --profile required-coordination
loom-daemon forge-inventory observed                 # host call sink vs inventory, both directions (#9831)
loom-daemon forge-inventory workflow-deps            # what workflows fetch from GitHub at run time (#9790)
```

`validate` and `gate` also run as Rust tests
(`loom-daemon/src/forge_inventory/tests/mod.rs`) against this checkout, which is
what makes them a pre-merge gate rather than a command somebody has to remember.

## Two rule sets, because "unknown" is phase 1's correct answer

`validate` asks whether the manifest is **well-formed and honest**.
`validate --qualification` asks whether the recorded evidence is **complete
enough to pass a provider**. They must stay separate: phase 1's job is to record
what is *not* yet established, so a single gate failing on
`platform_support = "unknown"` would force every row to assert support no probe
has produced — recreating the unverified prose #9769 set out to replace.

So an unknown must be **acknowledged** (the row says in `unknowns` what is
unestablished) to pass `validate`, and `--qualification` is the gate that
refuses to count an acknowledged unknown, a reserved-but-unimplemented test, or
probe-only evidence as a pass. **Do not wire `--qualification` into CI.**

## Four axes, never collapsed

`platform_support` (does the provider expose it?), `adapter_coverage` (has Loom
implemented a normalized operation?), `caller_integration` (does the production
caller route through it?) and `unknowns` are independent facts. A hosted probe
calling a provider through a thin driver establishes the *platform*, not Loom —
so `report` prints them in separate columns and `probe-manifest` records, per
entry, what a pass would **not** establish. Collapsing them is the single most
expensive misreading available at the decision gate.

## Adding or changing an operation

1. Add an `[[operation]]` row to the right `operations/*.toml`. Required-profile
   rows need a `test_id`; coverage fields default to the pessimistic value, so an
   unfilled row can never read as a pass.
2. Declare each caller under `[[operation.callers]]` with a repo-relative `path`
   that exists — a declared path is what the change gate treats as *classified*,
   and a stale one silently turns a migrated file back into a bypass.
3. Run `loom-daemon forge-inventory validate`. Operation IDs are stable once
   published: reports, probes and tests key on them, so **rename nothing**.
4. Declining a row in a non-waivable group (claims, identity/trust, reviews, CI,
   protection, guarded merge) requires `requirement_carried_by` naming the
   `required` row(s) that still carry the requirement. Profile reduction may
   decline a convenience operation; it may not *quietly* waive a requirement.

## The change gate and its baseline

`gate` scans every tracked `.sh`, `.rs` and workflow file for direct
`gh <noun>` / `gh api` invocations. Each such file must be a declared caller of
some operation, or be listed in `call-bypass-baseline.toml` with an owner area
and a removal issue. Deliberately **not** counted: installed `.loom/` mirrors
(measured at their `defaults/` source), tests and fixtures, `guard-*.sh`
prohibitions (a guard that recognises `gh pr merge` in order to deny it is the
opposite of a caller), and role prompts (instructions, not call sites — they are
inventoried as `kind = "role-prompt"` callers instead).

The baseline is a one-way ratchet, like `scripts/check-file-size-budget.sh`:
counts may only go down and entries may only be removed. Regenerate it with

```bash
loom-daemon forge-inventory gate --update --removal-issue <N>
```

Eliminating the baselined entries is the caller-migration issue's job (#9799),
not this ledger's. Scanning is lexical on purpose: a gate that had to resolve a
shell variable could not answer for exactly the dynamic command construction
#9777 asks to be counted, and an over-counting scan costs one baseline entry
where an under-counting clever one costs an invisible bypass.

## Workflow delivery dependencies and the qualification fixture (#9790)

The operation rows count *forge API coordination*. `workflow-deps` lists the
other half separately: what tracked `.github/workflows/` and `.gitea/workflows/`
files fetch from GitHub **while they run** — `uses:` action sources, GHCR
images, GitHub Releases / raw-content downloads — each on its own plane with a
per-kind integration estimate, plus the `gh`/`api.github.com` lines tagged
`forge-api` so the two lists can be told apart. It is a lexical scan
(`static_only: true`): what an action downloads internally, or what a runner
image bakes in, only a live run can show, so a clean scan is never a
network-independence claim. `--file <path>` scans named files instead.

`qualification/ci-fixture/` is the bounded real build/test/artifact fixture a
gitea-1 qualification run copies into a disposable repository, with its
`.gitea/workflows/qual-ci.yml`. A Rust test
(`loom-daemon/src/forge_inventory/tests/qualification_fixture.rs`) compiles and
tests it, breaks it, and asserts the result changes — so the fixture cannot
silently become a green stub. Live results, dispositions and the runbook are in
`docs/research/gitea-1-ci-delivery-qualification.md`.

## Evidence constraints

Runtime accounting (`loom-daemon/src/forge_call_stats.rs`) records an operation
ID, provider, origin host and `owner/repo` slug — bounded, sanitized, one line,
length-capped, and never an authentication header or a response body. The origin
is a separate field from the repo because two forges can carry the same
`owner/repo` slug, and a row that merged them would attribute one provider's
spend to the other on a mixed fleet. An operation the manifest does not know is
recorded as `unknown` rather than dropped, so an unmapped caller is visible.
Runtime traces **supplement** this source inventory; they can never establish
exhaustiveness on their own.

A call site names its operation with a typed constant from
`loom-daemon/src/forge_call_stats_ops.rs` (checked against this manifest by a
test), or with `ForgeOp::uninventoried("<why>")` when no row fits — a deliberate
`unknown` carries its reason at the call site. `forge-inventory observed` sets
the sink's operation IDs beside this inventory: *inventoried but never
observed* (rare, script-only, or not yet migrated) and *observed but not
inventoried* (a missing row or a typo), plus the callers still recording
`unknown`. A clean diff is not a completeness claim.
