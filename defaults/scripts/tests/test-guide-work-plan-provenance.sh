#!/usr/bin/env bash
# test-guide-work-plan-provenance.sh - Regression test for issue #13923
#
# On a shared host, one repo's Guide docs tick published another repo's
# WORK_PLAN.md render (observed on a consumer repo): unpinned `gh` queries,
# literal /tmp scratch paths, and no check that the rendered `#N` are this
# repo's. Verifies (stubbed `gh`, hermetic):
#   1. no unpinned `$GH_READ ... list` remains in the docs-maintenance steps,
#      and `_work_log_search_total_count` no longer reads the cwd remote.
#   2. verify_plan_provenance() ACCEPTS a body whose titles match the pinned
#      repo, REJECTS a foreign body whose numbers all exist under other titles,
#      fails closed when the title lookup fails, and passes a body with no #N.
#      Every ` *(x)*` suffix render_plan_body() can emit (read from its source,
#      so a new one cannot be added without the gate learning it) is accepted,
#      alone and combined with ` *(held)*` (#13908); a wrong order, an unknown
#      suffix and a held bullet under a foreign title are rejected.
#   3. splice_plan_body() writes through a per-tick scratch dir (removed after),
#      replaces only the marker region, and leaves the file untouched when the
#      markers are missing.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
if [[ -f "$REPO_ROOT/.claude/commands/loom/guide.md" ]]; then
    GUIDE_MD="$REPO_ROOT/.claude/commands/loom/guide.md"
else
    GUIDE_MD="$REPO_ROOT/defaults/.claude/commands/loom/guide.md"
fi
source "$SCRIPT_DIR/lib/assert.sh"

extract() { awk -v re="^$1\\\\(\\\\) \\\\{" '$0 ~ re {f=1} f{print} /^\}/{if(f) exit}' "$GUIDE_MD"; }

VPP="$(extract verify_plan_provenance)"
SPB="$(extract splice_plan_body)"
[[ -n "$VPP" && -n "$SPB" ]] || { echo "FATAL: helpers not found in guide.md"; exit 1; }
pass "helpers extracted from $GUIDE_MD"

echo "Test 1: queries are pinned"
# Docs-maintenance region: from Step 1's repo derivation to check_readme.
REGION="$(awk '/^GUIDE_REPO=/{f=1} f{print} /^check_readme_staleness\(\)/{if(f) exit}' "$GUIDE_MD")"
grep -q '^export GH_REPO="\$GUIDE_REPO"' <<<"$REGION" && pass "GH_REPO pinned to the docs-worktree repo for all list queries" || fail "GH_REPO not exported from GUIDE_REPO"
CNT="$(extract _work_log_search_total_count)"
if grep -q 'git remote get-url origin' <<<"$CNT"; then fail "_work_log_search_total_count still reads the cwd remote"; else pass "_work_log_search_total_count does not read the cwd remote"; fi

# Stub: forge titles for the pinned repo; FAIL_LOOKUP=1 simulates an API failure.
STUB_DIR="$(mktemp -d)"
trap 'rm -rf "$STUB_DIR"' EXIT
GH_STUB="$STUB_DIR/gh"
: > "$GH_STUB.calls"
cat >"$GH_STUB" <<'EOF'
#!/usr/bin/env bash
[[ "${FAIL_LOOKUP:-}" == 1 ]] && exit 1
if [[ "$2" == list ]]; then
  if [[ "$1 $3 $4" == "pr --label loom:pr" ]]; then
    if [[ "${!#}" == number,closingIssuesReferences ]]; then
      echo lookup >> "$0.calls"
      [[ "${FAIL_REFS:-}" == 1 ]] && exit 1
      jq 'map({number, closingIssuesReferences})' <<< "${VERIFY_PRS:-$APPROVED_PRS}"
    else
      jq -r "${!#}" <<< "$APPROVED_PRS"  # render_plan_body's --jq
    fi
  else
    jq -r "${!#}" <<< '[]'
  fi
  exit $?
fi
case "$2" in
  repos/owner/mine/issues/1) echo "Add widget" ;;
  repos/owner/mine/issues/2) echo "Fix gizmo" ;;
  repos/owner/mine/issues/3) echo "Odd title *(held)*" ;;
  repos/owner/mine/issues/4) echo "Title — closes #42" ;;
  repos/owner/mine/issues/13582) echo "fix: honour a trusted loom:issue adoption in the promotion author gate" ;;
  *) exit 1 ;;
esac
EOF
chmod +x "$GH_STUB"
export GH_READ="$GH_STUB" GUIDE_REPO="owner/mine"
eval "$VPP"

echo "Test 2: verify_plan_provenance"
verify_plan_provenance $'## Ready\n\n- **#1**: Add widget\n- **#2**: Fix gizmo *(curated)*' 2>/dev/null \
  && pass "matching titles accepted" || fail "matching titles rejected"
verify_plan_provenance $'- **#1**: Some other repo\x27s title\n- **#2**: Fix gizmo' 2>/dev/null \
  && fail "foreign titles (numbers exist) accepted" || pass "foreign titles rejected even though #N exist"
FAIL_LOOKUP=1 verify_plan_provenance '- **#1**: Add widget' 2>/dev/null \
  && fail "failed lookup accepted" || pass "failed title lookup fails closed"
verify_plan_provenance $'## None\n\n_None._' 2>/dev/null \
  && pass "body with zero #N accepted" || fail "zero-#N body rejected"

echo "Test 2b: every suffix render_plan_body() emits is accepted (#13908 lockstep)"
ok() { verify_plan_provenance "$1" 2>/dev/null && pass "accepted: ${2:-$1}" || fail "rejected: ${2:-$1}"; }
no() { verify_plan_provenance "$1" 2>/dev/null && fail "accepted: ${2:-$1}" || pass "rejected: ${2:-$1}"; }
# The suffix set comes from render_plan_body()'s own source, never a list kept
# here: a suffix added to the render without teaching the gate fails this test.
RPB="$(extract render_plan_body)"
TAGS="$(grep -v '^ *#' <<<"$RPB" | grep -oE ' \*\([a-z-]+\)\*' | sort -u | sed -E 's/^ \*\(|\)\*$//g')"
grep -qx held <<<"$TAGS" && pass "render emits the *(held)* suffix" || fail "render no longer emits *(held)* (test assumptions stale)"
for want_tag in curated architect hermit; do
  grep -qx "$want_tag" <<<"$TAGS" && pass "render emits *($want_tag)*" || fail "render no longer emits *($want_tag)*"
done
ok '- **#1**: Add widget *(held)*'
while IFS= read -r t; do
  [[ "$t" == held ]] && continue
  ok "- **#1**: Add widget *($t)*"
  ok "- **#1**: Add widget *($t)* *(held)*"
  no "- **#1**: Add widget *(held)* *($t)*"
  no "- **#1**: Foreign title *($t)* *(held)*"
done <<<"$TAGS"
ok $'## Very Important\n\n- **#1**: Add widget *(held)*\n- **#2**: Fix gizmo *(curated)* *(held)*\n- **#2**: Fix gizmo'
ok '- **#3**: Odd title *(held)*'
ok '- **#3**: Odd title *(held)* *(held)*'
no '- **#1**: Foreign title *(held)*'
no '- **#1**: Add widget *(held)* *(held)*'
no '- **#1**: Add widget *(curated)* *(hermit)*'
no '- **#1**: Add widget *(bogus)*'
no '- **#1**: Add widget extra'
no '- **#1**: Add'
FAIL_LOOKUP=1 verify_plan_provenance '- **#1**: Add widget *(held)*' 2>/dev/null \
  && fail "failed lookup accepted for a held bullet" || pass "failed lookup still fails closed for a held bullet"

echo "Test 2c: verify the rendered Approved section against forge closing refs"
# #13582 is the reported PR (title and closing ref captured with gh pr view).
# The other rows exercise unsorted refs and a literal title with no annotation.
export APPROVED_PRS='[
  {"number":13582,"title":"fix: honour a trusted loom:issue adoption in the promotion author gate","closingIssuesReferences":[{"number":13578}]},
  {"number":1,"title":"Add widget","closingIssuesReferences":[{"number":42},{"number":7}]},
  {"number":4,"title":"Title — closes #42","closingIssuesReferences":[]}
]'
eval "$RPB"
approved_body="$(render_plan_body | awk '/^## / {approved=($0 == "## Approved (Awaiting Merge)")} approved')"
assert_contains "$approved_body" 'Add widget — closes #7, #42' "rendered both refs in numeric order"
assert_contains "$approved_body" 'gate — closes #13578' "rendered the reported single-ref row"
assert_contains "$approved_body" '- **#4**: Title — closes #42' "rendered literal closes title"
ok "$approved_body" "genuine Approved rows"
assert_eq 1 "$(wc -l < "$GH_STUB.calls" | tr -d ' ')" "one refs lookup for all Approved rows"
for changed in ' — closes #7, #43' ' — closes #7' '' ' — closes #7, #42, #43' ' — closes #7 #42' ' *(bogus)*'; do
  no "${approved_body/Add widget — closes #7, #42/Add widget$changed}" "altered closing annotation [$changed]"
done
no "${approved_body/Add widget/Foreign title}" "wrong title with correct refs"
FAIL_REFS=1 no "$approved_body" "unavailable refs"
VERIFY_PRS='[]' no "$approved_body" "missing PRs"
VERIFY_PRS="$(jq 'map(if .number == 4 then del(.closingIssuesReferences) else . end)' <<<"$APPROVED_PRS")" \
  no "$approved_body" "missing refs field on a plain-title row"
# A held PR may appear in both sections, with refs only in Approved.
ok "$approved_body"$'\n## Operator Attention: Merge-Risk-Hold Pileup\n- **#1**: Add widget' "same PR in Approved and operator hold sections"
no $'## Ready\n- **#1**: Add widget — closes #7, #42'

echo "Test 3: splice_plan_body"
eval "$SPB"
DOCS_WT="$STUB_DIR"
printf 'head\n<!-- guide:plan-body:start -->\nOLD\n<!-- guide:plan-body:end -->\ntail\n' >"$DOCS_WT/WORK_PLAN.md"
BEFORE="$(ls -d "${TMPDIR:-/tmp}"/guide-plan.* 2>/dev/null | wc -l)"
splice_plan_body $'NEW1\nNEW2' && pass "splice succeeded" || fail "splice failed"
assert_eq "$(cat "$DOCS_WT/WORK_PLAN.md")" $'head\n<!-- guide:plan-body:start -->\nNEW1\nNEW2\n<!-- guide:plan-body:end -->\ntail' "only marker region replaced"
AFTER="$(ls -d "${TMPDIR:-/tmp}"/guide-plan.* 2>/dev/null | wc -l)"
assert_eq "$AFTER" "$BEFORE" "per-tick scratch dir removed"
printf 'no markers\n' >"$DOCS_WT/WORK_PLAN.md"
splice_plan_body 'X' && fail "splice without markers succeeded" || pass "missing markers refused"
assert_eq "$(cat "$DOCS_WT/WORK_PLAN.md")" "no markers" "file untouched when markers missing"

echo
echo "Tests run: $TESTS_RUN, passed: $TESTS_PASSED, failed: $TESTS_FAILED"
[[ "$TESTS_FAILED" -eq 0 ]] || exit 1
echo "All tests passed"
