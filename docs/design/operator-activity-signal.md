# Operator activity signal: design (#12870)

Status: design only. Nothing here is approved for implementation. The ruling on
#12870 authorizes this document and nothing else: no producer or consumer code,
no session hooks, no transcript reads, no new GitHub polling, no `FeedEvent`
change, no deployment, access grant or training campaign. `loom-ui#5498` stays
blocked.

## Verification legend

Every factual row is marked one of:

- **source-verified**: read from loom-ui source at `575c33b80` (local checkout
  `~/GitHub/loom-ui`, HEAD on 2026-10-20). `24dc2de5b`, the commit the issue
  cites, is an ancestor of it, and `git diff --stat 24dc2de5b..575c33b80` over
  every file cited below is empty.
- **live-verified**: backed by a query and its output. **No row in this document
  is live-verified.** No D1 or SigNoz query was run. Source inspection is not
  coverage verification, and every coverage statement below is a claim about
  what the code would store, not about what is stored.

## Goal and non-goal

Goal: a feature for the human-gated stages (`triage_wait`, `approval_wait`,
`merge_hold`) that says whether an operator has recently been seen acting.

Non-goal, and a definition. The signal is **recent observed activity**: "a
label or state change that GitHub attributed to a non-bot, non-fleet login was
recorded recently". It is **not** presence and it does not say a human is
online. Three things break any presence reading:

1. An operator login is also used by attended agents (a Claude or Codex session
   under the operator's token). The sender is the login, not the hands.
2. An item's `updated_at`, or its author, is not actor evidence. `updated_at`
   moves on bot comments, CI, label sweeps and edits by anyone.
3. Absence of events is not absence of the operator. Coverage gaps look
   identical to inactivity unless the feature says otherwise (see
   [Missing coverage](#missing-coverage-versus-inactivity)).

The output is per repo and fleet-wide. No per-person series, profile or model
feature exists or is added (the existing boundary in `operatorSessions.ts`:
"Login never leaves this module").

## Existing Worker data paths

| Path | Event types / repos | Actor or activity fields | Time field | Retention | Gaps | Status |
|---|---|---|---|---|---|---|
| Webhook `label.transition` row (D1 `records`, `host_id="github"`) | `issues`, `pull_request` deliveries only (`githubWebhook.ts:614`); repos with the GitHub App installed. Only `loom:*` label changes become `labeled`/`unlabeled` rows; `opened`/`closed`/`reopened` rows need a `loom:*` label after the change, except a PR, which always writes (`githubWebhook.ts:699`) | `actor` = `sender.login` (`githubWebhook.ts:638`, field at `:152`); optional | `at` = **Worker receipt time** (`:665`, doc at `:129`); `item_updated_at` = GitHub-clock `updated_at` of the item (`:682`) | `RETENTION_DAYS="90"` (`wrangler.2amlogic.toml:187`), but the row cap `MAX_RECORDS="2000000"` (`:290`) binds first by the file's own size notes: about 30 days at about 67k rows/day | Non-`loom:*` label changes, comments, reviews and pushes produce no row. `actor` is absent when the payload had no sender. A row is evicted by id order when the cap binds | source-verified |
| Poll-filed row (`source:"poll"`) | Same shape, from the GitHub issue-events API (`githubPoll.ts:446`, `narrowEvent` at `:482`) | `actor` = events-API `actor.login` (`:489`). **An `opened` row's actor is the item author** (`:662`), not the opener's sender | `at` = **GitHub event time** (`:473`, `:634`), a different clock from the webhook rows | Ingest-based sweep like webhook rows; the poller's own bound is a call budget, `DEFAULT_MAX_AGE_DAYS=90` (`:196`), not retention | Mixed clocks within one stream. Rows may be filed long after they happened | source-verified |
| Poll `backfill:true` / `pre_webhook:true` rows | History filed in bulk (`githubPoll.ts:446`, `:84-89`) | Same `actor` | Event time of the old event, not when it was filed | Retained from ingest time, so a 200-day-old event outlives its age | Filed late, so they are knowable only from their ingest. The wake feed excludes them (`forgeFeed.ts:163`). Treating them as live activity would leak the future into any as-of read | source-verified |
| Wake feed (`GET /v1/hosts/<id>/events`, `forgeFeed.ts`) | A view over the rows above | **None.** `FeedEvent` (`forgeFeed.ts:88`) has no `actor`, and `toFeedEvent` (`:123`) never copies one | `at` as stored | Rides the table | Drops `noop` and `backfill` rows (`:162-163`). The daemon cannot see an actor through it | source-verified |
| SigNoz `pr.review` / `pr.cr_size` facts (`prReview.ts`, `prCrSize.ts`) | `issue_comment`, `pull_request_review` deliveries | Counts only. The reviewer login is read and not kept (`prReview.ts:41-43`); the body and author are never copied (`prCrSize.ts:31-33`) | Fact time | SigNoz | Comments and reviews are **not** an actor source | source-verified |
| SigNoz `github.labels` set (the fit's `LabelSet`) | The label sets the stream above folds into | `sender`, `senderType` (`eta/fit/facts.ts:208-213`, parsed at `:812-813`). Absent on reconcile and snapshot sets | `at` | SigNoz | Absent sender means unknown, not "no operator" | source-verified |

Retention note: the issue text says 90 days. That is the configured
`RETENTION_DAYS`, not the effective window while the row cap binds. The
effective window is a coverage property to measure, not assume (see
[How to measure](#how-a-later-comparison-would-measure-value)).

## What `operatorSessions.ts` already does

`src/eta/fit/operatorSessions.ts` already derives human-class events from the
actor-tagged `LabelSet.sender`:

- Three action classes only: `hold_release`, `purge_close` (only inside a mass
  close of 3 or more within 10 minutes), `promotion` (`loom:curated` gains
  `loom:issue`).
- Sender gate `driverOfSender(...) === "operator"` (a `User` that is not a bot
  and not in `FLEET_SENDERS`, which holds only `loom-fleet-dispatch`;
  `dispatchFeatures.ts:74`, `:83-87`).
- A cadence filter that drops a promotion run on a regular rhythm, as an
  agent's tick (`CADENCE_*`).
- Sessions with a 75 minute gap, a 30 minute `IN_SESSION_MS`, a 7 day lookback,
  and `sessionRegimeAt` returning `null` (unknown, never "no operator") when no
  sender-bearing set exists in the lookback.
- As-of discipline: callers pass `t - KNOWABLE_LAG_MS` (120 s,
  `episodes.ts:57`), and only earlier events are read.
- The login is a map key and is dropped from the output.

Its features, declared in `featureRegistry.ts`: `CLOCK_BIN_V16` (six UTC hour
bins) and `SESSION_REGIME_V16` (`in_session`, `log_h_since_session_end`,
`session_end_unknown`, `log_sessions_7d`, each at repo and fleet scope, plus
`session_regime_unknown`). A separate feature, `approvalActivity.ts`
(`APPROVING_NOW_H=1`), counts human `loom:issue` / `loom:pr` additions.

## Incremental information over `operatorSessions.ts`

**None, from the data that exists.** "Recent observed activity" computed from
the Worker's stored `actor` is the same stream, through the same
`driverOfSender` gate, that `SESSION_REGIME_V16` already reads. The only
differences available are a rename and a different set of action classes. The
design therefore stops here on the producer side: there is no new signal to
produce.

What a new feature could add is limited to inputs the stored data does not
carry, and none of them are available without something ruled out:

| Candidate input | Available from existing data? |
|---|---|
| Operator comments, reviews, pushes | No. Counted, not attributed (`prReview.ts`, `prCrSize.ts`). Attributing them is a new collection |
| Actor in the daemon wake feed | No. `FeedEvent` has no `actor`; adding it is a `FeedEvent` change, out of scope |
| A daemon-computed gauge | No. Needs actors in the feed or new forge calls, both ruled out |
| Non-`loom:*` label activity | No. The webhook drops it (`githubWebhook.ts:660`) |
| Session hooks, transcripts | Ruled out (#12536) |

### Existing columns: keep or replace

- Keep `CLOCK_BIN_V16`: it is clock, not activity, and carries no overlap with
  the session regime.
- Keep `SESSION_REGIME_V16` as the one activity feature. Its description in
  docs and the registry may say "recent observed activity" instead of
  "session", a wording change with no feature change.
- Keep `approvalActivity`: a narrower fact (approval label added), which a
  later comparison may show is subsumed by `promotion` events. That is for the
  comparison to decide, not for this design.
- Replace nothing. Adding a second activity feature beside these without the
  comparison below would overlap them and is not proposed.

## Definition of "recent observed activity"

An event is a label or state change on a `loom:*`-tracked item whose stored
sender passes `driverOfSender === "operator"` and survives the existing
action-class and cadence filters. "Recent" is relative to an as-of cutoff.
Required caveats on every consumer surface:

- It describes what was observed. It is not presence and not a person.
- A login that is an attended agent is counted as activity. The cadence filter
  removes only a regular-rhythm promotion run, so an irregular attended agent
  under the operator's token still counts. This is an accepted limit, stated,
  not solved.
- `item_updated_at`, `updated_at` and `author` are not actor evidence. The
  poller's `opened` row writes the item author into `actor` (`githubPoll.ts:662`);
  it is a creation fact and is not one of the three human action classes, so it
  must stay outside the activity feature.
- No output carries a login.

## Event time versus receipt time

- Webhook rows: `at` is **receipt time**. GitHub sends no event time in the
  delivery; `item_updated_at` is the closest GitHub-clock value and is the
  field to compare against other GitHub timestamps (`githubWebhook.ts:129-131`,
  `:176-183`).
- Poll rows: `at` is **GitHub event time**. Backfill rows are filed long after
  the event.
- Both clocks sit in one `at` column, and a consumer must not assume one.
  Receipt lag for webhook rows is small and bounded by delivery; for poll and
  backfill rows it is unbounded. Any as-of read must therefore be keyed on when
  a row became knowable, not on its `at` alone.

## As-of cutoff, replay and dedup

- Reuse `KNOWABLE_LAG_MS` and `factsAsOf` (`episodes.ts:57`, `:500`): a fact
  with instant `at` is known at `t` iff `at < t - KNOWABLE_LAG_MS`. No second
  cutoff rule.
- A `backfill:true` or `pre_webhook:true` row must not be knowable at its event
  `at`. Where `recordedAt` exists (`facts.ts:815`, for `forge_backfill`) the
  knowable instant is `recordedAt`; a row without it must be excluded from
  activity rather than dated early. **Open question for the comparison:** does
  the current regime code already do this for every source? This design did not
  verify it (source-verified reading covers `facts.ts:815` only).
- Dedup: webhook redelivery is absorbed by the `delivery_id` partial unique
  index (migration 0004, `githubWebhook.ts:44`); poll rows use
  `gh-event:<id>` / `gh-opened:<repo>#<n>`. A webhook row and a poll row for the
  same change carry different delivery ids and are not deduplicated against
  each other by that index. Whether the fold collapses them was not verified
  here.
- Replay: derived events must be a pure function of rows at or before the
  cutoff, so a replay of a stream prefix equals the prefix of the replay (the
  property `factsAsOf` states).
- A `noop`-marked row means the item's `loom:*` set did not change; it must not
  count as activity. `noop` is written after insert, so its absence is not proof
  of a change (`githubWebhook.ts:231`).

## Missing coverage versus inactivity

The consumer must distinguish three states and never merge them:

1. **Observed, active**: a qualifying event inside the window.
2. **Observed, inactive**: sender-bearing sets exist in the lookback for this
   repo, none qualifying.
3. **Unobserved**: no sender-bearing set in the lookback.
   `sessionRegimeAt` already returns `null` for this and
   `session_regime_unknown` carries it into the model.

Gaps that make a repo unobserved without being quiet: no App install, webhook
outage, row-cap eviction shortening the real window below 90 days, poll rows
with a missing actor. A coverage fact (per repo: oldest and newest
sender-bearing row, and the effective window) is what separates state 2 from 3;
it does not exist as a stored value today.

## Responsibilities

- **Producer (Worker, loom-ui):** already stores `actor`. No change proposed.
  One thing worth confirming, read-only: the effective window and per-repo
  sender coverage (needs the live queries below).
- **Consumer (loom-ui fit/serving):** owns derivation, the human-class filter,
  as-of reads and the unobserved state. `loom-ui#5498` should expect "recent
  observed activity", not presence, and its stale "recommended close" dependency
  text should be reconciled by its owner. This repo does not edit it.
- **Loom daemon:** no role. It cannot see actors through the feed and must not
  gain forge calls for this.

## How a later comparison would measure value

Only after coverage is live-verified. Held-out backtest on the human-gated
stages, same split and the same as-of cutoffs:

1. Baseline: `CLOCK_BIN_V16` alone.
2. Current: baseline plus `SESSION_REGIME_V16` (and `approvalActivity`).
3. Candidate: any proposed change on top of 2.

Report delta in held-out error and calibration per stage, with the unobserved
fraction alongside, because a gain concentrated in well-covered repos does not
transfer. A candidate that does not beat 2 beyond noise is dropped; one that
does replaces the overlapping columns instead of being added beside them.

Live checks to run before any claim of coverage (not run here; each needs its
output recorded):

- Per repo, count of D1 `label.transition` rows with and without `actor`, split
  by `source` and `backfill`, and the oldest and newest `at`: coverage and the
  effective retention window.
- Per repo, count of SigNoz `github.labels` sets with `sender`, over 7 days:
  whether the 7 day lookback is populated.
- Count of rows where a webhook and a poll row describe the same change: the
  dedup question above.

## Bounded implementation proposal

Given the data, the bounded proposal is documentation and a measurement, not a
new feature:

1. Run the three live coverage queries above, read-only, and record the output
   in a follow-up issue. If sender coverage is thin for a repo, that repo's
   regime is `unknown`, which the code already does.
2. Run the held-out comparison of `CLOCK_BIN_V16` against
   `CLOCK_BIN_V16 + SESSION_REGIME_V16` per human-gated stage, to decide whether
   the activity feature earns its columns at all.
3. Reword consumer-facing text from "session" to "recent observed activity" and
   state the attended-agent caveat. No new signal.

Items 1 and 2 are the filings after review, as `loom:triage` issues in
`rjwalters/loom-ui`. Neither adds polling.

## Exact gap, returned for scope review

If a true availability signal is wanted beyond "recent observed activity from
`loom:*` label changes", the existing data is **insufficient**, and the exact
gap is: operator-authored comments, reviews and pushes are counted without an
actor, and `FeedEvent` carries no actor. Closing it needs one of (a) retaining
an actor class (operator, fleet, other) on the comment and review facts, or
(b) adding `actor` to `FeedEvent`. Both are outside the current ruling and need
a separate scope decision. Neither is requested here.

## Open items that this design did not verify

- Live coverage and effective retention (no query run).
- Whether the regime code excludes `backfill`/`pre_webhook` rows from activity
  at their event time for every source.
- Whether a webhook row and a poll row for one change are collapsed by the fold.
