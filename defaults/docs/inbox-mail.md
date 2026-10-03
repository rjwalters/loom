# Inbox mail: send and resolve a keyed human-task mail (#10000)

An agent that needs a human to **do** something (not decide) sends one mail to
the loom-ui inbox. Rule: [`label-state-machine.md`](label-state-machine.md)
§"Two ways to reach a human". This file is the inbox-only helper the roles
source; the full both-legs send (inbox + Matrix) stays in
`defaults/.claude/commands/loom/mail-send.md`.

- **One mail per ask.** `inbox_mail key KIND N` prints `mail-<repo>-<KIND>-<N>`
  (e.g. `mail-loom-crithold-pr-123`); `<repo>` comes from the `origin` remote, so
  every checkout and worktree of a repo derives the same key. Send and resolve
  both call it. `POST /api/inbox` is idempotent on `key`, so re-sending is safe.
- **Resolve** with the same key: `POST /api/inbox` `{key, resolve: true}`.
- **Resolve on a merge nobody announced**: `inbox_mail resolve-merged KIND MARKER`
  resolves KIND mail for every PR merged in the last 48h that carries a
  `MARKER` comment, whoever merged it (a human, the GitHub UI, `merge-pr.sh`).
  It costs one `gh pr list` (a `merged:>=` search, not creation order) and
  re-resolving is idempotent. A forged marker can only resolve a mail for a PR
  that is already merged.
- **No-op when unconfigured**: `LOOM_UI_INBOX_URL` or `LOOM_UI_INGEST_KEY` unset
  prints one note and returns 0, with no forge read. `inbox_mail on` is the same
  test (status only), for gating a caller's own reads. A failed POST warns and
  returns 0; a mail problem never breaks a role's tick.
- No secrets in `BODY`; the ingest key never goes on argv.

Loading it (a missing doc leaves a no-op whose `on` is false):

```bash
_im=$(awk '/^```bash inbox-mail/{f=1;next} /^```/{f=0} f' .loom/docs/inbox-mail.md 2>/dev/null)
[ -n "$_im" ] && eval "$_im"
type inbox_mail >/dev/null 2>&1 || inbox_mail() { [ "$1" != on ]; }
```

```bash inbox-mail
# inbox_mail send|resolve KEY [BODY] | key KIND N | on | resolve-merged KIND MARKER
#   | chore issue|pr N ACTION | resolve-closed issue|pr  (optional TITLE, TO)
inbox_mail() {
  local mode="${1:-}" key="${2:-}" body="${3:-}" pf out rc code r n l=
  case "$mode" in
    key) r=$(git remote get-url origin 2>/dev/null); r=${r%/}; r=${r%.git}; r=${r##*/}; r=${r##*:}
      echo "mail-${r:-repo}-$key-$body"; return 0 ;;
    on) [ -n "${LOOM_UI_INBOX_URL:-}" ] && [ -n "${LOOM_UI_INGEST_KEY:-}" ]; return ;;
  esac
  if ! inbox_mail on; then
    echo "inbox not configured — mail $mode skipped (key $key)"; return 0
  fi
  case "$mode:$key" in chore:issue|chore:pr|resolve-closed:issue|resolve-closed:pr) ;;
    chore:*|resolve-closed:*) echo "inbox mail: $mode needs issue|pr"; return 0 ;; esac
  if [ "$mode" = chore ]; then
    [ -n "${4:-}" ] || { echo "inbox mail: chore needs an action, not sent"; return 0; }
    r=$(gh "$key" view "$body" --json url -q .url 2>/dev/null)
    inbox_mail send "$(inbox_mail key "chore-$key" "$body")" "$4${r:+ — $r}"; return 0
  fi
  if [ "$mode" = resolve-merged ] || [ "$mode" = resolve-closed ]; then
    # Filter on merge/close date in the query: plain --limit orders by creation,
    # and a held item is usually days old when it clears. GNU date, then BSD date.
    r=$(date -u -d '-2 days' +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -v-2d +%Y-%m-%dT%H:%M:%SZ 2>/dev/null)
    [ -n "$r" ] || { echo "inbox mail: no date, $mode skipped"; return 0; }
    if [ "$mode" = resolve-merged ]; then
      out=$(gh pr list --state merged --limit 100 --search "merged:>=$r" --json number,comments 2>/dev/null)
    else # a chore clears when its item closes (a merged PR is closed too)
      out=$(gh "$key" list --state all --limit 100 --search "closed:>=$r" --json number,labels,comments 2>/dev/null)
      body='<!-- loom:chore-mail -->' l=loom:operator-mechanical key="chore-$key"
    fi
    [ "$(jq length <<<"$out" 2>/dev/null)" = 100 ] && echo "inbox mail: 100 items in 48h, window truncated"
    jq -r --arg m "$body" --arg l "$l" '.[] | select(any(.comments[]?; .body | contains($m))
        or ($l != "" and any(.labels[]?; .name == $l))) | .number' <<<"$out" 2>/dev/null |
      while read -r n; do inbox_mail resolve "$(inbox_mail key "$key" "$n")"; done
    return 0
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

## Chore mail (`loom:operator-mechanical`)

A role applying `loom:operator-only,loom:operator-mechanical` (a human chore: a
credential, host access, hardware) sends one chore mail right after the label
edit. ACTION is one imperative sentence naming the step; the item's URL is
appended. Key `mail-<repo>-chore-issue-<N>` (or `-chore-pr-<N>`):

```bash
# loader above first
inbox_mail chore issue "$N" "Rotate the deploy key on build-host-2"   # pr for a PR
```

- Put `<!-- loom:chore-mail -->` in the routing comment.
- **Resolve.** Champion runs `inbox_mail resolve-closed issue` and `... pr` once
  per pass: it resolves chore mail for every item closed in the last 48h that
  still carries `loom:operator-mechanical` or that marker. A role moving an
  item off `loom:operator-mechanical` without closing it resolves it itself:
  `inbox_mail resolve "$(inbox_mail key chore-issue "$N")"`.
- No mail for `loom:operator-blocked` (a wait) or `loom:operator-decision` (the
  decision is the ask). Roles do not apply `loom:operator-objective`: it is filed
  as a decision whose ranked options are the candidate objectives.

Test: `defaults/scripts/tests/test-inbox-mail.sh` (extracts the fence above and
drives the loader with the doc missing).
