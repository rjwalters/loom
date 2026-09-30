# Mail Send (loom-ui inbox + Matrix, one atomic send)

You are sending an operator-facing message on a person's behalf. Deliver it to
**both** surfaces — the loom-ui operator inbox (durable, threaded, behind
Access) and the team's Matrix room (where the team actually reads) — and
report both receipts. A send that reached only one surface is a failure, never
a success: if either leg fails, say so plainly, name the failed leg, and echo
the full content so it can be relayed by hand.

Do not use this for routine sweep narration, issue comments, or anything a
human has not asked to be escalated. It exists for asks that block work: a
credential, a token, a ruling, a spend approval.

## Phase 1: Gather the inputs

Ask the sender (or take from the task) for:

1. **`FROM`** — the human on whose behalf this is sent (e.g. `Joseph`).
   Default: `$LOOM_SENDER_IDENTITY`, else this host's short hostname. Never
   guess a person's name.
2. **`TITLE`** — ≤ 200 characters, single line.
3. **`BODY`** — the message. Loom-ui caps it at 20 action steps of ≤ 500
   characters each; trim to fit.
4. **`TO`** (optional) — the Matrix handle to mention; default
   `@rjwalters:matrix.org`.
5. **`SEVERITY`** (optional) — `low` / `normal` / `high` / `critical`;
   default `normal`.
6. **`KEY`** (optional) — the loom-ui inbox key; default
   `mail-<short-hostname>-<epoch-seconds>` (≤ 200 chars, no spaces).

Config that must already exist in the environment:

| Variable | Purpose |
| -------- | ------- |
| `LOOM_UI_INBOX_URL` | loom-ui Worker base URL |
| `LOOM_UI_INGEST_KEY` | this host's ingest key (loom-ui `docs/deploy-runbook.md` §8 — minted per host, only its hash stored server-side) |
| `LOOM_SENDER_IDENTITY` | default `FROM` |
| `matrix-post` skill | the operator-local `post.sh` (holds the only matrix.org credential on the machine; default path `~/.claude/skills/matrix-post/post.sh`) |

## Phase 2: Leg 1 — loom-ui operator inbox

Build the inbox key and POST (loom-ui `LIMITS`: key ≤ 200, title ≤ 200,
≤ 20 steps of ≤ 500 chars):

```bash
KEY="mail-$(hostname -s)-$(date +%s)"
STEPS=$(printf '%s\n' "$BODY" | cut -c1-500 | head -20 | jq -R . | jq -sc .)
curl -s -X POST "${LOOM_UI_INBOX_URL%/}/api/inbox" \
  -H "Authorization: Bearer ${LOOM_UI_INGEST_KEY}" \
  -H 'Content-Type: application/json' \
  -d "$(jq -n --arg key "$KEY" \
             --arg title "[from ${FROM} @ $(hostname -s)] ${TITLE}" \
             --arg who "$TO" --arg sev "$SEVERITY" --argjson steps "$STEPS" \
             '{key: $key, title: $title, action_steps: $steps, who: $who, severity: $sev}')"
```

A 2xx response is the leg-1 receipt (the response carries the item; note its
`id`). The route files the item as `host:<hostId>` — the `[from …]` title
prefix and `who` field are what make it human-attributed. Person-level
*identity* attribution is loom-ui#506's enhancement, not available yet.

## Phase 3: Leg 2 — Matrix room

Write the mirror body to a temp file, then post it through `matrix-post`:

```bash
MF=$(mktemp)
{
  echo "[mail from ${FROM} (host $(hostname -s)) — mirrored to the loom-ui inbox as key ${KEY}]"
  echo
  echo "« ${TITLE} »"
  echo
  echo "$BODY"
} >"$MF"
~/.claude/skills/matrix-post/post.sh --mention "$TO" "$MF"
```

`post.sh` prints the Matrix **event id** — that is the leg-2 receipt. It also
logs the temporary device out; do not skip that by calling the Matrix API
directly.

If `matrix-post` is not installed on this host, the leg **fails**: say
`matrix leg failed — post script not found at ~/.claude/skills/matrix-post/post.sh`,
echo the content, and do not pretend the send succeeded.

## Phase 4: Report both, or own the failure

Report both receipts together:

> delivered: loom-ui inbox item `<id>` (key `<KEY>`) · Matrix event `<event id>`

If either leg failed: exit/report non-zero with
`SEND INCOMPLETE — failed leg(s): …`, then echo `[from …] TITLE` and the full
BODY so the operator (or you, by hand) can relay it. Never mark a
half-delivered send as done, and never retry silently more than once — a
duplicated ask is better than a lost one, but three copies is noise.

## Rules

- **No secrets.** The Matrix room is unencrypted; the loom-ui inbox body is
  readable by its Access readers. Ask for credentials by name and location
  ("the DNS-scoped Cloudflare token", "rotate into SSM at /bifrost/prod/…"),
  never paste values.
- Attribution is honest: `FROM` is the person the sending agent acts for.
- One topic per send. A follow-up goes through the inbox thread
  (`POST /api/inbox/:id/reply`), not as a second full mail.
