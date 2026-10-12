# ADR-0024: Dedupe Read-Only Forge Pollers Across Hosts With One Captain-Published Snapshot per Repo

## Status

Proposed. Design note for the cross-host slice of #10512 (direction 3 of
#9243). No code ships with this ADR; it fixes the shape so the implementation
slices can be filed and reviewed independently.

## Context

Since about 07Z on 2026-10-06 daemon reads route to reader Apps. Reader App
5100879 used about 12.8k of its 15k core budget in one hour and App 5101048
about 9.5k (`github.ratelimit.used`, host label `loom-worker-1` / `-2`).
Three hosts (two workers and robb-studio) poll the same repos independently.

Billable REST calls (`invoke github`, `outcome=ok`), last hour, summed over the
three hosts, as reported in #10512:

| op | calls/h | status after slice 1 (PR #10530) |
|---|---|---|
| `star_liveness` | 2,267 | already conditional per host; 200s are real changes read identically by every host |
| `stage_dwell` | 1,953 | same |
| `visibility.repo` | 1,420 | fixed in-host (ETag store + long TTL) |
| `worktree.issue_state_rest` | 969 | fixed in-host (ETag store) |
| `work_finder` | 968 | already conditional per host; same duplication |
| `quarantine.issue_comments` | 812 | not a poller, see below |

Slice 1 removed the per-host waste that a local cache can remove. What remains
in the four read-only pollers (`star_liveness`, `stage_dwell`, `work_finder`,
and, nominally, quarantine) is **duplication across hosts**: each host's
conditional read still gets a `200` whenever the shared forge state changed,
and N hosts each pay for the same `200`.

### `quarantine.issue_comments` has no cheap in-host fix

The 406/h per worker figure is the startup reconciliation pass
(`daemon_startup_reconciliation.rs` -> `quarantine_reconciliation::reconcile_workspace`
-> `trusted_quarantine_comments`, which runs `--paginate` over issue comments),
not a periodic poller. The periodic pass uses a different function. The count is
identical on both workers because it is a per-restart, per-workspace-root scan,
not because of a loop. A correct fix replaces `--paginate` with page-by-page
conditional reads and persists the `QUARANTINE_SCAN` version key across
restarts; that is not a small change and is worth at most about 406 calls per
worker per restart. It is deferred to a follow-up and excluded from the
publisher design (a snapshot of per-issue comments is the wrong granularity).

## Decision

Designate **one publisher per repo** for the read-only listing pollers, and let
every other host consume a snapshot instead of calling the forge.

1. **Publisher selection.** The publisher is the host named by
   `fleet.captain` in the tracked `.loom/config.json`
   (`loom-daemon/src/fleet_captain.rs`, #8848): assigned, not elected, fail
   closed. Reusing it means no new election machinery and the existing
   "never armed on two hosts" guarantee. A repo-level override is out of scope.
   The read mirror in 2AMLogic/loom-ui (loom-ui#1756) is an acceptable
   alternative publisher; the consumer contract below is the same either way.
2. **Snapshot content.** Per repo, the publisher writes the already-computed
   result of its conditional listings: the starred set (`star_liveness`),
   the per-stage label listings (`stage_dwell`'s `STAGE_LABELS`), and the
   ready-queue listings (`work_finder`). Each entry carries the ETag/body the
   publisher holds, `fetched_at`, the publisher host id, the Loom version and
   a schema version. The snapshot is **read-only input**: it carries state
   only, never claims, leases or write intents.
3. **Consumer rule.** A non-captain host reads the snapshot instead of the
   forge when it is fresh (`now - fetched_at <= snapshot_max_age`, default
   2x the poll interval). A missing, stale, malformed, wrong-version or
   wrong-repo snapshot falls back to the host's existing per-host conditional
   read. This is the same posture as ADR-0021: the optimisation is additive
   over an unchanged polling floor, so a dead publisher costs calls, never
   correctness.
4. **Gate reads stay local.** Anything that gates a write or a dispatch
   decision (`guard.issue_state`, claim and lease checks, `check-claim`) keeps
   reading the forge directly. Only observability and discovery pollers
   consume snapshots, because a stale snapshot there delays at most one tick.
5. **Transport.** Out of scope for this ADR beyond the contract: candidates
   are a file under the shared state dir if hosts share storage, or the
   loom-ui mirror over HTTPS. The transport is chosen in the first
   implementation slice; the contract in 2-3 does not depend on it.

## Expected fleet total

Using the #10512 measurement as the baseline and three hosts:

| op | before (calls/h) | after (calls/h) | basis |
|---|---|---|---|
| `visibility.repo` | 1,420 | <= 45 | slice 1; curator estimate, acceptance bound is <= 50 |
| `worktree.issue_state_rest` | 969 | <= 100 | slice 1; curator estimate |
| `star_liveness` | 2,267 | about 760 | publisher only, divide by 3 |
| `stage_dwell` | 1,953 | about 650 | publisher only, divide by 3 |
| `work_finder` | 968 | about 320 | publisher only, divide by 3 |
| `quarantine.issue_comments` | 812 | 812 | unchanged, follow-up |
| **Total** | **8,389** | **about 2,690** | |

The divide-by-3 figures are an upper-bound estimate assuming the three hosts
poll equally; robb-studio has fewer roots (visibility showed 273 versus
573/574 on the workers), so the real saving on these ops is likely a little
smaller than a flat two thirds. These are projections, not measurements.

## Consequences

### Positive

- Reader-App core budget on the read-only pollers falls by roughly two thirds
  as the fleet grows, and no longer scales with host count.
- Reuses the existing captain designation and its fail-closed semantics.
- Degrades to today's behaviour when the snapshot is unavailable.

### Negative

- Non-captain hosts see forge changes up to `snapshot_max_age` later.
- The captain host becomes a soft dependency for freshness (not for
  correctness); captain failover remains the #8902 alert-only story.
- A new cross-host data contract (schema versioning, trust of peer-written
  data) that must be treated as untrusted input by consumers.

## Alternatives Considered

- **Per-host caches only.** Done in slice 1; cannot remove the N-times
  duplication of a real `200`.
- **Elect the publisher per repo by lease.** More failover, more forge calls
  for the lease, and the captain precedent explicitly rejected election.
- **More reader Apps.** Raises the ceiling but not the efficiency; consumption
  still scales with host count. Tracked separately as direction 4 of #9243.
- **Stretch poll cadences.** Covered by ADR-0021's amendment; complementary.

## Verification (not yet performed)

The before/after was not re-measured from the builder environment, which has no
SigNoz access. Re-measure with the same query as #10089 / #10512: billable
`invoke github` spans with `outcome=ok`, grouped by `op`, summed over hosts
`loom-worker-1`, `loom-worker-2` and `robb-studio`, for a one-hour window before
and after the slice-1 rollout, plus `github.ratelimit.used` for apps 5100879
and 5101048. Locally, `loom-daemon forge calls --by op` shows the
`not_modified` column (the 304s) per host until #10343 adds `github.http.*`
status attributes. Expected after slice 1: `visibility.repo` <= 50/h
fleet-wide and `worktree.issue_state_rest` <= about 100/h.

## References

- Related GitHub Issues: #10512, #9243, #10343, #10271, #8848, #8902
- Related ADRs: ADR-0014, ADR-0021
- PR #10530 (slice 1)
