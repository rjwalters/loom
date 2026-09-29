#!/usr/bin/env bash
# default-branch.sh — branch-NAME primitives: resolve the repository's default
# branch name, and validate any branch name before it reaches a git argv.
#
# Source this file (do not exec). Defines two functions:
#
#   loom_default_branch [remote] -> echoes the default branch name (e.g. "main"
#                                   or "master"); returns non-zero on failure.
#   check_branch_name <name> [what] -> returns 0 when <name> is safe to pass to
#                                   git as a REF OPERAND; non-zero + a stderr
#                                   explanation otherwise (#9106).
#
# Why both live here (and not in a new lib): `.loom/docs/shell-language-policy.md`
# closes `contract` to new shell files, so a new `lib/check-branch-name.sh`
# would be inadmissible; this file is the branch-NAME lib and is already
# sourced by seven of the nine scripts that hand a branch name to git
# (worktree.sh, merge-pr.sh, reconcile-stack.sh, check-main-freshness.sh,
# docs-worktree.sh, pr-worktree.sh, land-resync-commit.sh).
#
# Motivation (#3549): the worktree helpers historically hardcoded the base
# branch as the literal string `main` / `origin/main`. On a repo whose default
# branch is `master` (or any non-`main` name) every git call that references
# `origin/main` fails with `fatal: invalid reference: origin/main`, aborting
# worktree creation. This helper centralizes offline-first default-branch
# detection so both worktree.sh and pr-worktree.sh work regardless of the
# repo's default branch name.
#
# Detection order (first match wins) — offline-first, HARD-FAIL:
#
#   1. LOOM_DEFAULT_BRANCH env var         — explicit escape hatch / test seam.
#   2. git symbolic-ref --short            — the offline, no-network source of
#      refs/remotes/<remote>/HEAD            truth. Present in normal clones;
#                                            `git remote set-head <remote> -a`
#                                            populates it.
#   3. git ls-remote --symref <remote> HEAD — network fallback when origin/HEAD
#                                            is unset locally (fresh clones
#                                            sometimes lack it).
#   4. Local probe                          — check refs/remotes/<remote>/main
#                                            then refs/remotes/<remote>/master,
#                                            pick whichever exists.
#   5. Hard error (return 1 + remediation)  — NEVER silently default to `main`.
#      A silent wrong default reintroduces the exact class of bug this helper
#      exists to fix (an empty or wrong branch on a master-default repo).
#
# Design notes:
#   - `git symbolic-ref` is preferred over `gh repo view --json defaultBranchRef`
#     because Loom is forge-agnostic (it supports Gitea too), needs no network,
#     and needs no `gh` auth. A forge-API tier could be added later but must not
#     be the primary detector.
#   - The helper runs git against the current working directory. Callers that
#     need a specific repo context should `cd` there (or run git -C) before
#     calling, or resolve the branch once at the top of the script while cwd is
#     the main workspace.
#   - No side effects at source time; pure function, mirrors lib/worktree-root.sh.

# loom_default_branch [remote]
#
# Echoes the resolved default branch name on stdout. Returns 0 on success,
# 1 when the branch cannot be determined (with a remediation hint on stderr).
loom_default_branch() {
    local remote="${1:-origin}"

    # 1. Env var override — highest priority (escape hatch + test seam).
    if [[ -n "${LOOM_DEFAULT_BRANCH:-}" ]]; then
        echo "$LOOM_DEFAULT_BRANCH"
        return 0
    fi

    # 2. Local symbolic ref for the remote's HEAD — offline, no network.
    #    Returns e.g. "origin/main"; strip the "<remote>/" prefix.
    local sref
    sref=$(git symbolic-ref --short "refs/remotes/$remote/HEAD" 2>/dev/null || true)
    if [[ -n "$sref" ]]; then
        echo "${sref#"$remote"/}"
        return 0
    fi

    # 3. Network fallback: ask the remote for its HEAD symref.
    #    Output line looks like: "ref: refs/heads/main\tHEAD"; strip the prefix.
    local lsref
    lsref=$(git ls-remote --symref "$remote" HEAD 2>/dev/null \
        | awk '/^ref:/ { print $2; exit }' || true)
    if [[ -n "$lsref" ]]; then
        echo "${lsref#refs/heads/}"
        return 0
    fi

    # 4. Local probe: prefer main, then master, whichever ref exists.
    local candidate
    for candidate in main master; do
        if git show-ref --verify --quiet "refs/remotes/$remote/$candidate" 2>/dev/null; then
            echo "$candidate"
            return 0
        fi
    done

    # 5. Hard fail — do NOT default to main.
    echo "loom_default_branch: could not determine the default branch for remote '$remote'." >&2
    echo "  Fix: run 'git remote set-head $remote -a' to populate refs/remotes/$remote/HEAD," >&2
    echo "  or set LOOM_DEFAULT_BRANCH to the branch name explicitly." >&2
    return 1
}

# check_branch_name <name> [<what>]
#
# The ONE predicate every script must satisfy before a branch name is handed to
# git as a bare operand (`git fetch origin "$branch"`, `git rebase "$upstream"
# "$branch"`). Returns 0 when the name is safe; prints a named refusal to
# stderr and returns non-zero otherwise. `<what>` is a caller-supplied noun
# ("PR head branch", "--base branch", …) used only in the message.
#
# WHY (#9106): git's own ref-name validator ACCEPTS a leading-dash name —
# `git update-ref refs/heads/--upload-pack=/tmp/x` succeeds and the forge will
# host a PR whose headRefName is exactly that. Passed as an operand, git then
# parses it as a SWITCH:
#
#   git fetch origin '--upload-pack=/tmp/payload' main   # path/file:// origin
#                                                        # => EXECUTES /tmp/payload
#   git fetch origin '--depth=1'                         # silently shallow-ifies
#   git rebase <upstream> '--strategy=evil'              # exec merge-evil
#
# A PR author controls `headRefName` and `base.ref`, so this is attacker-
# controlled input reaching an argv. The `--` end-of-options separator added at
# every call site is defence in depth; this predicate is the primary gate, and
# it FAILS CLOSED — a name that does not match is refused, never "best effort".
#
# The predicate, in one place so the shell and Rust (`loom-daemon`'s
# `refname::check_refname`) halves cannot drift:
#
#   1. non-empty
#   2. matches ^[A-Za-z0-9][A-Za-z0-9._/-]*$ — which is what rejects a leading
#      `-` (the switch form), a `=` anywhere (the option-argument form), and
#      every shell/glob metacharacter, whitespace and control byte at once
#   3. no trailing `.` or `/`
#   4. no empty (`//`), dot-leading (`/.`) or `..` path segment
#
# Steps 1/3/4 are the parts git's check-ref-format also enforces; step 2 is
# deliberately STRICTER than git (it is an allowlist, not a denylist) because
# the failure mode here is code execution, not a malformed ref. Legitimate Loom
# branch names — `main`, `feature/issue-42`, `hotfix/x.y`, `a-b_c.d` — all pass.
check_branch_name() {
    # `local LC_ALL=C` is load-bearing, not tidiness. Bracket ranges in a bash
    # `[[ =~ ]]` are resolved by the CURRENT locale's collation, so under the
    # UTF-8 locale a fleet host normally runs, `[[ "ábc" =~ ^[A-Za-z0-9]+$ ]]`
    # MATCHES — the allowlist silently stops being ASCII-only and drifts away
    # from the Rust half (`refname::check_refname`, which uses
    # `is_ascii_alphanumeric` and has a `rejects_non_ascii` test). Forcing the C
    # locale makes the match byte-wise, so both halves accept exactly the same
    # set on every host. Bash restores the previous value when the function
    # returns, so no caller's locale is disturbed.
    local LC_ALL=C
    local name="${1-}" what="${2:-branch name}" why=""
    if   [[ -z "$name" ]];                                     then why="it is empty"
    elif [[ "$name" == -* ]];                                  then why="it starts with '-', which git parses as a command-line SWITCH rather than a ref"
    elif [[ "$name" == *=* ]];                                 then why="it contains '=', the option-argument form (e.g. --upload-pack=/tmp/x)"
    elif ! [[ "$name" =~ ^[A-Za-z0-9][A-Za-z0-9._/-]*$ ]];     then why="it does not match ^[A-Za-z0-9][A-Za-z0-9._/-]*\$"
    elif [[ "$name" == *. || "$name" == */ ]];                 then why="it ends in '.' or '/'"
    elif [[ "$name" == *//* || "$name" == */.* || "$name" == *..* ]]; then why="it has an empty, dot-leading, or '..' path segment"
    else return 0
    fi
    printf '%s\n' "check_branch_name: REFUSING this $what — $(printf '%q' "$name") is not a safe git ref operand: $why." >&2
    printf '%s\n' "  A forge-controlled ref name git can parse as a switch is an option-injection vector (#9106): on a path/file:// origin 'git fetch origin --upload-pack=/tmp/x' EXECUTES /tmp/x, and '--depth=1' silently shallow-ifies the clone. Refusing fail-closed rather than running git on it." >&2
    return 1
}
