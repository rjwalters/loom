# Ratification reviewer lane

The daemon-side lane that gives a ratification PR two independent key reviews
(EE and market) and records each as a marker comment. Part of #13186; the
Champion hold that consumes the result is in [`ratification-hold.md`](ratification-hold.md).

Code: `loom-daemon/src/role_runner/ratification_lane.rs` (selection, markers,
tick) and `ratification_dispatch.rs` (the two sessions).

## Config

`ratificationLane` in `.loom/config.json`. Off by default; a missing,
malformed or half-configured block means the lane never runs.

| Key | Meaning |
|-----|---------|
| `enabled` | `true` to run the lane (anything else is off) |
| `ee`, `market` | each `{repo, path, commit}`: the instruction file for that key, pinned to a full 40-hex commit SHA. Both are required |

### Enabling it fleet-wide

Set the block once in the fleet-tier config (a repo's own `.loom/config.json`
overrides it; see [`fleet-config-lifecycle.md`](fleet-config-lifecycle.md)):

```json
{
  "ratificationLane": {
    "enabled": true,
    "ee":     {"repo": "<owner>/<instructions-repo>", "path": "<path-to-ee.md>",     "commit": "<40-hex SHA>"},
    "market": {"repo": "<owner>/<instructions-repo>", "path": "<path-to-market.md>", "commit": "<40-hex SHA>"}
  }
}
```

The `{repo, path, commit}` pins for `ee` and `market` are operator-owned values;
Loom ships no defaults for them, so the lane stays off until the operator sets
them. Selection also honors `champion.ratificationPaths` / `ratificationPhrases`.

## Flow

1. The Champion role tick calls the lane for the root when `enabled`.
2. Selection finds open PRs that Champion's ratification hold would hold: the
   same predicate (`champion_hold::ratification::scan` returns `Hold`), so every
   spec/decision file declaring ratification-by-merge counts, not only status
   flips. A candidate file that cannot be read is not selected (logged). A PR
   whose current head already carries both markers is skipped. The head SHA is captured here.
3. Only the keys still missing a current-head marker are dispatched (#14797),
   `ratification-ee` and `ratification-market`, in that order when both are
   missing. Each prompt is its pinned instructions plus the PR reference and
   nothing else.
4. For each attempted key whose session ends in success **and** whose output
   parses as that key's review (below) the daemon posts one comment. A key
   that failed closed, whose session did not succeed, or whose output carries
   no verdict posts nothing and the reason is logged; the other key is
   unaffected. A withheld key posts no marker, so that key is retried at the
   same head on a later tick.

### Missing-key retry (#14797)

A key is **complete** for a head when a trusted comment (the same trust
policy as every marker read, `.loom/docs/comment-trust.md`) carries its
marker for the PR's *current* head — whether the lane or a hand-run lane
posted it, and whatever the verdict it records (`escalate` and
`request-changes` reviews were still performed). A later tick dispatches,
reads instructions for and posts **only the incomplete keys**; a complete key
consumes no role session, no marker post and no withheld count. The head is
still the one captured at selection: a push mid-review leaves the new head
with no markers, so both keys run again there. Two completed keys do not
release the separate human ratification hold
([`ratification-hold.md`](ratification-hold.md)).

### Unreadable comment lists

The marker state lives in the PR's comment history, so the lane must be able
to tell "read, and there are no markers" from "could not read". A comment
listing that fails or does not parse is **unreadable**: the PR is skipped
before any instruction read or session runs and counts in the tick's
`unreadable` (a `degraded` tick), the same accounting as an unreadable
candidate file. An unreadable list never authorizes a rerun — treating it as
"no markers" would duplicate completed keys. A successfully read empty list
selects both keys as missing.

## Tick outcome counts

The existing `ratification_lane.tick` span reports one final outcome per
attempted key. Each entry below is a count at
`loom.ratification.<ee|market>.<outcome>`. All twelve fields are present,
including zeroes. Review content and diagnostic details stay off the span.

| Outcome | When it is counted |
|---|---|
| `posted` | The marker was posted for a valid verdict, including a negative review. |
| `pre_launch_failed` | An instruction pin failed or the runner reported a known pre-launch refusal. |
| `invocation_failed` | An invocation did not succeed, including interruption or failure with an unknown launch stage. |
| `output_missing` | The invocation succeeded without captured output. |
| `verdict_invalid` | The captured output did not parse as a review for that key. |
| `post_failed` | The review parsed and the forge did not accept the marker post. |

The two `posted` fields sum to `loom.ratification.posted`. The other
ten sum to `loom.ratification.withheld`. A trusted marker for the current
head skips that key, and an unreadable candidate counts once in
`loom.ratification.unreadable`. Neither case adds a key outcome.

### Check retained counts after rollout

After ordinary rollout, substitute the exact deployed daemon SHA and a
bounded UTC interval with retained work in the query below. It collapses
replayed spans by trace and span ID before comparing and summing the same
tick rows. Rows missing fields are counted separately from genuine zeroes.
Require nonzero `ticks`, zero `incomplete_ticks`, and zero
`unreconciled_ticks`. Use a window with nonzero posted or withheld counts
to observe actual outcomes. Check the fields remain present through the
collector route if any are missing.

```sql
WITH
  ['ee', 'market'] AS review_keys,
  ['posted', 'pre_launch_failed', 'invocation_failed', 'output_missing',
   'verdict_invalid', 'post_failed'] AS outcomes,
  arrayConcat(
    ['loom.ratification.posted', 'loom.ratification.withheld'],
    arrayFlatten(arrayMap(k -> arrayMap(o -> concat('loom.ratification.', k, '.', o),
                                        outcomes), review_keys))
  ) AS fields,
  per_tick AS (
    SELECT argMax(attributes_string, timestamp) AS attrs
    FROM signoz_traces.signoz_index_v3
    WHERE name = 'ratification_lane.tick'
      AND attributes_string['loom.daemon.revision'] = '<deployed-sha>'
      AND timestamp >= toDateTime64('<start-utc>', 9, 'UTC')
      AND timestamp < toDateTime64('<end-utc>', 9, 'UTC')
    GROUP BY trace_id, span_id
  ),
  decoded AS (
    SELECT arrayMap(f -> toUInt64OrNull(attrs[f]), fields) AS parsed,
           arrayAll(x -> isNotNull(x), parsed) AS complete,
           arrayMap(x -> ifNull(x, toUInt64(0)), parsed) AS n
    FROM per_tick
  )
SELECT count() AS ticks,
       countIf(NOT complete) AS incomplete_ticks,
       countIf(complete AND (n[1] != n[3] + n[9]
         OR n[2] != arraySum(arraySlice(n, 3)) - n[3] - n[9])) AS unreconciled_ticks,
       mapFromArrays(fields,
         arrayResize(sumForEach(arrayMap(x -> if(complete, x, toUInt64(0)), n)),
                     length(fields), toUInt64(0))) AS totals
FROM decoded;
```

## Marker format

```
<!-- ratify:key=<ee|market> sha=<40-hex head> -->
reviewer=<ratification-ee|ratification-market>
```

The first line is the wire format `keys_marked` parses; `reviewer=` is a visible
line beside it and is not part of the regex. The `sha` is the head captured at
selection, not re-read at post time: a push mid-review leaves the new head
without markers, so the PR is selected again. After the marker the comment
carries a `Verdict:` section: the extracted review text, never the session's
log. The review is delimited by the reviewer's own heading — `## EE-key
review:` or `## Market-key review:` — and runs from that heading's **last**
occurrence in the session output to the first launcher/wrapper line after it
(a timestamped `[<ISO-8601>] [INFO|WARN|ERROR]` log line or a
`[claude-wrapper]` diagnostic), so launcher or wrapper noise on either side
of the review is never read as a verdict or posted — no auth-mode line, no
wrapper tail.
A marker is posted only when the review also carries an `**Overall verdict:**`
line whose first token is one of the key's verdicts: `approve` /
`request-changes` for `ee`; `competitive` / `adequate-for-catalog` /
`uncompetitive` / `escalate` for `market`. Anything else — the reviewer
declined, authored the PR or reported a disclosure problem (all no verdict),
no heading, an unknown token, or two different tokens — posts no marker and
is logged as withheld. The text is at most 4000 characters — the head is
kept on truncation, so the heading and verdict line survive — with an
explicit notice when cut; its `<!--` and `-->` are escaped, so it cannot
form or alter a marker.

## Reading the markers: `ratification-lane status`

A held PR can carry dozens of key-marker comments at one head — daemons
older than the missing-key retry fix (#14797/#14800) re-dispatched both keys
every tick — and the markers exist in two formats. The read-only report

```
loom-daemon ratification-lane status <PR> [--repo owner/name] [--root PATH]
```

fetches the PR's current head and its complete, paginated comment history
(trusted authors only, `.loom/docs/comment-trust.md`), then prints the
reviewed head and one row per key: the **effective** marker — the review to
read first — with a link, one indented `superseded` link for every other
eligible marker at the same head, and an explicit `MISSING` line when a
complete read finds no marker for a key. An on-demand report also works for
historical PR threads where both keys are already present and the lane has
stopped posting. The command performs only reads: it never dispatches,
posts, edits, minimises or deletes anything, and it never releases a hold —
the human retains the ratification decision. If the PR head or the comment
history cannot be read (including a torn later page or a malformed record),
it reports the error and exits non-zero rather than guessing.

### Which marker is effective

- A marker is **eligible** for a head when a trusted author posted it, it
  names the PR's current head with a full 40-hex SHA, and — when the marker
  carries a `block=owner/repo#<n>` identity — that identity names exactly
  this PR. Markers for another head or another PR are neither effective nor
  superseded: they are not this head's markers.
- The **effective** marker for a key is the newest eligible one by the forge
  comment's **creation time**, with the numeric comment id as the
  tie-breaker. Both formats compete on exactly the same terms; an edit, a
  date written inside the review, or a more favourable verdict gives a
  comment no priority. Every other eligible comment for that key is listed
  as **superseded** and linked once. No review text is deleted.
- A marker inside a fenced code block or on a block-quote line is a quoted
  example, not a marker: review prose cannot establish a head or a key.

Both wire formats are recognized:

| Format | Shape | Posted by |
|--------|-------|-----------|
| Lane | `<!-- ratify:key=<ee\|market> sha=<40-hex head> -->` | the lane (`marker_body`) |
| Hand | `<!-- RATIFY-KEY: <ee\|market> verdict=… block=owner/repo#<pr> sha=<40-hex head> reviewer=… date=… -->` | a hand-run key-apply |

### A mixed-format example

Suppose a part repo's PR #98 carried both Market formats at head `6a3cd2c4…`
(#14834): the lane posted
`<!-- ratify:key=market sha=6a3cd2c43c31821fe93bbc18828c0eb6e06aa063 -->`
(created 19:00:37Z), then a hand-applied
`<!-- RATIFY-KEY: market verdict=adequate-for-catalog
block=example-org/part-repo#98 sha=6a3cd2c… -->`
followed (created 19:14:25Z). The report prints:

```
example-org/part-repo#98
Reviewed head: 6a3cd2c43c31821fe93bbc18828c0eb6e06aa063

ee: MISSING — no trusted ee marker names this head

market: effective 2026-11-20T19:14:25Z https://github.com/example-org/part-repo/pull/98#issuecomment-7048390167 (RATIFY-KEY)
  superseded 2026-11-20T19:00:37Z https://github.com/example-org/part-repo/pull/98#issuecomment-7048105117 (ratify:key)
```

The RATIFY-KEY review is the one to read first; the earlier `ratify:key`
review is still linked, still complete, just superseded.

## Trust model

No signing key, broker or per-key App. The daemon posts each marker as the fleet
identity and readers trust it under `.loom/docs/comment-trust.md`, as they do a
Judge verdict; a marker from an untrusted author is prose.

## Independence rule

The two keys are separate sessions, and neither is the session that built the
PR. Author login is not the test: in a fleet the Builder opens the PR as the
same identity that dispatches the lane. The builder's sweep id is read from the
PR's `<!-- loom:pr-lease[-released] role=builder ... sweep=<id> -->` comment;
a lane session (`LOOM_SWEEP_ID`) equal to it is refused before any session is
launched or comment posted. Nothing crosses between the sessions.
