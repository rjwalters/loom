#!/usr/bin/env bash
# Exercises the published Guide functions with an inert forge and persistent label state.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
DOC="$HERE/../../docs/park-record.md"
for fn in check_and_unblock_prs pr_has_superseding_block pr_trusted_marker pr_conflict_snapshot pr_has_reason_only_park pr_park_clearance_marker pr_record_park_clearance route_parked_pr_conflict previous_review_label; do
  eval "$(sed -n "/^$fn() {\$/,/^}\$/p" "$DOC")"
done

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
cd "$TMP"
mkdir -p .loom/scripts
export COMMENTS="$TMP/comments"
export GH_READ=gh
: > "$COMMENTS"
: > edits
cat > .loom/scripts/post-comment.sh <<'SH'
#!/usr/bin/env bash
jq -nc --argjson pr "$1" --arg body "$4" '{pr:$pr,body:$body}' >> "$COMMENTS"
SH
chmod +x .loom/scripts/post-comment.sh
fixture_head=1111111111111111111111111111111111111111
fixture_base=2222222222222222222222222222222222222222

set_labels() {
  jq -c --argjson number "$1" --argjson labels "$2" 'map(if .number == $number then .labels = $labels else . end)' rows.json > next.json
  mv next.json rows.json
}
labels_of() { jq -c --argjson n "$1" '.[] | select(.number == $n) | .labels' rows.json; }
expect_label() { labels_of "$1" | jq -e --arg name "$2" 'index($name) != null' >/dev/null; }
clearance() {
  local row="$1" event="$2" hash
  hash=$(printf 'park-%s' "$row" | git hash-object --stdin)
  jq -nc --argjson pr "$row" --arg body "<!-- loom:guide-park-cause-cleared head=$fixture_head park=$hash event=$event -->" '{pr:$pr,body:$body}' >> "$COMMENTS"
}
reset_rows() {
  jq -nc '[range(1;14) | . as $n | {number:.,body:("park-" + tostring),labels:(["loom:blocked"] +
    (if $n==1 then ["loom:ci-failure"] elif $n==5 then ["loom:operator"] elif $n==6 then ["loom:frozen"] else ["loom:review-requested"] end))}]' > rows.json
  : > "$COMMENTS"
  : > edits
  clearance 12 799
  clearance 13 800
  jq -nc '{pr:3,body:"Guide said main is green. The old account got a 401."}' >> "$COMMENTS"
}

gh() {
  local n labels status mergeable op row
  case "$1 $2" in
    'pr list') jq -c '[.[] | select(.labels|index("loom:blocked"))|.labels |= map({name:.})]' rows.json ;;
    'pr view')
      n=$3
      row=$(jq -c --argjson n "$n" '.[] | select(.number==$n)' rows.json)
      status=DIRTY; mergeable=CONFLICTING
      if [ "$n" = 8 ]; then status=UNKNOWN; mergeable=UNKNOWN; fi
      jq -c --arg head "$fixture_head" --arg base "$fixture_base" --arg ms "$status" --arg m "$mergeable" '
        .labels |= map({name:.})|. + {state:"OPEN",headRefOid:$head,baseRefOid:$base,isDraft:false,
        mergeable:$m,mergeStateStatus:$ms,reviewDecision:""}' <<<"$row" ;;
    'pr edit')
      n=$3; op=$4
      printf '%s %s\n' "$n" "$op" >> edits
      labels=$(labels_of "$n")
      case "$op" in
        --add-label)
          labels=$(jq -c '. + ["loom:changes-requested","loom:merge-conflict"]|unique' <<<"$labels")
          if [ "$n" = 11 ]; then labels=$(jq -c '. + ["loom:frozen"]' <<<"$labels"); fi
          set_labels "$n" "$labels" ;;
        --remove-label)
          if [ "$n" = 10 ] && [ ! -f failed-once ]; then touch failed-once; return 1; fi
          if [ "$5" = loom:review-requested ] && [ "$7" = loom:blocked ]; then
            labels=$(jq -c '. - ["loom:blocked","loom:review-requested"]' <<<"$labels")
            set_labels "$n" "$labels"
          else
            return 1
          fi ;;
        *) return 1 ;;
      esac ;;
    'api --paginate') printf 'labeled 800\n' ;;
    *) return 1 ;;
  esac
}

loom-daemon() {
  local body
  case "$1 $2" in
    'park-record parse')
      body=$(cat)
      case "$body" in
        park-2|park-3|park-12)
          printf '%s\n' '{"records":[{"blocker":null,"by":"judge","at":"2026-10-29T22:50:00Z","reason":"Judge log read failed with 401"}]}' ;;
        park-13)
          printf '%s\n' '{"records":[{"blocker":null,"by":"judge","at":"2026-10-29T22:50:00Z","reason":"ci-gate"}]}' ;;
        park-4) printf '%s\n' '{"records":[{"blocker":521}]}' ;;
        *) printf '%s\n' '{"records":[{"blocker":520}]}' ;;
      esac ;;
    'forge trusted-comments') jq -s --argjson n "$4" '[.[] | select(.pr == $n)]' "$COMMENTS" ;;
    'champion-hold classify')
      if [ "$4" = 7 ]; then
        printf 'LOOM-CHAMPION-HOLD state=champion-hold pr=7 detail=critical-file\n'
      else
        printf 'LOOM-CHAMPION-HOLD state=foreign-park pr=%s detail=-\n' "$4"
      fi ;;
    *) return 1 ;;
  esac
}

resolve_dep_states() {
  if [ "$1" = 520 ]; then printf 'true\n#520\n'; else printf 'false\n\n'; fi
}

reset_rows
LOOM_REVIEW_CONFLICT_RECONCILE=off check_and_unblock_prs
expect_label 1 loom:blocked
expect_label 2 loom:blocked
test ! -s edits
jq -es 'any(.[]; .pr==1 and (.body | contains("conflict-route-disabled")))' "$COMMENTS" >/dev/null

reset_rows
check_and_unblock_prs > candidates
expect_label 2 loom:blocked
receipt=$(sed -n 's/^PR #2: .* Receipt key //p' candidates)
test -n "$receipt"
printf '%s\n' 'In this fixture the original Judge identity can read the formerly denied run and job logs.' > evidence.md
! pr_record_park_clearance 2 "${receipt/800/799}" evidence.md
expect_label 2 loom:blocked
pr_record_park_clearance 2 "$receipt" evidence.md
for n in 1 2 9; do
  ! expect_label "$n" loom:blocked
  ! expect_label "$n" loom:review-requested
  expect_label "$n" loom:changes-requested
  expect_label "$n" loom:merge-conflict
done
expect_label 1 loom:ci-failure
for n in 3 4 5 6 7 8 10 11 12 13; do expect_label "$n" loom:blocked; done
! rg -q '^(3|4|5|6|7|8|12|13) ' edits
! rg -q '^11 --remove-label' edits
jq -es 'all(.[]; .pr != 6)' "$COMMENTS" >/dev/null

check_and_unblock_prs
! expect_label 10 loom:blocked
expect_label 10 loom:merge-conflict
jq -es '[.[] | select(.body | startswith("<!-- loom:guide-conflict-handoff")) | .pr] | sort == [1,2,9,10,11]' "$COMMENTS" >/dev/null
jq -es 'all(.[]; (.body | startswith("<!-- loom:guide-conflict-handoff") | not) or (.body|contains("Doctor") and contains("loom:merge-conflict")))' "$COMMENTS" >/dev/null
printf '%s\n' 'All conflict handoff assertions passed'
