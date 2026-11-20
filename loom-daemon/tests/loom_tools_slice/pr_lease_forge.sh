#!/bin/sh
# Synthetic gh for the tools-only PR lease integration tests. Calls never
# leave this directory. Writes take the same lock as the fixture's reads.
set -eu
fixture=$LOOM_TEST_PR_LEASE
case "${GH_CONFIG_DIR:-}" in
    "$fixture/gateway/.loom/gh-config-gateway/acme") backend=gateway ;;
    "$fixture/writer") backend=writer ;;
    *) echo "unexpected gh profile: ${GH_CONFIG_DIR:-empty}" >&2; exit 2 ;;
esac
[ "$1" = api ] || exit 2
shift
method=GET
endpoint=
body=
while [ "$#" -gt 0 ]; do
    case "$1" in
        -X) method=$2; shift ;;
        repos/*) endpoint=$1 ;;
        body=*) body=${1#body=} ;;
    esac
    shift
done
case "$method $endpoint" in
    "GET repos/acme/widgets") operation=permissions ;;
    GET\ */issues/77/comments) operation=comments ;;
    GET\ */issues/77) operation=state ;;
    GET\ */pulls/77) operation='head' ;;
    GET\ */commits/*) operation=checks ;;
    "POST repos/acme/widgets/issues/77/comments") operation=post ;;
    PATCH\ repos/acme/widgets/issues/comments/*)
        case "$body" in
            *"loom:pr-lease-renewed"*) operation=renew ;;
            *"loom:pr-lease-released"*) operation=release ;;
            *) exit 2 ;;
        esac ;;
    "DELETE repos/acme/widgets/issues/comments/"*) operation=delete ;;
    *) echo "unexpected fake forge call: $method $endpoint" >&2; exit 2 ;;
esac
until mkdir "$fixture/lock" 2>/dev/null; do sleep 0.01; done
trap 'rmdir "$fixture/lock"' 0
printf '%s %s\n' "$backend" "$operation" >> "$fixture/calls"
if [ -f "$fixture/fail-comments" ] && [ "$operation" = comments ]; then
    echo 'gh: Server Error (HTTP 502)' >&2
    exit 1
fi
make_row() {
    newline='
'
    first=${body%%"$newline"*}
    utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    printf '{"id":%s,"updated_at":"%s","body":"%s","user":{"login":"loom-fleet-dispatch[bot]","type":"Bot"},"author_association":"NONE"}\n' "$id" "$utc" "$first"
}
case "$operation" in
    permissions) echo '{"push":true}' ;;
    comments) cat "$fixture/rows" ;;
    state) echo '{"state":"open","labels":["loom:reviewing","loom:review-requested"]}' ;;
    head) echo '{"state":"open","head":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","labels":["loom:reviewing","loom:review-requested"]}' ;;
    checks) echo '{"check_runs":[],"statuses":[]}' ;;
    post)
        [ "$backend" = writer ] || exit 2
        id=$(cat "$fixture/next")
        printf '%s\n' "$((id + 1))" > "$fixture/next"
        make_row >> "$fixture/rows"
        printf '%s\n' "$id" ;;
    renew|release|delete)
        [ "$backend" = writer ] || exit 2
        id=${endpoint##*/}
        grep -v '^{"id":'"$id"',' "$fixture/rows" > "$fixture/rows.new" || true
        if [ "$operation" != delete ]; then make_row >> "$fixture/rows.new"; fi
        mv "$fixture/rows.new" "$fixture/rows" ;;
esac
