#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — force scope.
#
# One slice of the former monolithic tests/hooks/test-guard-destructive.sh,
# split per #7741. Shared fixtures, assertions and catastrophic-phrase payloads
# live in tests/hooks/lib/guard-destructive-harness.sh.
#
# Usage: ./tests/hooks/test-guard-destructive-force-scope.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

echo -e "${YELLOW}--- Force-op branch scope (guards.forceScope / LOOM_FORCE_SCOPE) (#3674) ---${NC}"
# =========================================================================
#
# guards.forceScope controls branch-aware handling of git push --force / -f /
# --force-with-lease and git reset --hard:
#   "all"       (default) — every force op asks (byte-for-byte pre-#3674).
#   "protected"           — ask only when the resolved target is a protected
#                           branch (repo default / main / master) or the branch
#                           identity is ambiguous (detached HEAD); own working
#                           branches pass through.
#   "off"                 — never ask/deny; the ALWAYS_BLOCK main/master
#                           force-push hard-denies STILL apply.
#
# Fresh `git init` repos here default to main or master (git-version-dependent);
# both are in the protected literal set, so default-branch cases work either way.
# A LOOM_DEFAULT_BRANCH seam drives the non-main/master default-branch cases
# (exercising resolve_default_branch(), not just the main/master literals).

# Configure a small git repo with forceScope config + optional branch setup.
git -c init.defaultBranch=master >/dev/null 2>&1 || true

# ---- Default state (forceScope absent → "all"): existing behaviour preserved. ----
FORCE_ALL_REPO=$(make_sql_repo '{"champion":{"auto_merge_max_lines":200}}')
assert_ask "forceScope default(all): force-push to a working branch still asks" \
    "git push --force origin feature/my-branch" "$FORCE_ALL_REPO"
assert_ask "forceScope default(all): git reset --hard still asks" \
    "git reset --hard HEAD~1" "$FORCE_ALL_REPO"
assert_ask "forceScope default(all): force-with-lease still asks" \
    "git push --force-with-lease origin feature/x" "$FORCE_ALL_REPO"

# ---- protected mode: default-branch repo (checked-out branch is main/master). ----
FORCE_PROT_DEFAULT=$(make_sql_repo '{"guards":{"forceScope":"protected"}}')
# reset --hard while on the default branch → protected → ask.
assert_ask "forceScope protected: reset --hard on default branch asks" \
    "git reset --hard HEAD~1" "$FORCE_PROT_DEFAULT"
# force-push resolving HEAD to the default branch → ask.
assert_ask "forceScope protected: force-push HEAD (resolves to default branch) asks" \
    "git push --force origin HEAD" "$FORCE_PROT_DEFAULT"
# force-push to a non-default working branch → allow.
assert_allow "forceScope protected: force-push to working branch allowed" \
    "git push --force origin feature/my-branch" "$FORCE_PROT_DEFAULT"
# force-push naming a bare ref with a leading '+' (stripped) → working branch allow.
assert_allow "forceScope protected: force-push +feature/x (plus stripped) allowed" \
    "git push -f origin +feature/x" "$FORCE_PROT_DEFAULT"
# <src>:<dst> refspec targeting a working branch → allow.
assert_allow "forceScope protected: force-push HEAD:feature/x refspec allowed" \
    "git push --force origin HEAD:feature/x" "$FORCE_PROT_DEFAULT"

# ---- protected mode with a non-main/master default branch (LOOM_DEFAULT_BRANCH). ----
# Exercises resolve_default_branch() rather than the main/master literals.
assert_ask_env "forceScope protected: force-push to configured default branch (develop) asks" \
    "LOOM_DEFAULT_BRANCH=develop" "git push --force origin develop" "$FORCE_PROT_DEFAULT"
assert_ask_env "forceScope protected: force-push HEAD:develop to default branch asks" \
    "LOOM_DEFAULT_BRANCH=develop" "git push --force origin HEAD:develop" "$FORCE_PROT_DEFAULT"
assert_ask_env "forceScope protected: force-push +develop (plus stripped) to default asks" \
    "LOOM_DEFAULT_BRANCH=develop" "git push -f origin +develop" "$FORCE_PROT_DEFAULT"
assert_allow_env "forceScope protected: force-push to feature/x when default=develop allowed" \
    "LOOM_DEFAULT_BRANCH=develop" "git push --force origin feature/x" "$FORCE_PROT_DEFAULT"

# ---- protected mode: working-branch repo (reset/push resolve to a feature branch). ----
FORCE_PROT_FEATURE=$(make_sql_repo '{"guards":{"forceScope":"protected"}}')
git -C "$FORCE_PROT_FEATURE" checkout -q -b feature/work 2>/dev/null || \
    git -C "$FORCE_PROT_FEATURE" checkout -q -b feature/work
assert_allow "forceScope protected: reset --hard on own working branch allowed" \
    "git reset --hard HEAD~1" "$FORCE_PROT_FEATURE"
assert_allow "forceScope protected: bare force-push (no refspec) on working branch allowed" \
    "git push --force" "$FORCE_PROT_FEATURE"

# ---- protected mode: detached HEAD → ambiguous → ask (never silently allow). ----
FORCE_PROT_DETACHED=$(make_sql_repo '{"guards":{"forceScope":"protected"}}')
git -C "$FORCE_PROT_DETACHED" -c user.email=t@t -c user.name=t commit -q --allow-empty -m init
git -C "$FORCE_PROT_DETACHED" checkout -q --detach
assert_ask "forceScope protected: reset --hard on detached HEAD asks (ambiguous)" \
    "git reset --hard HEAD~1" "$FORCE_PROT_DETACHED"

# ---- protected mode: git -C <other repo> resolves cwd from the -C argument. ----
# Command runs with the hook cwd = default-branch repo, but -C points at the
# feature-branch repo, so the target resolves to feature/work → allow. Without
# -C the same command would resolve the default branch and ask.
assert_allow "forceScope protected: git -C <feature-repo> reset --hard honors -C cwd" \
    "git -C $FORCE_PROT_FEATURE reset --hard HEAD~1" "$FORCE_PROT_DEFAULT"

# ---- off mode: force ops bypass entirely; main/master hard-deny still applies. ----
FORCE_OFF_REPO=$(make_sql_repo '{"guards":{"forceScope":"off"}}')
assert_allow "forceScope off: force-push to a non-protected branch bypassed" \
    "git push --force origin develop" "$FORCE_OFF_REPO"
assert_allow "forceScope off: reset --hard bypassed" \
    "git reset --hard HEAD~1" "$FORCE_OFF_REPO"
assert_deny "forceScope off: explicit force-push to main STILL hard-denied (ALWAYS_BLOCK)" \
    "git push --force origin main" "$FORCE_OFF_REPO"
assert_deny "forceScope off: explicit force-push to master STILL hard-denied (ALWAYS_BLOCK)" \
    "git push -f origin master" "$FORCE_OFF_REPO"

# ---- Env overrides config for the toggle itself. ----
# LOOM_FORCE_SCOPE=all overrides config "protected" → ask even on a working branch.
assert_ask_env "forceScope: LOOM_FORCE_SCOPE=all overrides config protected (working branch asks)" \
    "LOOM_FORCE_SCOPE=all" "git push --force origin feature/my-branch" "$FORCE_PROT_DEFAULT"
# LOOM_FORCE_SCOPE=off overrides config "protected" → allow even on default branch.
assert_allow_env "forceScope: LOOM_FORCE_SCOPE=off overrides config protected (default branch allowed)" \
    "LOOM_FORCE_SCOPE=off" "git reset --hard HEAD~1" "$FORCE_PROT_DEFAULT"
# LOOM_FORCE_SCOPE=protected overrides a config "all" for a working branch → allow.
assert_allow_env "forceScope: LOOM_FORCE_SCOPE=protected overrides config-absent all (working branch allowed)" \
    "LOOM_FORCE_SCOPE=protected" "git push --force origin feature/x" "$FORCE_PROT_FEATURE"

# ---- Malformed / out-of-range config falls through to "all" (asks). ----
FORCE_BAD_REPO=$(make_sql_repo '{ this is not valid json ')
assert_ask "forceScope malformed-config: falls through to all (force-push asks)" \
    "git push --force origin feature/x" "$FORCE_BAD_REPO"
FORCE_BOGUS_REPO=$(make_sql_repo '{"guards":{"forceScope":"bogus"}}')
assert_ask "forceScope out-of-range value: falls through to all (reset asks)" \
    "git reset --hard HEAD~1" "$FORCE_BOGUS_REPO"

# ---- forceScope must NOT weaken unrelated guards, and main/master deny holds in every mode. ----
assert_deny "forceScope protected: explicit force-push to main STILL hard-denied" \
    "git push --force origin main" "$FORCE_PROT_DEFAULT"
assert_deny_env "forceScope all(env): explicit force-with-lease to main STILL hard-denied" \
    "LOOM_FORCE_SCOPE=all" "git push --force-with-lease origin main" "$FORCE_PROT_DEFAULT"
assert_deny "forceScope protected: gh repo delete still blocked" \
    "gh repo delete myrepo --yes" "$FORCE_PROT_DEFAULT"
# A commit message merely MENTIONING --force / rm -rf is not a force op → allow.
assert_allow "forceScope protected: commit message mentioning --force is not a force op" \
    'git commit -m "document --force handling and rm -rf cleanup"' "$FORCE_PROT_DEFAULT"

# ---- protected mode: EVERY positional refspec is resolved, not just the first. ----
# Regression for the multi-refspec gap: parse_force_ops() previously inspected
# only pos[2] (the first refspec), so a protected branch in a non-first refspec
# position slipped through in protected mode. Now every refspec is emitted and
# the caller asks if ANY resolves to a protected/ambiguous target. The protected
# branch literal is assembled from a variable so this test file's own command
# text never contains a raw "push --force origin <protected>" substring that the
# session guard hook would trip on.
_PROT=main
# Protected branch as the SECOND refspec (was silently allowed pre-fix — THE gap).
assert_ask "forceScope protected: multi-refspec force-push with protected 2nd refspec asks" \
    "git push --force origin feature/x $_PROT" "$FORCE_PROT_DEFAULT"
# Protected branch as the FIRST refspec: the raw command carries the
# "push --force origin main" substring, so ALWAYS_BLOCK hard-denies it before the
# force-scope block is ever reached — kept as a control that the deny still holds.
assert_deny "forceScope protected: multi-refspec force-push with protected 1st refspec hard-denied" \
    "git push --force origin $_PROT feature/x" "$FORCE_PROT_DEFAULT"
# Protected branch in a non-first <src>:<dst> refspec is resolved to <dst> and asks.
assert_ask "forceScope protected: multi-refspec force-push with protected dst in 2nd refspec asks" \
    "git push --force origin feature/x HEAD:$_PROT" "$FORCE_PROT_DEFAULT"
# Configured non-main/master default branch in a non-first refspec → resolved → ask.
assert_ask_env "forceScope protected: multi-refspec with default branch (develop) 2nd refspec asks" \
    "LOOM_DEFAULT_BRANCH=develop" "git push --force origin feature/x develop" "$FORCE_PROT_DEFAULT"
# Multiple non-protected refspecs → every target resolves to a working branch → allow.
assert_allow "forceScope protected: multi-refspec force-push, all working branches allowed" \
    "git push --force origin feature/x feature/y" "$FORCE_PROT_DEFAULT"
# Multiple non-protected refspecs including a stripped '+' and a <src>:<dst> form → allow.
assert_allow "forceScope protected: multi-refspec force-push +feature/x and HEAD:feature/y allowed" \
    "git push -f origin +feature/x HEAD:feature/y" "$FORCE_PROT_DEFAULT"
# In "all" mode, a multi-refspec force-push still asks (unchanged behaviour).
assert_ask "forceScope default(all): multi-refspec force-push asks" \
    "git push --force origin feature/x feature/y" "$FORCE_ALL_REPO"

# ---- protected mode: `cd <worktree> &&` prefix before an "@HEAD@"-target force
# op (#5156). ----
# Regression: the hook's reported session cwd can still be the MAIN repo root
# while the COMMAND itself first `cd`s into a linked worktree and
# force-operates on that worktree's own already-checked-out branch — a
# routine, safe operation (e.g. fast-forwarding a worktree to its own
# just-pushed/rebased branch). Before the #5156 fix, "@HEAD@" branch-identity
# resolution for a hard reset / refspec-less force-push fell back to the raw
# session cwd whenever no explicit `-C` flag was present, so it queried the
# checked-out branch of the MAIN root (protected) instead of the worktree's own
# feature branch, and incorrectly asked citing the protected branch. A REAL
# linked `git worktree add` fixture is used (not a plain subdirectory) so the
# worktree genuinely has its own independent HEAD, mirroring make_wt_repo_linked
# above.
FORCE_CD_REPO=$(mktemp -d 2>/dev/null)
FORCE_CD_REPO=$(cd "$FORCE_CD_REPO" && pwd -P)
git -C "$FORCE_CD_REPO" init -q >/dev/null 2>&1
mkdir -p "$FORCE_CD_REPO/.loom"
printf '%s' '{"guards":{"forceScope":"protected"}}' > "$FORCE_CD_REPO/.loom/config.json"
# .loom/config.json must be COMMITTED (not left untracked) so it is present in
# the linked worktree's own checkout too -- force_scope_mode() resolves
# REPO_ROOT from `git rev-parse --show-toplevel` on the hook's OWN reported
# cwd, which for a cwd inside the worktree is the WORKTREE root, not this main
# root; an untracked file here would be invisible from there.
git -C "$FORCE_CD_REPO" add .loom/config.json >/dev/null 2>&1
git -C "$FORCE_CD_REPO" -c user.email=loom@test -c user.name=loom \
    commit -q -m init >/dev/null 2>&1
mkdir -p "$FORCE_CD_REPO/.loom/worktrees"
git -C "$FORCE_CD_REPO" worktree add -q "$FORCE_CD_REPO/.loom/worktrees/issue-1" \
    -b feature/issue-1 >/dev/null 2>&1
FORCE_CD_WT="$FORCE_CD_REPO/.loom/worktrees/issue-1"

# Hook cwd = MAIN repo root; command cd's into the worktree, then hard-resets
# the worktree's own already-checked-out branch -> must ALLOW (the false-ask
# this issue fixes).
assert_allow "forceScope protected (#5156): cd into worktree then reset --hard own branch allows (hook cwd=main root)" \
    "cd $FORCE_CD_WT && git reset --hard origin/feature/issue-1" "$FORCE_CD_REPO"
# Same effective operation with the hook cwd already AT the worktree -> must
# also ALLOW (this was already correct pre-fix; kept as a matching control).
assert_allow "forceScope protected (#5156): reset --hard own branch allows (hook cwd=worktree already)" \
    "git reset --hard origin/feature/issue-1" "$FORCE_CD_WT"
# A refspec-less force-push after a cd-prefix resolves "@HEAD@" the same way ->
# ALLOW.
assert_allow "forceScope protected (#5156): cd into worktree then bare force-push allows (hook cwd=main root)" \
    "cd $FORCE_CD_WT && git push --force" "$FORCE_CD_REPO"

# Control: cd-ing BACK into the main (protected-branch) root and hard-resetting
# there must still ASK -- the fix must never widen an allow past a genuine
# protected-branch target.
assert_ask "forceScope protected (#5156): cd into main root then reset --hard still asks (hook cwd=worktree)" \
    "cd $FORCE_CD_REPO && git reset --hard HEAD~1" "$FORCE_CD_WT"
# Control: cd into a directory with no real git checkout must stay ambiguous ->
# ASK, never silently allow ("never widen a deny into an allow").
assert_ask "forceScope protected (#5156): cd into an unresolvable directory still asks (ambiguous)" \
    "cd /nonexistent-dir-5156-does-not-exist && git reset --hard HEAD~1" "$FORCE_CD_REPO"
# Control: an explicit branch refspec is untouched by cd-tracking -- a
# cd-prefixed push naming a protected branch by refspec still asks (it was
# already correctly resolved from the refspec text, not cwd/HEAD).
assert_ask_env "forceScope protected (#5156): cd-prefixed push naming a protected refspec branch still asks (explicit-refspec path untouched)" \
    "LOOM_DEFAULT_BRANCH=develop" "cd $FORCE_CD_WT && git push --force origin develop" "$FORCE_CD_REPO"

# #5315: the SAME cd-tracking here now tilde/$HOME-expands its argument via
# expand_cd_arg(). With HOME set to the main repo root, `cd ~/.loom/worktrees/
# issue-1` must resolve to the worktree exactly like the literal-path control
# at the top of this block -> hard-resetting the worktree's own branch ALLOWS.
# Pre-#5315 the literal `~` was joined onto curcwd (`<main>/~/.loom/...`), a
# bogus path whose HEAD cannot resolve -> the guard would have (wrongly) asked.
assert_allow_env "forceScope protected (#5315): 'cd ~/.loom/worktrees/issue-1' (HOME=main root) then reset --hard own branch allows" \
    "HOME=$FORCE_CD_REPO" "cd ~/.loom/worktrees/issue-1 && git reset --hard origin/feature/issue-1" "$FORCE_CD_REPO"
# Control: cd back into the main (protected) root via a bare `~` must still ASK
# -- the expansion must never widen an ask into an allow.
assert_ask_env "forceScope protected (#5315): 'cd ~' (HOME=main root) then reset --hard still asks (no widening)" \
    "HOME=$FORCE_CD_REPO" "cd ~ && git reset --hard HEAD~1" "$FORCE_CD_WT"
# Control: a QUOTED tilde is not expanded -> the cd resolves to a bogus literal
# path -> ambiguous -> ASK (fail-closed), never silently allowed.
assert_ask_env "forceScope protected (#5315): 'cd '\''~/.loom/worktrees/issue-1'\''' (quoted tilde stays literal) still asks (ambiguous)" \
    "HOME=$FORCE_CD_REPO" "cd '~/.loom/worktrees/issue-1' && git reset --hard origin/feature/issue-1" "$FORCE_CD_REPO"

# #5372: parse_force_ops()'s `cd`-argument classification now reuses
# strip_cd_quoting() (#5363), mirroring extract_write_targets(). A FULLY
# quoted absolute `cd` argument ('<worktree>' / "<worktree>") starts with a
# quote character rather than `/`, so the pre-#5372 naive `~ /^\//` test
# misclassified it RELATIVE and joined it onto curcwd (`<main-root>/'<wt>'`,
# a nonexistent path) instead of recognizing it as absolute -- headcpath
# resolved to an unresolvable directory and the guard fell back to ASK
# (fail-closed, never a bypass -- this feeds the ask-gate, not
# write-confinement). Post-fix it correctly resolves to the worktree's own
# checked-out branch -> ALLOW.
for _q5372 in "'" '"'; do
    assert_allow "forceScope protected (#5372): cd ${_q5372}-quoted worktree path && reset --hard own branch allows (hook cwd=main root)" \
        "cd ${_q5372}$FORCE_CD_WT${_q5372} && git reset --hard origin/feature/issue-1" "$FORCE_CD_REPO"
done
unset _q5372

# PARTIALLY quoted absolute `cd` argument -- the quote closes MID-TOKEN
# (e.g. '<parent>'/issue-1) -- is also now classified ABSOLUTE (mirrors the
# extract_write_targets() partial-quote fixture, #5363 probe A).
assert_allow "forceScope protected (#5372): cd PARTIALLY-quoted worktree path && reset --hard own branch allows (hook cwd=main root)" \
    "cd '$FORCE_CD_REPO/.loom/worktrees'/issue-1 && git reset --hard origin/feature/issue-1" "$FORCE_CD_REPO"

# Control: an unbalanced/unterminated quote keeps today's verdict (ASK) --
# strip_cd_quoting()'s fallback contract never widens ambiguity into an
# allow.
assert_ask "forceScope protected (#5372): unbalanced leading single-quote in cd argument keeps today's ask" \
    "cd '$FORCE_CD_WT && git reset --hard origin/feature/issue-1" "$FORCE_CD_REPO"

# Control: cd-ing (quoted) BACK into the main (protected-branch) root and
# hard-resetting there must still ASK -- the fix must never widen an allow
# past a genuine protected-branch target.
assert_ask "forceScope protected (#5372): cd quoted main root then reset --hard still asks (hook cwd=worktree)" \
    "cd '$FORCE_CD_REPO' && git reset --hard HEAD~1" "$FORCE_CD_WT"

rm -rf "$FORCE_CD_REPO"

# ---- force-op:detached (#5772): a known-safe reset RECOVERY target in a ----
# ---- Loom-managed worktree must not stall on the transient detached-HEAD ----
# ---- state alone.                                                       ----
#
# Guard-decision telemetry (#3898) showed force-op:detached firing at ASK
# tier -- no human to answer in a headless run -- for the SAME shape every
# time: an operator/role resetting a worktree it already owns back to
# origin/main or plain HEAD via `git -C "$WT" reset --hard ...` while that
# worktree happened to be in a detached-HEAD state at the time. `reset
# --hard` never switches branches, so a detached worktree has no branch ref
# to protect in the first place -- the RESET TARGET itself (parsed via
# parse_force_ops()'s third field, see its header comment) is what actually
# matters, and "origin/main"/"origin/master"/"origin/<default>"/"HEAD" name
# nothing protected. The exemption is deliberately narrow: it requires BOTH
# a recognized recovery-target literal AND a cwd that resolves inside a
# Loom-managed worktree (`.loom-managed` sentinel) -- never the main
# checkout, never an unrecognized target, never a push (which mutates a
# remote, a materially different risk this exemption does not touch).
FORCE_DETACHED_WT_REPO=$(mktemp -d 2>/dev/null)
FORCE_DETACHED_WT_REPO=$(cd "$FORCE_DETACHED_WT_REPO" && pwd -P)
git -C "$FORCE_DETACHED_WT_REPO" init -q >/dev/null 2>&1
mkdir -p "$FORCE_DETACHED_WT_REPO/.loom"
printf '%s' '{"guards":{"forceScope":"protected"}}' > "$FORCE_DETACHED_WT_REPO/.loom/config.json"
git -C "$FORCE_DETACHED_WT_REPO" add .loom/config.json >/dev/null 2>&1
git -C "$FORCE_DETACHED_WT_REPO" -c user.email=loom@test -c user.name=loom \
    commit -q -m init >/dev/null 2>&1
mkdir -p "$FORCE_DETACHED_WT_REPO/.loom/worktrees"
git -C "$FORCE_DETACHED_WT_REPO" worktree add -q "$FORCE_DETACHED_WT_REPO/.loom/worktrees/issue-2" \
    -b feature/issue-2 >/dev/null 2>&1
FORCE_DETACHED_WT="$FORCE_DETACHED_WT_REPO/.loom/worktrees/issue-2"
# Mirrors write_loom_sentinel() in defaults/scripts/worktree.sh exactly (#7530
# reads the `# Branch: ` line as the worktree's authoritative own-branch
# record) -- a bare `: >` sentinel (pre-#7530) had no such line.
cat > "$FORCE_DETACHED_WT/.loom-managed" <<'EOF'
# Loom-managed worktree marker
# Created by .loom/scripts/worktree.sh
# Issue: 2
# Branch: feature/issue-2
# Removing this file makes Loom treat the worktree as user-owned and refuse
# to clean it up automatically.
EOF
git -C "$FORCE_DETACHED_WT" checkout -q --detach >/dev/null 2>&1

assert_allow "force-op:detached (#5772): git -C <managed worktree, detached HEAD> reset --hard origin/main allows" \
    "git -C $FORCE_DETACHED_WT reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"
assert_allow "force-op:detached (#5772): git -C <managed worktree, detached HEAD> reset --hard origin/master allows" \
    "git -C $FORCE_DETACHED_WT reset --hard origin/master" "$FORCE_DETACHED_WT_REPO"
assert_allow "force-op:detached (#5772): git -C <managed worktree, detached HEAD> reset --hard HEAD allows (explicit HEAD)" \
    "git -C $FORCE_DETACHED_WT reset --hard HEAD" "$FORCE_DETACHED_WT_REPO"
assert_allow "force-op:detached (#5772): bare 'git reset --hard' (no target, hook cwd=managed worktree, detached HEAD) allows (defaults to HEAD)" \
    "git reset --hard" "$FORCE_DETACHED_WT"
assert_allow "force-op:detached (#5772): cd into managed worktree (detached HEAD) then reset --hard origin/main allows (cd form, hook cwd=main root)" \
    "cd $FORCE_DETACHED_WT && git reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"
assert_allow_env "force-op:detached (#5772): reset --hard origin/<configured default branch> allows (resolve_default_branch path)" \
    "LOOM_DEFAULT_BRANCH=develop" "git -C $FORCE_DETACHED_WT reset --hard origin/develop" "$FORCE_DETACHED_WT_REPO"

# Control: an unrecognized reset target on a detached managed worktree still
# asks -- the exemption is narrow, never a blanket "detached is fine".
assert_ask "force-op:detached (#5772): git -C <managed worktree, detached HEAD> reset --hard to an unrecognized target still asks" \
    "git -C $FORCE_DETACHED_WT reset --hard some-other-branch" "$FORCE_DETACHED_WT_REPO"
# Control: a bare local-branch-shaped literal ("main", not "origin/main") is
# NOT in the recognized recovery-literal set -- still asks.
assert_ask "force-op:detached (#5772): git -C <managed worktree, detached HEAD> reset --hard to bare 'main' (not origin/main) still asks" \
    "git -C $FORCE_DETACHED_WT reset --hard main" "$FORCE_DETACHED_WT_REPO"
# Control: the same recognized target (origin/main) with a detached HEAD but
# OUTSIDE any Loom-managed worktree still asks -- reuses FORCE_PROT_DETACHED
# (detached HEAD, no `.loom-managed` sentinel anywhere in its path).
assert_ask "force-op:detached (#5772): reset --hard origin/main on a detached HEAD OUTSIDE a managed worktree still asks (no sentinel)" \
    "git reset --hard origin/main" "$FORCE_PROT_DETACHED"
# Control: the exemption is reset-only -- a bare force-PUSH on the very same
# detached, managed worktree still asks (mutating a remote is a materially
# different risk this exemption does not touch).
assert_ask "force-op:detached (#5772): bare force-push (not reset) on detached HEAD in a managed worktree still asks (exemption is reset-only)" \
    "git push --force" "$FORCE_DETACHED_WT"

# ---- #6152: SAME-COMMAND $VAR resolution at the -C/cd cwd-capture points ----
#
# #5775 (immediately above) added the managed-worktree detached-HEAD reset-
# recovery allowlist, but only worked when the `-C`/`cd` argument was a
# LITERAL path. Guard-decision telemetry (#3898) kept showing force-op:detached
# firing at ASK for the Guide role's own `docs-guide-lock.sh release` path,
# which threads its cwd through a shell variable assigned on a preceding line:
#
#   DOCS_WT="/path/to/.loom/worktrees/docs-guide"
#   git -C "$DOCS_WT" reset --hard HEAD
#
# parse_force_ops() captured `cpath`/`cdarg` as the literal unexpanded "$VAR"
# token (no call to resolve_var()), so `_in_any_managed_worktree` downstream
# always got an empty/non-absolute cwd and could never recognize the target
# as safe -- the exact #5775 allowlist decided this shape was fine, but the
# guard could never SEE that. Reuses FORCE_DETACHED_WT / FORCE_DETACHED_WT_REPO
# (still detached HEAD, `.loom-managed` sentinel present) from the block above.
assert_allow "force-op:detached + \$VAR resolution (#6152): DOCS_WT assigned then git -C \"\$DOCS_WT\" reset --hard HEAD allows" \
    "DOCS_WT=\"$FORCE_DETACHED_WT\"
git -C \"\$DOCS_WT\" reset --hard HEAD" "$FORCE_DETACHED_WT_REPO"
assert_allow "force-op:detached + \$VAR resolution (#6152): DOCS_WT assigned then git -C \"\$DOCS_WT\" reset --hard origin/main allows" \
    "DOCS_WT=\"$FORCE_DETACHED_WT\"
git -C \"\$DOCS_WT\" reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"
assert_allow "force-op:detached + \$VAR resolution (#6152): braced \${DOCS_WT} form in -C also resolves and allows" \
    "DOCS_WT=\"$FORCE_DETACHED_WT\"
git -C \"\${DOCS_WT}\" reset --hard HEAD" "$FORCE_DETACHED_WT_REPO"
assert_allow "force-op:detached + \$VAR resolution (#6152): cd \"\$DOCS_WT\" && git reset --hard origin/main allows (cd-prefix form, hook cwd=main root)" \
    "DOCS_WT=\"$FORCE_DETACHED_WT\"
cd \"\$DOCS_WT\" && git reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"

# Control: an UNRESOLVABLE variable (no matching same-command assignment at
# all) must NOT be guessed -- stays exactly the pre-#6152 literal-unexpanded-
# token treatment, so it keeps asking (fail-toward-asking unchanged).
assert_ask "force-op:detached + \$VAR resolution (#6152): unresolvable \$VAR in -C (no matching assignment) still asks" \
    "git -C \"\$NOSUCHVARFORLOOMTEST6152\" reset --hard HEAD" "$FORCE_DETACHED_WT_REPO"
assert_ask "force-op:detached + \$VAR resolution (#6152): unresolvable \$VAR in cd prefix (no matching assignment) still asks" \
    "cd \"\$NOSUCHVARFORLOOMTEST6152\" && git reset --hard HEAD" "$FORCE_DETACHED_WT_REPO"
# Control: a $VAR assigned from ANOTHER unresolved $VAR (chained -- this
# single-pass resolver deliberately does not follow chains, mirrors the
# #4881 write-confinement chained-$VAR fixture) also stays fail-closed.
assert_ask "force-op:detached + \$VAR resolution (#6152): \$VAR assigned from an unresolved \$VAR (chained) stays fail-closed, still asks" \
    "DOCS_WT=\"\$SOMETHINGUNKNOWN6152\"
git -C \"\$DOCS_WT\" reset --hard HEAD" "$FORCE_DETACHED_WT_REPO"
# Control: the resolved value must still respect the existing recovery-target
# allowlist -- an unrecognized reset TARGET via a resolved $VAR cwd still
# asks (the exemption narrows the cwd-resolution gap, not the target check).
assert_ask "force-op:detached + \$VAR resolution (#6152): DOCS_WT resolves but reset target is unrecognized -- still asks" \
    "DOCS_WT=\"$FORCE_DETACHED_WT\"
git -C \"\$DOCS_WT\" reset --hard some-other-branch" "$FORCE_DETACHED_WT_REPO"

# ---- #6724: NAME=$(pwd) capture of a proven same-command `cd`, at the -C/cd ----
# ---- cwd-capture points.                                                    ----
#
# #6152 (immediately above) resolves a same-command `$VAR` at the -C/cd
# capture points, but only when record_assign() captured a LITERAL string.
# Guard-decision telemetry (#3898) showed force-op:detached firing at ASK for
# the exact shape below -- a worktree path resolved via `cd <path>` followed
# by capturing the NEW cwd into a variable with `WORKTREE_ABS="$(pwd)"`,
# rather than a static string in the command text -- because record_assign()
# stores the substitution TEXT "$(pwd)" verbatim and resolve_var()'s
# chain-refusal guard (it starts with "$") correctly refuses to touch it.
# Reuses FORCE_DETACHED_WT / FORCE_DETACHED_WT_REPO (still detached HEAD,
# `.loom-managed` sentinel present) from the block above.
assert_allow "force-op:detached + \$(pwd) capture (#6724): cd <worktree> then WORKTREE_ABS=\"\$(pwd)\" then git -C \"\$WORKTREE_ABS\" reset --hard allows" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS=\"\$(pwd)\"
git -C \"\$WORKTREE_ABS\" reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"
assert_allow "force-op:detached + \$(pwd) capture (#6724): braced \${WORKTREE_ABS} form also resolves and allows" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS=\"\$(pwd)\"
git -C \"\${WORKTREE_ABS}\" reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"
assert_allow "force-op:detached + \$(pwd) capture (#6724): backtick-substitution spelling WORKTREE_ABS=\`pwd\` also resolves and allows" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS=\"\`pwd\`\"
git -C \"\$WORKTREE_ABS\" reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"
assert_allow "force-op:detached + \$(pwd) capture (#6724): unquoted WORKTREE_ABS=\$(pwd) also resolves and allows" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS=\$(pwd)
git -C \"\$WORKTREE_ABS\" reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"

# Control: a $(pwd) capture with NO preceding same-command `cd` (curcwd is
# only the hook's own default invocation cwd, never proven) must NOT be
# guessed -- stays fail-closed, still asks. The command's own hook cwd here
# is the detached worktree itself, so a naive "trust curcwd unconditionally"
# implementation would incorrectly allow this.
assert_ask "force-op:detached + \$(pwd) capture (#6724): \$(pwd) capture with NO preceding cd stays fail-closed, still asks" \
    "WORKTREE_ABS=\"\$(pwd)\"
git -C \"\$WORKTREE_ABS\" reset --hard origin/main" "$FORCE_DETACHED_WT"

# Control: this fix is scoped to the literal `pwd` substitution only -- any
# other command substitution stays unresolved and still asks.
assert_ask "force-op:detached + \$(pwd) capture (#6724): a non-pwd command substitution stays unresolved, still asks" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS=\"\$(git rev-parse --show-toplevel)\"
git -C \"\$WORKTREE_ABS\" reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"

# Control: a SINGLE-QUOTED '$(pwd)' is a literal string the shell never
# evaluates (not a cwd capture) -- must stay unresolved, still asks.
assert_ask "force-op:detached + \$(pwd) capture (#6724): single-quoted literal '\$(pwd)' is NOT a cwd capture, still asks" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS='\$(pwd)'
git -C \"\$WORKTREE_ABS\" reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"

# Control: the resolved value must still respect the existing recovery-target
# allowlist -- an unrecognized reset TARGET via a resolved $(pwd)-captured cwd
# still asks (the exemption narrows the cwd-resolution gap, not the target
# check).
assert_ask "force-op:detached + \$(pwd) capture (#6724): WORKTREE_ABS resolves but reset target is unrecognized -- still asks" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS=\"\$(pwd)\"
git -C \"\$WORKTREE_ABS\" reset --hard some-other-branch" "$FORCE_DETACHED_WT_REPO"

# ---- #7532: NAME=$(cat <file>) capture of a proven same-command `cd`, at ----
# ---- the -C/cd cwd-capture points.                                      ----
#
# #6724 (immediately above) resolves a same-command `NAME=$(pwd)` capture,
# but guard-decision telemetry (#7419, dated AFTER #6724 merged) showed
# force-op:detached still firing at ASK for a DIFFERENT capture shape -- the
# cwd is round-tripped through a FILE instead of a direct `$(pwd)`
# substitution:
#   cd <worktree>
#   WORKTREE_ABS=$(cat /tmp/worktree_abs_7419.txt)
#   git -C "$WORKTREE_ABS" reset --hard origin/main
# record_assign() stores the substitution TEXT "$(cat <file>)" verbatim, and
# resolve_var()'s chain-refusal guard (it starts with "$") correctly refuses
# to touch it. The guard is a PreToolUse hook -- it evaluates the WHOLE
# command BEFORE any of it runs -- so these tests populate <file> on disk
# with real content BEFORE invoking the guard (mirroring "a file already
# sitting on disk with the cwd, written moments earlier or by any other
# legitimate means"), rather than embedding the write inside the guarded
# command text itself, which the guard never executes. Reuses
# FORCE_DETACHED_WT / FORCE_DETACHED_WT_REPO (still detached HEAD,
# `.loom-managed` sentinel present) from the block above.
FORCE_7532_CATFILE="$(mktemp -u 2>/dev/null)"
FORCE_7532_OTHERFILE="$(mktemp -u 2>/dev/null)"
printf '%s' "$FORCE_DETACHED_WT" > "$FORCE_7532_CATFILE"
printf '%s' "/some/other/path/not-the-worktree" > "$FORCE_7532_OTHERFILE"

assert_allow "force-op:detached + \$(cat <file>) capture (#7532): cd <worktree> then WORKTREE_ABS=\$(cat <file>) then git -C \"\$WORKTREE_ABS\" reset --hard allows" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS=\$(cat $FORCE_7532_CATFILE)
git -C \"\$WORKTREE_ABS\" reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"
assert_allow "force-op:detached + \$(cat <file>) capture (#7532): double-quoted whole substitution WORKTREE_ABS=\"\$(cat <file>)\" also resolves and allows" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS=\"\$(cat $FORCE_7532_CATFILE)\"
git -C \"\$WORKTREE_ABS\" reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"
assert_allow "force-op:detached + \$(cat <file>) capture (#7532): backtick-substitution spelling WORKTREE_ABS=\`cat <file>\` also resolves and allows" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS=\"\`cat $FORCE_7532_CATFILE\`\"
git -C \"\$WORKTREE_ABS\" reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"
assert_allow "force-op:detached + \$(cat <file>) capture (#7532): braced \${WORKTREE_ABS} form in -C also resolves and allows" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS=\$(cat $FORCE_7532_CATFILE)
git -C \"\${WORKTREE_ABS}\" reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"
assert_allow "force-op:detached + \$(cat <file>) capture (#7532): cd \"\$WORKTREE_ABS\" && git reset --hard origin/main allows (cd-prefix form, hook cwd=main root)" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS=\$(cat $FORCE_7532_CATFILE)
cd \"\$WORKTREE_ABS\" && git reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"

# Control: a $(cat <file>) capture whose file contents do NOT match the
# same-command `cd` target stays fail-closed -- still asks (AC2).
assert_ask "force-op:detached + \$(cat <file>) capture (#7532): \$(cat <file>) whose contents do NOT match the same-command cd target stays fail-closed, still asks" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS=\$(cat $FORCE_7532_OTHERFILE)
git -C \"\$WORKTREE_ABS\" reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"

# Control: a $(cat <file>) capture whose file does not exist at all stays
# fail-closed -- still asks (unreadable file, same fail-toward-asking default).
assert_ask "force-op:detached + \$(cat <file>) capture (#7532): \$(cat <file>) naming a file that does not exist stays fail-closed, still asks" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS=\$(cat /tmp/loom-test-7532-does-not-exist-$$.txt)
git -C \"\$WORKTREE_ABS\" reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"

# Control: a $(cat <file>) capture with NO preceding same-command `cd` (curcwd
# is only the hook's own default invocation cwd, never proven) must NOT be
# guessed -- stays fail-closed, still asks, mirroring the #6724 $(pwd) control.
assert_ask "force-op:detached + \$(cat <file>) capture (#7532): \$(cat <file>) with NO preceding cd stays fail-closed, still asks" \
    "WORKTREE_ABS=\$(cat $FORCE_7532_CATFILE)
git -C \"\$WORKTREE_ABS\" reset --hard origin/main" "$FORCE_DETACHED_WT"

# Control: this fix is scoped to the literal `cat <file>` substitution only --
# any other command reading the file (e.g. `head -1`) stays unresolved and
# still asks.
assert_ask "force-op:detached + \$(cat <file>) capture (#7532): a non-cat command substitution reading the same file stays unresolved, still asks" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS=\$(head -1 $FORCE_7532_CATFILE)
git -C \"\$WORKTREE_ABS\" reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"

# Control: a SINGLE-QUOTED '$(cat <file>)' is a literal string the shell never
# evaluates (not a capture) -- must stay unresolved, still asks.
assert_ask "force-op:detached + \$(cat <file>) capture (#7532): single-quoted literal '\$(cat <file>)' is NOT a capture, still asks" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS='\$(cat $FORCE_7532_CATFILE)'
git -C \"\$WORKTREE_ABS\" reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"

# Control: a relative (non-absolute) <file> argument is left unresolved --
# still asks, mirroring the narrow "only absolute paths are read" scope.
assert_ask "force-op:detached + \$(cat <file>) capture (#7532): a relative <file> argument stays unresolved, still asks" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS=\$(cat relative-file.txt)
git -C \"\$WORKTREE_ABS\" reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"

# Control: the resolved value must still respect the existing recovery-target
# allowlist -- an unrecognized reset TARGET via a resolved $(cat)-captured cwd
# still asks (the exemption narrows the cwd-resolution gap, not the target
# check).
assert_ask "force-op:detached + \$(cat <file>) capture (#7532): WORKTREE_ABS resolves but reset target is unrecognized -- still asks" \
    "cd $FORCE_DETACHED_WT
WORKTREE_ABS=\$(cat $FORCE_7532_CATFILE)
git -C \"\$WORKTREE_ABS\" reset --hard some-other-branch" "$FORCE_DETACHED_WT_REPO"

rm -f "$FORCE_7532_CATFILE" "$FORCE_7532_OTHERFILE"
# ---- #7530: extend the #5772 safe-list to the worktree's OWN tracked ----
# ---- branch, read from the `.loom-managed` sentinel's `# Branch:` line. ----
#
# Guard-decision telemetry (#3898) showed force-op:detached firing at ASK for
# a builder/doctor resyncing its OWN worktree to its OWN `origin/feature/
# issue-N` after an upstream force-push/rebase -- a shape #5772's literal
# HEAD/origin/main/origin/master/origin/<default> allow-list did not cover.
# FORCE_DETACHED_WT's sentinel (above) now records `# Branch: feature/issue-2`,
# matching worktree.sh's write_loom_sentinel() format exactly.
assert_allow "force-op:detached (#7530): git -C <managed worktree, detached HEAD> reset --hard origin/<own tracked branch> allows" \
    "git -C $FORCE_DETACHED_WT reset --hard origin/feature/issue-2" "$FORCE_DETACHED_WT_REPO"
assert_allow "force-op:detached (#7530): cd into managed worktree (detached HEAD) then reset --hard origin/<own tracked branch> allows (cd form)" \
    "cd $FORCE_DETACHED_WT && git reset --hard origin/feature/issue-2" "$FORCE_DETACHED_WT_REPO"

# Control: the exemption is scoped to THIS worktree's own branch -- resetting
# to a DIFFERENT issue's branch must still ask, even though it has the exact
# same `origin/feature/issue-*` shape. The exemption must never widen to "any
# origin/feature/issue-* target", only the worktree's own (per the issue's
# explicit acceptance criterion).
assert_ask "force-op:detached (#7530): git -C <managed worktree, detached HEAD> reset --hard origin/<ANOTHER issue's branch> still asks" \
    "git -C $FORCE_DETACHED_WT reset --hard origin/feature/issue-9999" "$FORCE_DETACHED_WT_REPO"

# Control: the exemption is reset-only -- a bare force-push (ambiguous target,
# no explicit ref -- parse_force_ops() never populates a RESET-TARGET for a
# push line, so the new own-branch check is unreachable here) on the same
# detached, managed worktree still asks (mutating a remote is a materially
# different risk this exemption does not touch; mirrors the existing #5772
# "exemption is reset-only" control above, re-asserted here as a #7530
# regression guard on the -C form specifically).
assert_ask "force-op:detached (#7530): git -C <managed worktree own branch, detached HEAD> bare force-push still asks (exemption is reset-only)" \
    "git -C $FORCE_DETACHED_WT push --force" "$FORCE_DETACHED_WT_REPO"

# Control: the same own-branch reset target with a detached HEAD but OUTSIDE
# any Loom-managed worktree (no `.loom-managed` sentinel) still asks -- the
# exemption stays sentinel-gated, reusing FORCE_PROT_DETACHED.
assert_ask "force-op:detached (#7530): reset --hard origin/feature/issue-2 on a detached HEAD OUTSIDE a managed worktree still asks (no sentinel)" \
    "git reset --hard origin/feature/issue-2" "$FORCE_PROT_DETACHED"

# Control: origin/main and origin/<default> behavior is unchanged by this
# extension -- re-assert alongside the new own-branch cases so a future
# regression that narrows or removes the #5772 literals is caught here too.
assert_allow "force-op:detached (#7530 regression guard): origin/main safe-list entry from #5772 is unchanged" \
    "git -C $FORCE_DETACHED_WT reset --hard origin/main" "$FORCE_DETACHED_WT_REPO"
assert_allow_env "force-op:detached (#7530 regression guard): origin/<configured default branch> safe-list entry from #5772 is unchanged" \
    "LOOM_DEFAULT_BRANCH=develop" "git -C $FORCE_DETACHED_WT reset --hard origin/develop" "$FORCE_DETACHED_WT_REPO"

rm -rf "$FORCE_DETACHED_WT_REPO"

# ---- #6077: guard-decision telemetry audit — reproduce the EXACT real-world ----
# ---- command shapes cited as suspected false positives, against CURRENT    ----
# ---- code.                                                                 ----
#
# Investigation finding: every cited sample already resolves correctly on
# current `defaults/hooks/guard-destructive-generic.sh` — the underlying bug
# was real, but was already fixed by #5156 (cd-prefix tracking), #5315 (tilde/
# $HOME expansion), #5372 (quoted cd-argument classification), and #5772 (the
# detached-HEAD reset-recovery exemption), all merged 2026-08-04 or earlier.
# The `.loom/logs/guard-decisions.log` samples #6077 cites are dated
# 2026-08-02 through 2026-08-03 — BEFORE the #5156 fix landed (2026-08-04) —
# so the telemetry was already stale by the time this issue was filed. No
# production code change is made here; these fixtures lock in the
# already-correct behavior against regressions and cover shapes (trailing
# pipes/chained commands, a raw-SHA reset target, a `pr-N`-style detached
# worktree) not previously exercised verbatim.
FORCE_6077_REPO=$(mktemp -d 2>/dev/null)
FORCE_6077_REPO=$(cd "$FORCE_6077_REPO" && pwd -P)
git -C "$FORCE_6077_REPO" init -q >/dev/null 2>&1
mkdir -p "$FORCE_6077_REPO/.loom"
printf '%s' '{"guards":{"forceScope":"protected"}}' > "$FORCE_6077_REPO/.loom/config.json"
git -C "$FORCE_6077_REPO" add .loom/config.json >/dev/null 2>&1
git -C "$FORCE_6077_REPO" -c user.email=loom@test -c user.name=loom \
    commit -q -m init >/dev/null 2>&1
git -C "$FORCE_6077_REPO" -c user.email=loom@test -c user.name=loom \
    commit -q --allow-empty -m second >/dev/null 2>&1
FORCE_6077_SHA=$(git -C "$FORCE_6077_REPO" rev-parse HEAD)
mkdir -p "$FORCE_6077_REPO/.loom/worktrees"
git -C "$FORCE_6077_REPO" worktree add -q "$FORCE_6077_REPO/.loom/worktrees/issue-3950" \
    -b feature/issue-3950 >/dev/null 2>&1
git -C "$FORCE_6077_REPO" worktree add -q "$FORCE_6077_REPO/.loom/worktrees/issue-4028" \
    -b feature/issue-4028 >/dev/null 2>&1
git -C "$FORCE_6077_REPO" worktree add -q "$FORCE_6077_REPO/.loom/worktrees/pr-5042" \
    -b pr-5042-review >/dev/null 2>&1
git -C "$FORCE_6077_REPO/.loom/worktrees/pr-5042" checkout -q --detach >/dev/null 2>&1

# (1) worktree-scoped `git reset --hard <own-remote-branch>`, piped/chained
# trailing commands (mirrors the issue's `issue-5110` sample) -> allow.
assert_allow "#6077: cd into worktree then reset --hard own remote branch, piped to tail, allows" \
    "cd $FORCE_6077_REPO/.loom/worktrees/issue-3950 && git reset --hard origin/feature/issue-3950 2>&1 | tail -2" \
    "$FORCE_6077_REPO"
assert_allow "#6077: cd into worktree then reset --hard own remote branch with chained trailing commands allows" \
    "cd $FORCE_6077_REPO/.loom/worktrees/issue-3950 && git reset --hard origin/feature/issue-3950 && git log --oneline -2 && git status --short" \
    "$FORCE_6077_REPO"

# (2) worktree-scoped `git push --force-with-lease` (no refspec), piped/
# chained (mirrors the issue's `issue-3950`/`issue-4031` samples) -> allow.
assert_allow "#6077: cd into worktree then bare force-with-lease piped to tail allows" \
    "cd $FORCE_6077_REPO/.loom/worktrees/issue-3950 && git push --force-with-lease 2>&1 | tail -20" \
    "$FORCE_6077_REPO"
assert_allow "#6077: cd into worktree then bare force-with-lease with trailing stderr redirect allows" \
    "cd $FORCE_6077_REPO/.loom/worktrees/issue-3950 && git push --force-with-lease 2>&1" \
    "$FORCE_6077_REPO"

# A raw-SHA reset target (not an origin/<branch> refspec) on a worktree's own
# checked-out (non-detached) branch resolves via the CHECKED-OUT branch
# identity, not the reset-target literal -- allows regardless of target shape
# (mirrors the issue's `issue-4028` sample: `git reset --hard 26dfb265`).
assert_allow "#6077: cd into worktree then reset --hard to a raw SHA on own (non-detached) branch allows" \
    "cd $FORCE_6077_REPO/.loom/worktrees/issue-4028 && git reset --hard $FORCE_6077_SHA" \
    "$FORCE_6077_REPO"

# (3) A `pr-N`-style REVIEW worktree that is genuinely on a detached HEAD, force
# op targets a raw SHA (not a recognized origin/main|master|<default>|HEAD
# recovery literal), with chained trailing commands (mirrors the issue's
# `pr-5042` anomaly) -- must take the force-op:DETACHED path, never
# force-op:protected.
assert_ask_reason_matches "#6077: cd into a detached-HEAD pr-N worktree then reset --hard <raw sha> asks via force-op:detached, not force-op:protected" \
    "cd $FORCE_6077_REPO/.loom/worktrees/pr-5042 && git reset --hard $FORCE_6077_SHA --quiet && git status --short && git log --oneline -1" \
    "detached or unresolved branch" \
    "$FORCE_6077_REPO"

# Control: a force op that genuinely targets main/master/default -- even from
# a `cd`-prefixed worktree path -- must still ask force-op:protected. Never
# widen the fix into a bypass for a real protected-branch target.
assert_ask_reason_matches "#6077: cd back into the main (protected) root and reset --hard still asks via force-op:protected (no widening)" \
    "cd $FORCE_6077_REPO && git reset --hard HEAD~1" \
    "targets protected branch" \
    "$FORCE_6077_REPO/.loom/worktrees/issue-3950"

rm -rf "$FORCE_6077_REPO"

# Clean up force-scope temp repos.
for _force_dir in "$FORCE_ALL_REPO" "$FORCE_PROT_DEFAULT" "$FORCE_PROT_FEATURE" \
    "$FORCE_PROT_DETACHED" "$FORCE_OFF_REPO" "$FORCE_BAD_REPO" "$FORCE_BOGUS_REPO"; do
    [[ -n "$_force_dir" && "$_force_dir" != "/" && -d "$_force_dir/.loom" ]] && rm -rf "$_force_dir"
done

echo ""

# =========================================================================

print_summary
