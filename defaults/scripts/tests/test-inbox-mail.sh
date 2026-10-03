#!/usr/bin/env bash
# Tests the inbox_mail helper in defaults/docs/inbox-mail.md (#10000) against a
# stubbed curl: unset env -> no-op, send/resolve payloads, failure is non-fatal,
# plus doc-lint for the shared rule and labels.yml keeping operator-mechanical.
set -u
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
fails=0
ok() { echo "ok: $1"; }
bad() { echo "FAIL: $1"; fails=$((fails+1)); }

awk '/^```bash inbox-mail/{f=1;next} /^```/{f=0} f' "$ROOT/defaults/docs/inbox-mail.md" >"$T/fn.sh"
[ -s "$T/fn.sh" ] || { echo "FAIL: no inbox-mail fence"; exit 1; }

mkdir "$T/bin"
cat >"$T/bin/curl" <<'STUB'
#!/usr/bin/env bash
echo called >>"$STUB_LOG"
while [ $# -gt 0 ]; do [ "$1" = --data-binary ] && { cat "${2#@}" >>"$STUB_LOG"; echo >>"$STUB_LOG"; }; shift; done
cat >/dev/null 2>&1 </dev/null || true
printf '{}\n%s' "${STUB_CODE:-200}"; exit "${STUB_RC:-0}"
STUB
chmod +x "$T/bin/curl"
export PATH="$T/bin:$PATH" STUB_LOG="$T/log"
# shellcheck disable=SC1091
. "$T/fn.sh"

: >"$STUB_LOG"; unset LOOM_UI_INBOX_URL LOOM_UI_INGEST_KEY
out=$(inbox_mail send k1 "hello"); rc=$?
{ [ $rc -eq 0 ] && grep -q "not configured" <<<"$out" && [ ! -s "$STUB_LOG" ]; } && ok "unset env: no-op, no curl" || bad "unset env"
out=$(inbox_mail resolve k1); rc=$?
{ [ $rc -eq 0 ] && [ ! -s "$STUB_LOG" ]; } && ok "unset env resolve: no-op" || bad "unset env resolve"

export LOOM_UI_INBOX_URL=http://inbox.invalid LOOM_UI_INGEST_KEY=secret TO=@op:x
: >"$STUB_LOG"; inbox_mail send mail-loom-crithold-pr-1 "PR 1 needs a human merge" >/dev/null
grep -q '"key": "mail-loom-crithold-pr-1"' "$STUB_LOG" && grep -q 'needs a human merge' "$STUB_LOG" && ! grep -q secret "$STUB_LOG" \
  && ok "send payload keyed, no secret in body" || bad "send payload"
: >"$STUB_LOG"; inbox_mail send mail-loom-crithold-pr-1 "PR 1 needs a human merge" >/dev/null
[ "$(grep -c called "$STUB_LOG")" = 1 ] && grep -q '"key": "mail-loom-crithold-pr-1"' "$STUB_LOG" && ok "re-send reuses same key" || bad "re-send key"
: >"$STUB_LOG"; inbox_mail resolve mail-loom-crithold-pr-1 >/dev/null
grep -q '"resolve": true' "$STUB_LOG" && grep -q '"key": "mail-loom-crithold-pr-1"' "$STUB_LOG" && ok "resolve sends resolve:true" || bad "resolve payload"
out=$(STUB_CODE=500 STUB_RC=22 inbox_mail send k2 body); rc=$?
{ [ $rc -eq 0 ] && grep -q FAILED <<<"$out"; } && ok "failure is non-fatal" || bad "failure handling"

grep -q 'Two ways to reach a human' "$ROOT/defaults/docs/label-state-machine.md" && ok "rule in label-state-machine.md" || bad "rule missing"
[ "$(grep -rl --exclude-dir=tests 'a call is a decision, a human task is a mail' "$ROOT/defaults" | wc -l)" = 1 ] && ok "rule text appears once" || bad "rule text count"
grep -q 'loom:operator-mechanical' "$ROOT/defaults/.github/labels.yml" && ok "operator-mechanical label kept" || bad "label removed"
grep -q 'inbox_mail resolve' "$ROOT/defaults/.claude/commands/loom/champion-critical-file-hold.md" && ok "hold resolves mail" || bad "hold resolve missing"
grep -q 'inbox_mail send' "$ROOT/defaults/.claude/commands/loom/champion-critical-file-hold.md" && ok "hold sends mail" || bad "hold send missing"

[ "$fails" -eq 0 ] && echo "ALL PASSED" || { echo "$fails failed"; exit 1; }
