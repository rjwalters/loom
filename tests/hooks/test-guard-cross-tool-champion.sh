#!/usr/bin/env bash
# Cross-tool guard test (#13269): the Champion's pin/facts writes and the merge
# handoff's shell steps must pass BOTH Loom's hooks and the Repo Skills guard.
#
# Why: the fleet runs Loom's guards and Repo Skills' canonical
# guard-destructive.sh together, but each tool only tested against its own. #13181
# passed Loom's tests and was still refused by Repo Skills' guard on 53 repos
# (#13200); loom-ui merged nothing for ~3 days. This suite replays the Champion's
# real commands, as PreToolUse JSON, through every hook with a Loom-managed
# worktree present, and fails on any deny/ask from either tool.
#
# The replayed command strings are not hand-copied in isolation: each template is
# asserted to still appear verbatim in the Champion docs, so a doc change that
# moves the write cannot leave this test stale.
#
# The Repo Skills guard comes from $LOOM_GUARD_CANONICAL (same contract as
# compare-guard-engines.sh). When it is absent the Loom half still runs, but the
# suite exits 3 — INCOMPLETE, not a pass. CI fetches a pinned, sha256-checked
# copy (see the "Fetch pinned Repo Skills guard" step in ci.yml).
#
# Usage: LOOM_GUARD_CANONICAL=<path> ./tests/hooks/test-guard-cross-tool-champion.sh
#
# Exit: 0 all pass; 1 a replayed command was denied/asked or a template drifted;
#       3 INCOMPLETE (canonical guard not supplied/readable).

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

CANONICAL="${LOOM_GUARD_CANONICAL:-}"
HOOKS="$REPO_ROOT/defaults/hooks"
DOC_MERGE="$REPO_ROOT/defaults/.claude/commands/loom/champion-pr-merge.md"
DOC_TARGETED="$REPO_ROOT/defaults/.claude/commands/loom/champion-targeted.md"

INCOMPLETE=0
if [[ -z "$CANONICAL" ]]; then
    INCOMPLETE=1; WHY="no canonical guard supplied (LOOM_GUARD_CANONICAL)"
elif [[ ! -r "$CANONICAL" ]]; then
    INCOMPLETE=1; WHY="canonical guard not readable: $CANONICAL"
fi

# Templates, exactly as written in the docs ("<number>" and "N" are the docs'
# placeholders for the PR number). Each line is one shell command.
TEMPLATES="$(mktemp)"
cat >"$TEMPLATES" <<'T'
CHAMPION_KEY=$(git rev-parse --path-format=absolute --git-common-dir | sha256sum | cut -c1-12)
mkdir -p -m 700 /tmp/loom-champion "/tmp/loom-champion/$CHAMPION_KEY"
rm -f "/tmp/loom-champion/$CHAMPION_KEY/facts-pr-<number>.json"
  commits: [.[1:-1][][] | {oid: .sha, committedDate: .commit.committer.date}], comments: .[-1]}' > "/tmp/loom-champion/$CHAMPION_KEY/facts-pr-<number>.json" \
  || { rm -f "/tmp/loom-champion/$CHAMPION_KEY/facts-pr-<number>.json"; echo "SKIP #$PR_NUMBER: PR facts unread (or comments unauthenticated) — no merge, no comment"; continue; }  # single-PR step: outcome=skipped reason=facts_unread
PR_FACTS="/tmp/loom-champion/$CHAMPION_KEY/facts-pr-<number>.json"
MERGEABLE=$(jq -r '.mergeable' "/tmp/loom-champion/$CHAMPION_KEY/facts-pr-<number>.json")
rm -f "/tmp/loom-champion/$CHAMPION_KEY/pin-pr-N"
[ -n "$EVALUATED_SHA" ] && printf '%s %s\n' "$EVALUATED_SHA" "$PPID" > "/tmp/loom-champion/$CHAMPION_KEY/pin-pr-N"
PF="/tmp/loom-champion/$CHAMPION_KEY/pin-pr-$PR_NUMBER"; PIN=; PIN_PID=; { read -r PIN PIN_PID < "$PF"; } 2>/dev/null
{ [ -n "$(find "$PF" -mmin -60)" ] || { [ "${PIN_PID:-0}" -gt 1 ] && kill -0 "$PIN_PID" && [ -n "$(find "$PF" -mmin -1440)" ]; } || PIN=; } 2>/dev/null
T

# The facts write is a multi-line pipeline in the doc; its two continuation
# lines above are drift-checked verbatim, and the replay wraps them in the head
# of that pipeline so the redirect-to-/tmp is exercised as one command.
FACTS_HEAD='printf '"'"'%s\n'"'"' "$P" "$K" "$C" | jq -s '"'"'.[0] as $p | {labels: [$p.labels[] | {name}], headRefOid: $p.head.sha,'

NUM=4242
expand() { # <template> -> command with the docs' placeholders replaced by digits
    local c="$1"
    c="${c//<number>/$NUM}"
    c="${c//pin-pr-N/pin-pr-$NUM}"
    printf '%s' "$c"
}

# --- 1. Drift: every template still appears verbatim in the Champion docs ----
echo -e "${YELLOW}--- Template drift: replayed commands still appear in the Champion docs ---${NC}"
while IFS= read -r tpl; do
    [[ -z "$tpl" ]] && continue
    TOTAL=$((TOTAL + 1))
    if grep -qF -- "$tpl" "$DOC_MERGE" "$DOC_TARGETED"; then
        PASS=$((PASS + 1)); echo -e "${GREEN}PASS${NC}: in docs: ${tpl:0:100}"
    else
        FAIL=$((FAIL + 1)); echo -e "${RED}FAIL${NC}: template no longer in champion-pr-merge.md / champion-targeted.md: $tpl"
    fi
done <"$TEMPLATES"

# --- 2. Replay through every hook --------------------------------------------
# Fixture: a main checkout with a real linked, .loom-managed worktree — the
# state in which a worktree-write-confining guard is armed.
REPO="$(make_wt_repo_linked)"
trap 'rm -rf "$REPO" "$TEMPLATES"' EXIT

# Print the permission decision (allow/ask/deny) a hook gives a Bash command.
decision() { # <hook> <command> <cwd>
    local out
    out="$(make_input "$2" "$3" | bash "$1" 2>/dev/null)" || true
    if [[ -z "$out" ]]; then echo allow; return; fi
    printf '%s' "$out" | jq -r '.hookSpecificOutput.permissionDecision // "allow"' 2>/dev/null || echo allow
}

replay() { # <label> <hook> <command> <cwd>
    local label="$1" hook="$2" cmd="$3" cwd="$4" d
    TOTAL=$((TOTAL + 1))
    d="$(decision "$hook" "$cmd" "$cwd")"
    if [[ "$d" == allow ]]; then
        PASS=$((PASS + 1)); echo -e "${GREEN}PASS${NC}: [$label] allow: ${cmd:0:90}"
    else
        FAIL=$((FAIL + 1)); echo -e "${RED}FAIL${NC}: [$label] $d: $cmd"
    fi
}

echo -e "${YELLOW}--- Replay through Loom's hooks and the Repo Skills guard (cwd: main checkout and worktree) ---${NC}"
COMMANDS="$(mktemp)"
while IFS= read -r tpl; do
    [[ -z "$tpl" ]] && continue
    # The two redirect/continuation lines only make sense inside their pipeline.
    case "$tpl" in
        "  commits:"*) expand "$FACTS_HEAD
$tpl" ;;
        "  || {"*) expand "true $tpl" ;;
        *) expand "$tpl" ;;
    esac
    printf '\0'
done <"$TEMPLATES" >"$COMMANDS"

for cwd in "$REPO" "$REPO/.loom/worktrees/issue-1"; do
    where="main"; [[ "$cwd" != "$REPO" ]] && where="worktree"
    while IFS= read -r -d '' cmd; do
        for hook in guard-worktree-paths.sh guard-destructive-generic.sh guard-destructive.sh; do
            replay "loom:$hook@$where" "$HOOKS/$hook" "$cmd" "$cwd"
        done
        if [[ "$INCOMPLETE" -eq 0 ]]; then
            replay "repo-skills@$where" "$CANONICAL" "$cmd" "$cwd"
        fi
    done <"$COMMANDS"
done
rm -f "$COMMANDS"

if [[ "$INCOMPLETE" -eq 1 ]]; then
    echo -e "${RED}INCOMPLETE${NC}: Repo Skills guard not measured ($WHY) — this is not a pass." >&2
    echo "  Total: $TOTAL  Passed: $PASS  Failed: $FAIL (Loom half only)"
    [[ "$FAIL" -gt 0 ]] && exit 1
    exit 3
fi
print_summary
