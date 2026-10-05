# Ops Lane (`loom:ops`, Issue #10357)

Some approved work has no diff. Its deliverable is **forge state**: labels,
comments, closes and body rewrites across many issues (the motivating case is
#10009). Builder -> PR -> Judge -> merge cannot finish it, so a Builder dispatch
loops PR-less and the item lands on the operator. `loom:ops` is the marker for
that work kind.

This is the canonical definition. The first slice (#10357) ships the marker,
the skip, and the rules below. The executor that actually runs ops issues is
deferred to follow-ups, so today an ops issue is parked from Builder, not run.

## Marker

`loom:ops` is a registry label (`defaults/labels.json`, `skip: true`,
`park: false`). Skip means the work finder never dispatches an ops issue to
Builder, so the PR-less retry hold is never reached. It is not a park, so an
ops executor can still reach the item.

## Curator criteria

Apply `loom:ops` only when BOTH hold:

1. `## Affected Files` is "none", and
2. the deliverable is forge state (labels, comments, closes, body rewrites).

Curator then applies `loom:curated` as usual. The body MUST carry a
`## Verification` section: a command or enumeration that can be re-run on live
state and that passes only when the work is done.

## Ledger comment

Each pass posts one ledger comment on the tracker issue:

```
<!-- loom:ops-ledger pass=N -->
Changed: ...
Skipped: ... (already satisfied, idempotent)
Inaccessible: ... (repo/permission failures, recorded as unverified)
```

Reruns resume from the highest `pass=N` marker and must not repeat a mutation
or a verdict already recorded.

## Lane guardrails

- Mutation cap per pass; the next pass resumes from the ledger markers.
- Repository-qualified calls only.
- Dry-run ledger first: before the first mutating pass of each repo or batch,
  post a ledger that lists the intended mutations without making them.
- Authorization is the issue's approval (`loom:issue`, or a star). A blast-radius
  ack, if later added, is a single comment, never a re-park to `loom:operator`.

## Champion close rule

Champion closes an ops issue when its `## Verification` passes on live state,
and comments the evidence. It does not wait for a merged PR. Partial coverage
(an inaccessible repo, a failed write) is recorded as unverified and the issue
stays open.
