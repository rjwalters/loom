#!/usr/bin/env bash
# check-structural.sh — run every structural gate CI's `Structural Checks` job
# runs, locally, before anything expensive (#9140).
#
# THE GAP THIS CLOSES. `.loom/scripts/build-gate.sh` is the gate a Builder runs
# to decide "am I CI-green?", and it ran cargo, the doctests and five bash
# suites — but none of the ~15 structural/ratchet checkers CI's required
# `Structural Checks` job enforces. So a Builder could report a fully green
# local gate and still fail CI on checks that are fast, deterministic and need
# no build at all. PR #9137 is the receipt: an honest 9300/9303 local green,
# then five CI failures (markdown-token ratchet, role-prompt ratchet,
# dangling-links, docs/defaults parity, install-surface links), every one of
# them reproducible locally in about a second. One full
# Builder -> CI -> Judge -> Doctor lap spent on findings a pre-push gate
# could have returned instantly.
#
# WHY THIS PARSES ci.yml RATHER THAN HOLDING ITS OWN LIST. The failure above is
# not "build-gate.sh is missing four checkers" — it is that CI grew gates the
# local gate never learned about. A hand-mirrored list here would reproduce
# exactly that drift the first time someone adds a 16th gate to the workflow
# and not to this file. So the gate set is DERIVED, on every run, from the one
# place it already has to be correct: the `Structural Checks` job in
# .github/workflows/ci.yml. There is one definition, it lives in CI, and a gate
# added there reaches this runner with no second edit.
#
# The direction matters. CI keeps its per-step shape — each gate is its own
# named step under `if: ${{ !cancelled() }}`, so one red gate never hides
# another and the step name says which one failed — and each step stays inside
# its `# component:` marker, which `loom-daemon`'s required-check freshness
# guard pins per component (see loom-daemon/src/merge_pr/stale_checks/inputs.rs
# REQUIRED_CHECKS). Having CI call THIS script instead would have collapsed
# fifteen named steps into one opaque one and broken that pin test. Reading the
# workflow costs CI nothing and changes nothing.
#
# WHAT IT RUNS. Every `bash <script>.sh [args…]` invocation appearing in the
# job's step bodies, in workflow order, INCLUDING the `--self-test` steps — a
# ratchet that has silently stopped measuring reports OK forever, which is why
# CI self-tests them and why this does too. It deliberately does not reproduce
# the job's non-`bash` steps (the pinned lychee download, the `jq -e` config
# parse, the `bash --version` report): those are runner provisioning or one
# inline expression, not a gate with a name.
#
# HOW IT FAILS. Like CI: every gate runs even after one fails (no `set -e`
# around the gate loop), each gate's own output goes straight to the terminal so
# the violation is readable, and the summary line names the tally. Exit 0 iff no
# gate failed.
#
# WHAT IT SKIPS, LOUDLY. Three cases are reported as SKIP rather than silently
# dropped or run wrongly:
#   - an invocation carrying a `${{ … }}` Actions expression (the version-bump
#     gate needs the PR's base/head SHAs, which exist only in the workflow
#     context);
#   - an invocation using shell quoting or metacharacters, which this runner
#     splits on whitespace and will not `eval`;
#   - a gate whose script is not present in this tree (a consumer repo, or a
#     workflow ahead of the checkout);
#   - a gate that exited 78 (`EX_CONFIG`), the REQUIRED-TOOL-ABSENT sentinel.
#     `check-doc-anchors.sh` exits it deliberately when `lychee` is absent, and
#     CI installs a pinned lychee that a dev host has no reason to have. A
#     missing tool is "could not check", not "checked and failed" — reporting it
#     as a red gate forever would train a Builder to ignore the phase, which is
#     worse than the gap. CI still enforces it for real.
# Skips are counted and printed. They never turn a red gate green, and they are
# never silent — a runner that cannot tell "checked, fine" from "could not
# check" reports OK forever.
#
# WHY THE SENTINEL IS 78 AND NOT 127 (#9494). Until #9494 the arm above keyed on
# 127, and that arm was wider than its own subject. 127 is not a private
# sentinel: it is what `set -e` returns from ANY `command not found` inside a
# gate and what bash returns for a missing interpreter. So a gate that was
# simply BROKEN got reported as "could not check" — indistinguishable from the
# deliberate lychee case, and fail-open in exactly the place honesty is this
# runner's entire job. 78 is reachable only by an explicit `exit 78`, and it is
# already this repo's "required part of the environment is absent" code
# (spawn-claude.sh on an empty token pool, spawn-worker.sh on an unknown
# runtime). 127 is therefore a FAIL again, with a NOTE naming both readings so
# the author of a new sentinel is told which code to use instead. Keying on the
# exit code rather than on a matched marker line keeps every gate's own output
# streaming straight to the terminal, unbuffered and uncaptured, which is the
# property that makes a failing gate readable.
#
# Usage:
#   check-structural.sh                    Run every gate. Exit 1 if any failed.
#   check-structural.sh --list             Print the derived gate set, run nothing.
#   check-structural.sh --self-test        Verify the derivation itself still works.
#   check-structural.sh --workflow FILE    Read the job from FILE (default:
#                                          .github/workflows/ci.yml).
#   check-structural.sh --root DIR         Run the gates from DIR (default: the
#                                          git toplevel, else the cwd).
#   check-structural.sh --job NAME         Job `name:` to mirror (default:
#                                          "Structural Checks").
#   check-structural.sh --help
#
# Exit codes: 0 = every runnable gate passed (or --list/--self-test succeeded);
# 1 = at least one gate failed (or the self-test failed); 2 = the job was found
# but no gate could be derived from it — the workflow's shape or this parser
# changed, and a run that measured nothing must not read as "all clear".
#
# Shell rather than a `loom-daemon` subcommand for the same reason its fifteen
# subjects are (shell-language-policy.md objection 3): it is a grep/read scan
# that must run on a bare checkout with no cargo, no build step and no
# loom-daemon binary — the whole point is that it answers before anything
# expensive, including before the binary that would otherwise host it exists.
set -euo pipefail

WORKFLOW_DEFAULT=".github/workflows/ci.yml"
JOB_NAME_DEFAULT="Structural Checks"
# The one status a gate may use to say "a tool I require is not installed", and
# so be reported SKIP rather than FAIL. See the header note on why it is not 127.
TOOL_ABSENT_RC=78

WORKFLOW=""
JOB_NAME="$JOB_NAME_DEFAULT"
ROOT=""
MODE="run"

usage() {
  sed -n '/^# Usage:/,/^# Exit codes:/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --list) MODE="list"; shift ;;
    --self-test) MODE="self-test"; shift ;;
    --workflow) WORKFLOW="${2:?--workflow needs a file}"; shift 2 ;;
    --job) JOB_NAME="${2:?--job needs a name}"; shift 2 ;;
    --root) ROOT="${2:?--root needs a directory}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "check-structural: unknown argument '$1' (try --help)" >&2; exit 2 ;;
  esac
done

if [[ -z "$ROOT" ]]; then
  ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
fi
if [[ -z "$WORKFLOW" ]]; then
  WORKFLOW="$ROOT/$WORKFLOW_DEFAULT"
fi

# Derive the gate set from one job of a workflow file.
#
# Written as a bash read loop rather than awk on purpose: the two awks ubuntu
# and macOS ship disagree about POSIX character classes often enough that the
# repo's own gates carry warnings about it, and this has to be right on both.
# No `mapfile` and no `declare -A` either — macOS ships bash 3.2.
#
# The YAML shape it relies on is the one GitHub Actions mandates: job keys sit
# at two spaces under `jobs:`, a job's own keys (`name:`, `steps:`) at four, so
# a two-space key is an unambiguous "a new job starts here" and closes the one
# being collected. Within the job, shell line continuations are joined so a
# multi-line invocation is classified whole, and any line whose first non-blank
# character is `#` is dropped — a YAML comment and a shell comment inside a
# `run: |` block are both prose, and the job has plenty of both naming scripts
# it does not run.
extract_gates() {
  local wf="$1" job="$2"
  local ln s buf="" injob=0 cont=0

  while IFS= read -r ln || [[ -n "$ln" ]]; do
    # A two-space-indented key ends whatever job was being collected.
    if [[ "$ln" =~ ^\ \ [A-Za-z0-9_-]+:[[:space:]]*$ ]]; then
      injob=0; cont=0; buf=""
      continue
    fi
    if (( ! injob )); then
      if [[ "$ln" =~ ^\ \ \ \ name:[[:space:]]*(.*[^[:space:]])[[:space:]]*$ ]] \
         && [[ "${BASH_REMATCH[1]}" == "$job" ]]; then
        injob=1
      fi
      continue
    fi

    # Trim both ends; a line with no non-blank character drops out here.
    s=""
    if [[ "$ln" =~ ^[[:space:]]*(.*[^[:space:]]) ]]; then s="${BASH_REMATCH[1]}"; fi
    if [[ -z "$s" ]]; then continue; fi
    if [[ "$s" == '#'* ]]; then cont=0; buf=""; continue; fi

    if (( cont )); then buf="${buf%\\} $s"; else buf="$s"; fi
    if [[ "$s" == *\\ ]]; then cont=1; continue; fi
    cont=0

    # `bash` must start a token and be followed by a `.sh` operand, so the
    # job's `bash --version` report and its `$(command -v bash)` echo do not
    # look like gates. Everything from `bash` to end of line is the command.
    if [[ "$buf" =~ (^|[[:space:]])(bash[[:space:]]+[^[:space:]]+\.sh.*)$ ]]; then
      printf '%s\n' "${BASH_REMATCH[2]}"
    fi
  done < "$wf"
}

# Why a gate cannot be run here, or "" when it can be.
skip_reason() {
  local cmd="$1" script
  case "$cmd" in
    *'${{'*) echo "needs the Actions workflow context (\${{ … }} expression)"; return 0 ;;
    *[\"\'\$\|\&\;\<\>\(\)\`]*) echo "invocation uses shell quoting/metacharacters"; return 0 ;;
  esac
  script="${cmd#bash }"
  script="${script%% *}"
  if [[ ! -f "$ROOT/$script" ]]; then
    echo "$script is not present in this tree"
    return 0
  fi
  echo ""
}

# --- list ------------------------------------------------------------------

list_gates() {
  local cmd reason
  while IFS= read -r cmd; do
    reason="$(skip_reason "$cmd")"
    if [[ -n "$reason" ]]; then
      printf 'SKIP  %s  [%s]\n' "$cmd" "$reason"
    else
      printf 'RUN   %s\n' "$cmd"
    fi
  done < <(extract_gates "$WORKFLOW" "$JOB_NAME")
}

# --- run -------------------------------------------------------------------

run_gates() {
  local -a gates=()
  local cmd reason rc=0 passed=0 failed=0 skipped=0 total_start step_start elapsed
  local -a failed_names=()

  if [[ ! -f "$WORKFLOW" ]]; then
    echo "[structural] no $WORKFLOW — nothing to mirror, skipping."
    return 0
  fi

  while IFS= read -r cmd; do gates+=("$cmd"); done \
    < <(extract_gates "$WORKFLOW" "$JOB_NAME")

  if [[ "${#gates[@]}" -eq 0 ]]; then
    # Two very different situations, and only one of them is fine.
    if grep -Fq "name: $JOB_NAME" "$WORKFLOW"; then
      echo "[structural] FAIL: found the '$JOB_NAME' job in $WORKFLOW but derived NO gates from it." >&2
      echo "[structural] The job's shape or this parser changed. A run that measured nothing is not a pass —" >&2
      echo "[structural] re-check extract_gates() against the job, then run --self-test." >&2
      return 2
    fi
    echo "[structural] no '$JOB_NAME' job in $WORKFLOW — nothing to mirror, skipping."
    return 0
  fi

  echo "[structural] ${#gates[@]} gate(s) derived from $WORKFLOW (job: $JOB_NAME)"
  total_start=$(date +%s)
  for cmd in "${gates[@]}"; do
    reason="$(skip_reason "$cmd")"
    if [[ -n "$reason" ]]; then
      printf '[structural] SKIP  %s  — %s\n' "$cmd" "$reason"
      skipped=$((skipped + 1))
      continue
    fi
    printf '[structural] ---- %s\n' "$cmd"
    local -a argv=()
    read -ra argv <<<"$cmd"
    step_start=$(date +%s)
    rc=0
    ( cd "$ROOT" && "${argv[@]}" ) || rc=$?
    elapsed=$(( $(date +%s) - step_start ))
    if [[ "$rc" -eq 0 ]]; then
      printf '[structural] PASS  (%ss)  %s\n' "$elapsed" "$cmd"
      passed=$((passed + 1))
    elif [[ "$rc" -eq "$TOOL_ABSENT_RC" ]]; then
      # 78 (EX_CONFIG) = the gate declares a tool it requires is not installed.
      # "Could not check", not "checked and failed" — see the header's skip
      # table, and the header note on why this is not 127.
      printf '[structural] SKIP  %s  — exited %s (EX_CONFIG): a tool it requires is not installed\n' \
        "$cmd" "$TOOL_ABSENT_RC"
      skipped=$((skipped + 1))
    else
      if [[ "$rc" -eq 127 ]]; then
        printf '[structural] NOTE  exit 127 means a command INSIDE this gate was not found — a broken gate, counted as FAIL.\n' >&2
        printf '[structural] NOTE  A deliberate "required tool absent" sentinel must exit %s (EX_CONFIG) to be skipped (#9494).\n' "$TOOL_ABSENT_RC" >&2
      fi
      printf '[structural] FAIL  (%ss, exit %s)  %s\n' "$elapsed" "$rc" "$cmd"
      failed=$((failed + 1))
      failed_names+=("$cmd")
    fi
  done

  elapsed=$(( $(date +%s) - total_start ))
  printf '[structural] %s passed, %s failed, %s skipped in %ss\n' \
    "$passed" "$failed" "$skipped" "$elapsed"
  if [[ "$failed" -gt 0 ]]; then
    echo "[structural] failing gate(s):" >&2
    printf '[structural]   %s\n' "${failed_names[@]}" >&2
    echo "[structural] These are the same gates CI's '$JOB_NAME' job runs. Fix them before pushing." >&2
    return 1
  fi
  return 0
}

# --- self-test -------------------------------------------------------------
#
# Two halves, and the second is the one that matters. Synthetic fixtures pin the
# derivation's shape (job scoping, continuations, comments, skip classification,
# continue-on-failure). The live assertion pins it against THIS repo's real
# ci.yml, so a workflow restructure that silently stops matching fails here
# instead of leaving the runner quietly reporting zero gates.

self_test() {
  local tmp fails=0 out rc
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" RETURN

  mkdir -p "$tmp/.github/workflows" "$tmp/scripts"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$tmp/scripts/ok-one.sh"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$tmp/scripts/ok-two.sh"
  printf '#!/usr/bin/env bash\necho "fixture gate is angry" >&2\nexit 1\n' >"$tmp/scripts/bad.sh"
  printf '#!/usr/bin/env bash\necho "fixture: SKIP - no such tool" >&2\nexit 78\n' >"$tmp/scripts/needs-tool.sh"
  # The #9494 pair to needs-tool.sh: a gate that is simply BROKEN. `set -e` plus
  # a command that does not exist is how a real gate reaches 127, and it must
  # read as FAIL, not as "could not check".
  printf '#!/usr/bin/env bash\nset -euo pipefail\nnot-a-real-command-9494\n' >"$tmp/scripts/broken.sh"

  cat >"$tmp/.github/workflows/fixture.yml" <<'YAML'
name: CI
on: [push]
jobs:
  other-job-before:
    name: Some Other Job
    steps:
      - run: bash scripts/must-not-be-collected-before.sh
  structural-checks:
    name: Structural Checks
    steps:
      # A comment naming bash scripts/ghost.sh must not be collected.
      - name: simple
        run: bash scripts/ok-one.sh --self-test
      - name: block
        run: |
          # another comment: bash scripts/ghost-two.sh
          bash scripts/ok-two.sh
          bash scripts/bad.sh
          bash scripts/needs-tool.sh
          bash scripts/broken.sh
      - name: provisioning, not a gate
        run: |
          echo "PATH bash: $(command -v bash)"
          bash --version
      - name: needs workflow context
        run: |
          bash scripts/ok-one.sh --forbid-bump \
            --base "${{ github.event.pull_request.base.sha }}" \
            --head "${{ github.event.pull_request.head.sha }}"
      - name: absent script
        run: bash scripts/not-in-this-tree.sh
  other-job-after:
    name: Another Other Job
    steps:
      - run: bash scripts/must-not-be-collected-after.sh
YAML

  echo "check-structural --self-test: deriving the gate set from a synthetic workflow…"
  out="$("${BASH_SOURCE[0]}" --workflow "$tmp/.github/workflows/fixture.yml" --root "$tmp" --list)"

  local expected
  expected="RUN   bash scripts/ok-one.sh --self-test
RUN   bash scripts/ok-two.sh
RUN   bash scripts/bad.sh
RUN   bash scripts/needs-tool.sh
RUN   bash scripts/broken.sh
SKIP  bash scripts/ok-one.sh --forbid-bump  --base \"\${{ github.event.pull_request.base.sha }}\"  --head \"\${{ github.event.pull_request.head.sha }}\"  [needs the Actions workflow context (\${{ … }} expression)]
SKIP  bash scripts/not-in-this-tree.sh  [scripts/not-in-this-tree.sh is not present in this tree]"

  if [[ "$out" == "$expected" ]]; then
    echo "  ok: job-scoped, continuations joined, comments dropped, skips classified"
  else
    echo "SELF-TEST FAIL: derived gate set does not match the fixture's expectation." >&2
    echo "--- expected ---" >&2; printf '%s\n' "$expected" >&2
    echo "--- actual   ---" >&2; printf '%s\n' "$out" >&2
    fails=$((fails + 1))
  fi

  echo "check-structural --self-test: one failing gate must not stop the others…"
  rc=0
  out="$("${BASH_SOURCE[0]}" --workflow "$tmp/.github/workflows/fixture.yml" --root "$tmp" 2>&1)" || rc=$?
  if [[ "$rc" -ne 1 ]]; then
    echo "SELF-TEST FAIL: expected exit 1 from a fixture with one failing gate, got $rc" >&2
    printf '%s\n' "$out" >&2
    fails=$((fails + 1))
  elif ! printf '%s\n' "$out" | grep -F "2 passed, 2 failed, 3 skipped" >/dev/null; then
    echo "SELF-TEST FAIL: expected '2 passed, 2 failed, 3 skipped' in the summary" >&2
    printf '%s\n' "$out" >&2
    fails=$((fails + 1))
  elif ! printf '%s\n' "$out" | grep -F "fixture gate is angry" >/dev/null; then
    echo "SELF-TEST FAIL: the failing gate's own output was not surfaced" >&2
    printf '%s\n' "$out" >&2
    fails=$((fails + 1))
  elif ! printf '%s\n' "$out" | grep -F "exited 78 (EX_CONFIG)" >/dev/null; then
    echo "SELF-TEST FAIL: a gate exiting 78 (EX_CONFIG) must be reported as a SKIP, not counted as a failure" >&2
    printf '%s\n' "$out" >&2
    fails=$((fails + 1))
  elif ! printf '%s\n' "$out" | grep -E '^\[structural\] FAIL .*exit 127.*broken\.sh' >/dev/null; then
    # #9494: the narrowing. A gate that reaches 127 by `command not found` is
    # BROKEN, and must be counted as a failure rather than skipped as
    # "could not check" — the old arm skipped it.
    echo "SELF-TEST FAIL: a gate exiting 127 (command not found) must be a FAIL, not a SKIP" >&2
    printf '%s\n' "$out" >&2
    fails=$((fails + 1))
  elif ! printf '%s\n' "$out" | grep -F "sentinel must exit 78" >/dev/null; then
    echo "SELF-TEST FAIL: a 127 FAIL must NOTE that the tool-absent sentinel is 78, not 127" >&2
    printf '%s\n' "$out" >&2
    fails=$((fails + 1))
  else
    echo "  ok: ran past the failure, surfaced its output, skipped the 78, FAILED the 127, exited 1"
  fi

  echo "check-structural --self-test: a job with no derivable gate must FAIL, not pass…"
  cat >"$tmp/.github/workflows/empty.yml" <<'YAML'
jobs:
  structural-checks:
    name: Structural Checks
    steps:
      - run: echo "nothing here is a gate"
YAML
  rc=0
  out="$("${BASH_SOURCE[0]}" --workflow "$tmp/.github/workflows/empty.yml" --root "$tmp" 2>&1)" || rc=$?
  if [[ "$rc" -ne 2 ]]; then
    echo "SELF-TEST FAIL: expected exit 2 for a found-but-unparseable job, got $rc" >&2
    printf '%s\n' "$out" >&2
    fails=$((fails + 1))
  else
    echo "  ok: measured-nothing is reported as a failure, not as all-clear"
  fi

  echo "check-structural --self-test: a workflow without the job must skip cleanly…"
  cat >"$tmp/.github/workflows/nojob.yml" <<'YAML'
jobs:
  something-else:
    name: Something Else
    steps:
      - run: bash scripts/ok-one.sh
YAML
  rc=0
  out="$("${BASH_SOURCE[0]}" --workflow "$tmp/.github/workflows/nojob.yml" --root "$tmp" 2>&1)" || rc=$?
  if [[ "$rc" -ne 0 ]] || ! printf '%s\n' "$out" | grep -F "nothing to mirror" >/dev/null; then
    echo "SELF-TEST FAIL: expected a clean skip for a workflow with no '$JOB_NAME_DEFAULT' job (rc=$rc)" >&2
    printf '%s\n' "$out" >&2
    fails=$((fails + 1))
  else
    echo "  ok: a repo without the job is skipped, not failed"
  fi

  # The live half: the drift alarm. If a workflow restructure stops matching,
  # this fails loudly here rather than letting the runner report zero gates.
  echo "check-structural --self-test: deriving from this repo's own $WORKFLOW_DEFAULT…"
  if [[ -f "$WORKFLOW" ]]; then
    local live n
    live="$(extract_gates "$WORKFLOW" "$JOB_NAME")"
    n="$(printf '%s\n' "$live" | grep -c . || true)"
    if [[ "$n" -lt 10 ]]; then
      echo "SELF-TEST FAIL: derived only $n gate(s) from the real $JOB_NAME job; expected at least 10." >&2
      fails=$((fails + 1))
    else
      echo "  ok: $n gate(s) derived from the real job"
    fi
    # The four #9137 failures that opened #9140, by name. If the workflow stops
    # running one of these the assertion should be updated deliberately — it is
    # not allowed to lapse by accident.
    local probe
    for probe in check-markdown-token-budget.sh check-role-prompt-budget.sh \
                 check-dangling-links.sh check-docs-defaults-parity.sh; do
      if printf '%s\n' "$live" | grep -F "$probe" >/dev/null; then
        echo "  ok: $probe is in the derived set"
      else
        echo "SELF-TEST FAIL: $probe is not in the derived set (it is one of PR #9137's five CI failures)." >&2
        fails=$((fails + 1))
      fi
    done
  else
    echo "  note: $WORKFLOW absent — live half skipped"
  fi

  if [[ "$fails" -gt 0 ]]; then
    echo "check-structural --self-test: FAILED ($fails check(s))" >&2
    return 1
  fi
  echo "check-structural --self-test: PASSED"
  return 0
}

case "$MODE" in
  list) list_gates ;;
  self-test) self_test ;;
  run) run_gates ;;
esac
