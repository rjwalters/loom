# Curator: GraphQL/REST Budget (#10039)

Why `curator.md`'s per-issue recipes are REST, and what is still unmeasured.

## Why

`gh issue view/edit/close` are GraphQL (`edit --add-label` also pages the label
list). #10039 reports ~8-10 GraphQL requests per curated issue, and 8 parallel
Curators on one identity draining its pool for the hour (supplied, not
re-measured). `rate_limit`'s core figure cannot see GraphQL run out, hence the
two-pool gate. The ~500 floor and ~3-Curator cap are policy defaults, not
vendor limits; nothing enforces the cap across repos.

## Static per-pass GraphQL cost (from the recipes, not measured)

Per claimed issue: 1 `rateLimit` read; a `loom:blocked` re-check adds
`closedByPullRequestsReferences` and a `gh pr view` per linked PR. Per pass:
the list query. `gh-cached` uses
`gh` (GraphQL) only when `loom-daemon` declines a read; helper scripts'
internal calls are not counted.

## Measurement (close-blocking on #10039, not done)

Same identity, concurrent activity recorded: read `used` from both pools,
curate 15 issues, read again; repeat on a comparable batch with the old
prompt. Report deltas, commands and timestamps; if background activity cannot
be excluded, say so instead of claiming a precise saving.
