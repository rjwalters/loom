#!/usr/bin/env bash
# check-role-prompt-budget.sh — ratchet the WHOLE prompt prefix a role session
# injects, per role, rather than file by file (#8053).
#
# Why a per-role check when check-markdown-token-budget.sh already exists:
# that gate (#7725) freezes each agent-facing markdown file at its own current
# size. It has no notion of a "role", so it cannot see the number that actually
# costs money — the SUM of everything one spawned session carries before it has
# read a single line of the issue it was dispatched onto. Measured over 28 days
# of transcripts (#8053), ~91% of fleet cost-equivalent is cache writes plus
# cache reads of exactly that prefix; output tokens are ~9%. Eight roles were
# measured injecting 50-170k fresh (uncached) tokens per session. A per-file
# ratchet lets that aggregate grow without any single file growing: split one
# 3k file into three 1k files and every per-file number goes DOWN while the
# role's total goes UP. This check closes that hole.
#
# Scope (deliberately narrow — this issue MEASURES, it does not trim):
#   IN  — resolving each role's always-loaded file set and enforcing a ceiling.
#   OUT — trimming file content (#8064), reordering the prefix for
#         cross-session cache hits (#8066), and verifying/fixing whether
#         sweep's documented progressive disclosure is honored at spawn time
#         (#8065). A static per-file sum cannot distinguish "read on demand"
#         from "always inlined", so this gate takes each prompt's own
#         documented load contract at its word; #8065 is what checks it.
#
# Token proxy: tokens = ceil(bytes / 4), IDENTICAL to
# check-markdown-token-budget.sh's estimator, which is the canonical definition
# — no real tokenizer is available in CI. It is intentionally not exact but it
# is monotonic in file size, which is all a ratchet needs. --self-test
# cross-checks this script's number for a shared file against the other
# script's --list output, so the two estimators cannot silently drift apart.
#
# --- What is a "role", and what does its session load? ---------------------
#
# ROLE DISCOVERY is structural, never a hardcoded name list:
#   1. Every `defaults/roles/<name>.json` — the declared Loom roles (the
#      `terminals` array's role names), each with a prompt at
#      `defaults/.claude/commands/loom/<name>.md`.
#   2. Plus `sweep`, named in EXTRA_ENTRY_POINTS below. `/loom:sweep` is a
#      spawned orchestrator session with an injected prefix like any role, but
#      it is a slash command rather than a terminal role, so it has no
#      `defaults/roles/sweep.json` to be discovered by. It is also the single
#      largest measured prefix in the fleet, so omitting it would gut the
#      check.
#
# FILE-SET RESOLUTION per role, starting from its own prompt file:
#   - Follow every RELATIVE MARKDOWN LINK — `](sibling.md)` — to a sibling
#     `*.md` in the same commands directory, transitively, with a visited set
#     so a back-link (`sweep-arguments.md` -> `sweep.md`) terminates instead of
#     looping. A markdown link is the "go read this file" affordance; a
#     backticked filename in prose is a CITATION, not a load, and is
#     deliberately not followed. That distinction is load-bearing: judge.md
#     cites `curator.md`, `doctor.md`, `builder.md` and `sweep.md` in prose,
#     and following those would charge judge for four other roles' prompts.
#   - MINUS any sibling the referring file GATES in a load-gate table. A
#     sibling is gated when the referring file has markdown table row(s)
#     naming it and NONE of those rows says "Always" (sweep.md's reference
#     file map: "**Always, first.**" vs. "**Mode C only.**"; champion.md's
#     "When to Load" column: "Priority 1 or 5 work found"). ANY row saying
#     "Always" wins, because a file legitimately appears in more than one
#     table — `sweep-arguments.md` is "**Always, first.**" in the load-gate
#     table and also a row in sweep.md's unconditioned "Section lookup" table.
#   - PLUS the two files every session carries regardless of role
#     (SHARED_PREFIX): the repo's own root `CLAUDE.md`, and
#     `defaults/.loom/CLAUDE.md` (the tracked source of the installed
#     `.loom/CLAUDE.md`).
#
# Everything resolves to `defaults/` paths, never to the installed
# `.claude/commands/loom/` or `.loom/` copies. Those are untracked resync
# mirrors of `defaults/` — absent from a fresh CI checkout, so a check written
# against them would measure nothing and pass forever. Same exemption, same
# reason, as check-markdown-token-budget.sh (".loom/* — installed mirror,
# measured at defaults/ source") and check-file-size-budget.sh.
#
# Usage:
#   check-role-prompt-budget.sh              Check every role against its budget.
#   check-role-prompt-budget.sh --update     Regenerate the budget file (see below).
#   check-role-prompt-budget.sh --list       Per-role totals, widest first.
#   check-role-prompt-budget.sh --files      Per-role resolved file set + per-file tokens.
#   check-role-prompt-budget.sh --role NAME  Restrict --list/--files/check to one role.
#   check-role-prompt-budget.sh --self-test  Verify the gate itself still works.
#   check-role-prompt-budget.sh --threshold N  Ceiling for a role with no budget line.
#   check-role-prompt-budget.sh --help
#
# The budget file is scripts/role-prompt-budget.txt:
#
#   <role>  <budget-tokens>  <goal-tokens|->
#
# `budget` is the enforced ceiling, frozen at the role's measured total: it may
# shrink, never grow. `goal` is the ASPIRATIONAL target from #8053 where that
# issue named one (judge/curator/doctor 30000, sweep 60000) and `-` otherwise;
# it is reported, never enforced. Budget and goal are separate on purpose — a
# gate set to an aspiration nobody has met yet fails on `main` from the moment
# it lands, which makes it noise rather than a gate (see
# .loom/docs/ci-principles.md). The goal column is what #8064 and #8066 shrink
# toward; this check is what stops the gap widening while they do.
#
# COVERAGE is enforced both ways: a discovered role with no budget line fails,
# and a budget line for a role that no longer exists fails. A new role cannot
# quietly land without a measured prefix, and a deleted one cannot leave a
# stale number behind that nobody notices.
#
# --threshold only matters for the transient state where a role exists but has
# no budget line yet (it is reported as a coverage failure regardless; the
# threshold decides whether the message also flags it as already oversized).
#
# --update is for two legitimate cases: (1) recording shrinkage after a real
# trim, so the ratchet tightens; (2) admitting a deliberate, reviewed increase.
# It is NOT a way to silence a failure. A reviewer should treat an --update
# that RAISES an existing number as a red flag, exactly as with the per-file
# baseline. Goal values are preserved across --update; only budgets are
# recomputed.
#
# A UNIFORM delta across every role means a SHARED file moved, not a prompt.
# Unlike its per-file siblings (check-file-size-budget.sh,
# check-markdown-token-budget.sh), this budget is a whole-tree measurement:
# it sums both CLAUDE.md files and every always-loaded sibling, which a given
# PR does not necessarily own. A baseline recorded on a branch is therefore
# invalidated by ANY concurrent merge touching one of those shared files, even
# though the branch's own CI run was honest about the tree it measured. That
# is what took `main` red at 304cccb7 (#8095): the baseline was generated on a
# branch whose CLAUDE.md was 111 bytes smaller than the one it squash-merged
# onto, so all 12 roles read exactly +28. When the reported delta is identical
# for every role, re-run --update against current `main` and say so in the PR
# description; do not hunt for a per-role cause, there isn't one.
#
# Exit codes: 0 = every role within budget; 1 = a role is over budget, or
# coverage is broken; 2 = bad args.

set -euo pipefail

THRESHOLD_DEFAULT=70000
MODE="check"
THRESHOLD="${ROLE_PROMPT_TOKEN_THRESHOLD:-$THRESHOLD_DEFAULT}"
ONLY_ROLE=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --update)    MODE="update"; shift ;;
    --list)      MODE="list"; shift ;;
    --files)     MODE="files"; shift ;;
    --self-test) MODE="self-test"; shift ;;
    --role)      ONLY_ROLE="${2:?--role needs a value}"; shift 2 ;;
    --threshold) THRESHOLD="${2:?--threshold needs a value}"; shift 2 ;;
    --help|-h)   sed -n '2,129p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)           echo "check-role-prompt-budget: unknown argument '$1'" >&2; exit 2 ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ! ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)"; then
  ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
fi
cd "$ROOT"

BUDGETS="$ROOT/scripts/role-prompt-budget.txt"
CMD_DIR="defaults/.claude/commands/loom"

# Carried by every spawned session regardless of role. Order is irrelevant to
# a sum; ordering the prefix for cache hits is #8066, explicitly not this gate.
SHARED_PREFIX="CLAUDE.md defaults/.loom/CLAUDE.md"

# Spawned session entry points that are NOT declared Loom roles. See the
# header: `/loom:sweep` is a slash command, so it has no defaults/roles/*.json.
EXTRA_ENTRY_POINTS="sweep"

# --- Role discovery ----------------------------------------------------------
discover_roles() {
  {
    git ls-files -- 'defaults/roles/*.json' 2>/dev/null |
      sed -e 's|.*/||' -e 's|\.json$||'
    printf '%s\n' $EXTRA_ENTRY_POINTS
  } | LC_ALL=C sort -u | while IFS= read -r r; do
    [[ -n "$r" ]] || continue
    [[ -f "$CMD_DIR/$r.md" ]] || continue
    printf '%s\n' "$r"
  done
}

# --- File-set resolution -----------------------------------------------------

# Relative markdown links to a sibling *.md, one per line, deduped.
links_in() {
  grep -oE '\]\([A-Za-z0-9_.-]+\.md\)' "$1" 2>/dev/null |
    sed -e 's|^](||' -e 's|)$||' | LC_ALL=C sort -u || true
}

# is_gated <referring-file> <sibling-basename>
# 0 (true, gated)  => the referrer has table row(s) naming the sibling and
#                     none of them says "Always".
# 1 (false)        => no table row names it, or at least one row says "Always".
is_gated() {
  local referrer="$1" sibling="$2" rows
  rows="$(grep -F -- "$sibling" "$referrer" 2>/dev/null | grep -E '^[[:space:]]*\|' || true)"
  [[ -z "$rows" ]] && return 1
  # Here-string, NOT a pipe: `grep -q` is an early-exit consumer, and under
  # `set -o pipefail` a producer that takes SIGPIPE fails the whole pipeline
  # (#7790/#7789, ratcheted by scripts/check-pipefail-early-exit.sh).
  grep -qE '(^|[^[:alnum:]])Always([^[:alnum:]]|$)' <<< "$rows" && return 1
  return 0
}

# resolve_role_files <role> — every file that role's session always loads.
# Breadth-first over markdown links with `work` doubling as the visited set,
# so a back-link or a reference cycle terminates.
resolve_role_files() {
  local role="$1"
  local -a work
  work=("$CMD_DIR/$role.md")
  local i=0 cur link path known j
  while (( i < ${#work[@]} )); do
    cur="${work[$i]}"
    i=$((i + 1))
    [[ -f "$cur" ]] || continue
    while IFS= read -r link; do
      [[ -n "$link" ]] || continue
      path="$CMD_DIR/$link"
      [[ -f "$path" ]] || continue
      is_gated "$cur" "$link" && continue
      known=0
      for (( j = 0; j < ${#work[@]}; j++ )); do
        if [[ "${work[$j]}" == "$path" ]]; then known=1; break; fi
      done
      (( known )) || work+=("$path")
    done < <(links_in "$cur")
  done
  printf '%s\n' "${work[@]}"
  local s
  for s in $SHARED_PREFIX; do
    [[ -f "$s" ]] && printf '%s\n' "$s"
  done
}

# --- Measurement -------------------------------------------------------------
# tokens = ceil(bytes / 4). Canonical definition lives in
# check-markdown-token-budget.sh; --self-test asserts the two agree.
tokens_of() {
  local bytes
  bytes="$(wc -c < "$1" | tr -d '[:space:]')"
  printf '%d\n' $(( (bytes + 3) / 4 ))
}

# role_total <role> -> "<tokens> <file-count>"
role_total() {
  local role="$1" f total=0 count=0
  while IFS= read -r f; do
    [[ -n "$f" ]] || continue
    total=$(( total + $(tokens_of "$f") ))
    count=$(( count + 1 ))
  done < <(resolve_role_files "$role" | LC_ALL=C sort -u)
  printf '%d %d\n' "$total" "$count"
}

budget_for() {
  [[ -f "$BUDGETS" ]] || { printf '\n'; return 0; }
  awk -v r="$1" '$1 !~ /^#/ && $1 == r { print $2; found = 1; exit }
                 END { if (!found) print "" }' "$BUDGETS"
}

goal_for() {
  [[ -f "$BUDGETS" ]] || { printf -- '-\n'; return 0; }
  awk -v r="$1" '$1 !~ /^#/ && $1 == r { print ($3 == "" ? "-" : $3); found = 1; exit }
                 END { if (!found) print "-" }' "$BUDGETS"
}

roles_in_budget_file() {
  awk '$1 !~ /^#/ && NF >= 2 { print $1 }' "$BUDGETS" 2>/dev/null | LC_ALL=C sort -u
}

# NOTE: called from process substitution, so it must never `exit` — a subshell
# exit would be invisible to the caller and turn a bad --role into a silent
# pass. --role is validated once in the main body instead, before dispatch.
selected_roles() {
  if [[ -n "$ONLY_ROLE" ]]; then
    printf '%s\n' "$ONLY_ROLE"
  else
    discover_roles
  fi
}

write_budgets() {
  local tmp="$BUDGETS.tmp" role total count goal
  {
    echo "# role-prompt-budget.txt — generated by scripts/check-role-prompt-budget.sh --update"
    echo "#"
    echo "# One line per spawned session entry point:"
    echo "#"
    echo "#   <role>  <budget-tokens>  <goal-tokens|->"
    echo "#"
    echo "# budget — the ENFORCED ceiling on the role's whole resolved prompt prefix"
    echo "#          (its prompt + every always-loaded sibling + both CLAUDE.md files),"
    echo "#          estimated as ceil(bytes/4) and frozen at the measured total. It"
    echo "#          may shrink, never grow. This is a debt ledger, not a target: the"
    echo "#          numbers should only ever go DOWN."
    echo "# goal   — the ASPIRATIONAL target from #8053 where that issue named one,"
    echo "#          '-' otherwise. Reported, never enforced — see the script header"
    echo "#          for why the two columns are separate."
    echo "#"
    echo "# A diff that RAISES a budget needs a clear reason in the PR description."
    echo "# Do not hand-edit these upward; that is the ratchet slipping. See"
    echo "# .loom/docs/file-size-policy.md."
    while IFS= read -r role; do
      [[ -n "$role" ]] || continue
      read -r total count < <(role_total "$role")
      goal="$(goal_for "$role")"
      printf '%-12s %7d %8s\n' "$role" "$total" "$goal"
    done < <(discover_roles)
  } > "$tmp"
  mv "$tmp" "$BUDGETS"
}

# --- Self-test ---------------------------------------------------------------
# Exercises role discovery, link-following, gate-table honoring, cycle
# termination, the shared prefix, both verdicts, and coverage both ways —
# against synthetic fixtures in a throwaway repo. Then cross-checks the token
# estimator against check-markdown-token-budget.sh on the REAL tree, so the
# two gates cannot drift onto different estimators.
self_test() {
  local tmp rc=0 out S real=""
  tmp="$(mktemp -d)"
  trap 'cleanup_self_test "$tmp"; cleanup_self_test "$real"' RETURN

  mkdir -p "$tmp/scripts" "$tmp/defaults/.claude/commands/loom" \
    "$tmp/defaults/roles" "$tmp/defaults/.loom"
  cp "${BASH_SOURCE[0]}" "$tmp/scripts/check-role-prompt-budget.sh"
  chmod +x "$tmp/scripts/check-role-prompt-budget.sh"
  git -C "$tmp" init -q
  git -C "$tmp" config user.email t@t.test
  git -C "$tmp" config user.name t

  local L="$tmp/defaults/.claude/commands/loom"

  _fill() { printf '%s' "$(head -c "$2" /dev/zero | tr '\0' 'x')" > "$1"; }

  # Shared prefix: 40 + 40 bytes => 10 + 10 tokens on every role.
  _fill "$tmp/CLAUDE.md" 40
  _fill "$tmp/defaults/.loom/CLAUDE.md" 40

  # alpha: a role that references no sibling at all (the common shape).
  _fill "$L/alpha.md" 80
  echo '{}' > "$tmp/defaults/roles/alpha.json"

  # beta: links shared.md, which links deep.md, which links back to beta.md
  # (cycle must terminate). Also CITES gamma.md in prose backticks — a
  # citation, never followed.
  {
    printf 'see [shared](shared.md) and `gamma.md` in prose\n'
    printf '%s' "$(head -c 40 /dev/zero | tr '\0' 'x')"
  } > "$L/beta.md"
  echo '{}' > "$tmp/defaults/roles/beta.json"
  printf 'onward [deep](deep.md)\n' > "$L/shared.md"
  printf 'back [beta](beta.md)\n' > "$L/deep.md"
  _fill "$L/gamma.md" 4000

  # sweep: a dispatcher with a load-gate table. `always.md` is Always (and
  # also appears in a second, unconditioned table row, which must not
  # re-gate it); `modec.md` is gated; `nogate.md` is linked but in no table.
  {
    printf '| File | Load when |\n|---|---|\n'
    printf '| [`always.md`](always.md) | **Always, first.** |\n'
    printf '| [`modec.md`](modec.md) | **Mode C only.** |\n'
    printf '\n| Cited section | Now in |\n|---|---|\n'
    printf '| "Arguments" | `always.md` |\n'
    printf 'plus [nogate](nogate.md)\n'
  } > "$L/sweep.md"
  _fill "$L/always.md" 400
  _fill "$L/modec.md" 8000
  _fill "$L/nogate.md" 40

  git -C "$tmp" add -A >/dev/null
  git -C "$tmp" commit -qm fixtures

  S="$tmp/scripts/check-role-prompt-budget.sh"

  _expect() {
    local label="$1" want="$2" got="$3"
    if [[ "$want" == "$got" ]]; then
      echo "  ok   $label"
    else
      echo "  FAIL $label (want $want, got $got)" >&2
      rc=1
    fi
  }

  out="$( (cd "$tmp" && $S --list) | awk '$1 !~ /^#|^role/ {print $1}' | LC_ALL=C sort | tr '\n' ',')"
  _expect "discovery = every roles/*.json plus sweep" "alpha,beta,sweep," "$out"

  out="$( (cd "$tmp" && $S --role alpha --files) | grep -c 'commands/loom' || true)"
  _expect "role with no sibling links resolves to its prompt only" "1" "$out"

  out="$( (cd "$tmp" && $S --role alpha --list) | awk '$1 == "alpha" {print $3}')"
  _expect "alpha total = ceil(80/4) + shared prefix 10 + 10" "40" "$out"

  out="$( (cd "$tmp" && $S --role beta --files) | grep -c 'gamma\.md' || true)"
  _expect "backticked citation is NOT followed" "0" "$out"

  out="$( (cd "$tmp" && $S --role beta --files) | grep -cE 'beta\.md|shared\.md|deep\.md' || true)"
  _expect "links are followed transitively, cycle terminates" "3" "$out"

  out="$( (cd "$tmp" && $S --role sweep --files) | grep -c 'always\.md' || true)"
  _expect "sibling marked Always is included" "1" "$out"

  out="$( (cd "$tmp" && $S --role sweep --files) | grep -c 'modec\.md' || true)"
  _expect "sibling gated to one mode is excluded" "0" "$out"

  out="$( (cd "$tmp" && $S --role sweep --files) | grep -c 'nogate\.md' || true)"
  _expect "linked sibling in no gate table is included" "1" "$out"

  out="$( (cd "$tmp" && $S --role alpha --files) | grep -cE '(^| )CLAUDE\.md|defaults/\.loom/CLAUDE\.md' || true)"
  _expect "both shared CLAUDE.md files counted" "2" "$out"

  # An unknown --role must be a bad-args exit, NEVER a silent pass: the
  # validation cannot live inside selected_roles(), which runs in a process
  # substitution where an exit is invisible to the caller.
  (cd "$tmp" && $S --role nosuch >/dev/null 2>&1) && out=0 || out=$?
  _expect "unknown --role exits 2 in check mode" "2" "$out"
  (cd "$tmp" && $S --role nosuch --list >/dev/null 2>&1) && out=0 || out=$?
  _expect "unknown --role exits 2 in --list mode" "2" "$out"

  (cd "$tmp" && $S >/dev/null 2>&1) && out=0 || out=$?
  _expect "missing budget file is a coverage failure" "1" "$out"

  (cd "$tmp" && $S --update >/dev/null)
  out="$(awk '$1 !~ /^#/ && NF >= 2 {print $1}' "$tmp/scripts/role-prompt-budget.txt" | LC_ALL=C sort | tr '\n' ',')"
  _expect "--update records every discovered role" "alpha,beta,sweep," "$out"

  (cd "$tmp" && $S >/dev/null 2>&1) && out=0 || out=$?
  _expect "freshly updated tree passes" "0" "$out"

  _fill "$L/alpha.md" 4000
  (cd "$tmp" && $S >/dev/null 2>&1) && out=0 || out=$?
  _expect "growth of a role's own prompt fails" "1" "$out"

  _fill "$L/alpha.md" 80
  (cd "$tmp" && $S >/dev/null 2>&1) && out=0 || out=$?
  _expect "restoring the size passes again" "0" "$out"

  # Growth via a sibling the per-file ratchet would see SHRINK: the hole this
  # gate exists to close.
  _fill "$L/shared.md" 4000
  (cd "$tmp" && $S >/dev/null 2>&1) && out=0 || out=$?
  _expect "growth in an always-loaded SIBLING fails the role" "1" "$out"
  printf 'onward [deep](deep.md)\n' > "$L/shared.md"

  # Boundary: --update froze every budget AT the measured total, so the tree
  # sitting exactly ON its budget must pass (asserted above). One estimated
  # token over — 4 more bytes — must fail.
  out="$( (cd "$tmp" && $S --role alpha --list) | awk '$1 == "alpha" {print $3}')"
  _expect "alpha sits exactly on its recorded budget" \
    "$(awk '$1 == "alpha" {print $2}' "$tmp/scripts/role-prompt-budget.txt")" "$out"

  _fill "$L/alpha.md" 84
  (cd "$tmp" && $S --role alpha >/dev/null 2>&1) && out=0 || out=$?
  _expect "one estimated token over budget fails" "1" "$out"
  _fill "$L/alpha.md" 80
  (cd "$tmp" && $S --role alpha >/dev/null 2>&1) && out=0 || out=$?
  _expect "back exactly on the boundary passes" "0" "$out"

  # A new role with no budget line => coverage failure.
  _fill "$L/delta.md" 80
  echo '{}' > "$tmp/defaults/roles/delta.json"
  git -C "$tmp" add -A >/dev/null
  (cd "$tmp" && $S >/dev/null 2>&1) && out=0 || out=$?
  _expect "new role with no budget line fails coverage" "1" "$out"

  # A stale budget line for a role that no longer exists => coverage failure.
  git -C "$tmp" rm -q --cached defaults/roles/delta.json >/dev/null
  rm -f "$tmp/defaults/roles/delta.json" "$L/delta.md"
  (cd "$tmp" && $S --update >/dev/null)
  printf 'ghost 123 -\n' >> "$tmp/scripts/role-prompt-budget.txt"
  (cd "$tmp" && $S >/dev/null 2>&1) && out=0 || out=$?
  _expect "stale budget line for a removed role fails coverage" "1" "$out"

  # Estimator parity with check-markdown-token-budget.sh on the REAL tree.
  local sibling peer mine
  sibling="$CMD_DIR/probe-protocol.md"
  if [[ -x "$ROOT/scripts/check-markdown-token-budget.sh" && -f "$ROOT/$sibling" ]]; then
    peer="$( (cd "$ROOT" && bash scripts/check-markdown-token-budget.sh --list) |
      awk -v p="$sibling" '$2 == p { print $1 }')"
    mine="$( cd "$ROOT" && tokens_of "$sibling" )"
    _expect "token estimator agrees with check-markdown-token-budget.sh" "$peer" "$mine"
  else
    echo "  skip check-markdown-token-budget.sh parity (peer script not present)"
  fi

  # --- Real-tree round trip (#8095) -------------------------------------
  # Everything above runs on three synthetic roles. This runs the SAME
  # generator and checker over the REAL tree's roles and resolved file sets:
  # --update, then check, must exit 0. Generator and checker share
  # role_total(), so this cannot drift today -- it is the tripwire for a
  # future refactor that splits them, which is the mechanism #8095 was first
  # hypothesised to be before the cause was measured.
  #
  # It operates on a MIRROR under mktemp, never on the repo: --update rewrites
  # scripts/role-prompt-budget.txt, and a self-test must not dirty the tree
  # (nor compare against the committed baseline, which a concurrent merge can
  # legitimately stale -- see the header).
  real="$(mktemp -d)"
  mkdir -p "$real/scripts" "$real/$CMD_DIR" "$real/defaults/roles" "$real/defaults/.loom"
  cp "$ROOT/CLAUDE.md" "$real/CLAUDE.md"
  cp "$ROOT/defaults/.loom/CLAUDE.md" "$real/defaults/.loom/CLAUDE.md"
  cp "$ROOT/$CMD_DIR"/*.md "$real/$CMD_DIR/"
  cp "$ROOT"/defaults/roles/*.json "$real/defaults/roles/"
  cp "${BASH_SOURCE[0]}" "$real/scripts/check-role-prompt-budget.sh"
  chmod +x "$real/scripts/check-role-prompt-budget.sh"
  git -C "$real" init -q
  git -C "$real" config user.email t@t.test
  git -C "$real" config user.name t
  git -C "$real" add -A >/dev/null

  local R nreal failout
  R="$real/scripts/check-role-prompt-budget.sh"
  failout="$real/fail.txt"
  nreal="$(discover_roles | wc -l | tr -d '[:space:]')"

  out="$( (cd "$real" && $R --list) | awk 'NR > 1 && NF' | wc -l | tr -d '[:space:]')"
  _expect "mirror resolves the same role set as the real tree" "$nreal" "$out"

  (cd "$real" && $R --update >/dev/null)
  (cd "$real" && $R >/dev/null 2>&1) && out=0 || out=$?
  _expect "real tree passes the budget it just generated (round trip)" "0" "$out"

  # A SHARED_PREFIX file is charged to every role identically. 120 bytes on
  # CLAUDE.md is exactly 30 estimated tokens (120/4, no rounding slack), so
  # every discovered role must report +30 -- the mechanism behind #8095's
  # uniform +28. Pinned here so that dropping CLAUDE.md from SHARED_PREFIX, or
  # charging it to only some roles, has to be a deliberate, visible change.
  head -c 120 /dev/zero | tr '\0' 'x' >> "$real/CLAUDE.md"
  (cd "$real" && $R >/dev/null 2>"$failout") && out=0 || out=$?
  _expect "shared-prefix growth fails the check" "1" "$out"
  out="$(sed -n 's/.*(+\([0-9]\{1,\}\))$/\1/p' "$failout" | LC_ALL=C sort -u | tr '\n' ',')"
  _expect "every over-budget role reports the SAME delta" "30," "$out"
  out="$(grep -c '(+30)' "$failout" || true)"
  _expect "shared-prefix growth is charged to ALL roles, not some" "$nreal" "$out"

  if [[ $rc -eq 0 ]]; then
    echo "check-role-prompt-budget --self-test: all checks passed."
  else
    echo "check-role-prompt-budget --self-test: FAILURES above." >&2
  fi
  return $rc
}

# Separated so the trap above never expands a bare variable into a delete.
cleanup_self_test() {
  [[ -n "${1:-}" ]] || return 0
  case "$1" in
    /tmp/*|/var/folders/*|/private/var/folders/*) rm -rf -- "$1" ;;
    *) echo "self-test: refusing to clean unexpected temp path '$1'" >&2 ;;
  esac
}

if [[ -n "$ONLY_ROLE" && "$MODE" != "self-test" && ! -f "$CMD_DIR/$ONLY_ROLE.md" ]]; then
  echo "check-role-prompt-budget: no role prompt at $CMD_DIR/$ONLY_ROLE.md" >&2
  echo "Known roles: $(discover_roles | tr '\n' ' ')" >&2
  exit 2
fi

case "$MODE" in
  self-test)
    self_test
    exit $?
    ;;
  files)
    while IFS= read -r role; do
      [[ -n "$role" ]] || continue
      echo "== $role"
      while IFS= read -r f; do
        [[ -n "$f" ]] || continue
        printf '  %7d  %s\n' "$(tokens_of "$f")" "$f"
      done < <(resolve_role_files "$role" | LC_ALL=C sort -u)
    done < <(selected_roles)
    exit 0
    ;;
  list)
    {
      printf 'role files tokens budget goal\n'
      while IFS= read -r role; do
        [[ -n "$role" ]] || continue
        read -r total count < <(role_total "$role")
        printf '%s %s %s %s %s\n' "$role" "$count" "$total" \
          "$(b="$(budget_for "$role")"; printf '%s' "${b:--}")" "$(goal_for "$role")"
      done < <(selected_roles)
    } | awk 'NR == 1 { print; next } { print | "sort -rnk3" }' |
      awk '{ printf "%-12s %6s %8s %8s %8s\n", $1, $2, $3, $4, $5 }'
    exit 0
    ;;
  update)
    write_budgets
    n="$(awk '$1 !~ /^#/ && NF >= 2' "$BUDGETS" | wc -l | tr -d '[:space:]')"
    echo "check-role-prompt-budget: budget file regenerated — $n role(s) tracked."
    echo "Review the diff: budgets should only go DOWN."
    exit 0
    ;;
esac

if [[ ! -f "$BUDGETS" ]]; then
  echo "check-role-prompt-budget: no budget file at $BUDGETS." >&2
  echo "Create it once with: scripts/check-role-prompt-budget.sh --update" >&2
  exit 1
fi

over=() unbudgeted=() under=() stale=()

while IFS= read -r role; do
  [[ -n "$role" ]] || continue
  read -r total count < <(role_total "$role")
  budget="$(budget_for "$role")"
  if [[ -z "$budget" ]]; then
    unbudgeted+=("$role|$total")
  elif (( total > budget )); then
    over+=("$role|$budget|$total")
  elif (( total < budget )); then
    under+=("$role|$budget|$total")
  fi
done < <(selected_roles)

# Coverage the other way: a budget line whose role no longer exists.
if [[ -z "$ONLY_ROLE" ]]; then
  known="$(discover_roles | tr '\n' ' ')"
  while IFS= read -r role; do
    [[ -n "$role" ]] || continue
    case " $known " in
      *" $role "*) ;;
      *) stale+=("$role") ;;
    esac
  done < <(roles_in_budget_file)
fi

if (( ${#over[@]} > 0 || ${#unbudgeted[@]} > 0 || ${#stale[@]} > 0 )); then
  {
    echo "check-role-prompt-budget: FAIL"
    echo ""
    if (( ${#over[@]} > 0 )); then
      echo "These roles inject MORE than their budget (estimated tokens):"
      for e in "${over[@]}"; do
        IFS='|' read -r r b n <<< "$e"
        printf '  %-12s %s -> %s  (+%s)\n' "$r" "$b" "$n" "$((n - b))"
      done
      echo ""
      echo "A role's whole prompt prefix is frozen at its current size: it may shrink,"
      echo "not grow. This total is the SUM of the role's prompt, every sibling it"
      echo "always loads, and both CLAUDE.md files — so splitting one file into three"
      echo "does not help here even though it lowers every per-file number."
      echo "To land this change:"
      echo "  - Move the addition behind a load gate (a 'Load when' table row that is"
      echo "    NOT 'Always'), so only the runs that need it pay for it."
      echo "  - Or remove at least as many tokens as you added from the same role's set."
      echo "  - Inspect the set with: scripts/check-role-prompt-budget.sh --role <role> --files"
      echo ""
      echo "If this change legitimately trims content, or deliberately accepts a larger"
      echo "prefix, run --update and say so in the PR description. Do NOT hand-edit a"
      echo "budget upward — that is the ratchet slipping. See .loom/docs/file-size-policy.md."
      echo ""
    fi
    if (( ${#unbudgeted[@]} > 0 )); then
      echo "These roles have no line in $(basename "$BUDGETS") (coverage gap):"
      for e in "${unbudgeted[@]}"; do
        IFS='|' read -r r n <<< "$e"
        if (( n > THRESHOLD )); then
          printf '  %-12s %s tokens (estimated) — ALREADY over the %s-token threshold\n' "$r" "$n" "$THRESHOLD"
        else
          printf '  %-12s %s tokens (estimated)\n' "$r" "$n"
        fi
      done
      echo ""
      echo "Every spawned role must carry a measured budget. Run --update to record"
      echo "them, and say in the PR description what the new role's prefix costs."
      echo ""
    fi
    if (( ${#stale[@]} > 0 )); then
      echo "These budget lines name a role that no longer exists:"
      for r in "${stale[@]}"; do
        printf '  %s\n' "$r"
      done
      echo ""
      echo "Run --update to drop them."
      echo ""
    fi
  } >&2
  exit 1
fi

msg="check-role-prompt-budget: OK — no role grew (token estimate = ceil(bytes/4))."
if (( ${#under[@]} > 0 )); then
  msg="$msg ${#under[@]} role(s) shrank; run --update to tighten the ratchet."
fi
echo "$msg"
exit 0
