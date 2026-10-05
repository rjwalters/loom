//! `loom-daemon merge-pr stacked-children` (#3747 item 2 / #7982, a slice of
//! #8191): `merge-pr.sh`'s `_check_no_open_stacked_children`.
//!
//! # Exit codes
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | no stackable branch / no open child | nothing | 0 |
//! | bypassed, dry-run report, or pin written | records (see below) | 0 |
//! | tip could not be pinned | `CHILDREN`, then `BLOCK<TAB>…` lines | 1 |
//!
//! # The record protocol
//!
//! One `LEVEL<TAB>line` per line, the shape [`super::merge_pr_delete_branch`]
//! established and [`super::merge_pr_version_policy`] extended to multi-line
//! messages. `merge-pr.sh` routes operator text through its own
//! `warning`/`error`, so this subcommand never prints color or prefixes itself.
//!
//! | record | meaning for the stub |
//! |---|---|
//! | `CHILDREN<TAB><compact json>` | assign `STACKED_CHILDREN_JSON` |
//! | `PIN-WRITTEN` | assign `STACKED_CHILDREN_PIN_WRITTEN=true` |
//! | `WARNING<TAB>line` | collect, then one `warning` call |
//! | `BLOCK<TAB>line` | collect, then `error` (exit 1) |
//!
//! `WARNING` lines are **re-joined** by the stub into a single `warning` call
//! rather than replayed one per line (which is what `version-policy` does).
//! Both of this guard's warnings end in a colon followed by an indented command
//! block, and the retired shell emitted them as one `warning "$msg"$'\n'"$cmds"`
//! — so joining keeps the operator-visible bytes identical, whereas per-line
//! replay would interleave a color reset into every command line. At most one
//! warning is ever produced per invocation, so there is nothing to disambiguate.
//!
//! `CHILDREN` is emitted whenever an open child was found, **including on the
//! `--allow-stacked-children` path** — the retired shell set
//! `STACKED_CHILDREN_JSON` before testing the bypass, because the post-merge
//! reconcile wants the pre-merge snapshot regardless of whether this guard chose
//! to pin (a post-merge re-query can return zero rows once
//! `delete_branch_on_merge` retargets the children, #8010 item 2).
//! `PIN-WRITTEN` is emitted only where a ref was actually written, which is the
//! distinction the #8010 follow-up turned on.
//!
//! # Why this verb does not fail closed
//!
//! Argued in full in [`loom_daemon::merge_pr::stacked_children`]'s module docs:
//! the guard establishes a postcondition for a best-effort cleanup step, not a
//! verdict about whether this tree may merge, and the fail-closed
//! `verdict-contradiction` gate runs on the same binary later in the same
//! script. The stub therefore treats any exit other than 0/1 as "did not run",
//! loudly, and never as a block.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::merge_pr::stacked_children::{
    blocked_message, children_json, decide, discover_open_children, invalid_ref_message,
    parse_children, pinned_message, try_establish_pin, Child, Inputs, Outcome, PinRefusal,
};

#[derive(clap::Args)]
pub(crate) struct StackedChildrenArgs {
    /// The repository as owner/repo (`$REPO_NWO`), for the live child query.
    #[arg(long, value_name = "OWNER/REPO", default_value = "")]
    repo: String,

    /// The checkout whose refs the pin is written into (`$REPO_ROOT`).
    #[arg(long, value_name = "PATH")]
    repo_root: PathBuf,

    /// The parent PR's head branch (`$PR_BRANCH`). Anything other than
    /// `feature/issue-<N>` or `feature/harness-ops-<N>` skips the guard.
    #[arg(long, value_name = "BRANCH", default_value = "")]
    branch: String,

    /// The parent PR's head SHA (`$PR_HEAD_SHA`) — the tip to pin.
    #[arg(long, value_name = "SHA", default_value = "")]
    head_sha: String,

    /// The parent PR number, for the operator-facing text.
    #[arg(long, value_name = "N", default_value = "")]
    pr: String,

    /// The operator asserts the children are already reconciled: warn and
    /// proceed without pinning (`--allow-stacked-children`).
    #[arg(long)]
    allow_stacked_children: bool,

    /// Report the would-be outcome and write no ref.
    #[arg(long)]
    dry_run: bool,

    /// Read the `gh pr list --json number,headRefName` array from stdin instead
    /// of querying the forge — the deterministic seam for suites and for
    /// replaying a real repo's children offline.
    #[arg(long)]
    from_stdin: bool,
}

/// Render one message as protocol lines, splitting on `\n` exactly so a
/// trailing empty line survives.
fn render(level: &str, msg: &str) -> String {
    let mut s = String::new();
    for line in msg.split('\n') {
        s.push_str(level);
        s.push('\t');
        s.push_str(line);
        s.push('\n');
    }
    s
}

impl StackedChildrenArgs {
    pub(crate) fn run(self) -> Result<()> {
        let children = self.children();
        let inputs = Inputs {
            pr_number: &self.pr,
            branch: &self.branch,
            head_sha: &self.head_sha,
            children: &children,
            allow_stacked_children: self.allow_stacked_children,
            dry_run: self.dry_run,
        };

        let outcome = decide(&inputs);
        if outcome == Outcome::Skip {
            // No record at all: the stub must be able to tell "found nothing"
            // from "found children", because `STACKED_CHILDREN_JSON` staying
            // unset is itself an assertion the retained suite makes.
            return Ok(());
        }

        // Every non-Skip outcome found at least one open child, so the snapshot
        // is reported before the branch that decides what to do about them.
        println!("CHILDREN\t{}", children_json(&children));

        match outcome {
            Outcome::Skip => unreachable!("handled above"),
            Outcome::Bypass(msg) | Outcome::DryRun(msg) => {
                print!("{}", render("WARNING", &msg));
                Ok(())
            }
            Outcome::NeedsPin => {
                match try_establish_pin(&self.repo_root, &self.branch, &self.head_sha) {
                    Ok(()) => {
                        println!("PIN-WRITTEN");
                        print!("{}", render("WARNING", &pinned_message(&inputs)));
                        Ok(())
                    }
                    // #9106/#9479. Both arms BLOCK and exit 1 — the behaviour
                    // the boolean `establish_pin` already had — but an unsafe
                    // ref operand gets its own text: `blocked_message` would
                    // send the operator to reconcile the children or reach for
                    // --allow-stacked-children, and neither can fix a name git
                    // would re-parse as a switch.
                    Err(PinRefusal::InvalidRef(e)) => {
                        print!("{}", render("BLOCK", &invalid_ref_message(&self.pr, &e)));
                        std::process::exit(1);
                    }
                    Err(_) => {
                        print!("{}", render("BLOCK", &blocked_message(&inputs)));
                        std::process::exit(1);
                    }
                }
            }
        }
    }

    /// The open children targeting `--branch`, from stdin or a live query.
    ///
    /// The branch-shape gate is applied first so a non-issue parent branch costs
    /// no forge call at all, matching the retired shell's ordering.
    fn children(&self) -> Vec<Child> {
        if !loom_daemon::merge_pr::stacked_children::is_stackable_parent_branch(&self.branch) {
            return Vec::new();
        }
        let raw = if self.from_stdin {
            let mut buf = String::new();
            use std::io::Read;
            let _ = std::io::stdin().read_to_string(&mut buf);
            buf
        } else {
            let gh = std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string());
            discover_open_children(&gh, &self.repo, &self.branch)
        };
        parse_children(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn every_line_carries_its_level_including_blank_ones() {
        assert_eq!(render("BLOCK", "a\n\nb"), "BLOCK\ta\nBLOCK\t\nBLOCK\tb\n");
    }

    #[test]
    fn an_indented_command_block_keeps_its_indentation() {
        assert_eq!(
            render("WARNING", "landed:\n  ./x.sh 1 b"),
            "WARNING\tlanded:\nWARNING\t  ./x.sh 1 b\n"
        );
    }
}
