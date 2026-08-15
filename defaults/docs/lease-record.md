# Lease Record Format (Epic #6165, Phase 1: #6179)

Epic #6165 gives the `loom:building` claim a liveness dimension — a
**lease**. `loom:building` on its own only says "someone claimed this issue
at some point"; it carries no signal about whether that someone is still
alive and working, or crashed/hung hours ago. The lease record is the
missing liveness signal, layered on top of the existing label claim without
changing what the label itself means.

This document defines the record's on-forge shape. It is **write-only**:
this phase (#6179) writes the record at dispatch time and nothing else. No
reclamation or dispatch-decision logic reads it back yet — that is Phase 2
of the epic, a future issue. Phase 3 (fencing) is the phase after that. Both
are expected to consume this exact format without re-deriving it.

The sibling issue #6180 (`defaults/docs/lease-renewal.md`) implements the
other half: a sweep-owned background loop that keeps a lease fresh for the
lifetime of the sweep holding the claim, reusing the identical marker shape
documented here.

## What a lease record is

A lease record is an ordinary issue (or PR) **comment**, posted on the
number a dispatch just claimed, at the moment `loom-daemon`'s dispatch path
successfully flips that issue's label from `loom:issue` to `loom:building`.
It follows the same HTML-comment-marker idiom already used elsewhere in this
repo — `<!-- loom:standdown claim=… -->` (peer-claim standdown),
`<!-- champion:hold-state head=… -->` (Champion's merge-risk hold) — so it
is grep/dedup-detectable the same way those markers are, without needing a
dedicated forge field.

### Shape

The comment body's literal **first line** is the marker:

```
<!-- loom:lease host=<hostname> sweep=<sweep-id> -->
```

- `<hostname>` — this host's identity, exactly as
  `loom-daemon`'s `sweep_registry::host_identity()` resolves it
  (`LOOM_HOST_ID` env > `$HOSTNAME` > the `hostname` binary >
  `unknown-host`). The same value already used for peer-claim
  advertisements (#4028) and cross-host collision records (#4085), so a
  lease's host identity is directly comparable against those.
- `<sweep-id>` — the dispatching sweep's own `SweepId`
  (`generate_sweep_id`'s output), the same identifier the daemon's registry,
  logs, and outcome journal already key sweeps by.

Everything **after** the marker's closing `-->` is free-form, human-readable
prose (who claimed it, when, and pointers to this doc and the renewal doc).
Machine readers — present and future — must locate the record via
`.starts_with("<!-- loom:lease host=")` only, and must **never** parse or
depend on anything in the prose that follows.

### The liveness signal is the comment's `updated_at`, not embedded text

This is the load-bearing design decision, so it is worth stating plainly:
**a reader determines freshness from the comment's own forge-assigned
`updated_at` timestamp — never from a timestamp written into the marker or
prose text.**

This differs deliberately from `peer_claims.rs`'s existing TTL approach,
which timestamps a claim at local receipt and corrects for clock skew
between hosts because there is no shared clock in that channel. A forge
comment does not have that problem: every host reads the *same* `updated_at`
value, assigned by the forge server itself, for the same comment. Using it
as the sole liveness signal gives every host a shared clock for free, with
no skew-correction logic needed — a reader (Phase 2) just compares
"now minus this comment's `updated_at`" against a threshold.

This is also why the marker's first line is written once and never rewritten
byte-for-byte identical on renewal — see `lease-renewal.md` for why an
idempotent PATCH still needs to change *something* in the body for a forge
to reliably advance `updated_at`.

### Example

```
<!-- loom:lease host=studio-host sweep=sweep-2026-08-13T23-01-04Z-a1b2c3 -->
This issue's `loom:building` claim was acquired by sweep
`sweep-2026-08-13T23-01-04Z-a1b2c3` on host `studio-host` at
2026-08-13T23:01:04Z. This comment is a lease record (Issue #6179, Epic
#6165) — its liveness signal is this comment's own forge-assigned
`updated_at`, never a timestamp embedded in this text. See
`defaults/docs/lease-record.md` for the format contract this establishes,
and `defaults/docs/lease-renewal.md` for how the owning sweep keeps it
fresh for the lifetime of its claim. Nothing reads this record yet
(write-only, Phase 1) — a future phase will use it to decide reclamation
of an abandoned claim.
```

The embedded `at=...` timestamp in that prose is for human debugging only —
it is what the dispatcher *believed* the time was when it wrote the comment,
not an authoritative value any reader may rely on.

## When it is written

`loom-daemon`'s `SweepRegistry::dispatch_inner` (in
`loom-daemon/src/sweep_registry/dispatch.rs`) writes the lease record
immediately after a **confirmed successful** `flip_label_to_building` call —
never before, and never when the flip itself failed or was skipped (e.g.
`skip_label_flip` test fixtures). No claim, no lease: a lease record only
ever exists for an issue this host actually just flipped to
`loom:building`.

The write itself (`SweepRegistry::write_lease_comment` in
`loom-daemon/src/sweep_registry/guards.rs`) is **best-effort and fail-open**,
matching every other forge mutation on the dispatch path (`gh` calls
throughout `guards.rs`/`watchdog.rs`): a failed or timed-out `gh issue
comment` only logs a warning and never fails, retries, or unwinds the
dispatch. The claim (`loom:building`) is authoritative regardless of whether
its lease record made it onto the forge — a lost lease comment degrades a
future reclamation decision's evidence, not the claim's own validity.

## What this phase explicitly does not do

- **No reading.** Nothing in `loom-daemon`'s reclamation or dispatch-decision
  path parses, locates, or reasons about lease comments in this phase. This
  is a pure addition with zero behavior change to any existing decision.
- **No renewal from the daemon.** The daemon writes exactly one lease
  comment per successful dispatch and never touches it again. Keeping a
  lease fresh for the sweep's entire runtime is the sweep-owned renewal loop
  documented separately in `lease-renewal.md` (#6180) — the daemon process
  that dispatched a sweep routinely does not outlive it (#6129), so daemon-
  owned renewal would be the wrong owner.
- **No reclamation or fencing logic.** Deciding what to do with a lease that
  has gone stale (Phase 2) and bounding the cost of the underlying
  acquisition race #4028 describes (Phase 3) are both out of scope here.

## For Phase 2 (reclamation) and Phase 3 (fencing)

A reader should:

1. Locate the most recent comment on a `loom:building` issue whose body
   starts with `<!-- loom:lease host=`.
2. Parse `host=` and `sweep=` out of that first line only (a simple prefix
   strip + space-split is sufficient — the format is intentionally flat,
   not a general key-value grammar).
3. Use the comment's own `updated_at` (not any embedded timestamp) as the
   freshness signal, compared against whatever staleness threshold that
   phase defines.
4. Treat an issue with `loom:building` but **no** lease comment as a claim
   predating this feature (or one whose lease write failed) — not evidence
   of anything either way; Phase 2 must define its own fallback for that
   case rather than assuming absence means abandonment.

**Phase 2 (Issue #6286) has now shipped this contract.**
`loom-daemon`'s `claim_reconciliation::forge::fetch_freshest_lease_updated_at`
(the periodic/startup reconciliation pass,
`reconcile_workspace_with_coordination`) and
`worktree_ops::gh::freshest_lease_updated_at` (the `recover-orphans` CLI's
`check_untracked_building`) both implement exactly the four steps above —
locate via `LEASE_MARKER_PREFIX`, freshness from the REST comments
endpoint's `updated_at` only, TTL = 3x the ~5-minute renewal interval (15
minutes, `claim_reconciliation::resolve_lease_ttl_minutes`), and a missing
lease comment falls through to whatever the pre-existing host-scoped
evidence (journal / run-registry / label-age) already decided. Both call
sites consult the lease as the LAST gate, immediately before a reclaim would
otherwise fire — see `claim_reconciliation.rs`'s "Lease-record freshness"
section and its top-of-file doc comment for the full before/after picture.

See also: [`lease-renewal.md`](lease-renewal.md) for the renewal mechanism
this format was co-designed with, and
[`lease-renewal-measurement.md`](lease-renewal-measurement.md) for the
write-volume measurement methodology and a projected (not yet measured)
estimate against this design's rate-limit headroom (#6181).

## Phase 2, dispatch-time half: claim-then-verify-order (#6287)

Issue #6287 implements one half of Phase 2 — the operator-directed
claim-then-verify-order dedup at dispatch time (2026-08-15), landed
alongside the reclamation-guard half (#6286). It follows this doc's own
reader recipe above with one refinement: rather than locating only the
*most recent* lease comment, `SweepRegistry::read_lease_comments`
(`loom-daemon/src/sweep_registry/guards.rs`) reads back **every** live
lease comment on the issue via `gh api .../issues/N/comments`, and
`SweepRegistry::resolve_lease_order` compares their forge-assigned comment
`id`s (never a locally-recorded timestamp) to decide whether *this*
dispatcher's own comment is the earliest. A dispatcher that loses — a peer's
lease comment has an earlier `id` — yields before spawning a builder or
touching a worktree: it retracts its own peer-claim advertisement, releases
its own claim lock, and posts a `<!-- loom:lease-yield ... -->` standdown
annotation, but deliberately leaves the shared `loom:building` label alone
(it is already correct — idempotent across both racing flips, and reverting
it would destroy the winning claimant's only cross-host mutex out from under
its still-live sweep). The comparison is bounded to comments created within
a short lookback window of the dispatch attempt's own pre-flip instant
(`LEASE_ORDER_LOOKBACK_SECS`), so a long-completed prior claim's lease
comment — an issue accumulates one per dispatch over its whole lifetime,
never deleted — can never out-rank a normal, uncontested re-dispatch.

## Phase 3 (Issue #6309) has now shipped: sweep-side fencing before push/PR-open

Phase 2 (above) is the *daemon's* reclamation-side check; Phase 3 is the
*sweep's own*, symmetric check — fencing, not reclamation. The sweep checks
its own lease, never the daemon, for the identical reason Phase 1's renewal
loop is sweep-owned: role agents routinely outlive the daemon that spawned
them (#6129), so only the sweep itself, at the moment of action, can know
whether it is still the intended owner.

`defaults/scripts/sweep-lease-fence.sh check <issue>` implements this doc's
reader recipe from a shell/orchestration context (rather than
`loom-daemon`'s Rust): it fetches every lease-marker comment on `<issue>` via
the REST comments endpoint (NDJSON output across `--paginate` pages, the same
#4637 workaround `SweepRegistry::read_lease_comments` uses), locally picks
the one with the freshest `updated_at`, and confirms BOTH (a) that comment is
still within `ttl_minutes` of now (default 15, same TTL Phase 2 uses) and (b)
its `host=` field still names this sweep's own host. It is wired into the
Builder phase immediately before `git push` + opening the PR
(`defaults/roles/builder-pr.md` § "Lease Fencing: Confirm You Still Own the
Claim") — on either failure (expired, exit `3`; superseded by a different
host, exit `4`) the Builder aborts before doing anything externally-visible,
without touching the `loom:building` label or contesting the peer's claim.
Absence of a matching lease comment, a malformed marker, or a `gh` fetch
failure all fail OPEN (exit `0`, proceed) — this doc's own "no lease comment
== no evidence either way" contract, applied identically to this new reader.
