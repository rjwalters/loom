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
3. Two sessions run, `ratification-ee` and `ratification-market`. Each prompt is
   its pinned instructions plus the PR reference and nothing else.
4. For each key whose session ends in success **and** whose output parses as
   that key's review (below) the daemon posts one comment. A key that failed
   closed, whose session did not succeed, or whose output carries no verdict
   posts nothing and the reason is logged; the other key is unaffected. A
   withheld key posts no marker, so the PR is selected again at the same head
   on a later tick.

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
