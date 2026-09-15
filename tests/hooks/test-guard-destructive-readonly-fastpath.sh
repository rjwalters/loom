#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — readonly fastpath.
#
# One slice of the former monolithic tests/hooks/test-guard-destructive.sh,
# split per #7741. Shared fixtures, assertions and catastrophic-phrase payloads
# live in tests/hooks/lib/guard-destructive-harness.sh.
#
# Usage: ./tests/hooks/test-guard-destructive-readonly-fastpath.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

echo -e "${YELLOW}--- Read-only fast path (guards.readOnlyFastPath / LOOM_GUARD_READONLY_FASTPATH, #3687) ---${NC}"
# =========================================================================

# assert_allow_silent: allow AND zero stdout+stderr bytes. The fast path must
# emit nothing at all on admission (no decision JSON, no log noise).

# --- Admission + silence: every built-in allowlisted verb allows with 0 bytes ---
assert_allow_silent "Fast path: git status admits silently" "git status"
assert_allow_silent "Fast path: git log admits silently" "git log --oneline -5"
assert_allow_silent "Fast path: git diff admits silently" "git diff HEAD"
assert_allow_silent "Fast path: git show admits silently" "git show HEAD"
assert_allow_silent "Fast path: ls admits silently" "ls -la"
assert_allow_silent "Fast path: grep admits silently" "grep -n foo bar.txt"
assert_allow_silent "Fast path: rg admits silently" "rg pattern src/"
assert_allow_silent "Fast path: gh pr view admits silently" "gh pr view 12"
assert_allow_silent "Fast path: gh issue list admits silently" "gh issue list --label loom:issue"
assert_allow_silent "Fast path: aws ec2 describe-instances admits silently" "aws ec2 describe-instances"
assert_allow_silent "Fast path: aws s3 ls admits silently" "aws s3 ls s3://bucket"
assert_allow_silent "Fast path: aws lambda get-function admits silently" "aws lambda get-function --function-name f"
# --- #3772: broadened default allowlist verbs admit read-only invocations ---
assert_allow_silent "Fast path: jq admits silently (#3772)" "jq -n '.'"
assert_allow_silent "Fast path: wc admits silently (#3772)" "wc -l file.txt"
assert_allow_silent "Fast path: head admits silently (#3772)" "head -n5 file.txt"
assert_allow_silent "Fast path: tail admits silently (#3772)" "tail -n5 file.txt"
assert_allow_silent "Fast path: test admits silently (#3772)" "test -f file.txt"
assert_allow_silent "Fast path: [ admits silently (#3772)" "[ -f file.txt ]"
assert_allow_silent "Fast path: [[ admits silently (#3772)" "[[ -f file.txt ]]"
assert_allow_silent "Fast path: find (no action primary) admits silently (#3772)" "find . -name '*.sh'"

# The two "default ON" observable assertions below only apply when the fast path
# is not force-disabled via the ambient env var. Under a
# `LOOM_GUARD_READONLY_FASTPATH=0 ./tests/...` full-suite run they are skipped so
# the pre-existing cases still verify byte-for-byte (issue #3687 test plan #4).
_FP_AMBIENT_ON=1
case "${LOOM_GUARD_READONLY_FASTPATH:-}" in 0|false|no) _FP_AMBIENT_ON=0 ;; esac

# --- Observable admission: fast path bypasses the SQL-DDL substring false-
#     positive for a read-only grep. The DDL literal is assembled from shell
#     fragments so this file's own source never carries a raw "DROP TABLE"
#     (mirrors the force-push fragment convention used for the #3679 tests). ---
_FP_DDL="DR""OP TA""BLE"
if [[ "$_FP_AMBIENT_ON" == "1" ]]; then
    assert_allow_silent "Fast path: read-only 'grep <ddl>' bypasses SQL-DDL false-positive (default on)" \
        "grep '$_FP_DDL' schema.sql"
    # --- #3772: observable-admission proof for the broadened verbs. Each carries
    #     the DDL literal as an argument (guard-scanned, never executed). A bare
    #     silent-allow can't distinguish "fast-pathed" from "fell through to the
    #     full path and allowed anyway", but the full path would `ask` on this
    #     content, so a silent allow proves the fast path decided the outcome. ---
    assert_allow_silent "Fast path: 'jq <ddl arg>' bypasses SQL-DDL false-positive (#3772)" \
        "jq -n --arg s '$_FP_DDL' '.'"
    assert_allow_silent "Fast path: 'wc <ddl arg>' bypasses SQL-DDL false-positive (#3772)" \
        "wc -l '$_FP_DDL'"
    assert_allow_silent "Fast path: 'head <ddl arg>' bypasses SQL-DDL false-positive (#3772)" \
        "head -n1 '$_FP_DDL'"
    assert_allow_silent "Fast path: 'tail <ddl arg>' bypasses SQL-DDL false-positive (#3772)" \
        "tail -n1 '$_FP_DDL'"
    assert_allow_silent "Fast path: 'test <ddl arg>' bypasses SQL-DDL false-positive (#3772)" \
        "test '$_FP_DDL' = x"
    assert_allow_silent "Fast path: 'find -iname <ddl arg>' bypasses SQL-DDL false-positive (#3772)" \
        "find . -iname '$_FP_DDL'"
fi

# --- #3772: find's dangerous action-primaries are structurally excluded. Using
#     the same DDL-content harness makes the assertion falsifiable: -delete /
#     -exec disqualify fast-path eligibility, so the command falls through to the
#     full path where the SQL-DDL deny pattern still fires on the DDL argument.
#     (assert_deny holds regardless of the ambient fast-path toggle, mirroring
#     the 'grep <ddl> | cat' full-path deny above.) ---
assert_deny "Fast path security: 'find … -delete' is NOT fast-pathed (#3772)" \
    "find . -iname '$_FP_DDL' -delete"
assert_deny "Fast path security: 'find … -exec' is NOT fast-pathed (#3772)" \
    "find . -iname '$_FP_DDL' -exec rm {} \\;"
# -fls is a FILE-WRITING action-primary (the -ls-format sibling of -fprint*):
# `find … -fls FILE` truncates/overwrites FILE with the listing on both GNU and
# BSD/macOS find. It must disqualify fast-path eligibility exactly like its
# -fprint* siblings — a silent fast-path allow here would bypass every deny/ask
# check and violate the read-only invariant.
assert_deny "Fast path security: 'find … -fls' is NOT fast-pathed (#3772)" \
    "find . -iname '$_FP_DDL' -fls out.txt"

# --- Security: compound / substitution / redirection / wrapper / non-bare forms
#     are NOT eligible and keep their exact pre-existing verdict via the full
#     path. False positives are the only danger, so these are the core gate. ---
# && chain carrying a real force-push → ALWAYS_BLOCK still fires (deny).
assert_deny "Fast path security: 'git status && <force-push main>' still denies" \
    "git status && $_FP_MAIN"
# ; chain carrying a real force-push → ALWAYS_BLOCK still fires (deny).
assert_deny "Fast path security: 'git status ; <force-push main>' still denies" \
    "git status ; $_FP_MAIN"
# $(...) substitution: excluded char → full path; the inner catastrophic rm is
# still caught by the ALWAYS_BLOCK raw scan (deny). The rm root target is
# assembled from a fragment so this file's source carries no raw "rm -rf /".
_FP_ROOT="/"
assert_deny "Fast path security: 'git status \$(rm -rf /)' takes full path and denies" \
    "git status \$(rm -rf $_FP_ROOT)"
# Pipe to a read-only sink: VERDICT CHANGED by #5263. A read-only search piped to
# a read-only sink (cat/head/tail/wc/less/more) is 100% read-only — the DDL phrase
# lives only inside grep's quoted search argument, which grep never executes — so
# the narrow search-pipe carve-out (fastpath_grep_pipe_admits) now admits it,
# matching the already-allowed bare `grep <ddl>` form. Before #5263 the pipe
# disqualified the fast path and the full-path SQL-DDL check false-positived on
# grep's own argument (deny). This was the self-defeating false positive #5263
# fixes: `grep 'DROP TABLE' … | head` is one of the most common interactive idioms.
if [[ "$_FP_AMBIENT_ON" == "1" ]]; then
    assert_allow_silent "Fast path: 'grep <ddl> | cat' read-only search-pipe now admits (#5263)" \
        "grep '$_FP_DDL' x.sql | cat"
    assert_allow_silent "Fast path: 'grep <ddl> | head' read-only search-pipe admits (#5263)" \
        "grep '$_FP_DDL' x.sql | head"
    assert_allow_silent "Fast path: 'grep <ddl> | head -n 40' (head takes any args) admits (#5263)" \
        "grep '$_FP_DDL' x.sql | head -n 40"
    assert_allow_silent "Fast path: 'grep <ddl> | tail -5' read-only search-pipe admits (#5263)" \
        "grep '$_FP_DDL' x.sql | tail -5"
    assert_allow_silent "Fast path: 'grep <ddl> | wc -l' read-only search-pipe admits (#5263)" \
        "grep '$_FP_DDL' x.sql | wc -l"
    assert_allow_silent "Fast path: 'grep <ddl> | less' stdin-sink admits (#5263)" \
        "grep '$_FP_DDL' x.sql | less"
    assert_allow_silent "Fast path: 'grep <ddl> | cat -n' (flag-only cat) admits (#5263)" \
        "grep '$_FP_DDL' x.sql | cat -n"
    assert_allow_silent "Fast path: 'rg <ddl> | head' rg upstream admits (#5263)" \
        "rg '$_FP_DDL' x.sql | head"
    assert_allow_silent "Fast path: 'egrep <ddl> | wc -l' egrep upstream admits (#5263)" \
        "egrep '$_FP_DDL' x.sql | wc -l"
    assert_allow_silent "Fast path: 'fgrep <ddl> | cat' fgrep upstream admits (#5263)" \
        "fgrep '$_FP_DDL' x.sql | cat"
fi
# Security (#5263): the search-pipe carve-out is NARROW. A real DDL-executing
# command piped to a read-only sink has a non-search first token, so it is NOT
# admitted and the full-path SQL-DDL check still fires (deny). This is the
# obfuscation-still-caught guarantee: a pipe to `cat` cannot launder a live DDL.
assert_deny "Fast path security: 'mysql -e <ddl> | cat' (real DDL executor) still denies (#5263)" \
    "mysql -e '$_FP_DDL' | cat"
assert_deny "Fast path security: 'psql -c <ddl> | head' (real DDL executor) still denies (#5263)" \
    "psql -c '$_FP_DDL' | head"
# A search piped to a NON-sink command (not in the read-only sink allowlist) is
# NOT admitted — only the fixed sink allowlist qualifies, so this falls through to
# the full path where the SQL-DDL check fires on grep's argument (deny).
assert_deny "Fast path security: 'grep <ddl> | sh' (pipe to non-sink) still denies (#5263)" \
    "grep '$_FP_DDL' x.sql | sh"
# cat WITH a credential-file operand must NOT be fast-pathed: the stdin-only sink
# rule rejects any positional operand, so the command falls through to the full
# path where cat's existing .ssh ASK carve-out still fires (ask, not silent allow).
# A NON-DDL search is used here so the verdict isolates the cat carve-out — a DDL
# phrase in grep's argument would deny at the earlier catastrophic sql-ddl tier
# first, masking whether the credential ASK was preserved.
assert_ask "Fast path security: 'grep foo | cat ~/.ssh/id_rsa' still asks (cat operand not fast-pathed, #5263)" \
    "grep foo x.sql | cat ~/.ssh/id_rsa"
# A second pipe declines the (single-pipe) carve-out and falls through to the full
# path, where the SQL-DDL check fires on grep's argument (deny). Conservative by
# design: a multi-stage read-only pipe is a false negative, never a hole.
assert_deny "Fast path security: 'grep <ddl> | grep x | head' (two pipes) declines carve-out, denies (#5263)" \
    "grep '$_FP_DDL' x.sql | grep foo | head"

# --- #5673: fastpath_grep_pipe_admits() must count only REAL (shell-
#     significant) pipes, not a raw `|` character scan. Before this fix, a
#     `|` inside grep's OWN quoted alternation pattern (a very natural way to
#     search for either of two related terms, e.g. the DDL literal itself
#     joined with a second term) was mistaken for a second shell pipe, so the
#     genuine trailing `| head` looked like a third/second pipe and the whole
#     command declined the carve-out — falling through to the full path,
#     which then denied on the bare substring match inside grep's own
#     argument. See the live incident report (#5673): this exact shape was
#     denied roughly an hour after #5274 shipped the narrow-pipe carve-out.
if [[ "$_FP_AMBIENT_ON" == "1" ]]; then
    assert_allow_silent "Fast path: double-quoted alternation '<ddl>\\|OTHER' | head admits (#5673)" \
        "grep -n \"$_FP_DDL\\|SQL_DDL_PATTERN\" x.sql | head -5"
    assert_allow_silent "Fast path: single-quoted alternation '<ddl>\\|OTHER' | wc -l admits (#5673)" \
        "grep '$_FP_DDL\\|OTHER' x.sql | wc -l"
    assert_allow_silent "Fast path: unquoted backslash-escaped pipe '<ddl>\\|OTHER' | head admits (#5673)" \
        "grep $_FP_DDL\\|OTHER x.sql | head"
    assert_allow_silent "Fast path: rg upstream with quoted alternation | cat admits (#5673)" \
        "rg \"$_FP_DDL\\|OTHER\" x.sql | cat"
fi
# Security regression guard: a quoted alternation pipe must NOT hide a real
# SECOND pipe from the count — two genuine pipes (one quoted decoy plus two
# real ones) must still decline and deny, exactly like the plain two-pipe
# case above. If the quote-aware counter ever started ignoring real pipes
# too, this would silently regress to an allow.
assert_deny "Fast path security: quoted alternation + TWO real pipes still declines, denies (#5673)" \
    "grep \"$_FP_DDL\\|OTHER\" x.sql | grep foo | head"
# A quoted alternation with NO real pipe at all is unrelated to this fix (the
# base allowlist's fastpath_structural_ok() naively rejects any literal `|`
# regardless of quoting, a separate, pre-existing gap outside #5673's scope)
# — still denies via the full path exactly as before, unaffected either way.
assert_deny "Fast path: quoted alternation with no real pipe still denies (unaffected by #5673)" \
    "grep \"$_FP_DDL\\|OTHER\" x.sql"

# Wrapper: first token is bash (not an allowlist word) → not admitted, and the
# search-pipe carve-out is UNCHANGED for wrappers (its metachar reject rules out
# the quoted payload's own pipe too). Observable via the SQL grep the wrapper
# carries (full path denies). #5263 deliberately does NOT relax this.
assert_deny "Fast path security: 'bash -c \"grep <ddl>\"' wrapper not admitted (SQL-DDL denies)" \
    "bash -c \"grep '$_FP_DDL' x.sql\""
assert_deny "Fast path security: 'bash -c \"grep <ddl> | head\"' wrapper+pipe not admitted (SQL-DDL denies, #5263)" \
    "bash -c \"grep '$_FP_DDL' x.sql | head\""
# Non-bare git subcommand form: `git -C /p status` is not admitted; still allows
# via the existing full path (verdict unchanged, just unoptimized).
assert_allow "Fast path: 'git -C /tmp status' not fast-pathed, still allowed via full path" \
    "git -C /tmp status"
# cat is deliberately excluded: its existing .ssh ASK carve-out must still fire.
assert_ask "Fast path: 'cat ~/.ssh/id_rsa' still asks (cat excluded from fast path)" \
    "cat ~/.ssh/id_rsa"

# --- Toggle off restores the full-path verdict byte-for-byte (env + config) ---
assert_deny_env "Fast path off (env): 'grep <ddl>' takes full path and denies" \
    "LOOM_GUARD_READONLY_FASTPATH=0" "grep '$_FP_DDL' schema.sql"
# The #5263 search-pipe carve-out is gated by the SAME toggle: with the fast path
# force-disabled, the piped grep also takes the full path and denies (proving the
# carve-out is not a separate always-on bypass).
assert_deny_env "Fast path off (env): 'grep <ddl> | head' search-pipe also denies (#5263)" \
    "LOOM_GUARD_READONLY_FASTPATH=0" "grep '$_FP_DDL' schema.sql | head"
FASTPATH_OFF_REPO=$(make_sql_repo '{"guards":{"readOnlyFastPath":false}}')
assert_deny "Fast path off (config): 'grep <ddl>' takes full path and denies" \
    "grep '$_FP_DDL' schema.sql" "$FASTPATH_OFF_REPO"
assert_deny "Fast path off (config): 'grep <ddl> | head' search-pipe also denies (#5263)" \
    "grep '$_FP_DDL' schema.sql | head" "$FASTPATH_OFF_REPO"
# Env override wins over config (mirrors the sqlDdl/cloudCli precedent): env=1
# forces the fast path ON even when the config disables it.
assert_allow_env "Fast path: LOOM_GUARD_READONLY_FASTPATH=1 overrides config-off (allow)" \
    "LOOM_GUARD_READONLY_FASTPATH=1" "grep '$_FP_DDL' schema.sql" "$FASTPATH_OFF_REPO"

# --- Extend-only escape hatch: guards.readOnlyFastPathExtra admits a custom
#     bare first-word command (full-generality bypass for that word). ---
FASTPATH_EXTRA_REPO=$(make_sql_repo '{"guards":{"readOnlyFastPathExtra":["psql"]}}')
# psql is not a built-in allowlist word; the extra list admits it, bypassing the
# SQL-DDL check (allow). Demonstrates the escape hatch works. Skipped under an
# ambient LOOM_GUARD_READONLY_FASTPATH=0 run (the env var would disable it).
if [[ "$_FP_AMBIENT_ON" == "1" ]]; then
    assert_allow "Fast path extra: 'psql <ddl>' admitted via readOnlyFastPathExtra (bypass)" \
        "psql -c '$_FP_DDL'" "$FASTPATH_EXTRA_REPO"
fi
# A first word NOT in the extra list still takes the full path (SQL-DDL denies),
# proving the extra list does not leak to arbitrary commands.
assert_deny "Fast path extra: 'mysql <ddl>' (not listed) still denies via full path" \
    "mysql -c '$_FP_DDL'" "$FASTPATH_EXTRA_REPO"

# Clean up temp repos created in this section.
for _fp_dir in "$FASTPATH_OFF_REPO" "$FASTPATH_EXTRA_REPO"; do
    [[ -n "$_fp_dir" && "$_fp_dir" != "/" && -d "$_fp_dir/.loom" ]] && rm -rf "$_fp_dir"
done

# --- Tiered config (Epic #3835 Phase 5, #4262): .loom-project/project.json --
# Create a throwaway git repo whose .loom-project/project.json (the tracked
# tier) holds the given JSON, optionally alongside a legacy .loom/config.json
# to exercise tier precedence. Echoes the repo path.

if [[ "$_FP_AMBIENT_ON" == "1" ]]; then
    PROJECT_TIER_REPO=$(make_project_tier_repo '{"guards":{"readOnlyFastPath":false}}')
    assert_deny "Fast path tiered config: .loom-project/project.json readOnlyFastPath=false disables fast path" \
        "grep '$_FP_DDL' schema.sql" "$PROJECT_TIER_REPO"

    # Project tier (higher precedence) overrides a conflicting legacy tier.
    OVERRIDE_REPO=$(make_project_tier_repo '{"guards":{"readOnlyFastPath":false}}' '{"guards":{"readOnlyFastPath":true}}')
    assert_deny "Fast path tiered config: project tier overrides conflicting legacy tier (project wins)" \
        "grep '$_FP_DDL' schema.sql" "$OVERRIDE_REPO"

    # readOnlyFastPathExtra also resolves from the project tier.
    PROJECT_EXTRA_REPO=$(make_project_tier_repo '{"guards":{"readOnlyFastPathExtra":["psql"]}}')
    assert_allow "Fast path tiered config: readOnlyFastPathExtra from .loom-project admits 'psql'" \
        "psql -c '$_FP_DDL'" "$PROJECT_EXTRA_REPO"

    for _fp_dir in "$PROJECT_TIER_REPO" "$OVERRIDE_REPO" "$PROJECT_EXTRA_REPO"; do
        [[ -n "$_fp_dir" && "$_fp_dir" != "/" && -d "$_fp_dir/.loom-project" ]] && rm -rf "$_fp_dir"
    done
fi

echo ""

# =========================================================================

print_summary
