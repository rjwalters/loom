# Story points: verification note (Issue #9430)

Honest record of what was and was not run. **No numbers in this phase were
extracted live by the Builder.**

## Run live: nothing against data

- `wrangler d1 execute loom-fleet-telemetry --remote` from the Builder
  environment failed: wrangler is installed but non-interactive runs need
  `CLOUDFLARE_API_TOKEN`, which is not provisioned for this session. No
  credentials were sought elsewhere.
- Live SigNoz was not queried: per the issue body it holds ~7 days and 5
  `sweep.outcome` records, which cannot support this analysis.
- `defaults/observability/story-points-queries.sql` (SP1..SP4) was therefore
  **not executed against D1 or SigNoz**. It has not been run against any
  data. It was only syntax-checked with local `sqlite3` against an empty
  `sweep_facts` + `landed-size.sql` (parses, returns the all-NULL/zero rows).
  It reuses column names from `sweep-facts-rollup.sql` and the landing
  predicate of `issue-effort.sql`.

## Where the figures in `story-points.md` come from

Every number (329 landings, 295 with tokens, 284 relaxed-clean, 119 strict,
the 26,260-record window from 2026-08-15, alpha ~0.83, the ~1.5x
Sonnet 5 / Opus 5 gap, the lines^0.25 scaling, the per-class medians, the
<0.04 wording result on paired n=240) is **quoted from the #9430 issue body**
(curator-consolidated on 2026-09-29, tuning set, #9466 for the size
definition). They are cited, not reproduced; the exclusion split of the 45
non-relaxed landings (judge count vs doctor phase) is not in the body and is
unknown here.

## To complete (follow-up for whoever holds D1 access)

1. Run `sweep-facts-rollup.sql`, then `story-points-queries.sql` with the
   D1 token; paste SP1 counts here to replace the quoted ones.
2. Fit `landed-size.sql` params from SP2/SP3, re-run SP4, and compare its
   class medians against the seed table. If a bound moves, update
   `defaults/docs/story-points.md` and cite the run date.
