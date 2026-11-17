#!/usr/bin/env bash
# test-guide-work-plan-held-issue-suffix.sh - Regression test for issue #13908
#
# guide.md's `render_plan_body()` (WORK_PLAN.md's machine-generated region)
# consulted the hold-label set only in the `ready_json` query; the `vimp`,
# `imp`, `curated`, `architect`, `hermit` and `epics` queries filtered
# `loom:frozen` alone. An issue carrying any OTHER registry hold
# (`loom:blocked`, `loom:operator`, retired names until `labels migrate`,
# #11795) still rendered under Very Important / Important / the proposal and
# epic sections as takeable work, with no indication it is held. In a live
# workspace measured on 2026-11-15, 7 of 8 `## Important` entries and the
# sole `## Very Important` entry were on a hold, all rendered as takeable.
#
# THE FIX (the issue's smaller option, per its curation): held entries stay
# in place but gain a ` *(held)*` suffix — the convention ` *(curated)*` /
# ` *(hermit)*` already set. The hold set is the registry's full list
# (`$hold_q` from `loom-daemon labels list --property hold --with-retired`,
# hoisted above its first consumer), never a hardcoded label list;
# `loom:frozen` stays fully excluded by each query's own select(). Because
# gh's `--jq` cannot bind jq variables, the set rides inline as a JSON array
# literal (`$hold_names`, built by jq) inside `$held_sfx`, the suffix
# expression appended to each query's `--jq` program.
#
# Verifies that:
#   1. STRUCTURE: `render_plan_body()` derives the suffix from `$hold_q`
#      (registry + fallback), and the six priority/proposal/epic queries
#      append `$held_sfx` while keeping their `loom:frozen` select().
#   2. THE REGRESSION, executed rather than grepped: the REAL
#      `render_plan_body()` (extracted from guide.md and eval'd against a
#      stubbed `$GH_READ` + `loom-daemon`) suffixes held `loom:important` /
#      `loom:very-important` / curated / architect / hermit / epic entries
#      with ` *(held)*`, composing after ` *(curated)*` / ` *(architect)*` /
#      ` *(hermit)*` where those exist.
#   3. UNHELD ENTRIES BYTE-IDENTICAL: an entry with no hold renders exactly
#      the pre-fix bullet (no trailing suffix); `loom:frozen` issues remain
#      absent; held entries still RENDER (suffix, not exclusion).
#   4. EDGE CASES: an issue carrying two hold labels gets ONE suffix; an
#      unmigrated retired hold name (registry set, not hardcoded) is
#      suffixed; when the daemon call fails, the literal fallback
#      (loom:blocked/loom:operator/loom:frozen) still suffixed `loom:blocked`
#      — and no longer suffixed the retired name, which the fallback cannot
#      know.
#   5. NEGATIVE CONTROL: the pre-fix jq pipeline (no `$held_sfx`) renders the
#      held issue with no suffix — i.e. this test would have failed before
#      the fix.
#
# Hermetic: `gh` and `loom-daemon` are stubbed with fixture JSON; only the
# real `jq` binary is invoked (skipped if unavailable) — no forge/network
# calls.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
# guide.md is shipped (installed at .claude/commands/loom/guide.md), so
# resolve the installed path first (consumer repos, and Loom's own dogfooded
# checkout), falling back to the defaults/ source-tree path (a bare source
# checkout with no .claude/commands/loom/ copy yet). See issue #6194 / #6241.
if [[ -f "$REPO_ROOT/.claude/commands/loom/guide.md" ]]; then
    GUIDE_MD="$REPO_ROOT/.claude/commands/loom/guide.md"
else
    GUIDE_MD="$REPO_ROOT/defaults/.claude/commands/loom/guide.md"
fi

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/assert.sh"

# Local assert_eq kept: takes (actual, expected, msg); shared assert_eq is (expected, actual, msg).
assert_eq() {
    local actual="$1" expected="$2" msg="$3"
    if [[ "$actual" == "$expected" ]]; then pass "$msg"; else fail "$msg (got '$actual', expected '$expected')"; fi
}

if [[ ! -f "$GUIDE_MD" ]]; then
    echo -e "${RED}FATAL${NC}: guide.md not found at $GUIDE_MD"
    exit 1
fi

# Same extraction the #6993/#5930/#6457/#7008 suites use: the function's
# closing `}` is the first line starting at column 0 after the opening line
# (its nested `section()`/`count()` helpers are indented).
RPB_BODY="$(awk '/^render_plan_body\(\) \{/{flag=1} flag{print} /^\}/{if(flag){exit}}' "$GUIDE_MD")"

if [[ -z "$RPB_BODY" ]]; then
    fail "could not extract render_plan_body() body from guide.md"
    echo -e "${RED}FATAL${NC}: cannot continue without the function body"
    exit 1
fi
pass "render_plan_body() body extracted from $GUIDE_MD"

# ---------------------------------------------------------------------------
# Test 1: STRUCTURE — the suffix derives from the registry hold set, and all
# six priority/proposal/epic queries append it after their frozen select.
# ---------------------------------------------------------------------------
echo ""
echo "Test 1: the six queries append \$held_sfx, built from \$hold_q (never hardcoded)"

# The suffix machinery: $hold_names is built by jq from $hold_q, and the
# suffix expression is a jq if/else on membership in that array.
if grep -qF "  hold_q=\$(loom-daemon labels list --property hold --with-retired 2>/dev/null || printf 'loom:blocked\\nloom:operator\\nloom:frozen')" <<<"$RPB_BODY"; then
    pass "hold_q is the registry's hold set (--with-retired), falling back to the three holds"
else
    fail "expected hold_q= to query the registry with --with-retired and the literal fallback"
fi

if grep -qF '  hold_names=$(printf '"'"'%s'"'"' "$hold_q" | jq -Rsc ' <<<"$RPB_BODY"; then
    pass "hold_names is derived from \$hold_q by jq, not a hand-written array"
else
    fail "expected hold_names= to be built from \$hold_q via jq -Rsc"
fi

HELD_SFX_DEF="held_sfx=' + (if any(.labels[]?.name; . as \$n | '\"\$hold_names\"' | index(\$n)) then \" *(held)*\" else \"\" end)'"
if grep -qF 'then " *(held)*" else "" end)' <<<"$RPB_BODY" \
   && grep -qF "$HELD_SFX_DEF" <<<"$RPB_BODY"; then
    pass "held_sfx is the *(held)* membership test over \$hold_names"
else
    fail "expected held_sfx= to append \" *(held)*\" on membership in \$hold_names"
fi

# Every one of the six queries keeps its frozen exclusion AND gains the
# suffix. The frozen select() must survive — the suffix must not become the
# only hold handling (frozen stays fully excluded, per the issue).
SIX_QUERIES="vimp imp curated architect hermit epics"
for q in $SIX_QUERIES; do
    jqline="$(grep -A1 "^  $q=" <<<"$RPB_BODY" | grep -- '--jq' || true)"
    if [[ -z "$jqline" ]]; then
        fail "could not find the --jq line of the $q= query"
        continue
    fi
    if grep -q 'select(any(.labels\[\]?.name; . == \\"loom:frozen\\") | not)' <<<"$jqline" 2>/dev/null \
       || grep -qF 'select(any(.labels[]?.name; . == "loom:frozen") | not)' <<<"$jqline"; then
        pass "the $q= query still fully excludes loom:frozen"
    else
        fail "expected the $q= query to keep its loom:frozen select()"
    fi
    if grep -qF "\$held_sfx\")" <<<"$jqline"; then
        pass "the $q= query appends \$held_sfx"
    else
        fail "expected the $q= query's --jq program to end with '\"\\\"\$held_sfx\") (got: $jqline)"
    fi
done

# The suffix must not be hardcoded to loom:blocked/loom:operator anywhere in
# the six queries' own lines (the set comes from the registry via
# $hold_names; only held_sfx's build site may name labels, and it does not).
for q in $SIX_QUERIES; do
    qline="$(grep -A1 "^  $q=" <<<"$RPB_BODY" | tr '\n' ' ')"
    if grep -qE '"loom:(blocked|operator)"' <<<"$qline"; then
        fail "the $q= query hardcodes a hold label (the set must come from \$held_sfx)"
    else
        pass "the $q= query hardcodes no hold label"
    fi
done

if ! command -v jq >/dev/null 2>&1; then
    echo ""
    echo "  SKIP: jq not available — skipping the executable suffix tests"
    echo ""
    echo "================================"
    echo "Tests run:    $TESTS_RUN"
    echo -e "Tests passed: ${GREEN}${TESTS_PASSED}${NC}"
    [[ $TESTS_FAILED -gt 0 ]] && { echo -e "Tests failed: ${RED}${TESTS_FAILED}${NC}"; exit 1; }
    echo "All tests passed"
    exit 0
fi

# ---------------------------------------------------------------------------
# Harness: a `gh` stub that serves per-label JSON fixtures (including a
# `labels` array on each item) from a directory, applies `--search
# "-label:X"` negation the way real `gh` combines it with `--label`, and
# honours `--jq` exactly like real `gh ... --json ... --jq EXPR` does.
# ---------------------------------------------------------------------------
TMPROOT="$(mktemp -d)"
trap 'rm -rf "$TMPROOT"' EXIT

GH_STUB="$TMPROOT/gh-stub"
cat > "$GH_STUB" <<'STUB'
#!/usr/bin/env bash
set -uo pipefail
kind="${1:-}"; shift || true
label=""; search=""; jqexpr=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --label)  label="${2:-}"; shift 2 ;;
    --search) search="${2:-}"; shift 2 ;;
    --jq)     jqexpr="${2:-}"; shift 2 ;;
    *)        shift ;;
  esac
done
fixture="$LOOM_FIXTURE_DIR/${kind}-${label//:/_}.json"
if [[ -f "$fixture" ]]; then json="$(cat "$fixture")"; else json='[]'; fi
for tok in $search; do
  case "$tok" in
    -label:*)
      excl="${tok#-label:}"
      json="$(printf '%s' "$json" | jq --arg l "$excl" '[.[] | select(((.labels // []) | map(.name) | index($l)) | not)]')"
      ;;
  esac
done
if [[ -n "$jqexpr" ]]; then printf '%s' "$json" | jq -r "$jqexpr"; else printf '%s' "$json"; fi
STUB
chmod +x "$GH_STUB"

mkdir -p "$TMPROOT/fixtures"
FIX="$TMPROOT/fixtures"

# `loom:important` fixture: #100 plain, #101 loom:blocked, #102
# loom:operator, #103 TWO hold labels (one suffix only), #104 loom:frozen
# (fully excluded), #105 an unmigrated retired hold name (registry-set
# coverage, not a hardcoded list).
cat > "$FIX/issue-loom_important.json" <<'JSON'
[
  {"number":100,"title":"Unheld important issue","labels":[{"name":"loom:important"}]},
  {"number":101,"title":"Blocked important issue","labels":[{"name":"loom:important"},{"name":"loom:blocked"}]},
  {"number":102,"title":"Operator important issue","labels":[{"name":"loom:important"},{"name":"loom:operator"}]},
  {"number":103,"title":"Doubly held important issue","labels":[{"name":"loom:important"},{"name":"loom:blocked"},{"name":"loom:operator"}]},
  {"number":104,"title":"Frozen important issue","labels":[{"name":"loom:important"},{"name":"loom:frozen"}]},
  {"number":105,"title":"Unmigrated hold important issue","labels":[{"name":"loom:important"},{"name":"loom:operator-only"}]}
]
JSON

# `loom:very-important` fixture: #200 held, #201 free.
cat > "$FIX/issue-loom_very-important.json" <<'JSON'
[
  {"number":200,"title":"Held very important issue","labels":[{"name":"loom:very-important"},{"name":"loom:operator"}]},
  {"number":201,"title":"Free very important issue","labels":[{"name":"loom:very-important"}]}
]
JSON

# Proposal/epic fixtures: one held entry each (curated gets a free one too,
# to pin the *(curated)*-only bullet).
cat > "$FIX/issue-loom_curated.json" <<'JSON'
[
  {"number":300,"title":"Held curated issue","labels":[{"name":"loom:curated"},{"name":"loom:blocked"}]},
  {"number":301,"title":"Free curated issue","labels":[{"name":"loom:curated"}]}
]
JSON
cat > "$FIX/issue-loom_architect.json" <<'JSON'
[{"number":400,"title":"Held architect proposal","labels":[{"name":"loom:architect"},{"name":"loom:blocked"}]}]
JSON
cat > "$FIX/issue-loom_hermit.json" <<'JSON'
[{"number":401,"title":"Held hermit proposal","labels":[{"name":"loom:hermit"},{"name":"loom:operator"}]}]
JSON
cat > "$FIX/issue-loom_epic.json" <<'JSON'
[{"number":500,"title":"Held epic","labels":[{"name":"loom:epic"},{"name":"loom:blocked"}]}]
JSON

# `loom-daemon labels list --property P --with-retired` stub: the registry's
# sets, retired names included (#11795) — the real daemon's answer when one
# is built, else those sets spelled out.
mkdir -p "$TMPROOT/bin"
REAL_DAEMON="${LOOM_TEST_REAL_DAEMON:-${LOOM_DAEMON_BIN:-}}"
for prop in hold operator_gate; do
    if [[ -n "$REAL_DAEMON" && -x "$REAL_DAEMON" ]]; then
        "$REAL_DAEMON" labels list --property "$prop" --with-retired >"$TMPROOT/registry-$prop"
    elif [[ "$prop" == "hold" ]]; then
        printf '%s\n' loom:blocked loom:operator loom:frozen loom:operator-only loom:operator-decision \
            loom:operator-mechanical loom:operator-blocked loom:needs-capability >"$TMPROOT/registry-$prop"
    else
        printf '%s\n' loom:operator loom:operator-only loom:operator-decision loom:operator-mechanical \
            loom:operator-blocked >"$TMPROOT/registry-$prop"
    fi
done
printf '#!/usr/bin/env bash\n[ "$1 $2 $3 ${5:-}" = "labels list --property --with-retired" ] || exit 2\ncat "%s/registry-$4"\n' \
    "$TMPROOT" >"$TMPROOT/bin/loom-daemon"
chmod +x "$TMPROOT/bin/loom-daemon"

RENDER="$(PATH="$TMPROOT/bin:$PATH" LOOM_FIXTURE_DIR="$FIX" GH_READ="$GH_STUB" RPB_SRC="$RPB_BODY" bash -c 'eval "$RPB_SRC"; render_plan_body')"

if [[ -z "$RENDER" ]]; then
    fail "render_plan_body() produced no output against the fixtures (harness broken)"
    echo -e "${RED}FATAL${NC}: cannot continue without a render"
    exit 1
else
    pass "render_plan_body() executed against the stubbed \$GH_READ"
fi

VIMP_BODY="$(awk '/^## Very Important/{flag=1; next} /^## /{flag=0} flag' <<<"$RENDER")"
IMP_BODY="$(awk '/^## Important/{flag=1; next} /^## /{flag=0} flag' <<<"$RENDER")"
CURATED_BODY="$(awk '/^## Proposed$/{flag=1; next} /^## /{flag=0} flag' <<<"$RENDER")"
PROPOSALS_BODY="$(awk '/^## Proposed \(Architect \/ Hermit\)/{flag=1; next} /^## /{flag=0} flag' <<<"$RENDER")"
EPICS_BODY="$(awk '/^## Epics/{flag=1; next} /^## /{flag=0} flag' <<<"$RENDER")"

# ---------------------------------------------------------------------------
# Test 2 (AC): held entries render with the *(held)* suffix
# ---------------------------------------------------------------------------
echo ""
echo "Test 2: held entries gain *(held)* in every priority/proposal/epic section"

expect_line() {
    # expect_line <haystack> <needle-line> <msg>: the exact line must appear.
    local haystack="$1" needle="$2" msg="$3"
    if grep -qxF -- "$needle" <<<"$haystack"; then
        pass "$msg"
    else
        fail "$msg (expected exact line '$needle' in section:
$haystack)"
    fi
}

expect_line "$VIMP_BODY" '- **#200**: Held very important issue *(held)*' \
    "held loom:very-important entry is suffixed *(held)*"
expect_line "$IMP_BODY" '- **#101**: Blocked important issue *(held)*' \
    "loom:blocked loom:important entry is suffixed *(held)*"
expect_line "$IMP_BODY" '- **#102**: Operator important issue *(held)*' \
    "loom:operator loom:important entry is suffixed *(held)*"
expect_line "$IMP_BODY" '- **#105**: Unmigrated hold important issue *(held)*' \
    "an unmigrated retired hold name (registry set, not hardcoded) is suffixed *(held)*"
expect_line "$CURATED_BODY" '- **#300**: Held curated issue *(curated)* *(held)*' \
    "held curated entry composes *(curated)* *(held)*"
expect_line "$PROPOSALS_BODY" '- **#400**: Held architect proposal *(architect)* *(held)*' \
    "held architect entry composes *(architect)* *(held)*"
expect_line "$PROPOSALS_BODY" '- **#401**: Held hermit proposal *(hermit)* *(held)*' \
    "held hermit entry composes *(hermit)* *(held)*"
expect_line "$EPICS_BODY" '- **#500**: Held epic *(held)*' \
    "held epic entry is suffixed *(held)*"

# An issue carrying TWO hold labels gets ONE suffix, not two.
doubly="$(grep -F -- '- **#103**:' <<<"$IMP_BODY" || true)"
# `tr -d ' '`: BSD `wc -l` (macOS) left-pads its count, so a bare compare to "1" fails there.
suffix_count="$(grep -oF ' *(held)*' <<<"$doubly" | wc -l | tr -d ' ')"
assert_eq "$suffix_count" "1" "an issue with two hold labels carries exactly one *(held)* suffix (got: $doubly)"

# The suffix decorates; it must not EXCLUDE — held issues still render.
if grep -q '#101' <<<"$IMP_BODY"; then
    pass "held entries still render in place (suffix, not exclusion)"
else
    fail "#101 must still appear in the Important section (the fix suffixes, it does not remove)"
fi

# ---------------------------------------------------------------------------
# Test 3 (AC): unheld entries render byte-identically; frozen stays absent
# ---------------------------------------------------------------------------
echo ""
echo "Test 3: unheld entries render byte-identically; loom:frozen stays omitted"

expect_line "$IMP_BODY" '- **#100**: Unheld important issue' \
    "unheld loom:important entry is byte-identical to the pre-fix bullet (no suffix)"
expect_line "$VIMP_BODY" '- **#201**: Free very important issue' \
    "unheld loom:very-important entry has no suffix"
expect_line "$CURATED_BODY" '- **#301**: Free curated issue *(curated)*' \
    "unheld curated entry keeps exactly its *(curated)* bullet"

if grep -q '#104' <<<"$IMP_BODY"; then
    fail "#104 (loom:important + loom:frozen) must remain fully excluded"
else
    pass "#104 (loom:important + loom:frozen) remains absent from the Important section"
fi

# ---------------------------------------------------------------------------
# Test 4 (edge): daemon failure falls back to the three-hold literal list
# ---------------------------------------------------------------------------
echo ""
echo "Test 4: daemon call failure uses the fallback hold list"

mkdir -p "$TMPROOT/bin-fail"
printf '#!/usr/bin/env bash\nexit 2\n' >"$TMPROOT/bin-fail/loom-daemon"
chmod +x "$TMPROOT/bin-fail/loom-daemon"

RENDER_FALLBACK="$(PATH="$TMPROOT/bin-fail:$PATH" LOOM_FIXTURE_DIR="$FIX" GH_READ="$GH_STUB" RPB_SRC="$RPB_BODY" bash -c 'eval "$RPB_SRC"; render_plan_body')"
IMP_FALLBACK="$(awk '/^## Important/{flag=1; next} /^## /{flag=0} flag' <<<"$RENDER_FALLBACK")"

expect_line "$IMP_FALLBACK" '- **#101**: Blocked important issue *(held)*' \
    "fallback list still suffixes loom:blocked when the daemon call fails"
if grep -qF -- '- **#105**: Unmigrated hold important issue *(held)*' <<<"$IMP_FALLBACK"; then
    fail "the retired name loom:operator-only is NOT in the fallback list — it must not be suffixed there"
else
    pass "fallback list does not suffix the retired name (it only knows the three holds)"
fi

# ---------------------------------------------------------------------------
# Test 5: NEGATIVE CONTROL — the pre-fix pipeline renders no suffix
# ---------------------------------------------------------------------------
echo ""
echo "Test 5: negative control — the pre-fix query renders no suffix (bug reproduced)"

PRE="$(LOOM_FIXTURE_DIR="$FIX" "$GH_STUB" issue list --label "loom:important" --state open --limit 200 \
    --json number,title,labels --jq 'sort_by(.number) | .[] | select(any(.labels[]?.name; . == "loom:frozen") | not) | "- **#\(.number)**: \(.title)"')"

if grep -qF -- '- **#101**: Blocked important issue' <<<"$PRE" && ! grep -qF ' *(held)*' <<<"$PRE"; then
    pass "without \$held_sfx, #101 WOULD have rendered as takeable with no indication (bug reproduced)"
else
    fail "expected the pre-fix pipeline to render #101 with no *(held)* suffix"
fi

# ---------------------------------------------------------------------------
echo ""
echo "================================"
echo "Tests run:    $TESTS_RUN"
echo -e "Tests passed: ${GREEN}${TESTS_PASSED}${NC}"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "Tests failed: ${RED}${TESTS_FAILED}${NC}"
    exit 1
fi
echo "All tests passed"
exit 0
