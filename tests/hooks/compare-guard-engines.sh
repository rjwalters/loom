#!/usr/bin/env bash
# compare-guard-engines.sh — run one command corpus through two generic-guard
# engines and report where their verdicts differ (#8570, loom-guard extraction).
#
# Engine A ("vendored") is Loom's defaults/hooks/guard-destructive-generic.sh.
# Engine B ("canonical") is the Repo Skills guard (hooks/repo/guard-destructive.sh
# in rjwalters/repo, or an installed .claude/skills/repo/hooks/ copy).
#
# This is a COMPARISON TOOL, not a CI suite: it is deliberately not a `test-*.sh`
# and not in ci-wired.txt. CI has no Repo Skills checkout, and a tool that
# "passed" without its second engine would be exactly the skipped-comparison
# false green the design forbids. Instead the missing engine is a distinct exit.
#
# Usage:
#   tests/hooks/compare-guard-engines.sh [--canonical <path>] [--corpus <tsv>] [--quiet]
#
# The canonical guard path comes from --canonical, else $LOOM_GUARD_CANONICAL. It
# is never guessed, so the receipt always names exactly what was compared.
#
# Exit codes:
#   0 - every row: both engines gave the expected verdict (or agreed, if `any`)
#   1 - at least one gap or divergence (see the per-row STATUS column)
#   2 - usage error / missing prerequisite (jq, vendored guard, corpus)
#   3 - INCOMPLETE: the canonical guard was not supplied/readable. The vendored
#       column is still reported against `expect`, but nothing is compared and
#       this is NOT a pass.
#
# STATUS values: ok | vendored-gap | canonical-gap | both-gap | diverge (expect=any)
#                | incomplete (canonical not measured)
# Classifying a flagged row (protection gap / false-positive fix / reason-text
# difference / product-specific) is a human step recorded in
# docs/design/loom-guard-extraction.md; this tool only supplies the evidence.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
VENDORED="${LOOM_GUARD_OVERRIDE:-$REPO_ROOT/defaults/hooks/guard-destructive-generic.sh}"
CANONICAL="${LOOM_GUARD_CANONICAL:-}"
CORPUS="$HERE/fixtures/guard-engine-compat-corpus.tsv"
QUIET=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --canonical) CANONICAL="${2:-}"; shift 2 ;;
        --corpus)    CORPUS="${2:-}"; shift 2 ;;
        --quiet)     QUIET=1; shift ;;
        -h|--help)   sed -n '2,/^set -uo/p' "${BASH_SOURCE[0]}" | sed '$d'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 2; }
[[ -r "$VENDORED" ]] || { echo "vendored guard not readable: $VENDORED" >&2; exit 2; }
[[ -r "$CORPUS" ]] || { echo "corpus not readable: $CORPUS" >&2; exit 2; }

# Hermetic env: ambient guard overrides must not skew either engine.
unset LOOM_FORCE_SCOPE LOOM_DEFAULT_BRANCH LOOM_GUARD_SQL LOOM_GUARD_CLOUD \
      LOOM_GUARD_REVERSIBLE_GH LOOM_RM_SCOPE LOOM_GUARD_READONLY_FASTPATH \
      LOOM_GUARD_WORKTREE_ISOLATION LOOM_WORKTREE_PATH LOOM_WORKTREE_ROOT \
      LOOM_GUARD_DECISION_LOG LOOM_GUARD_DECISION_LOG_FILE LOOM_GUARD_STASH_SCOPE \
      LOOM_ROLE LOOM_GUARD_CARGO_CLEAN CARGO_TARGET_DIR LOOM_GUARD_SCRATCH_ROOT \
      LOOM_GUARD_TMPFS_SCRATCH REPO_GUARD_TMPFS_SCRATCH

WORK="$(mktemp -d)"
# The `tmp-tmpfs` rows need a cwd that the fixture table classifies as NOT RAM: the
# guard stays silent when the acting cwd sits on the same RAM mount as the target
# (the "already in RAM" exemption), and the default repo lives under $TMPDIR. A
# second repo under /var/tmp (ext4 in every fixture table) gives that cwd.
ALT="$(mktemp -d /var/tmp/guard-compare.XXXXXX 2>/dev/null)" \
    || { echo "a writable /var/tmp is required for the tmp-tmpfs rows" >&2; exit 2; }
trap 'rm -rf "$WORK" "$ALT"' EXIT
init_repo() { # <dir>
    mkdir -p "$1"
    git -C "$1" init -q
    git -C "$1" checkout -q -b main 2>/dev/null || true
    git -C "$1" -c user.email=t@t -c user.name=t commit -q --allow-empty -m init
}
init_repo "$WORK/repo"
init_repo "$ALT/repo"
TEST_REPO="$(cd "$WORK/repo" && pwd -P)"
ALT_REPO="$(cd "$ALT/repo" && pwd -P)"

# Fixture mount table (same format as /proc/mounts).
MOUNTS="$WORK/mounts"
cat >"$MOUNTS" <<'M'
/dev/root / ext4 rw 0 0
tmpfs /dev/shm tmpfs rw 0 0
tmpfs /run/user/1000 tmpfs rw 0 0
ramfs /mnt/ramdisk ramfs rw 0 0
M
# Variant tables selected by the corpus' optional 4th column:
#   unreadable - a path that does not exist (macOS / no /proc/mounts): must be silent
#   tmp-tmpfs  - /tmp is tmpfs but /dev/shm is NOT a mount (systemd-style host),
#                so classification must follow the mount type, never a /dev/shm prefix
MOUNTS_UNREADABLE="$WORK/no-such-mount-table"
MOUNTS_TMP="$WORK/mounts-tmp-tmpfs"
cat >"$MOUNTS_TMP" <<'M'
/dev/root / ext4 rw 0 0
tmpfs /tmp tmpfs rw 0 0
M

mounts_for() { # <variant> -> path of the mount table to expose to the guard
    case "$1" in
        unreadable) printf '%s' "$MOUNTS_UNREADABLE" ;;
        tmp-tmpfs)  printf '%s' "$MOUNTS_TMP" ;;
        *)          printf '%s' "$MOUNTS" ;;
    esac
}

verdict() { # <guard> <command> [mounts-variant]
    local out mt cwd="$TEST_REPO"
    mt="$(mounts_for "${3:-}")"
    [[ "${3:-}" == tmp-tmpfs ]] && cwd="$ALT_REPO"
    local out
    out="$(jq -n --arg c "$2" --arg d "$cwd" \
        '{tool_name:"Bash",tool_input:{command:$c},cwd:$d}' \
        | REPO_GUARD_MOUNTS_FILE="$mt" LOOM_GUARD_MOUNTS_FILE="$mt" \
          bash "$1" 2>/dev/null)"
    if [[ -z "$out" ]]; then echo allow; return; fi
    printf '%s' "$out" | jq -r '.hookSpecificOutput.permissionDecision // "allow"' 2>/dev/null || echo allow
}

sha() { sha256sum "$1" | cut -c1-12; }
gitrev() { git -C "$(dirname "$1")" rev-parse --short=12 HEAD 2>/dev/null || echo "not-in-git"; }

INCOMPLETE=0
why=""
if [[ -z "$CANONICAL" ]]; then
    INCOMPLETE=1; why="no canonical guard supplied (--canonical / LOOM_GUARD_CANONICAL)"
elif [[ ! -r "$CANONICAL" ]]; then
    INCOMPLETE=1; why="canonical guard not readable: $CANONICAL"
fi

echo "# receipt"
echo "vendored : $VENDORED sha256=$(sha "$VENDORED") repo-head=$(gitrev "$VENDORED")"
if [[ "$INCOMPLETE" -eq 0 ]]; then
    echo "canonical: $CANONICAL sha256=$(sha "$CANONICAL") repo-head=$(gitrev "$CANONICAL")"
else
    echo "canonical: NOT MEASURED ($why)"
fi
echo "corpus   : $CORPUS"
printf '%-13s %-7s %-12s %-12s %s\n' STATUS EXPECT VENDORED CANONICAL "AREA | COMMAND"

bad=0; n=0
while IFS=$'\t' read -r expect area cmd variant; do
    [[ -z "${expect// }" || "$expect" == \#* ]] && continue
    n=$((n + 1))
    v="$(verdict "$VENDORED" "$cmd" "$variant")"
    if [[ "$INCOMPLETE" -eq 1 ]]; then
        c="not-measured"
    else
        c="$(verdict "$CANONICAL" "$cmd" "$variant")"
    fi
    if [[ "$expect" == any ]]; then
        if [[ "$INCOMPLETE" -eq 1 ]]; then status="incomplete"
        elif [[ "$v" == "$c" ]]; then status="ok"; else status="diverge"; fi
    else
        vok=0; cok=0
        [[ "$v" == "$expect" ]] && vok=1
        [[ "$c" == "$expect" ]] && cok=1
        if [[ "$INCOMPLETE" -eq 1 ]]; then
            if [[ "$vok" -eq 1 ]]; then status="incomplete"; else status="vendored-gap"; fi
        elif [[ "$vok" -eq 1 && "$cok" -eq 1 ]]; then status="ok"
        elif [[ "$vok" -eq 0 && "$cok" -eq 1 ]]; then status="vendored-gap"
        elif [[ "$vok" -eq 1 && "$cok" -eq 0 ]]; then status="canonical-gap"
        else status="both-gap"; fi
    fi
    [[ "$status" == ok ]] || bad=$((bad + 1))
    if [[ "$QUIET" -eq 0 || "$status" != ok ]]; then
        printf '%-13s %-7s %-12s %-12s %s | %s\n' "$status" "$expect" "$v" "$c" "$area" "$cmd"
    fi
done <"$CORPUS"

echo "# rows=$n flagged=$bad"
if [[ "$INCOMPLETE" -eq 1 ]]; then
    echo "# INCOMPLETE EVIDENCE: canonical engine not measured — this is not a pass." >&2
    exit 3
fi
if [[ "$bad" -eq 0 ]]; then exit 0; else exit 1; fi
