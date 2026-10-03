#!/usr/bin/env bash
# Tests the inbox_mail helper in defaults/docs/inbox-mail.md (#10000) against
# stubbed curl/gh: unset env -> no-op (no curl, no gh), send/resolve payloads and
# the Authorization header, failure is non-fatal, the origin-derived key, the
# human-merge resolve path (resolve-merged), the hold file's loader with the doc
# missing, the chore mail (chore / resolve-closed), plus doc-lint for the shared
# rule, every role's sub-kind routing, and labels.yml (slice 2 of #10000).
set -u
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
fails=0
ok() { echo "ok: $1"; }
bad() { echo "FAIL: $1"; fails=$((fails+1)); }
DOC="$ROOT/defaults/docs/inbox-mail.md"
HOLD="$ROOT/defaults/.claude/commands/loom/champion-critical-file-hold.md"

awk '/^```bash inbox-mail/{f=1;next} /^```/{f=0} f' "$DOC" >"$T/fn.sh"
[ -s "$T/fn.sh" ] || { echo "FAIL: no inbox-mail fence"; exit 1; }

mkdir "$T/bin"
# curl stub: logs the payload file AND the --config stdin (where the header lives).
cat >"$T/bin/curl" <<'STUB'
#!/usr/bin/env bash
echo called >>"$STUB_LOG"
for a in "$@"; do echo "argv: $a" >>"$STUB_LOG"; done
while [ $# -gt 0 ]; do [ "$1" = --data-binary ] && { cat "${2#@}" >>"$STUB_LOG"; echo >>"$STUB_LOG"; }; shift; done
sed 's/^/config: /' >>"$STUB_LOG"
printf '{}\n%s' "${STUB_CODE:-200}"; exit "${STUB_RC:-0}"
STUB
# gh stub: honours the `merged:>=` / `closed:>=` DATE search like the forge
# does; no such qualifier -> [] (so a query without the date filter finds
# nothing). `gh issue|pr view N` prints a URL.
cat >"$T/bin/gh" <<'STUB'
#!/usr/bin/env bash
echo "gh $*" >>"$GH_LOG"
[ "${2:-}" = view ] && { echo "https://forge.invalid/acme/widgets/$1/$3"; exit 0; }
since=; f=mergedAt; fx=$GH_FIXTURE
while [ $# -gt 0 ]; do [ "$1" = --search ] && { since=$(sed -nE 's/.*(merged|closed):>=([^ ]*).*/\2/p' <<<"$2")
  case "$2" in *closed:*) f=closedAt fx=$GH_CLOSED_FIXTURE ;; esac; }; shift; done
[ -n "$since" ] || { echo '[]'; exit 0; }
jq --arg s "$since" --arg f "$f" '[.[] | select(.[$f] >= $s)]' "$fx" 2>/dev/null || cat "$fx"
STUB
chmod +x "$T/bin/curl" "$T/bin/gh"
export PATH="$T/bin:$PATH" STUB_LOG="$T/log" GH_LOG="$T/ghlog" GH_FIXTURE="$T/prs.json" GH_CLOSED_FIXTURE="$T/closed.json"
# shellcheck disable=SC1091
. "$T/fn.sh"

ago() { date -u -d "-$1 hours" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -v-"$1"H +%Y-%m-%dT%H:%M:%SZ; }
now=$(ago 0); old=$(ago 96); held=$(ago 30)
M='<!-- champion:critical-file-hold -->'
jq -n --arg now "$now" --arg old "$old" --arg held "$held" --arg m "$M" '[
  {number: 10, mergedAt: $held, comments: [{body: $m}]},
  {number: 11, mergedAt: $now, comments: [{body: ("## hold\n" + $m)}]},
  {number: 12, mergedAt: $now, comments: [{body: "unrelated"}]},
  {number: 13, mergedAt: $old, comments: [{body: $m}]}]' >"$GH_FIXTURE"

: >"$STUB_LOG"; : >"$GH_LOG"; unset LOOM_UI_INBOX_URL LOOM_UI_INGEST_KEY
out=$(inbox_mail send k1 "hello"); rc=$?
{ [ $rc -eq 0 ] && grep -q "not configured" <<<"$out" && [ ! -s "$STUB_LOG" ]; } && ok "unset env: no-op, no curl" || bad "unset env"
out=$(inbox_mail resolve k1); rc=$?
{ [ $rc -eq 0 ] && [ ! -s "$STUB_LOG" ]; } && ok "unset env resolve: no-op" || bad "unset env resolve"
inbox_mail on && bad "on: true while unconfigured" || ok "on: false while unconfigured"
inbox_mail resolve-merged crithold-pr "$M" >/dev/null
{ [ ! -s "$GH_LOG" ] && [ ! -s "$STUB_LOG" ]; } && ok "unset env resolve-merged: no gh read" || bad "unset env resolve-merged read the forge"

# Key: from the origin remote, not the checkout directory name.
git init -q "$T/issue-77" && git -C "$T/issue-77" remote add origin git@github.com:acme/widgets.git
[ "$(cd "$T/issue-77" && inbox_mail key crithold-pr 5)" = mail-widgets-crithold-pr-5 ] && ok "key from origin (ssh)" || bad "key ssh: $(cd "$T/issue-77" && inbox_mail key crithold-pr 5)"
git -C "$T/issue-77" remote set-url origin https://github.com/acme/widgets/
[ "$(cd "$T/issue-77" && inbox_mail key crithold-pr 5)" = mail-widgets-crithold-pr-5 ] && ok "key from origin (https)" || bad "key https"

export LOOM_UI_INBOX_URL=http://inbox.invalid LOOM_UI_INGEST_KEY=secret TO=@op:x
inbox_mail on && ok "on: true when configured" || bad "on: configured"
: >"$STUB_LOG"; inbox_mail send mail-loom-crithold-pr-1 "PR 1 needs a human merge" >/dev/null
grep -q '"key": "mail-loom-crithold-pr-1"' "$STUB_LOG" && grep -q 'needs a human merge' "$STUB_LOG" \
  && ok "send payload keyed" || bad "send payload"
grep -qx 'config: header = "Authorization: Bearer secret"' "$STUB_LOG" && ok "Authorization header via --config stdin" || bad "auth header"
grep -q '^argv: .*secret' "$STUB_LOG" && bad "ingest key on argv" || ok "ingest key not on argv"
: >"$STUB_LOG"; inbox_mail send mail-loom-crithold-pr-1 "PR 1 needs a human merge" >/dev/null
[ "$(grep -c called "$STUB_LOG")" = 1 ] && grep -q '"key": "mail-loom-crithold-pr-1"' "$STUB_LOG" && ok "re-send reuses same key" || bad "re-send key"
: >"$STUB_LOG"; inbox_mail resolve mail-loom-crithold-pr-1 >/dev/null
grep -q '"resolve": true' "$STUB_LOG" && grep -q '"key": "mail-loom-crithold-pr-1"' "$STUB_LOG" && ok "resolve sends resolve:true" || bad "resolve payload"
out=$(STUB_CODE=500 STUB_RC=22 inbox_mail send k2 body); rc=$?
{ [ $rc -eq 0 ] && grep -q FAILED <<<"$out"; } && ok "failure is non-fatal" || bad "failure handling"

# Human-merge path: a held PR merged outside Champion gets its mail resolved.
: >"$STUB_LOG"; : >"$GH_LOG"
(cd "$T/issue-77" && inbox_mail resolve-merged crithold-pr "$M" >/dev/null); rc=$?
grep -Eq 'pr list --state merged --limit 100 --search merged:>=[0-9]{4}-[0-9]{2}-[0-9]{2}T' "$GH_LOG" \
  && ok "resolve-merged: query filters on merge date (merged:>=)" || bad "resolve-merged query: $(cat "$GH_LOG")"
{ [ $rc -eq 0 ] && [ "$(grep -c called "$STUB_LOG")" = 2 ] \
  && grep -q '"key": "mail-widgets-crithold-pr-10"' "$STUB_LOG" && grep -q '"key": "mail-widgets-crithold-pr-11"' "$STUB_LOG" \
  && [ "$(grep -c '"resolve": true' "$STUB_LOG")" = 2 ]; } && ok "resolve-merged: held PRs #10 (30h) and #11 resolved; old/unmarked skipped" || bad "resolve-merged selection"
echo 'not json' >"$GH_FIXTURE"; (inbox_mail resolve-merged crithold-pr "$M" >/dev/null) && ok "resolve-merged: bad gh output non-fatal" || bad "resolve-merged rc"

# The hold file's loader, run where the doc is missing: defines a no-op, `on` false.
sed -n '/^_im=\$(awk/,/^type inbox_mail/p' "$HOLD" >"$T/loader.sh"
[ "$(wc -l <"$T/loader.sh" | tr -d ' ')" = 3 ] && ok "hold file carries the 3-line loader" || bad "loader not found in hold file"
out=$(cd "$T" && bash -c '. ./loader.sh; inbox_mail send k b; echo "send=$?"; inbox_mail on; echo "on=$?"' 2>&1)
{ grep -q 'send=0' <<<"$out" && grep -q 'on=1' <<<"$out" && ! grep -q 'not found' <<<"$out"; } && ok "missing doc: no-op fallback defined" || bad "missing doc fallback: $out"
mkdir -p "$T/repo/.loom/docs" && cp "$DOC" "$T/repo/.loom/docs/"
out=$(cd "$T/repo" && bash -c '. ../loader.sh; type inbox_mail | grep -c resolve-merged') && ok "doc present: loader evals the fence" || bad "loader with doc"

grep -q 'Two ways to reach a human' "$ROOT/defaults/docs/label-state-machine.md" && ok "rule in label-state-machine.md" || bad "rule missing"
[ "$(grep -rl --exclude-dir=tests 'a call is a decision, a human task is a mail' "$ROOT/defaults" | wc -l | tr -d ' ')" = 1 ] && ok "rule text appears once" || bad "rule text count"
grep -q 'loom:operator-mechanical' "$ROOT/defaults/.github/labels.yml" && ok "operator-mechanical label kept" || bad "label removed"
grep -q 'inbox_mail resolve "\$CF_MAIL_KEY"' "$HOLD" && ok "hold resolves mail" || bad "hold resolve missing"
grep -q 'inbox_mail send' "$HOLD" && ok "hold sends mail" || bad "hold send missing"
grep -q 'inbox_mail on &&' "$HOLD" && ok "hold gates its forge read on inbox config" || bad "hold read ungated"
grep -qF 'inbox_mail resolve-merged crithold-pr "<!-- champion:critical-file-hold -->"' \
  "$ROOT/defaults/.claude/commands/loom/champion-pr-merge.md" && ok "Champion runs resolve-merged per pass" || bad "resolve-merged not wired"

# --- Slice 2: chore mail (loom:operator-mechanical) -------------------------
CMD="$ROOT/defaults/.claude/commands/loom"; LSM="$ROOT/defaults/docs/label-state-machine.md"
CM='<!-- loom:chore-mail -->'
jq -n --arg now "$now" --arg old "$old" --arg held "$held" --arg m "$CM" '[
  {number: 20, closedAt: $held, labels: [{name: "loom:operator-mechanical"}], comments: []},
  {number: 21, closedAt: $now, labels: [], comments: [{body: ("Routing\n" + $m)}]},
  {number: 22, closedAt: $now, labels: [{name: "loom:operator-blocked"}], comments: [{body: "x"}]},
  {number: 23, closedAt: $old, labels: [{name: "loom:operator-mechanical"}], comments: [{body: $m}]}]' >"$GH_CLOSED_FIXTURE"
unset LOOM_UI_INBOX_URL LOOM_UI_INGEST_KEY; : >"$STUB_LOG"; : >"$GH_LOG"
out=$(inbox_mail chore issue 7 "Rotate the key"; inbox_mail resolve-closed issue); rc=$?
{ [ $rc -eq 0 ] && [ ! -s "$GH_LOG" ] && [ ! -s "$STUB_LOG" ] && grep -q "not configured" <<<"$out"; } \
  && ok "chore + resolve-closed unconfigured: no-op, no gh, no curl" || bad "chore unconfigured"
export LOOM_UI_INBOX_URL=http://inbox.invalid LOOM_UI_INGEST_KEY=secret
: >"$STUB_LOG"; (cd "$T/issue-77" && inbox_mail chore issue 7 "Rotate the deploy key on host-2" >/dev/null)
{ [ "$(grep -c called "$STUB_LOG")" = 1 ] && grep -q '"key": "mail-widgets-chore-issue-7"' "$STUB_LOG" \
  && grep -q 'Rotate the deploy key on host-2 — https://forge.invalid/acme/widgets/issue/7' "$STUB_LOG"; } \
  && ok "chore: one keyed mail naming the action, item linked" || bad "chore payload: $(cat "$STUB_LOG")"
: >"$STUB_LOG"; (cd "$T/issue-77" && inbox_mail chore pr 8 "Grant the CI token" >/dev/null)
grep -q '"key": "mail-widgets-chore-pr-8"' "$STUB_LOG" && ok "chore pr: key mail-<repo>-chore-pr-N" || bad "chore pr key"
: >"$STUB_LOG"; out=$(inbox_mail chore issue 7; inbox_mail chore epic 7 "x"); rc=$?
{ [ $rc -eq 0 ] && [ ! -s "$STUB_LOG" ]; } && ok "chore without action / bad kind: not sent, rc 0" || bad "chore guards"
: >"$STUB_LOG"; : >"$GH_LOG"; (cd "$T/issue-77" && inbox_mail resolve-closed issue >/dev/null); rc=$?
grep -Eq 'issue list --state all --limit 100 --search closed:>=[0-9]{4}-' "$GH_LOG" \
  && ok "resolve-closed: query filters on close date" || bad "resolve-closed query: $(cat "$GH_LOG")"
{ [ $rc -eq 0 ] && [ "$(grep -c called "$STUB_LOG")" = 2 ] && [ "$(grep -c '"resolve": true' "$STUB_LOG")" = 2 ] \
  && grep -q '"key": "mail-widgets-chore-issue-20"' "$STUB_LOG" && grep -q '"key": "mail-widgets-chore-issue-21"' "$STUB_LOG"; } \
  && ok "resolve-closed: #20 (label) and #21 (marker) resolved; unrelated/old skipped" || bad "resolve-closed selection: $(cat "$STUB_LOG")"
: >"$GH_LOG"; inbox_mail resolve-closed pr >/dev/null; grep -q '^gh pr list --state all' "$GH_LOG" && ok "resolve-closed pr lists PRs" || bad "resolve-closed pr"

# The Champion blocks' one-line loader: no-op where the doc is missing, real where present.
L1=$(grep -h '^ *eval "$(awk .*inbox-mail.md' "$CMD/champion-issue-promo.md" "$CMD/champion-epic.md" | sed 's/^ *//' | sort -u)
[ "$(grep -c . <<<"$L1")" = 1 ] && ok "Champion one-line loaders are identical" || bad "Champion loaders differ/missing"
printf '%s\n' "$L1" >"$T/l1.sh"
out=$(cd "$T" && bash -c '. ./l1.sh; inbox_mail chore issue 1 x; echo "rc=$?"; inbox_mail on; echo "on=$?"' 2>&1)
{ grep -q 'rc=0' <<<"$out" && grep -q 'on=1' <<<"$out"; } && ok "one-line loader: no-op without the doc" || bad "one-line loader: $out"
(cd "$T/repo" && bash -c '. ../l1.sh; type inbox_mail | grep -q resolve-closed') && ok "one-line loader evals the fence" || bad "one-line loader with doc"

# (a) every doc site that applies loom:operator-mechanical sends the chore mail within 3 lines.
mech_lint() { awk 'FNR==1{p=0} /--add-label[ =]*"[^"]*loom:operator-mechanical/ {p=FNR; f=FILENAME}
  p && /inbox_mail chore/ && FNR-p<=3 {p=0} p && FNR-p==3 {print f ":" p; p=0} END{if (p) print f ":" p}' "$@"; }
printf 'gh issue edit 1 --add-label "loom:operator-only,loom:operator-mechanical"\n' >"$T/bad.md"
[ -n "$(mech_lint "$T/bad.md")" ] && ok "mechanical lint flags an unmailed application" || bad "mechanical lint vacuous"
miss=$(mech_lint "$CMD"/*.md)
[ -z "$miss" ] && ok "every operator-mechanical application sends a chore mail" || bad "no chore mail after: $miss"
[ "$(grep -lE -- '--add-label[ =]*"[^"]*loom:operator-mechanical' "$CMD"/*.md | wc -l | tr -d ' ')" -ge 4 ] \
  && ok "lint is not vacuous (>=4 application sites)" || bad "too few mechanical sites found"
grep -q 'routed to loom:operator-only,loom:operator-mechanical' "$ROOT/defaults/scripts/check-promotion-landed.sh" \
  && grep -A1 "routed to loom:operator-only,loom:operator-mechanical' <<<" "$CMD/champion-issue-promo.md" | grep -q 'inbox_mail chore issue' \
  && ok "check-promotion-landed escalation mails via its caller" || bad "promotion-landed escalation not mailed"
grep -A2 -- '--remove-label "loom:operator-mechanical"' "$CMD/sweep-scheduling-signals.md" | grep -q 'resolve its chore mail' \
  && ok "lane relabel off mechanical resolves the mail" || bad "lane relabel resolve"
grep -q 'inbox_mail resolve-closed issue; inbox_mail resolve-closed pr' "$CMD/champion-pr-merge.md" \
  && ok "Champion resolves chore mail per pass" || bad "resolve-closed not wired"
# (b)+(c) the four roles that can apply the label: mail on mechanical, objective -> decision, pointer to the rule.
for r in curator builder judge doctor; do
  f="$CMD/$r.md"
  grep -q '^| `loom:operator-mechanical` |.*chore mail' "$f" && grep -q '^| `loom:operator-objective` | Not applied (#10000)' "$f" \
    && grep -q 'Two ways to reach a human' "$f" && ok "$r.md: mechanical mails, objective is a decision, points to the rule" || bad "$r.md routing"
done
grep -rnE -- '--add-label[^|]*loom:operator-objective' "$CMD" "$ROOT/defaults/docs" "$ROOT/defaults/scripts"/*.sh >/dev/null \
  && bad "something still applies loom:operator-objective" || ok "nothing applies loom:operator-objective"
sec=$(awk '/^## Two ways to reach a human/{f=1;next} /^## /{f=0} f' "$LSM")
{ grep -q '`loom:operator-blocked`: nothing' <<<"$sec" && grep -q '`loom:operator-objective`: not applied' <<<"$sec" \
  && grep -q '`loom:operator-mechanical`.*chore mail' <<<"$sec"; } && ok "shared rule maps every sub-kind" || bad "shared rule sub-kind map"
for l in loom:operator loom:operator-only loom:operator-mechanical loom:operator-blocked loom:operator-decision loom:operator-objective; do
  grep -q "^- name: \"\{0,1\}$l\"\{0,1\}$" "$ROOT/defaults/.github/labels.yml" || bad "label $l missing from labels.yml"
done; ok "all six operator labels kept in labels.yml"

[ "$fails" -eq 0 ] && echo "ALL PASSED" || { echo "$fails failed"; exit 1; }
