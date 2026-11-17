#!/usr/bin/env bash
# test-check-verified-corrections-preserved.sh - Unit tests for
# check-verified-corrections-preserved.sh, the diff-before-rewrite guard for
# the Curator's append-only "## Verified corrections" convention (#4135).
#
# This is the "scenario" acceptance criterion #4135 AC4 asks for: it exercises
# the incident that motivated the issue (#4042 -- a second Curator pass
# rewrote a body and silently dropped three live-verified findings) as a
# concrete, checkable regression test, plus the surrounding edge cases
# (append-only success, whole-section removal, no section to begin with, a
# dated counter-finding that supersedes without deleting).
#
# Usage:
#   ./.loom/scripts/tests/test-check-verified-corrections-preserved.sh

set -uo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$TEST_DIR/.." && pwd)"
CVCP="$SCRIPTS_DIR/check-verified-corrections-preserved.sh"

# shellcheck source=lib/assert.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/assert.sh"

if [[ ! -x "$CVCP" ]]; then
    echo -e "${RED}FAIL${NC}: check-verified-corrections-preserved.sh missing or not executable at $CVCP"
    exit 1
fi

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

run_cvcp() {
    OUT="$("$CVCP" "$@" 2>&1)"
    RC=$?
}

echo "=== check-verified-corrections-preserved.sh ==="
echo ""

# --- Scenario 1: the #4042 incident, reconstructed ------------------------
# Pass 1 records three live-verified findings. Pass 2 rewrites the body and
# (as actually happened) drops all three -- this must fail with all three
# named, not just report "something changed".
cat > "$WORK_DIR/4042-pass1.md" <<'EOF'
## Problem

`loom-daemon-update.sh` cannot manage a launchd-installed daemon.

## Verified corrections

- **2026-07-XX, verified via `launchctl print`**: `KeepAlive = false` in the
  live plist, contradicting the assumption that the daemon self-restarts.

- **2026-07-XX, verified via `--print-plist`**: no `LOOM_DAEMON_SUPERVISOR`
  variable is present in the rendered plist.

- **2026-07-XX, verified via `--print-plist` diff**: six autonomy variables
  (`LOOM_WORK_FINDER`, `LOOM_MAIN_HEALTH_GATE`, and four others) are present
  in the live plist but the updater never reads or re-renders them.

## Acceptance Criteria

- [ ] Updater preserves the six autonomy vars across an update
EOF

cat > "$WORK_DIR/4042-pass2-bad.md" <<'EOF'
## Problem

`loom-daemon-update.sh` cannot manage a launchd-installed daemon.

The updater needs no flag replay and no bootout/bootstrap; plist parsing
should not be reimplemented.

## Acceptance Criteria

- [ ] Confirm the updater's current approach is sufficient
EOF

run_cvcp "$WORK_DIR/4042-pass1.md" "$WORK_DIR/4042-pass2-bad.md"
assert_eq "1" "$RC" "(1a) Reconstructed #4042 pass-2 rewrite (whole section dropped) -> exit 1"
assert_contains "$OUT" "has none at all" "(1a) Failure names the section as entirely absent, not just changed"

# --- Scenario 2: a compliant re-curation pass appends, drops nothing ------
cat > "$WORK_DIR/4042-pass2-good.md" <<'EOF'
## Problem

`loom-daemon-update.sh` cannot manage a launchd-installed daemon.

## Verified corrections

- **2026-07-XX, verified via `launchctl print`**: `KeepAlive = false` in the
  live plist, contradicting the assumption that the daemon self-restarts.

- **2026-07-XX, verified via `--print-plist`**: no `LOOM_DAEMON_SUPERVISOR`
  variable is present in the rendered plist.

- **2026-07-XX, verified via `--print-plist` diff**: six autonomy variables
  (`LOOM_WORK_FINDER`, `LOOM_MAIN_HEALTH_GATE`, and four others) are present
  in the live plist but the updater never reads or re-renders them.

- **2026-08-01, re-verified after #4090 merged**: the updater now re-renders
  the plist from the live host state instead of the invoking shell's
  environment, closing the six-var gap identified above.

## Acceptance Criteria

- [ ] Updater preserves the six autonomy vars across an update (closed by #4090)
EOF

run_cvcp "$WORK_DIR/4042-pass1.md" "$WORK_DIR/4042-pass2-good.md"
assert_eq "0" "$RC" "(2a) Compliant re-curation pass (all 3 preserved + 1 appended) -> exit 0"
assert_contains "$OUT" "OK" "(2a) Success message reported"

# --- Scenario 3: dated counter-finding supersedes without deleting --------
# A later pass disagrees with one entry. It must ADD a dated counter-finding,
# not edit/replace the original -- the original stays verbatim.
cat > "$WORK_DIR/counter-finding.md" <<'EOF'
## Problem

Something is broken.

## Verified corrections

- **2026-07-27, verified against `origin/main` @ `a1b2c3d`**: the retry
  budget is hardcoded to 3.

- **2026-08-02, supersedes the 2026-07-27 entry above (re-verified after
  #4200 merged)**: the retry budget is now a configurable
  `RETRY_BUDGET` env var, default 3. The 07-27 finding was correct for the
  code at the time.

## Acceptance Criteria

- [ ] N/A
EOF

cat > "$WORK_DIR/counter-finding-old.md" <<'EOF'
## Problem

Something is broken.

## Verified corrections

- **2026-07-27, verified against `origin/main` @ `a1b2c3d`**: the retry
  budget is hardcoded to 3.

## Acceptance Criteria

- [ ] N/A
EOF

run_cvcp "$WORK_DIR/counter-finding-old.md" "$WORK_DIR/counter-finding.md"
assert_eq "0" "$RC" "(3a) Dated counter-finding appended alongside the original -> exit 0 (original preserved)"

# --- Scenario 4: no '## Verified corrections' section in the old body -----
# The common case -- most issues never accumulate this section. Must be a
# silent no-op, not a failure.
cat > "$WORK_DIR/no-section-old.md" <<'EOF'
## Problem

Nothing verified yet, just a first pass.

## Acceptance Criteria

- [ ] TBD
EOF

cat > "$WORK_DIR/no-section-new.md" <<'EOF'
## Problem

Nothing verified yet, just a first pass -- refreshed wording.

## Acceptance Criteria

- [ ] TBD, still
EOF

run_cvcp "$WORK_DIR/no-section-old.md" "$WORK_DIR/no-section-new.md"
assert_eq "0" "$RC" "(4a) Old body has no Verified corrections section -> exit 0 (no-op)"
assert_contains "$OUT" "nothing to preserve" "(4a) No-op reported explicitly"

# --- Scenario 5: heading is case-insensitive -------------------------------
cat > "$WORK_DIR/case-old.md" <<'EOF'
## Verified Corrections

- **2026-07-27**: a finding.
EOF

cat > "$WORK_DIR/case-new-good.md" <<'EOF'
## verified corrections

- **2026-07-27**: a finding.

- **2026-08-01**: an appended finding.
EOF

run_cvcp "$WORK_DIR/case-old.md" "$WORK_DIR/case-new-good.md"
assert_eq "0" "$RC" "(5a) Heading match is case-insensitive across old/new -> exit 0"

# --- Scenario 6: usage errors -----------------------------------------------
run_cvcp "$WORK_DIR/case-old.md"
assert_eq "2" "$RC" "(6a) Missing second argument -> exit 2 (usage error)"

run_cvcp "$WORK_DIR/does-not-exist.md" "$WORK_DIR/case-new-good.md"
assert_eq "2" "$RC" "(6b) Nonexistent old-body file -> exit 2 (usage error)"

# --- Scenario 7: large section, byte-identical OLD/NEW -> exit 0 (#6768) --
# Regression test for the SIGPIPE/pipefail false-positive: under
# `set -euo pipefail`, a `grep -qFx | tr ...` pipeline (the pre-fix
# implementation) can report a false FAIL once the "## Verified corrections"
# section grows large enough, because `grep -q` exits as soon as it finds its
# match and SIGPIPEs the upstream `tr`, which then makes the *pipeline's*
# exit status nonzero even though the match was found. A large, byte-identical
# section is the minimal repro shape for that.
gen_large_section() {
    local n="$1"
    echo "## Verified corrections"
    echo ""
    local i
    for ((i = 1; i <= n; i++)); do
        echo "- **2026-08-1${i}, verified via check #${i}**: finding number ${i} confirmed against the live host, paragraph padding text to make this entry realistically sized rather than a one-liner."
        echo ""
    done
}

gen_large_section 300 > "$WORK_DIR/large-section.md"

run_cvcp "$WORK_DIR/large-section.md" "$WORK_DIR/large-section.md"
assert_eq "0" "$RC" "(7a) Large (300-paragraph) section, byte-identical OLD/NEW -> exit 0 (no false-positive FAIL)"
assert_contains "$OUT" "OK" "(7a) Success message reported for the large identical section"

# --- Scenario 8: early-paragraph match in a large section (#6768) ---------
# The specific case that triggers the SIGPIPE/pipefail bug: the very first
# (or second) paragraph in the OLD section's search loop matches a paragraph
# that also appears early in the NEW paragraph stream, so `grep -q` returns
# almost immediately -- maximizing the chance the upstream `tr` is still
# writing when it gets SIGPIPE'd. A size-only test that never checks an early
# match specifically would not catch a regression back to the buggy pipe.
{
    echo "## Verified corrections"
    echo ""
    echo "- **2026-08-01, verified via check #1**: the very first finding, which must be located correctly even though hundreds of other paragraphs follow it in the stream."
    echo ""
    gen_large_section 300 | tail -n +3
} > "$WORK_DIR/early-match-old.md"

cp "$WORK_DIR/early-match-old.md" "$WORK_DIR/early-match-new.md"

run_cvcp "$WORK_DIR/early-match-old.md" "$WORK_DIR/early-match-new.md"
assert_eq "0" "$RC" "(8a) Early-paragraph (1st entry) match found correctly in a large section -> exit 0"
assert_contains "$OUT" "OK" "(8a) Success message reported for the early-match case"

# --- Scenario 9: genuine missing paragraph in a large section still FAILs -
# The fix must not loosen detection -- a real gap in a large section must
# still be reported, named, and exit 1.
gen_large_section 300 > "$WORK_DIR/large-section-missing-old.md"
gen_large_section 300 | grep -v "finding number 2 confirmed" > "$WORK_DIR/large-section-missing-new.md"

run_cvcp "$WORK_DIR/large-section-missing-old.md" "$WORK_DIR/large-section-missing-new.md"
assert_eq "1" "$RC" "(9a) Genuine missing paragraph in a large section -> exit 1 (still correctly detected)"
assert_contains "$OUT" "finding number 2 confirmed" "(9a) Failure names the specific missing paragraph"

# --- Scenario 10: h3 section nested under h2, deleted wholesale (#13938) --
# Case 1 from the report: the Curator template nests everything under one
# `## Curator Enhancement` heading with `###` subsections, so the section is
# written at h3. The pre-fix guard matched only a level-2 heading, printed
# "OK -- nothing to preserve" and exited 0 even when the section -- and every
# verified entry in it -- was deleted. The one input the guard exists to
# catch must exit 1.
cat > "$WORK_DIR/h3-wholesale-old.md" <<'EOF'
## Curator Enhancement

### Verified corrections

- **2026-11-14, verified via a live probe**: the sibling directory IS claimed.

### Acceptance Criteria

- [ ] fix it
EOF

cat > "$WORK_DIR/h3-wholesale-new.md" <<'EOF'
## Curator Enhancement

### Acceptance Criteria

- [ ] fix it
EOF

run_cvcp "$WORK_DIR/h3-wholesale-old.md" "$WORK_DIR/h3-wholesale-new.md"
assert_eq "1" "$RC" "(10a) h3 section deleted wholesale -> exit 1 (pre-fix silent pass, #13938 Case 1)"
assert_contains "$OUT" "has none at all" "(10a) Failure names the section as entirely absent, not just changed"

# --- Scenario 11: h3 section, sibling h3 subsection edited (#13938) -------
# The false-positive half of the report: with the section at h3 and an
# unrelated `### Acceptance Criteria` as a SIBLING (the template shape), the
# sibling closes the section, so an ordinary edit to its bullet is not a
# lost verified correction.
cat > "$WORK_DIR/h3-sibling-old.md" <<'EOF'
## Curator Enhancement

### Verified corrections

- **2026-11-14, verified via a live probe**: the sibling directory IS claimed.

### Acceptance Criteria

- [ ] fix it
EOF

cat > "$WORK_DIR/h3-sibling-new.md" <<'EOF'
## Curator Enhancement

### Verified corrections

- **2026-11-14, verified via a live probe**: the sibling directory IS claimed.

### Acceptance Criteria

- [ ] fix it properly
EOF

run_cvcp "$WORK_DIR/h3-sibling-old.md" "$WORK_DIR/h3-sibling-new.md"
assert_eq "0" "$RC" "(11a) Unrelated edit under a sibling h3 heading -> exit 0 (no false positive)"
assert_contains "$OUT" "OK" "(11a) Success message reported"

# --- Scenario 12: h3 section closed by a following h2, edit under it ------
# Edge case: a section opened at depth 3 closes at the next heading of the
# same or SHALLOWER depth -- a following h2 sibling ends it, so edits under
# that h2 are unrelated to the section.
cat > "$WORK_DIR/h3-then-h2-old.md" <<'EOF'
## Curator Enhancement

### Verified corrections

- **2026-11-14, verified via a live probe**: the sibling directory IS claimed.

## Acceptance Criteria

- [ ] fix it
EOF

sed 's/- \[ \] fix it/- [ ] fix it properly/' "$WORK_DIR/h3-then-h2-old.md" > "$WORK_DIR/h3-then-h2-new.md"

run_cvcp "$WORK_DIR/h3-then-h2-old.md" "$WORK_DIR/h3-then-h2-new.md"
assert_eq "0" "$RC" "(12a) h3 section closed by a following h2; edit under the h2 -> exit 0"

# --- Scenario 13: h2 section with nested h3 still protects inner entries --
# Deeper subheadings stay INSIDE the section (only same-or-shallower closes
# it), so a verified entry recorded under a nested h3 subsection is still
# append-only protected -- deleting it is detected and named. This is what
# keeps depth-matching from trading the silent pass for a silent hole.
cat > "$WORK_DIR/nested-h3-old.md" <<'EOF'
## Verified corrections

- **2026-11-01, verified via probe A**: outer entry intact.

### Probe details

- **2026-11-02, verified via probe B**: nested entry that must stay protected.

## Acceptance Criteria

- [ ] unrelated
EOF

grep -v "nested entry that must stay protected" "$WORK_DIR/nested-h3-old.md" > "$WORK_DIR/nested-h3-new.md"

run_cvcp "$WORK_DIR/nested-h3-old.md" "$WORK_DIR/nested-h3-new.md"
assert_eq "1" "$RC" "(13a) Entry under a nested h3 deleted -> exit 1 (deeper headings stay inside the section)"
assert_contains "$OUT" "nested entry that must stay protected" "(13a) Failure names the nested missing paragraph"

run_cvcp "$WORK_DIR/nested-h3-old.md" "$WORK_DIR/nested-h3-old.md"
assert_eq "0" "$RC" "(13b) Same nested document unchanged -> exit 0"

# --- Scenario 14: level-1 heading is ignored but not silent (#13938) ------
# A depth-1 `# Verified corrections` heading is deliberately not matched (a
# lone h1 is a document title, not a section inside a body). It must be
# named in a WARN on stderr so the absent-section OK stays distinguishable
# from a found-and-checked pass. No new exit code: still 0.
cat > "$WORK_DIR/h1-old.md" <<'EOF'
# Verified corrections

- **2026-11-14, verified via a live probe**: would be silently unprotected.
EOF

run_cvcp "$WORK_DIR/h1-old.md" "$WORK_DIR/h1-old.md"
assert_eq "0" "$RC" "(14a) Level-1 heading ignored -> exit 0 (no new exit code)"
assert_contains "$OUT" "WARN" "(14a) WARN emitted for the ignored heading"
assert_contains "$OUT" "# Verified corrections" "(14a) WARN names the ignored heading"
assert_contains "$OUT" "nothing to preserve" "(14a) Absent-section OK still reported (now distinguishable via the WARN)"

# --- Scenario 15: depth edge cases — mixed case, trailing whitespace, tab -
# Trailing whitespace on the heading is tolerated ([ \t]*$), matching is
# case-insensitive at any depth, and a tab after the hash run is a valid
# heading separator. printf keeps the trailing whitespace explicit.
printf '## Curator Enhancement\n\n### VERIFIED CORRECTIONS   \n\n- **2026-11-05, verified via probe T**: trailing-space heading entry.\n\n### Acceptance Criteria\n\n- [ ] x\n' > "$WORK_DIR/h3-edge-old.md"
printf '## Curator Enhancement\n\n### verified corrections\t\n\n- **2026-11-05, verified via probe T**: trailing-space heading entry.\n\n- **2026-11-06, verified via probe U**: appended entry.\n\n### Acceptance Criteria\n\n- [ ] x\n' > "$WORK_DIR/h3-edge-new.md"

run_cvcp "$WORK_DIR/h3-edge-old.md" "$WORK_DIR/h3-edge-new.md"
assert_eq "0" "$RC" "(15a) Mixed-case h3 heading with trailing space/tab, preserved + appended -> exit 0"

printf '## Curator Enhancement\n\n### Acceptance Criteria\n\n- [ ] x\n' > "$WORK_DIR/h3-edge-gone.md"
run_cvcp "$WORK_DIR/h3-edge-old.md" "$WORK_DIR/h3-edge-gone.md"
assert_eq "1" "$RC" "(15b) Mixed-case h3 heading with trailing whitespace, section deleted -> exit 1"

# --- Summary ---
echo ""
echo "────────────────────────────────"
echo "Results: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"

if [[ $TESTS_FAILED -gt 0 ]]; then
    exit 1
fi
exit 0
