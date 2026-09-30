//! The post-merge worktree-cleanup remove-vs-preserve decision (#6694/#6264),
//! a slice of the `merge-pr.sh` port, #8191.
//!
//! # What it decides, and why it was tripled
//!
//! `merge-pr.sh`'s post-merge cleanup considers up to three worktrees for
//! removal after a merge: the Loom-convention path (`$DEFAULT_WT_PATH`, an
//! `issue-<N>` or external-fork `pr-<N>` worktree), a non-standard-path
//! worktree the discovery fallback finds by walking `git worktree list
//! --porcelain` for the branch (`$DISCOVERED_WT`), and a co-existing
//! Judge/Doctor review worktree at `pr-<N>` alongside an `issue-<N>` worktree
//! (`$JUDGE_PR_WT_PATH`, #6264). Each of the three call sites ran the exact
//! same remove-vs-preserve rule — #4186's close-target-aware issue gate, then
//! #6694's landed-branch override — with the only difference being which noun
//! and which path variable appeared in the messages. [`decide`] is that one
//! rule; [`Kind`] supplies only the wording difference.
//!
//! # The rule itself
//!
//! `_issue_is_closed_for_cleanup` (still shell, `merge-pr issue-close-gate`)
//! answers whether cleanup is authorized on identity/state grounds alone. When
//! it says preserve, #6694 asks a second, independent question: has the
//! branch's content already landed on the default branch regardless (via the
//! shared `branch_has_landed` primitive, #7812)? A programme issue designed to
//! accumulate `Part of #N` increments forever — every merge to it non-closing
//! by design — never satisfies the issue gate, so without this override its
//! worktree/branch would preserve indefinitely even though nothing on it is
//! unmerged. [`Context::preserve_check`] and [`Context::landed`] are exactly
//! those two already-answered facts; both forge/git reads stay in the shell,
//! and only the two-input decision plus its message text move here.
//!
//! # Fail direction: unsafe-to-preserve
//!
//! This gates `_remove_loom_worktree`'s `git worktree remove --force`, so the
//! CLI wrapper treats any fault (a missing/older daemon, an unrecognized
//! answer) as [`Action::Preserve`] — never a guessed removal. A skipped
//! cleanup is always recoverable later (`loom-clean`, the daemon's reaper, a
//! future merge); a wrongly-removed worktree is not. The same direction
//! [`super::issue_close_gate`] and [`super::dirty_guard`] already take at the
//! neighbouring choke points in this same function.

/// Which of the three call sites this decision is for.
///
/// This affects ONLY the wording — the decision rule in [`decide`] is
/// identical across all three, which is the whole point of consolidating
/// them into one function instead of three copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The `.loom/worktrees/issue-<N>` (or external-fork `pr-<N>`) convention
    /// path — `$DEFAULT_WT_PATH`. The only kind with a silent "issue is a
    /// close target (or absent)" removal — the retired shell logged nothing
    /// there, and the port keeps stdout byte-identical.
    Default,
    /// A non-standard-path worktree found by the porcelain discovery fallback
    /// (#3334) — `$DISCOVERED_WT`.
    Discovered,
    /// A co-existing Judge/Doctor review worktree at `pr-<N>`, alongside the
    /// issue-<N> worktree [`Kind::Default`] already handled (#6264) —
    /// `$JUDGE_PR_WT_PATH`.
    JudgePr,
}

impl Kind {
    /// The noun used inside the two #6694 messages (landed-remove and
    /// preserve). `Default` says plain "worktree" in both — the retired shell
    /// never called it out as "the default worktree".
    fn noun(self) -> &'static str {
        match self {
            Kind::Default => "worktree",
            Kind::Discovered => "discovered worktree",
            Kind::JudgePr => "Judge/Doctor review worktree",
        }
    }
}

/// How a replayed line is to be printed by the shell wrapper, matching
/// [`super::dirty_guard::Level`]'s naming (this family has no `Plain` line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Replay through the shell's `info`.
    Info,
    /// Replay through the shell's `warning`.
    Warning,
}

impl Level {
    /// The protocol token, as the wrapper's `case` reads it.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warning => "WARNING",
        }
    }
}

/// What the shell should do with the worktree at `Context::path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Call `_remove_loom_worktree`.
    Remove,
    /// Leave it in place.
    Preserve,
}

impl Action {
    /// The protocol token, the first line of [`render`]'s output.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Action::Remove => "REMOVE",
            Action::Preserve => "PRESERVE",
        }
    }
}

/// Everything [`decide`] needs, all of it already resolved by the caller — no
/// forge or git call happens in this module.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// Which call site this is; only affects wording (see [`Kind`]).
    pub kind: Kind,
    /// The worktree path under consideration (`$DEFAULT_WT_PATH` /
    /// `$DISCOVERED_WT` / `$JUDGE_PR_WT_PATH`).
    pub path: &'a str,
    /// The repository root, for the preserve message's manual-removal hint.
    pub repo_root: &'a str,
    /// The merged PR's number.
    pub pr_number: &'a str,
    /// The merged PR's branch.
    pub branch: &'a str,
    /// The referenced issue number, or empty when the branch did not match
    /// `feature/issue-<N>` (the external-fork/ad-hoc-branch case, #4186).
    pub issue_num: &'a str,
    /// `-n "$ISSUE_NUM" && ! _issue_is_closed_for_cleanup "$ISSUE_NUM"` —
    /// whether the #4186 issue gate says preserve, already answered by the
    /// caller. `false` covers BOTH "no issue_num at all" and "the gate says
    /// cleanup is authorized" — the two shapes the retired shell's `else`
    /// arm collapsed into one removal.
    pub preserve_check: bool,
    /// `branch_has_landed`'s verdict (#7812/#6694), consulted only when
    /// `preserve_check` is true.
    pub landed: bool,
    /// `$BRANCH_LANDED_VERDICT`, quoted in the preserve message.
    pub landed_verdict: &'a str,
    /// `$BRANCH_LANDED_EVIDENCE`, quoted in both #6694 messages.
    pub landed_evidence: &'a str,
}

/// The #4186/#6694/#6264 remove-vs-preserve decision, and the message lines
/// (if any) the shell should replay in order before acting on it.
#[must_use]
pub fn decide(ctx: &Context<'_>) -> (Action, Vec<(Level, String)>) {
    if ctx.preserve_check {
        if ctx.landed {
            return (
                Action::Remove,
                vec![(
                    Level::Info,
                    format!(
                        "Issue #{} is not a close target of PR #{} (partial-increment case, #3667), but branch '{}' has already landed ({}) — its content is already on the default branch, so the {} holds nothing unmerged; removing it (#6694)",
                        ctx.issue_num, ctx.pr_number, ctx.branch, ctx.landed_evidence, ctx.kind.noun()
                    ),
                )],
            );
        }
        return (
            Action::Preserve,
            vec![
                (
                    Level::Warning,
                    format!(
                        "Preserving {} at {} — issue #{} is not a close target of PR #{}, its live state is not CLOSED, and branch '{}' has not landed ({}/{}) — it carries content the default branch does not have",
                        ctx.kind.noun(), ctx.path, ctx.issue_num, ctx.pr_number, ctx.branch, ctx.landed_verdict, ctx.landed_evidence
                    ),
                ),
                (
                    Level::Info,
                    format!(
                        "This may be the partial-increment case (#3667) awaiting a future closing merge, or an issue-state lookup failure — cleanup retries automatically on a merge that closes #{issue}. If #{issue} is a programme issue designed never to close (#6694), that retry never fires: remove manually with 'git -C \"{repo_root}\" worktree remove \"{path}\" --force && git -C \"{repo_root}\" branch -D {branch}'",
                        issue = ctx.issue_num, repo_root = ctx.repo_root, path = ctx.path, branch = ctx.branch
                    ),
                ),
            ],
        );
    }
    match ctx.kind {
        Kind::Default => (Action::Remove, Vec::new()),
        Kind::Discovered => (
            Action::Remove,
            vec![(
                Level::Info,
                format!("Discovered Loom-managed worktree at non-standard path: {}", ctx.path),
            )],
        ),
        Kind::JudgePr => (
            Action::Remove,
            vec![(
                Level::Info,
                format!(
                    "Found co-existing Judge/Doctor review worktree at {} (PR #{}, alongside issue-{} handling above) — removing (#6264)",
                    ctx.path, ctx.pr_number, ctx.issue_num
                ),
            )],
        ),
    }
}

/// Render `(action, lines)` as the shell wrapper's protocol: the action token
/// on its own line, then one `LEVEL<TAB>message` line per record.
#[must_use]
pub fn render(action: Action, lines: &[(Level, String)]) -> String {
    let mut out = format!("{}\n", action.token());
    for (level, message) in lines {
        out.push_str(level.token());
        out.push('\t');
        out.push_str(message);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests;
