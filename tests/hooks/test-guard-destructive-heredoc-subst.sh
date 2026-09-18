#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — an UNQUOTED
# heredoc body's `$( … )` / backtick spans under worktree-write-confinement
# (#8035).
#
# Sibling of test-guard-destructive-write-confinement.sh, which owns the rest
# of the #4178 Bash-tool write-confinement surface; this one is split out per
# .loom/docs/file-size-policy.md (that suite is over the size threshold and is
# frozen, so new coverage goes in a new sibling module rather than growing it).
# Shared fixtures, assertions and the GUARD path live in
# tests/hooks/lib/guard-destructive-harness.sh exactly as they do there.
#
# Usage: ./tests/hooks/test-guard-destructive-heredoc-subst.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

echo -e "${YELLOW}--- Unquoted-heredoc-body substitution spans vs. write confinement (#8035) ---${NC}"
# =========================================================================
# #8035 — an UNQUOTED heredoc body's `$( … )` / backtick spans are LIVE
# =========================================================================
#
# `cat > /tmp/x <<EOF` does NOT make its body literal. The outer shell performs
# command substitution on an unquoted-delimiter body BEFORE the sink reads a
# byte of it, so a `cp` / `mkdir` / `mv` / `sed -i` hidden in a `$( … )` span
# there executes for real — measured in a throwaway fixture: the file lands in
# the main checkout. The write-confinement scan never saw it, because
# extract_write_targets() keys every write idiom on toks[1] of a
# `;`/`&`/`|`-delimited segment and qsplit() does not treat `$(`/`)` as segment
# boundaries: the whole heredoc invocation is ONE segment whose command word is
# `cat`. Write-path analogue of the index-mutation hole closed by #8003
# (im_mask_heredocs()/IMHDQ[]/im_hd_expand()).
#
# The three discriminators are pinned here, not just the headline deny:
#   * UNQUOTED delimiter + live span  -> DENY  (the fix)
#   * QUOTED delimiter (<<'EOF'/<<"EOF") -> ALLOW (body genuinely inert; this
#     is why the masker is allowed to blank it at all, and it must not flip)
#   * backslash-escaped `\$( … )` in an unquoted body -> ALLOW (the one
#     expansion suppressor the shell honours inside a heredoc body)
WT8035_REPO=$(make_wt_repo_linked)
WT8035_DIR="$WT8035_REPO/.loom/worktrees/issue-1"

# --- AC4: the reported fail-open rows, all cwd = the managed worktree -------
assert_deny_in_worktree "write-confinement (#8035): cp into the main checkout inside an UNQUOTED heredoc body's \$( ) span denies" \
    "cat > /tmp/loom-8035-$$.txt <<EOF
\$( cp /tmp/loom-8035-src-$$ \"$WT8035_REPO/pwned.txt\" )
EOF" "$WT8035_DIR"
assert_deny_in_worktree "write-confinement (#8035): mkdir -p into the main checkout inside an UNQUOTED heredoc body's \$( ) span denies" \
    "cat > /tmp/loom-8035-$$.txt <<EOF
\$( mkdir -p \"$WT8035_REPO/pwned-dir\" )
EOF" "$WT8035_DIR"
assert_deny_in_worktree "write-confinement (#8035): mv into the main checkout inside an UNQUOTED heredoc body's \$( ) span denies" \
    "cat > /tmp/loom-8035-$$.txt <<EOF
\$( mv /tmp/loom-8035-src-$$ \"$WT8035_REPO/pwned.txt\" )
EOF" "$WT8035_DIR"
assert_deny_in_worktree "write-confinement (#8035): sed -i on a main-checkout file inside an UNQUOTED heredoc body's \$( ) span denies" \
    "cat > /tmp/loom-8035-$$.txt <<EOF
\$( sed -i 's/a/b/' \"$WT8035_REPO/defaults/hooks/f.sh\" )
EOF" "$WT8035_DIR"
# This one already DENIED before the fix and is pinned as a control, not as a
# regression: a bare `>` operator token is caught by the boundary-agnostic
# `>`/`>>` scan on the (already visible) live body line, with no command word
# needed. Every idiom keyed on toks[1] — cp/mv/mkdir/sed -i — is what escaped.
assert_deny_in_worktree "write-confinement (#8035 control): redirection into the main checkout inside an UNQUOTED heredoc body's \$( ) span denies (already covered pre-fix)" \
    "cat > /tmp/loom-8035-$$.txt <<EOF
\$( echo x > \"$WT8035_REPO/pwned.txt\" )
EOF" "$WT8035_DIR"

# Backtick spelling of the same substitution — the older syntax bash expands
# identically inside an unquoted body.
assert_deny_in_worktree "write-confinement (#8035): cp inside an UNQUOTED heredoc body's BACKTICK span denies" \
    "cat > /tmp/loom-8035-$$.txt <<EOF
\` cp /tmp/loom-8035-src-$$ \"$WT8035_REPO/pwned.txt\" \`
EOF" "$WT8035_DIR"

# `<<-EOF` (tab-stripping) opener — same body semantics, different opener.
assert_deny_in_worktree "write-confinement (#8035): cp inside an UNQUOTED <<-EOF heredoc body's \$( ) span denies" \
    "cat > /tmp/loom-8035-$$.txt <<-EOF
\$( cp /tmp/loom-8035-src-$$ \"$WT8035_REPO/pwned.txt\" )
EOF" "$WT8035_DIR"

# The span continued across a physical line with a trailing backslash: qsplit()
# rejoins the continuation (#7945) once the span text is scanned as its own
# command, so the real destination is not stranded on a line-only segment.
assert_deny_in_worktree "write-confinement (#8035): cp inside an UNQUOTED heredoc body's \$( ) span still denies when the span spans a backslash continuation" \
    "cat > /tmp/loom-8035-$$.txt <<EOF
\$( cp /tmp/loom-8035-src-$$ \\
\"$WT8035_REPO/pwned.txt\" )
EOF" "$WT8035_DIR"

# A span that OPENS on one line and CLOSES on a later one (no continuation
# backslash — a genuine multi-line `$( … )`).
assert_deny_in_worktree "write-confinement (#8035): cp inside a MULTI-LINE \$( ) span in an UNQUOTED heredoc body denies" \
    "cat > /tmp/loom-8035-$$.txt <<EOF
\$( echo one
cp /tmp/loom-8035-src-$$ \"$WT8035_REPO/pwned.txt\" )
EOF" "$WT8035_DIR"

# Nested one level down — the recursion (bounded at depth 5) re-scans each
# span's own inner text, so a command word only reachable inside an inner span
# is still surfaced.
assert_deny_in_worktree "write-confinement (#8035): cp inside a NESTED \$( \$( … ) ) span in an UNQUOTED heredoc body denies" \
    "cat > /tmp/loom-8035-$$.txt <<EOF
\$( echo \$( cp /tmp/loom-8035-src-$$ \"$WT8035_REPO/pwned.txt\" ) )
EOF" "$WT8035_DIR"

# The span reached through an INTERPRETER-fed unquoted heredoc (`bash <<EOF`),
# whose body the outer shell expands before bash ever runs it.
assert_deny_in_worktree "write-confinement (#8035): cp inside an UNQUOTED bash-heredoc body's \$( ) span denies" \
    "bash <<EOF
\$( cp /tmp/loom-8035-src-$$ \"$WT8035_REPO/pwned.txt\" )
EOF" "$WT8035_DIR"

# --- AC2: a QUOTED delimiter keeps today's behaviour (body stays inert) -----
assert_allow_in_worktree "write-confinement (#8035, AC2): a QUOTED <<'EOF' delimiter keeps its body inert — the same \$( cp ) text still allows" \
    "cat > /tmp/loom-8035-$$.txt <<'EOF'
\$( cp /tmp/loom-8035-src-$$ \"$WT8035_REPO/pwned.txt\" )
EOF" "$WT8035_DIR"
assert_allow_in_worktree "write-confinement (#8035, AC2): a QUOTED <<\"EOF\" delimiter keeps its body inert too" \
    "cat > /tmp/loom-8035-$$.txt <<\"EOF\"
\$( cp /tmp/loom-8035-src-$$ \"$WT8035_REPO/pwned.txt\" )
EOF" "$WT8035_DIR"

# --- AC3: a backslash-escaped `\$( … )` in an UNQUOTED body stays inert -----
assert_allow_in_worktree "write-confinement (#8035, AC3): a backslash-escaped \\\$( ) in an UNQUOTED heredoc body is literal text and still allows" \
    "cat > /tmp/loom-8035-$$.txt <<EOF
\\\$( cp /tmp/loom-8035-src-$$ \"$WT8035_REPO/pwned.txt\" )
EOF" "$WT8035_DIR"
assert_allow_in_worktree "write-confinement (#8035, AC3): a backslash-escaped backtick span in an UNQUOTED heredoc body is literal text and still allows" \
    "cat > /tmp/loom-8035-$$.txt <<EOF
\\\` cp /tmp/loom-8035-src-$$ \"$WT8035_REPO/pwned.txt\" \\\`
EOF" "$WT8035_DIR"

# --- Narrows, never widens: the #7247/#5181/#6056 false-positive fixes ------
# Ordinary prose in an unquoted body (including a literal `>` and a harmless
# single-line `$( … )` alongside it) must stay ALLOW — this pass only ever adds
# the SPAN text to the scan, never the surrounding prose.
assert_allow_in_worktree "write-confinement (#8035 x #7247): prose with a literal '>' plus a harmless \$( date ) span in an UNQUOTED body still allows" \
    "cat > /tmp/loom-8035-$$.md <<EOF
rotting >=3d, clean
generated \$( date -u +%Y-%m-%d )
EOF" "$WT8035_DIR"
# A span whose write lands INSIDE the acting worktree is a legitimate write and
# must not be denied just because it sits in a heredoc body.
assert_allow_in_worktree "write-confinement (#8035): a \$( cp ) span writing INSIDE the acting worktree still allows" \
    "cat > /tmp/loom-8035-$$.txt <<EOF
\$( cp /tmp/loom-8035-src-$$ \"$WT8035_DIR/src/ok.txt\" )
EOF" "$WT8035_DIR"
# An UNBALANCED `$(` in prose yields no span at all (recorded as not-covered,
# not as safe) — it must not manufacture a deny out of ordinary text.
assert_allow_in_worktree "write-confinement (#8035): an UNBALANCED '\$(' in an UNQUOTED body's prose manufactures no write target" \
    "cat > /tmp/loom-8035-$$.md <<EOF
use \$( to open a command substitution
EOF" "$WT8035_DIR"

rm -rf "$WT8035_REPO"

echo ""

# =========================================================================

print_summary
