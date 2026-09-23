#!/usr/bin/env bash
# check-retired-list-drift.sh — fail a change that deletes a shipped payload
# file from defaults/ without recording it in defaults/.loom-retired.list
# (#8675).
#
# Why this gate exists
# --------------------
# resync-installed.sh keeps an already-installed repo's .loom/ surfaces fresh by
# WALKING defaults/. A walk can only ever visit files that STILL EXIST upstream,
# so a payload file deleted from defaults/ is never noticed and survives forever
# in every installed repo. #5981 added the delete-side counterpart —
# defaults/.loom-retired.list, read by remove_retired_files() on every run —
# but nothing ever gated a deletion on adding the entry, so the list drifted:
# by #8675 three payload files had been deleted from defaults/ with no entry
# (scripts/lib/watchdog-peer-coord-dedup.sh and
# scripts/tests/test-loom-daemon-watchdog-dedup.sh in 80309c318 / #8605,
# scripts/tests/test-spawn-worker.sh in a9e2361e0 / #8363), orphaning them in
# 23 repos on one host.
#
# A mechanism nobody is required to use decays to one nobody uses. This is the
# required-use half, in the same shape .loom/docs/shell-language-policy.md's
# allowlist gate takes: a committed list, and a check that fails on anything in
# neither state.
#
# What it checks (two directions, one list)
# -----------------------------------------
#   1. FORWARD  — every payload path this change DELETED from defaults/ has a
#      matching entry in defaults/.loom-retired.list at HEAD.
#   2. REVERSE  — no entry in defaults/.loom-retired.list names a path that
#      still EXISTS under defaults/ today. remove_retired_files() runs AFTER
#      the walks, so such an entry would make every resync create the file and
#      then immediately delete it again.
#
# What counts as "payload"
# ------------------------
# EXACTLY the surfaces defaults/.loom-retired.list can express — i.e. the
# inverse of resync-installed.sh's retired_target_path() case arms, which is
# itself the inverse of that script's own walk map. Anything under defaults/
# that no walk copies (defaults/config/, defaults/optional/,
# defaults/observability/, defaults/hooks/tests/, the .loom-*.list files
# themselves) is NOT payload and is exempt: there is no installed copy to
# orphan, and no entry form that could name it.
#
#   defaults/hooks/<name>.sh            -> hooks/<name>.sh      (top-level *.sh ONLY)
#   defaults/scripts/<rel>              -> scripts/<rel>
#   defaults/roles/<rel>                -> roles/<rel>
#   defaults/docs/<rel>                 -> docs/<rel>
#   defaults/runtimes/<rel>             -> runtimes/<rel>
#   defaults/.loom/bin/<rel>            -> bin/<rel>
#   defaults/.claude/commands/loom/<rel>-> commands/loom/<rel>
#   defaults/.claude/README.md          -> .claude/README.md
#   defaults/.github/CONFIGURATION.md   -> .github/CONFIGURATION.md
#
# Two surfaces resync-installed.sh syncs are deliberately NOT here, because
# retired_target_path() has no case arm for them and so no entry could ever
# match: defaults/.loom/biome.jsonc + defaults/.claude/biome.jsonc (#6031) and
# defaults/pricing.json. Adding them here without adding the arm there would
# demand an entry that does nothing.
#
# Paths listed in defaults/.loom-internal.list are exempt in both directions:
# the installer never copies them, so there is no installed copy to retire.
#
# Renames are treated as delete + add (`--no-renames`), deliberately. A payload
# file renamed upstream orphans its old installed copy exactly as a deletion
# does, and rename detection is a similarity heuristic — whether this gate
# fires must not depend on how much of a file's content survived the move. See
# .loom/docs/ci-principles.md: dumb and reliable, on purpose.
#
# Usage:
#   bash scripts/check-retired-list-drift.sh              # the gate CI runs
#   bash scripts/check-retired-list-drift.sh --base <ref> # compare against <ref>
#   bash scripts/check-retired-list-drift.sh --self-test  # verify the gate works
#   bash scripts/check-retired-list-drift.sh --help
#
# Exit: 0 pass, 1 drift found (or the gate could not run).

set -uo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'
if [[ ! -t 1 || -n "${NO_COLOR:-}" ]]; then RED=''; GREEN=''; YELLOW=''; BOLD=''; NC=''; fi

MODE="check"
BASE_REF="${LOOM_RETIRED_DRIFT_BASE:-}"

usage() {
    cat <<'EOF'
check-retired-list-drift.sh — gate defaults/ payload deletions on a
defaults/.loom-retired.list entry (#8675).

  --base <ref>   Compare against <ref> instead of the auto-detected merge-base
                 with origin/main (env: LOOM_RETIRED_DRIFT_BASE).
  --self-test    Build throwaway repos and verify the gate still fails what it
                 is supposed to fail. A gate that silently stops checking
                 reports OK forever.
  --help         This text.

Exit 0 pass, 1 drift found (or the gate could not run).
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --base) BASE_REF="${2:-}"; [[ -n "$BASE_REF" ]] || { echo "--base needs a ref" >&2; exit 1; }; shift 2 ;;
        --self-test) MODE="self-test"; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 1 ;;
    esac
done

# ---------- the surface map (inverse of retired_target_path()) ----------

# entry_for_defaults_path <defaults-relative path> -> retired-list entry, or ""
# when the path is not a shipped payload file.
entry_for_defaults_path() {
    local rel="$1"
    case "$rel" in
        hooks/*/*)                 printf '' ;;   # only top-level *.sh is installed
        hooks/*.sh)                printf '%s' "$rel" ;;
        hooks/*)                   printf '' ;;
        scripts/*|roles/*|docs/*|runtimes/*) printf '%s' "$rel" ;;
        .loom/bin/*)               printf 'bin/%s' "${rel#.loom/bin/}" ;;
        .claude/commands/loom/*)   printf 'commands/loom/%s' "${rel#.claude/commands/loom/}" ;;
        .claude/README.md)         printf '.claude/README.md' ;;
        .github/CONFIGURATION.md)  printf '.github/CONFIGURATION.md' ;;
        *)                         printf '' ;;
    esac
}

# defaults_path_for_entry <retired-list entry> -> defaults-relative path, or ""
# when the entry names no expressible surface (a typo, or a form
# retired_target_path() would also skip).
defaults_path_for_entry() {
    local entry="$1"
    case "$entry" in
        hooks/*/*)                 printf '' ;;
        hooks/*.sh)                printf '%s' "$entry" ;;
        scripts/*|roles/*|docs/*|runtimes/*) printf '%s' "$entry" ;;
        bin/*)                     printf '.loom/bin/%s' "${entry#bin/}" ;;
        commands/loom/*)           printf '.claude/commands/loom/%s' "${entry#commands/loom/}" ;;
        .claude/README.md)         printf '.claude/README.md' ;;
        .github/CONFIGURATION.md)  printf '.github/CONFIGURATION.md' ;;
        *)                         printf '' ;;
    esac
}

# Strips comments/whitespace exactly the way remove_retired_files() does, so
# this gate and the consumer of the list cannot disagree about what an entry is.
strip_entry_line() {
    local line="$1"
    line="${line%%#*}"
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    printf '%s' "$line"
}

# list_has <newline-separated haystack> <exact needle>
# Deliberately pipe-free: `printf ... | grep -Fxq` is a pipefail + early-exit
# pipeline, which can SIGPIPE the producer and report a spurious failure
# (scripts/check-pipefail-early-exit.sh, #7790).
list_has() {
    local line
    while IFS= read -r line; do
        [[ "$line" == "$2" ]] && return 0
    done <<< "$1"
    return 1
}

read_list_file() {
    local f="$1" line stripped
    [[ -f "$f" ]] || return 0
    while IFS= read -r line || [[ -n "$line" ]]; do
        stripped="$(strip_entry_line "$line")"
        [[ -n "$stripped" ]] && printf '%s\n' "$stripped"
    done < "$f"
}

# ---------- base-ref resolution ----------
#
# Fails LOUD when it cannot find one rather than passing: a gate that cannot
# tell "checked, fine" from "could not check" reports OK forever
# (.loom/docs/shell-language-policy.md makes the same call for shallow clones).
resolve_base() {
    local r mb head
    head="$(git rev-parse HEAD 2>/dev/null || true)"
    [[ -n "$head" ]] || return 1
    if [[ -n "$BASE_REF" ]]; then
        git rev-parse --verify --quiet "$BASE_REF" >/dev/null 2>&1 || return 1
        mb="$(git merge-base HEAD "$BASE_REF" 2>/dev/null || true)"
        [[ -n "$mb" ]] && { printf '%s' "$mb"; return 0; }
        printf '%s' "$BASE_REF"; return 0
    fi
    for r in origin/main origin/master main master; do
        git rev-parse --verify --quiet "$r" >/dev/null 2>&1 || continue
        mb="$(git merge-base HEAD "$r" 2>/dev/null || true)"
        [[ -n "$mb" ]] || continue
        if [[ "$mb" != "$head" ]]; then printf '%s' "$mb"; return 0; fi
        # HEAD *is* the base branch (a push to main, or a local checkout of it):
        # compare against its own parent so a direct-to-main deletion is still
        # caught instead of trivially diffing a commit against itself.
        if git rev-parse --verify --quiet "HEAD^" >/dev/null 2>&1; then
            printf '%s' "$(git rev-parse HEAD^)"; return 0
        fi
        return 1
    done
    return 1
}

# ---------- the check ----------

run_check() {
    local repo_root list internal base
    repo_root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
    if [[ -z "$repo_root" ]]; then
        printf '%b\n' "${RED}check-retired-list-drift: not a git repository${NC}" >&2
        return 1
    fi
    cd "$repo_root" || return 1
    list="defaults/.loom-retired.list"
    internal="defaults/.loom-internal.list"

    if [[ ! -d defaults ]]; then
        printf '%b\n' "${YELLOW}check-retired-list-drift: no defaults/ in this repo — nothing to check${NC}"
        return 0
    fi

    local entries internal_entries
    entries="$(read_list_file "$list")"
    internal_entries="$(read_list_file "$internal")"

    base="$(resolve_base || true)"
    if [[ -z "$base" ]]; then
        printf '%b\n' "${RED}check-retired-list-drift: could not resolve a base revision to diff against.${NC}" >&2
        printf '%s\n' "  Tried: \$LOOM_RETIRED_DRIFT_BASE / --base, then origin/main, origin/master, main, master." >&2
        printf '%s\n' "  In CI this means the checkout is too shallow — use fetch-depth: 0." >&2
        printf '%s\n' "  Refusing to report OK on a check that did not run." >&2
        return 1
    fi

    local deleted rel entry missing_count=0 stale_count=0 checked_count=0
    deleted="$(git diff --no-renames --diff-filter=D --name-only "$base" HEAD -- defaults/ 2>/dev/null || true)"

    local -a missing_paths=() missing_entries=() stale_entries=()
    while IFS= read -r path; do
        [[ -n "$path" ]] || continue
        rel="${path#defaults/}"
        # An installer-skipped file has no installed copy to orphan.
        if list_has "$internal_entries" "$rel"; then continue; fi
        entry="$(entry_for_defaults_path "$rel")"
        [[ -n "$entry" ]] || continue
        checked_count=$((checked_count + 1))
        if list_has "$entries" "$entry"; then continue; fi
        missing_paths[${#missing_paths[@]}]="$path"
        missing_entries[${#missing_entries[@]}]="$entry"
        missing_count=$((missing_count + 1))
    done <<< "$deleted"

    # REVERSE: an entry whose source still exists would have every resync create
    # the file and then delete it again (remove_retired_files runs after the walks).
    local src
    while IFS= read -r entry; do
        [[ -n "$entry" ]] || continue
        src="$(defaults_path_for_entry "$entry")"
        [[ -n "$src" ]] || continue
        if [[ -e "defaults/$src" ]]; then
            stale_entries[${#stale_entries[@]}]="$entry"
            stale_count=$((stale_count + 1))
        fi
    done <<< "$entries"

    if [[ $missing_count -eq 0 && $stale_count -eq 0 ]]; then
        printf '%b\n' "${GREEN}check-retired-list-drift: OK${NC} (base $(git rev-parse --short "$base" 2>/dev/null || printf '%s' "$base"): ${checked_count} payload deletion(s) since it, all recorded; $(printf '%s\n' "$entries" | grep -c . || true) list entr(ies), none naming a live file)"
        return 0
    fi

    local i
    if [[ $missing_count -gt 0 ]]; then
        printf '%b\n' "${RED}${BOLD}check-retired-list-drift: ${missing_count} payload file(s) deleted from defaults/ with no defaults/.loom-retired.list entry${NC}" >&2
        for ((i = 0; i < ${#missing_paths[@]}; i++)); do
            printf '%b\n' "  ${BOLD}${missing_paths[$i]}${NC}" >&2
            printf '%s\n' "      add to defaults/.loom-retired.list:  ${missing_entries[$i]}" >&2
        done
        cat >&2 <<'EOF'

resync-installed.sh only ever WALKS files that still exist under defaults/, so a
deleted payload file is never noticed and survives forever in every installed
repo. defaults/.loom-retired.list is the delete-side counterpart (#5981): list
the path (target-relative, the form the per-file report uses) with a dated
comment naming the retiring PR/issue, in this same change.

Deleting a file that was never payload? Then it maps to no entry form and this
gate never saw it — re-read the surface map at the top of this script.
EOF
    fi

    if [[ $stale_count -gt 0 ]]; then
        printf '%b\n' "${RED}${BOLD}check-retired-list-drift: ${stale_count} defaults/.loom-retired.list entr(ies) name a file that STILL EXISTS under defaults/${NC}" >&2
        for entry in "${stale_entries[@]}"; do
            printf '%b\n' "  ${BOLD}${entry}${NC}  ->  defaults/$(defaults_path_for_entry "$entry") exists" >&2
        done
        cat >&2 <<'EOF'

remove_retired_files() runs AFTER the walks, so every resync would create this
file from defaults/ and then immediately delete it again. Either finish the
retirement (delete the file from defaults/) or drop the entry — it was added in
error, which is the one case the list's "NEVER remove an entry" rule does not
cover.
EOF
    fi
    return 1
}

# ---------- self-test ----------

st_fail=0
st_pass() { printf '%b\n' "  ${GREEN}PASS${NC}: $1"; }
st_fail_msg() { printf '%b\n' "  ${RED}FAIL${NC}: $1"; st_fail=1; }

# st_fixture <dir> — a throwaway repo with a defaults/ payload tree and one
# committed base revision.
st_fixture() {
    local d="$1"
    rm -rf "$d"
    mkdir -p "$d/defaults/scripts/lib" "$d/defaults/docs" "$d/defaults/config" "$d/defaults/hooks/tests"
    git -C "$d" init -q
    git -C "$d" config user.email loom@example.com
    git -C "$d" config user.name loom
    printf '# retired list fixture\n' > "$d/defaults/.loom-retired.list"
    printf '#!/usr/bin/env bash\necho payload\n' > "$d/defaults/scripts/lib/doomed.sh"
    printf 'docs payload\n' > "$d/defaults/docs/doomed.md"
    printf '{}\n' > "$d/defaults/config/not-payload.json"
    printf '#!/usr/bin/env bash\necho hook test\n' > "$d/defaults/hooks/tests/not-payload.sh"
    git -C "$d" add -A >/dev/null 2>&1
    git -C "$d" commit -qm base >/dev/null 2>&1
}

st_run() {
    local d="$1" base="$2"
    ( cd "$d" && NO_COLOR=1 bash "$SELF" --base "$base" 2>&1 )
}

self_test() {
    local tmp base out rc
    tmp="$(mktemp -d "${TMPDIR:-/tmp}/retired-drift-selftest.XXXXXX")" || return 1
    trap 'rm -rf "$tmp"' RETURN
    echo "check-retired-list-drift --self-test"

    # (1) payload deletion with NO entry -> must FAIL
    st_fixture "$tmp/a"
    base="$(git -C "$tmp/a" rev-parse HEAD)"
    git -C "$tmp/a" rm -q defaults/scripts/lib/doomed.sh
    git -C "$tmp/a" commit -qm "delete payload, no entry" >/dev/null 2>&1
    out="$(st_run "$tmp/a" "$base")"; rc=$?
    if [[ $rc -ne 0 ]] && grep -q 'scripts/lib/doomed.sh' <<<"$out"; then
        st_pass "an unrecorded payload deletion fails and names the entry to add"
    else
        st_fail_msg "an unrecorded payload deletion did not fail (rc=$rc): $out"
    fi

    # (2) the SAME deletion with the entry added -> must PASS
    st_fixture "$tmp/b"
    base="$(git -C "$tmp/b" rev-parse HEAD)"
    git -C "$tmp/b" rm -q defaults/scripts/lib/doomed.sh
    printf '\n# #0000 — fixture retirement\nscripts/lib/doomed.sh\n' >> "$tmp/b/defaults/.loom-retired.list"
    git -C "$tmp/b" add -A >/dev/null 2>&1
    git -C "$tmp/b" commit -qm "delete payload, with entry" >/dev/null 2>&1
    out="$(st_run "$tmp/b" "$base")"; rc=$?
    if [[ $rc -eq 0 ]]; then
        st_pass "a recorded payload deletion passes"
    else
        st_fail_msg "a recorded payload deletion did not pass (rc=$rc): $out"
    fi

    # (3) deleting a NON-payload path under defaults/ -> must PASS, no entry demanded
    st_fixture "$tmp/c"
    base="$(git -C "$tmp/c" rev-parse HEAD)"
    git -C "$tmp/c" rm -q defaults/config/not-payload.json defaults/hooks/tests/not-payload.sh
    git -C "$tmp/c" commit -qm "delete non-payload" >/dev/null 2>&1
    out="$(st_run "$tmp/c" "$base")"; rc=$?
    if [[ $rc -eq 0 ]]; then
        st_pass "deleting a non-payload defaults/ path needs no entry"
    else
        st_fail_msg "a non-payload deletion was wrongly gated (rc=$rc): $out"
    fi

    # (4) a RENAME within defaults/ is a deletion of the old path
    st_fixture "$tmp/d"
    base="$(git -C "$tmp/d" rev-parse HEAD)"
    git -C "$tmp/d" mv defaults/docs/doomed.md defaults/docs/renamed.md
    git -C "$tmp/d" commit -qm "rename payload" >/dev/null 2>&1
    out="$(st_run "$tmp/d" "$base")"; rc=$?
    if [[ $rc -ne 0 ]] && grep -q 'docs/doomed.md' <<<"$out"; then
        st_pass "a payload rename is gated like a deletion (old path orphans identically)"
    else
        st_fail_msg "a payload rename was not gated (rc=$rc): $out"
    fi

    # (5) an entry naming a file that still exists -> must FAIL (reverse drift)
    st_fixture "$tmp/e"
    base="$(git -C "$tmp/e" rev-parse HEAD)"
    printf 'scripts/lib/doomed.sh\n' >> "$tmp/e/defaults/.loom-retired.list"
    git -C "$tmp/e" add -A >/dev/null 2>&1
    git -C "$tmp/e" commit -qm "entry for a live file" >/dev/null 2>&1
    out="$(st_run "$tmp/e" "$base")"; rc=$?
    if [[ $rc -ne 0 ]] && grep -q 'STILL EXISTS' <<<"$out"; then
        st_pass "an entry naming a still-present defaults/ file fails"
    else
        st_fail_msg "an entry naming a still-present file did not fail (rc=$rc): $out"
    fi

    # (6) a clean change that touches nothing under defaults/ -> PASS
    st_fixture "$tmp/f"
    base="$(git -C "$tmp/f" rev-parse HEAD)"
    printf 'unrelated\n' > "$tmp/f/README.md"
    git -C "$tmp/f" add -A >/dev/null 2>&1
    git -C "$tmp/f" commit -qm "unrelated" >/dev/null 2>&1
    out="$(st_run "$tmp/f" "$base")"; rc=$?
    if [[ $rc -eq 0 ]]; then
        st_pass "a change that deletes nothing passes"
    else
        st_fail_msg "a no-deletion change did not pass (rc=$rc): $out"
    fi

    # (7) the surface map still matches resync-installed.sh's retired_target_path()
    #
    # ONE-DIRECTIONAL, on purpose: `arms_expected` below is a hardcoded literal,
    # not a value derived from this gate's own entry_for_defaults_path() /
    # defaults_path_for_entry(). So this catches retired_target_path() drifting
    # (the direction that matters — the consumer is what decides where an
    # installed copy lives, and it can change without this file being touched).
    # It does NOT catch someone editing this gate's map functions and updating
    # `arms_expected` to match: that edit is visible in the same diff, so it is
    # review-caught rather than test-caught. Do not describe this as "the two
    # maps agree" — only one of the two is read from source here.
    local resync arms_expected arms_actual
    resync="$REPO_ROOT/defaults/scripts/resync-installed.sh"
    if [[ -f "$resync" ]]; then
        arms_actual="$(sed -n '/^retired_target_path() {/,/^}/p' "$resync" \
            | sed -n '/case "\$rel" in/,/esac/p' \
            | sed -n 's/^ *\([^ )|]*\)).*/\1/p' | grep -v '^\*$' | LC_ALL=C sort)"
        arms_expected="$(printf '%s\n' \
            'hooks/*' 'scripts/*' 'roles/*' 'docs/*' 'runtimes/*' 'bin/*' \
            'commands/loom/*' '.claude/README.md' '.github/CONFIGURATION.md' | LC_ALL=C sort)"
        if [[ "$arms_actual" == "$arms_expected" ]]; then
            st_pass "resync-installed.sh's retired_target_path() case arms match this gate's expected set (one-directional)"
        else
            st_fail_msg "retired_target_path() case arms drifted from this gate's surface map:
--- resync-installed.sh
$arms_actual
--- this gate
$arms_expected"
        fi
    else
        st_fail_msg "could not find defaults/scripts/resync-installed.sh to cross-check the surface map"
    fi

    if [[ $st_fail -eq 0 ]]; then
        echo "check-retired-list-drift --self-test: all checks passed."
        return 0
    fi
    echo "check-retired-list-drift --self-test: FAILURES above." >&2
    return 1
}

SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || printf '%s' "$(cd "$(dirname "$SELF")/.." && pwd)")"

case "$MODE" in
    self-test) self_test ;;
    *)         run_check ;;
esac
