#!/usr/bin/env bash
# Exercises the published Guide functions with an inert forge and persistent label state.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
DOC="$HERE/../../docs/park-record.md"
for fn in check_and_unblock_prs pr_has_superseding_block pr_trusted_marker pr_conflict_snapshot pr_has_reason_only_park pr_park_episode pr_park_clearance_marker pr_record_park_clearance route_parked_pr_conflict previous_review_label; do
  eval "$(sed -n "/^$fn() {\$/,/^}\$/p" "$DOC")"
done

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
cd "$TMP"
mkdir -p .loom/scripts
if [ -n "${LOOM_GUIDE_FIXTURE_DAEMON:-}" ]; then
  mkdir -p .loom-project
  export LOOM_CONFIG_DEFAULTS_FILE=''
  unset LOOM_WORK_FINDER
  printf '%s\n' '{"autonomous":{"workFinder":{"enabled":true}}}' > .loom/config.json
fi
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
must_fail() {
  if "$@"; then
    printf 'Expected failure: %s\n' "$*" >&2
    exit 1
  fi
}
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
  rm -f episode failed-once .loom-project/project.json
  clearance 12 799
  clearance 13 800
  jq -nc '{pr:3,body:"Guide said main is green. The old account got a 401."}' >> "$COMMENTS"
}

gh() {
  local n labels status mergeable op row base_ref
  case "$1 $2" in
    'pr list') jq -c '[.[] | select(.labels|index("loom:blocked"))|.labels |= map({name:.})]' rows.json ;;
    'pr view')
      n=$3
      row=$(jq -c --argjson n "$n" '.[] | select(.number==$n)' rows.json)
      status=DIRTY; mergeable=CONFLICTING; base_ref=main
      if [ "$n" = 8 ]; then status=UNKNOWN; mergeable=UNKNOWN; fi
      base_ref=${FIXTURE_BASE_REF-$base_ref}
      if [ "${STACK_AFTER_ADD:-}" = "$n" ] && rg -q "^$n --add-label" edits; then
        base_ref=feature/issue-42
      fi
      jq -c --arg head "$fixture_head" --arg base "$fixture_base" --arg branch "$base_ref" --arg ms "$status" --arg m "$mergeable" '
        .labels |= map({name:.})|. + {state:"OPEN",headRefOid:$head,baseRefOid:$base,baseRefName:$branch,isDraft:false,
        mergeable:$m,mergeStateStatus:$ms,reviewDecision:""}' <<<"$row" ;;
    'pr edit')
      n=$3; op=$4
      printf '%s %s\n' "$n" "$op" >> edits
      labels=$(labels_of "$n")
      case "$op" in
        --add-label)
          labels=$(jq -c '. + ["loom:changes-requested","loom:merge-conflict"]|unique' <<<"$labels")
          if [ "$n" = 11 ]; then labels=$(jq -c '. + ["loom:frozen"]' <<<"$labels"); fi
          if [ "${RECREATE_FOR:-}" = "$n" ]; then printf 'unlabeled 801\nlabeled 802\n' > episode; fi
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
    'api --paginate')
      if [ "${FIXTURE_EVENTS:-}" = unreadable ]; then return 1; fi
      printf 'labeled 800\n'
      if [ -f episode ]; then cat episode; fi ;;
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
    'park-record guide-conflict-route')
      if [ -n "${LOOM_GUIDE_FIXTURE_DAEMON:-}" ]; then
        "$LOOM_GUIDE_FIXTURE_DAEMON" "$@"
        return
      fi
      if [[ "$4" =~ ^feature/(issue|harness-ops)-[0-9]+$ ]]; then
        echo stacked
      elif [ "${FIXTURE_POLICY:-}" = unreadable ]; then
        return 1
      elif [ "${LOOM_REVIEW_CONFLICT_RECONCILE:-}" = off ] ||
        [ "${FIXTURE_POLICY:-}" = disabled ] ||
        { [ "${FIXTURE_POLICY:-}" = recheck ] && [ -s edits ]; }; then
        echo disabled
      else
        echo enabled
      fi ;;
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
must_fail pr_record_park_clearance 2 "${receipt/800/799}" evidence.md
expect_label 2 loom:blocked
pr_record_park_clearance 2 "$receipt" evidence.md
for n in 1 2 9; do
  must_fail expect_label "$n" loom:blocked
  must_fail expect_label "$n" loom:review-requested
  expect_label "$n" loom:changes-requested
  expect_label "$n" loom:merge-conflict
done
expect_label 1 loom:ci-failure
for n in 3 4 5 6 7 8 10 11 12 13; do expect_label "$n" loom:blocked; done
must_fail rg -q '^(3|4|5|6|7|8|12|13) ' edits
must_fail rg -q '^11 --remove-label' edits
jq -es 'all(.[]; .pr != 6)' "$COMMENTS" >/dev/null

check_and_unblock_prs
must_fail expect_label 10 loom:blocked
expect_label 10 loom:merge-conflict
jq -es '[.[] | select(.body | startswith("<!-- loom:guide-conflict-handoff")) | .pr] | sort == [1,2,9,10,11]' "$COMMENTS" >/dev/null
jq -es 'all(.[]; (.body | startswith("<!-- loom:guide-conflict-handoff") | not) or (.body|contains("Doctor") and contains("loom:merge-conflict")))' "$COMMENTS" >/dev/null
# A comment quoting a record cannot establish clearance for the reason-only park.
reset_rows
park_hash=$(printf 'park-2' | git hash-object --stdin)
receipt=$(pr_park_clearance_marker "$fixture_head" "$park_hash" 800)
quoted=$(printf 'I did not clear this park. Example only.\n```text\n%s\n```' "$receipt")
jq -nc --arg body "$quoted" '{pr:2,body:$body}' >> "$COMMENTS"
check_and_unblock_prs 2 > candidates
expect_label 2 loom:blocked
test ! -s edits

# The interrupted Doctor pair is only resumable with this episode's first-line record.
for kind in quoted stale; do
  reset_rows
  set_labels 9 '["loom:blocked","loom:review-requested","loom:changes-requested","loom:merge-conflict"]'
  park_hash=$(printf 'park-9' | git hash-object --stdin)
  marker="<!-- loom:guide-conflict-handoff head=$fixture_head park=$park_hash event=800 -->"
  if [ "$kind" = quoted ]; then
    marker=$(printf 'Example only. Do not lift the park.\n```text\n%s\n```' "$marker")
  else
    printf 'unlabeled 801\nlabeled 802\n' > episode
  fi
  jq -nc --arg body "$marker" '{pr:9,body:$body}' >> "$COMMENTS"
  check_and_unblock_prs 9
  expect_label 9 loom:blocked
  test ! -s edits
done

# Both native branch families are protected, as is a missing base.
for branch in feature/issue-42 feature/harness-ops-42 ''; do
  reset_rows
  FIXTURE_BASE_REF="$branch" check_and_unblock_prs 9
  expect_label 9 loom:blocked
  test ! -s edits
done
reset_rows
FIXTURE_BASE_REF=feature/issue-42-extra check_and_unblock_prs 9
must_fail expect_label 9 loom:blocked

# A re-park during the edit has the same head, body, base and visible labels.
reset_rows
RECREATE_FOR=9 check_and_unblock_prs 9
expect_label 9 loom:blocked
rg -q '^9 --add-label' edits
must_fail rg -q '^9 --remove-label' edits

reset_rows
STACK_AFTER_ADD=9 check_and_unblock_prs 9
expect_label 9 loom:blocked
must_fail rg -q '^9 --remove-label' edits

for policy in disabled unreadable recheck; do
  reset_rows
  if [ -n "${LOOM_GUIDE_FIXTURE_DAEMON:-}" ]; then
    case "$policy" in
      disabled) printf '%s\n' '{"autonomous":{"workFinder":{"enabled":false},"roleRunner":{"roles":["guide"]}}}' > .loom-project/project.json ;;
      unreadable) printf '%s\n' '{unreadable' > .loom-project/project.json ;;
      recheck) continue ;;
    esac
  fi
  FIXTURE_POLICY="$policy" check_and_unblock_prs 9
  expect_label 9 loom:blocked
  must_fail rg -q '^9 --remove-label' edits
  if [ "$policy" != recheck ]; then test ! -s edits; fi
done

reset_rows
FIXTURE_EVENTS=unreadable check_and_unblock_prs 9
expect_label 9 loom:blocked
test ! -s edits
printf '%s\n' 'All conflict handoff assertions passed'
