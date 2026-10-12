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

echo -e "${YELLOW}--- Direct file-write heredocs vs. the catastrophic scan (#9870) ---${NC}"
# =========================================================================
# #9870 — `cat > file <<'EOF' ... EOF` whose body only MENTIONS a
# catastrophic-tier phrase
# =========================================================================
#
# mask_catastrophic_filesink_heredocs() narrows the UNGATED denial floor, so
# this block is mostly negative: the pass is a whole-command sink allowlist,
# and every shape outside it must keep denying at the catastrophic tier (the
# deny helper below pins the REASON, so a deny arriving from some other tier
# cannot make a regression here look green).
#
# NOTHING IN THIS BLOCK EXECUTES. Every command is a STRING handed to the guard
# as JSON. The phrases live in single-quoted variables, and the `$(` / backtick
# openers are spelled through DS/BT so this file contains no live substitution
# of them.
P9870_REPO='gh repo delete o/r'
P9870_RM='rm -rf /'
P9870_PUSH='git push --force origin main'
NL=$'\n'
TAB=$'\t'
DS='$('
BT='`'
# hd9870 <opener line> <body> <terminator line> -> one multi-line command string.
hd9870() { printf '%s\n%s\n%s' "$1" "$2" "$3"; }
deny9870() { assert_deny_reason_matches "$1" "$2" 'dangerous pattern'; }

# --- ALLOW: the intended shape, each phrase, `>` and `>>` -------------------
for p9870 in "$P9870_REPO" "$P9870_RM" "$P9870_PUSH"; do
    assert_allow "catastrophic (#9870): cat > file <<'EOF' whose body mentions '$p9870' allows" \
        "$(hd9870 "cat > notes/x.md <<'EOF'" "never run $p9870" "EOF")"
    assert_allow "catastrophic (#9870): cat >> file <<'EOF' whose body mentions '$p9870' allows" \
        "$(hd9870 "cat >> notes/x.md <<'EOF'" "never run $p9870" "EOF")"
done
assert_allow "catastrophic (#9870): a double-quoted <<\"EOF\" delimiter allows" \
    "$(hd9870 'cat > notes/x.md <<"EOF"' "never run $P9870_REPO" "EOF")"
assert_allow "catastrophic (#9870): heredoc-before-redirect order (cat <<'EOF' > file) allows" \
    "$(hd9870 "cat <<'EOF' > notes/x.md" "never run $P9870_RM" "EOF")"
assert_allow "catastrophic (#9870): <<-'EOF' with a tab-indented terminator allows" \
    "$(hd9870 "cat > notes/x.md <<-'EOF'" "${TAB}never run $P9870_PUSH" "${TAB}EOF")"
assert_allow "catastrophic (#9870): no blanks around the operators (cat >f<<'EOF') allows" \
    "$(hd9870 "cat >notes/x.md<<'EOF'" "never run $P9870_REPO" "EOF")"
assert_allow "catastrophic (#9870): a quoted body is inert even when it spells \$( ) around the phrase" \
    "$(hd9870 "cat > notes/x.md <<'EOF'" "example: ${DS} $P9870_REPO ) and ${BT}$P9870_RM${BT}" "EOF")"
assert_allow "catastrophic (#9870): two qualifying blocks separated by a blank line both allow" \
    "$(hd9870 "cat > a.md <<'EOF'" "never run $P9870_REPO" "EOF")${NL}${NL}$(hd9870 "cat >> b.md <<'B2'" "never run $P9870_RM" "B2")"
assert_allow "catastrophic (#9870): leading and trailing blank lines around the block allow" \
    "${NL}  ${NL}$(hd9870 "  cat > notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF")${NL}"

# --- DENY: delimiter not provably quoted, or block not closed ---------------
deny9870 "catastrophic (#9870): an UNQUOTED delimiter still denies" \
    "$(hd9870 "cat > notes/x.md <<EOF" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): an UNQUOTED delimiter with a live \$( ) body still denies" \
    "$(hd9870 "cat > notes/x.md <<EOF" "${DS} $P9870_REPO )" "EOF")"
deny9870 "catastrophic (#9870): an UNQUOTED delimiter with a live backtick body still denies" \
    "$(hd9870 "cat > notes/x.md <<EOF" "${BT} $P9870_REPO ${BT}" "EOF")"
deny9870 "catastrophic (#9870): a backslash-escaped <<\\EOF delimiter still denies (out of scope, fail closed)" \
    "$(hd9870 "cat > notes/x.md <<\\EOF" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): a half-quoted delimiter (<<'EOF) still denies" \
    "$(hd9870 "cat > notes/x.md <<'EOF" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): an UNCLOSED heredoc still denies" \
    "cat > notes/x.md <<'EOF'${NL}never run $P9870_REPO"
deny9870 "catastrophic (#9870): a terminator with trailing blanks does not close the block" \
    "$(hd9870 "cat > notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF ")"
deny9870 "catastrophic (#9870): a tab-indented terminator does not close a plain << block" \
    "$(hd9870 "cat > notes/x.md <<'EOF'" "never run $P9870_REPO" "${TAB}EOF")"
deny9870 "catastrophic (#9870): a herestring (<<<) is not a heredoc" \
    "cat > notes/x.md <<<'never run $P9870_REPO'"
deny9870 "catastrophic (#9870): a body that ends EARLY leaves the following line live" \
    "$(hd9870 "cat > notes/x.md <<'EOF'" "harmless" "EOF")${NL}$P9870_REPO${NL}EOF"

# --- DENY: opener line is not exactly one bare `cat` + one redirect ---------
deny9870 "catastrophic (#9870): opener piped onward (| bash) still denies" \
    "$(hd9870 "cat > notes/x.md <<'EOF' | bash" "$P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): opener followed by ';' still denies" \
    "$(hd9870 "cat > notes/x.md <<'EOF'; true" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): opener chained with && still denies" \
    "$(hd9870 "true && cat > notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): opener inside \$( ) still denies" \
    "$(hd9870 "x=${DS}cat > notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF")${NL})"
deny9870 "catastrophic (#9870): opener inside backticks still denies" \
    "$(hd9870 "x=${BT}cat > notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF")${NL}${BT}"
deny9870 "catastrophic (#9870): opener inside a ( subshell ) still denies" \
    "$(hd9870 "(cat > notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF")${NL}) | bash"
deny9870 "catastrophic (#9870): opener inside a { group; } still denies" \
    "$(hd9870 "{ cat > notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF")${NL}} | bash"
deny9870 "catastrophic (#9870): opener backgrounded with & still denies" \
    "$(hd9870 "cat > notes/x.md <<'EOF' &" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): opener with a line continuation still denies" \
    "$(hd9870 "cat > notes/x.md <<'EOF' \\" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): cat with an extra file argument still denies" \
    "$(hd9870 "cat other.txt > notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): cat with a '-' argument still denies" \
    "$(hd9870 "cat - > notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): cat with a flag still denies" \
    "$(hd9870 "cat -n > notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): a second stdout redirect still denies" \
    "$(hd9870 "cat > notes/x.md > notes/y.md <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): a second (stderr) redirect still denies" \
    "$(hd9870 "cat > notes/x.md 2> notes/err.txt <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): an fd-duplicating redirect (>&2) still denies" \
    "$(hd9870 "cat >&2 <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): a process-substitution target (> >(bash)) still denies" \
    "$(hd9870 "cat > >(bash) <<'EOF'" "$P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): no redirect at all (body goes to stdout) still denies" \
    "$(hd9870 "cat <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): an assignment prefix on the opener still denies" \
    "$(hd9870 "X=1 cat > notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): sudo cat still denies" \
    "$(hd9870 "sudo cat > notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): a path-qualified /bin/cat still denies" \
    "$(hd9870 "/bin/cat > notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): tee (copies the body to stdout) still denies" \
    "$(hd9870 "tee notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): a catastrophic command ON the opener line still denies" \
    "$(hd9870 "cat > notes/x.md <<'EOF'; $P9870_REPO" "harmless" "EOF")"

# --- DENY: target is not a plain literal file -------------------------------
# shellcheck disable=SC2016,SC2088  # the unexpanded $F / ~ spellings ARE the test data
for t9870 in /dev/stdout /dev/fd/1 /proc/self/fd/1 /DEV/stdout ../../dev/stdout notes/../x.md \
             '"$F"' '$F' '${F}' "x${DS}date).md" "x${BT}date${BT}.md" '~/x.md' 'x*.md' '{a,b}.md' \
             '"notes/x.md"' "'notes/x.md'" '=x'; do
    deny9870 "catastrophic (#9870): target $t9870 still denies" \
        "$(hd9870 "cat > $t9870 <<'EOF'" "never run $P9870_REPO" "EOF")"
done

# --- DENY: the target token is literal, but the path on disk is no inert file
# #9870 review: `cat > P` does not create P -- it OPENS whatever P already is.
# A pre-staged FIFO (or a symlink resolving to one) makes the heredoc body the
# reader's INPUT, so the masker must prove the sink before hiding anything
# from the floor. Fixtures live in a throwaway dir; absolute paths, so the
# assertions do not depend on the fixture repo's contents. The deny helper
# pins the catastrophic 'dangerous pattern' reason, i.e. the masking was
# skipped and the FLOOR denied -- not some other tier.
S9870_DIR=$(mktemp -d 2>/dev/null || mktemp -d -t loom-9870-sink)
mkfifo "$S9870_DIR/pipe"
mkdir "$S9870_DIR/realdir"
ln -s "$S9870_DIR/pipe" "$S9870_DIR/link-to-pipe"
ln -s "$S9870_DIR/gone" "$S9870_DIR/dangling"
ln -s "$S9870_DIR/realdir" "$S9870_DIR/link-to-dir"
printf 'plain\n' > "$S9870_DIR/plain.md"
deny9870 "catastrophic (#9870 review): a target that is an EXISTING FIFO still denies (the body would feed its reader)" \
    "$(hd9870 "cat > $S9870_DIR/pipe <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870 review): a target that is a SYMLINK TO A FIFO still denies" \
    "$(hd9870 "cat > $S9870_DIR/link-to-pipe <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870 review): a target that is a DANGLING SYMLINK still denies (fail closed)" \
    "$(hd9870 "cat > $S9870_DIR/dangling <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870 review): a target that is a SYMLINK TO A DIRECTORY still denies" \
    "$(hd9870 "cat > $S9870_DIR/link-to-dir <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870 review): a target that is an EXISTING DIRECTORY still denies" \
    "$(hd9870 "cat > $S9870_DIR/realdir <<'EOF'" "never run $P9870_REPO" "EOF")"
assert_allow "catastrophic (#9870 review): a target that is an EXISTING REGULAR FILE still allows" \
    "$(hd9870 "cat > $S9870_DIR/plain.md <<'EOF'" "never run $P9870_REPO" "EOF")"
assert_allow "catastrophic (#9870 review): a target that does not exist yet still allows (fresh regular file)" \
    "$(hd9870 "cat > $S9870_DIR/new.md <<'EOF'" "never run $P9870_REPO" "EOF")"

# Relative targets resolve against the SESSION cwd from the hook input, not
# the guard process's own cwd: the same proof must fire through it.
mkfifo "$TEST_REPO/pwn9870.fifo"
printf 'note\n' > "$TEST_REPO/notes9870.md"
deny9870 "catastrophic (#9870 review): a RELATIVE target that is a FIFO in the session cwd still denies" \
    "$(hd9870 "cat > pwn9870.fifo <<'EOF'" "never run $P9870_REPO" "EOF")"
assert_allow "catastrophic (#9870 review): a RELATIVE target that is a regular file in the session cwd still allows" \
    "$(hd9870 "cat > notes9870.md <<'EOF'" "never run $P9870_REPO" "EOF")"
rm -rf "$S9870_DIR" "$TEST_REPO/pwn9870.fifo" "$TEST_REPO/notes9870.md"


# --- DENY: anything else in the command (write-then-execute and friends) ----
W9870=$(hd9870 "cat > x.sh <<'EOF'" "$P9870_REPO" "EOF")
for run9870 in 'bash x.sh' 'sh ./x.sh' '. x.sh' 'source x.sh' 'chmod +x x.sh && ./x.sh' \
               'bash ./*.sh' 'for f in *; do bash "$f"; done' 'true'; do
    deny9870 "catastrophic (#9870): write-then-'$run9870' in the same command still denies" \
        "$W9870${NL}$run9870"
done
deny9870 "catastrophic (#9870): a catastrophic command AFTER the closing delimiter still denies" \
    "$(hd9870 "cat > notes/x.md <<'EOF'" "harmless" "EOF")${NL}$P9870_REPO"
deny9870 "catastrophic (#9870): a catastrophic command BEFORE the opener still denies" \
    "$P9870_PUSH${NL}$(hd9870 "cat > notes/x.md <<'EOF'" "harmless" "EOF")"
deny9870 "catastrophic (#9870): any command line before the opener leaves the body visible" \
    "cd /tmp${NL}$(hd9870 "cat > notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): a comment line before the opener leaves the body visible" \
    "# write a note${NL}$(hd9870 "cat > notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF")"
# The two same-command bypasses a per-block rule would have: an opener that is
# really INSIDE a quoted string (no heredoc exists; the "body" runs after the
# closing quote), and `cat` rebound to an interpreter.
deny9870 "catastrophic (#9870): an opener inside an open double-quoted string still denies" \
    "echo \"${NL}$(hd9870 "cat > notes/x.md <<'EOF'" "\"; $P9870_REPO; : \"" "EOF")${NL}\""
deny9870 "catastrophic (#9870): cat rebound to a shell function first still denies" \
    "cat() { bash; }${NL}$(hd9870 "cat > notes/x.md <<'EOF'" "$P9870_REPO" "EOF")"

# --- DENY: two heredocs where only one qualifies (no partial masking) -------
deny9870 "catastrophic (#9870): qualifying block then bash <<'E2' carrying the phrase still denies" \
    "$(hd9870 "cat > notes/x.md <<'EOF'" "harmless" "EOF")${NL}$(hd9870 "bash <<'E2'" "$P9870_REPO" "E2")"
deny9870 "catastrophic (#9870): bash <<'E2' then a qualifying block carrying the phrase still denies" \
    "$(hd9870 "bash <<'E2'" "true" "E2")${NL}$(hd9870 "cat > notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF")"
deny9870 "catastrophic (#9870): qualifying block carrying the phrase then a non-qualifying one still denies" \
    "$(hd9870 "cat > notes/x.md <<'EOF'" "never run $P9870_REPO" "EOF")${NL}$(hd9870 "cat > notes/y.md <<E2" "harmless" "E2")"

# --- DENY: a heredoc fed to anything that is not a file-writing cat ---------
for sink9870 in 'ssh host' 'bash' 'sh -s' 'zsh' 'python3' 'python' 'psql db' 'sudo -s' 'xargs -I{} sh -c {}' 'eval'; do
    deny9870 "catastrophic (#9870): '$sink9870 <<EOF' (quoted) carrying the phrase still denies" \
        "$(hd9870 "$sink9870 <<'EOF'" "$P9870_RM" "EOF")"
done
deny9870 "catastrophic (#9870): gh issue comment --body-file - <<'EOF' still denies (out of scope)" \
    "$(hd9870 "gh issue comment 1 --body-file - <<'EOF'" "never run $P9870_REPO" "EOF")"

echo ""

# =========================================================================

print_summary
