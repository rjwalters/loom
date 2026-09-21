# Transcript Token Ingestion

How `~/.loom/activity.db`'s token/cost tables get populated on a host whose work
arrives by `dispatch_sweep` (Issue #8059, part of #8052).

## The gap this closes

`resource_usage` had exactly one live writer — the IPC `GetTerminalOutput`
handler, which scrapes a **managed terminal's** scrollback. A dispatched sweep
(`claude -p` via `spawn-worker.sh`) issues no `SendInput`/`GetTerminalOutput`
round trips at all, so on every dispatch-driven host that table, the sibling
`token_usage` table, and all six `cost_by_*` views over them were not "empty
until correlation catches up" — they were **structurally empty forever**.
`loom-daemon stats` had no cost to report, and #8052's fleet-cost question ("what
is the fleet spending, by role x model x day?") could only be answered with a
hand-written scraper.

The data was on disk the whole time, in each sweep's own Claude Code transcripts
(`${CLAUDE_CONFIG_DIR:-~/.claude}/projects/<cwd-slug>/…`). This reads them.

## Running it

```bash
loom-daemon ingest-transcripts                     # every transcript, every project
loom-daemon ingest-transcripts --since 24h         # only recently-modified ones
loom-daemon ingest-transcripts --dry-run           # report, write nothing
loom-daemon ingest-transcripts --format json       # machine-readable summary
loom-daemon ingest-transcripts --workspace ~/GitHub/loom   # one repo's transcripts
```

`--since` takes `7d` / `12h` / `90m`, an RFC-3339 instant, or `all`.
`--projects-dir` and `--db` override the transcript and database locations
(defaults: `${CLAUDE_CONFIG_DIR:-~/.claude}/projects` and `~/.loom/activity.db`).

**Re-running is safe by construction.** Every ingested transcript is recorded in
a `transcript_ingest` ledger with its size and mtime: an unchanged file is
skipped, and a file that has grown (a sweep still running) is re-read in full
with its previous rows **replaced**, never appended to. A repeated pass can
therefore neither double-count nor miss a live session's tail. `--force`
re-reads even unchanged files; it still replaces rather than appends.

### In the daemon (on by default since #8477)

The daemon runs the pass itself every 15 minutes. **Precedence is `env var >
config value > built-in default`**, the same rule every `autonomous.*` knob
follows:

| Config key | Env override | Default | Meaning |
|---|---|---|---|
| `autonomous.transcriptIngest.enabled` | `LOOM_TRANSCRIPT_INGEST` | `true` | Master on/off. Env `0`/`false`/`no`/`off` opts this host **out**; `1`/`true`/`yes`/`on` forces it on over a config `false`. An unrecognized value falls through to config/default rather than silently disabling |
| `autonomous.transcriptIngest.intervalSecs` | `LOOM_TRANSCRIPT_INGEST_INTERVAL` | `900` | Seconds between passes. Zero/invalid → default |
| `autonomous.transcriptIngest.windowHours` | `LOOM_TRANSCRIPT_INGEST_WINDOW_HOURS` | `24` | How far back each pass looks; `0` = full history (still cheap after the first pass, thanks to the ledger) |

```json
{
  "autonomous": {
    "transcriptIngest": { "enabled": false }
  }
}
```

**Restart required** for all three: they are resolved once, during daemon
bring-up, and frozen for the life of the process (`try_init_transcript_ingest`
is called before the loop is spawned). Landing the edit on disk changes
nothing until the daemon restarts — see
[`fleet-config-lifecycle.md`](fleet-config-lifecycle.md).

#### Why this one defaults ON, against the FLAGS-OFF convention

The daemon's FLAGS-OFF doctrine governs **work generation** — loops that spawn
agents, spend tokens, and mutate the forge. This pass generates no work: it is
a passive telemetry writer into Loom's own `~/.loom/activity.db`, incremental,
ledgered, and idempotent (a repeat pass over unchanged files is a no-op).

Default-*off* was actively destroying data, and silently. See "The 30-day fuse"
below: every host that never hand-set `LOOM_TRANSCRIPT_INGEST=1` — which, as
measured across the fleet on 2026-09-20, was all of them — was losing its
token/cost history permanently as Claude Code pruned the transcripts it had
never read. A knob whose "safe" position deletes the data it guards is the
wrong polarity; opting *out* is now the deliberate act.

## The 30-day fuse (#8477)

**Claude Code deletes session transcripts after `cleanupPeriodDays` (default
30), and the cleanup runs at session start.** On a fleet host, agents start
constantly, so transcripts are pruned continuously as they cross the line —
observed live on 2026-09-20: 43 transcripts dated Aug 21 vanished from
`~/.claude/projects` within a few minutes.

The transcripts are the **only** copy of this data. Once a transcript is gone,
the tokens and cost it recorded are unrecoverable — there is no forge-side or
API-side backfill. So the window in which ingestion can run is exactly
`cleanupPeriodDays` wide, and anything that stops the pass for longer than that
(daemon down, config opt-out, a wedged database) burns history that no later
run can recover. The first run on that host wrote 100,652 rows spanning only
the surviving ~30 days; everything older was already gone.

Two levers, independently:

- **Ingest** (this document) — keeps the *derived* token/cost rows forever in
  `~/.loom/activity.db`, at a few hundred MB. On by default; verify with
  `loom-daemon health` (below).
- **Retain the raw transcripts** — raise `cleanupPeriodDays` in
  `~/.claude/settings.json`, trading disk for retention (~33 GB per 30 days on
  the measured host; a `.tar.zst` archive of the same set compressed 10.9:1,
  25.4 GB → 2.33 GB, so a rolling archive is cheap). Only needed for forensics
  or `claude --resume`; the cost/token views do not depend on it once the rows
  are ingested. **Anything that archives or prunes transcripts must exclude
  `~/.claude/projects/<project>/memory/`** — that holds persistent agent
  memory, not session transcripts.

### Checking it is actually running

```bash
loom-daemon health --json | jq '.sections[] | select(.key == "transcript_ingest")'
```

The `transcript_ingest` section always renders, and is **DEGRADED** when:

- ingestion is off on this host (`LOOM_TRANSCRIPT_INGEST=0` or
  `autonomous.transcriptIngest.enabled: false`) — deliberate or not, the host
  is losing history right now; or
- the newest `transcript_ingest` ledger entry is more than 6 hours old **while
  a newer transcript exists on disk** — the pass is enabled but has stopped
  keeping up (crashed thread, wedged database lock, daemon down). An old ledger
  entry on a quiet host with no newer transcripts is *not* flagged: nothing has
  arrived to ingest.

A second corroborating check, from the daemon log and a dry run:

```bash
grep 'Transcript ingestion' ~/.loom/daemon.log | tail -1
loom-daemon ingest-transcripts --dry-run --format json | jq '{transcripts_seen, skipped_unchanged}'
```

A healthy host reports `skipped_unchanged` ≈ `transcripts_seen`. A
`skipped_unchanged` of **0** means nothing has ever been ingested — the
symptom that opened #8477.

## What a row means

One `resource_usage` row per **(model, UTC day) per transcript** — fine enough
for `cost_by_day`/`cost_by_month` to be accurate across a session that spans
midnight, coarse enough that a 28-day backfill is thousands of rows rather than
millions.

Role, repo, session id and issue reach the row the way `cost_by_role` already
expects: through `input_id -> agent_inputs`. Ingestion writes **one
`agent_inputs` anchor row per transcript** (`terminal_id =
"transcript:<session-uuid>[/agent-<id>]"`, `input_type = system`, `agent_role` =
the attributed role, `context` = workspace/repo/branch/issue JSON) and hangs that
transcript's usage rows off it. Consequence worth knowing: `agent_inputs` is what
`stats` counts as "prompts", so an ingesting host counts one extra "prompt" per
ingested transcript.

### Method (the #8052 method notes, honoured)

- **Dedupe on `message.id`.** A streamed assistant message is written once per
  chunk, and every chunk repeats the id carrying the **cumulative** usage.
  Summing blocks over-counts: measured on a live fleet host (2026-09-18, a 24h
  window over 2,330 ingested transcripts) 62,643 non-synthetic usage blocks
  collapsed to 30,579 distinct messages — **51% of blocks were repeats**, so a
  naive sum would have roughly doubled the fleet's reported spend. Folding by id and
  taking the per-counter maximum is correct for identical repeats and for
  genuinely growing cumulative chunks.
- **Skip `model == "<synthetic>"`** — Claude Code's marker for internal/tool-echo
  messages, not billable consumption.
- **Role attribution by the first user message** (method note (a)): a
  `<command-name>/loom:NAME</command-name>` marker when present (how a
  `/loom:sweep` parent session and every role-runner session start), otherwise
  the earliest whole-word mention of a known role, which is how a subagent's
  dispatch prompt names itself ("Load and follow … `doctor.md`", "You are the
  Loom Builder…"). No match leaves the role NULL, which groups as `unknown`
  rather than guessing.
- **Issue attribution only from the slash command's own first argument.** A
  number in prose ("PR #7759, which closes #7726") is deliberately ignored — a
  wrong attribution is worse than an absent one.
- **Cost** uses the existing pricing table in `activity::resource_usage`
  (cache reads at 0.1x input, cache writes at 1.25x). Treat `cost_usd` as a
  **list-price proxy for limit weight**, not a bill — #8060 reports that table
  as one to two generations stale for several models.

### `resource_usage`, not `token_usage`

#8059 allowed either table. Rows go to **`resource_usage` only**: it is the table
every cost analytics view and the `agent_effectiveness` / `cost_per_issue` /
`daily_velocity` stats views already read, whereas `token_usage` has never had a
writer and carries no column that matters here. Writing both would double-book
the same tokens in one database and hand every future reporter the job of knowing
which table not to sum.

## Verifying

```bash
loom-daemon ingest-transcripts --since 24h
sqlite3 ~/.loom/activity.db 'SELECT COUNT(*) FROM resource_usage;'
sqlite3 ~/.loom/activity.db 'SELECT agent_role, request_count, ROUND(total_cost,2) FROM cost_by_role ORDER BY total_cost DESC;'
sqlite3 ~/.loom/activity.db 'SELECT * FROM cost_by_month;'
```
