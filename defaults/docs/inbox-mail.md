# Inbox mail: send and resolve a keyed human-task mail (#10000)

An agent that needs a human to **do** something (not decide) sends one mail to
the loom-ui inbox. Rule: [`label-state-machine.md`](label-state-machine.md)
§"Two ways to reach a human". This file is the inbox-only helper the roles
source; the full both-legs send (inbox + Matrix) stays in
`defaults/.claude/commands/loom/mail-send.md`.

- **One mail per ask.** `KEY` is `mail-<repo>-<kind>-<N>` (e.g.
  `mail-loom-crithold-pr-123`). `POST /api/inbox` is idempotent on `key`, so
  re-sending the same key is safe.
- **Resolve** with the same key: `POST /api/inbox` `{key, resolve: true}`.
- **No-op when unconfigured**: `LOOM_UI_INBOX_URL` or `LOOM_UI_INGEST_KEY` unset
  prints one note and returns 0. A failed POST warns and returns 0; a mail
  problem never breaks a role's tick.
- No secrets in `BODY`; the ingest key never goes on argv.

```bash inbox-mail
# inbox_mail send|resolve KEY [BODY]   (BODY required for send; optional TITLE, TO)
inbox_mail() {
  local mode="$1" key="$2" body="${3:-}" pf out rc code
  if [ -z "${LOOM_UI_INBOX_URL:-}" ] || [ -z "${LOOM_UI_INGEST_KEY:-}" ]; then
    echo "inbox not configured — mail $mode skipped (key $key)"; return 0
  fi
  pf=$(mktemp) || return 0
  case "$mode" in
    send) [ -n "$body" ] || { echo "inbox mail: empty body, not sent"; rm -f "$pf"; return 0; }
      jq -n --arg key "$key" --arg body "$(printf '%s' "$body" | head -c 20000)" \
            --arg who "${TO:-${LOOM_SENDER_IDENTITY:-$(hostname -s)}}" --arg title "${TITLE:-}" \
        '{key: $key, body: $body, who: $who, severity: "normal"}
          + (if $title == "" then {} else {title: $title} end)' >"$pf" ;;
    resolve) jq -n --arg key "$key" '{key: $key, resolve: true}' >"$pf" ;;
    *) echo "inbox mail: unknown mode $mode"; rm -f "$pf"; return 0 ;;
  esac
  out=$(printf 'header = "Authorization: Bearer %s"\n' "$LOOM_UI_INGEST_KEY" |
    curl -sS --max-time 30 --config - -X POST -H 'Content-Type: application/json' \
      --data-binary @"$pf" -w '\n%{http_code}' "${LOOM_UI_INBOX_URL%/}/api/inbox" 2>&1); rc=$?
  rm -f "$pf"; code=${out##*$'\n'}
  case "$rc:$code" in
    0:2??) echo "inbox mail $mode ok (key $key)" ;;
    *) echo "inbox mail $mode FAILED (curl exit $rc, HTTP ${code:-none}, key $key) — continuing" ;;
  esac
  return 0
}
```

Test: `defaults/scripts/tests/test-inbox-mail.sh` (extracts the fence above).
