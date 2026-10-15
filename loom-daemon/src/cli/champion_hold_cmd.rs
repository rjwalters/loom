//! `loom-daemon champion-hold classify | merge-failed-marker` (#11932): the
//! I/O half of [`loom_daemon::champion_hold::classify`], called by
//! `champion-pr-merge.md`'s Verdict-State Janitor, its merge step, and its
//! "Merge Failed" notice. Reads go through REST (`gh api`), live (uncached):
//! both answers gate a merge or a post.
//!
//! | verb | stdout | exit |
//! |---|---|---|
//! | `classify --pr N` | `LOOM-CHAMPION-HOLD state=<s> pr=<N> detail=<d>` | 0 |
//! | `classify`, a hold with `loom:operator`, events unreadable | `state=champion-hold` (no repair this pass) | 0 |
//! | `classify`, labels or (blocked PR) comments unreadable | (nothing) | 3 |
//! | `merge-failed-marker --pr N --head SHA`, notice due | the marker line | 0 |
//! | `merge-failed-marker`, notice already posted for this head | (nothing) | 1 |
//! | `merge-failed-marker`, comments unreadable or no head | (nothing) | 3 |
//!
//! Every failure leaves stdout empty, so a caller matching on the line can
//! never read a failed read as `state=clear` or as "post the notice".
//!
//! A failed read is exit **3**, not 2, because 2 is clap's usage error: what a
//! `loom-daemon` older than this verb answers. Prompts resync independently of
//! the binary, so Champion tells the two apart. On an old binary it keeps its
//! pre-#11932 behaviour (`merge-pr.sh`'s label guard still refuses the merge);
//! on a failed read it skips the PR.

use anyhow::Result;
use loom_daemon::champion_hold::classify::{
    classify_operator, fetch_operator_adder, merge_failed_notice_due, needs_comments,
    stray_operator_candidate,
};
use serde_json::Value;

#[derive(clap::Subcommand)]
pub(crate) enum ChampionHoldCommand {
    /// Classify a PR for Champion (#11932): `clear` (run the criteria),
    /// `champion-hold` (Champion's own hold: leave it), `foreign-park`
    /// (`loom:blocked` from another actor: skip, no merge, no comment), or
    /// `contradiction` (`loom:pr` beside a contradicting verdict label: the
    /// janitor strips `loom:pr`), or `stray-operator` (a hold the fleet App
    /// labelled `loom:operator`, #11970: the janitor removes `loom:operator`).
    /// Exit 3 with no stdout when a read failed.
    Classify(TargetArgs),

    /// Print the marker to post a "Merge Failed" notice under, when no trusted
    /// notice for `--head` stands yet (exit 0); exit 1 when one does, so the
    /// notice posts at most once per head (#11932); exit 3 when unreadable.
    MergeFailedMarker {
        #[command(flatten)]
        target: TargetArgs,

        /// The head SHA the merge failed at.
        #[arg(long, value_name = "SHA")]
        head: String,
    },
}

#[derive(clap::Args)]
pub(crate) struct TargetArgs {
    /// The pull request number.
    #[arg(long, value_name = "N")]
    pr: u64,

    /// Repository, `owner/repo`. Defaults to the current checkout's.
    #[arg(long, value_name = "NWO")]
    repo: Option<String>,
}

impl TargetArgs {
    fn repo(&self) -> &str {
        self.repo.as_deref().unwrap_or("{owner}/{repo}")
    }

    /// The PR's trusted comment bodies, oldest first. `None` when unreadable.
    fn trusted_bodies(&self, root: &std::path::Path) -> Option<Vec<String>> {
        let comments = loom_daemon::comment_trust::records::fetch_trusted_comments(
            self.repo(),
            &self.pr.to_string(),
            root,
            false,
        )?;
        Some(
            comments
                .iter()
                .filter_map(|c| c.get("body").and_then(Value::as_str).map(str::to_string))
                .collect(),
        )
    }

    /// The PR's label names, live. `None` when unreadable.
    fn labels(&self, root: &std::path::Path) -> Option<Vec<String>> {
        let path = format!("repos/{}/issues/{}/labels?per_page=100", self.repo(), self.pr);
        let out = loom_daemon::script_helpers::run_gh(&["api", &path], root, false);
        let doc = serde_json::from_slice::<Value>(&out.ok_output()?.stdout).ok()?;
        loom_daemon::verdict_gate::label_names(&doc)
    }
}

fn unreadable(verb: &str, pr: u64, what: &str) -> ! {
    eprintln!("champion-hold {verb}: PR #{pr}'s {what} could not be read");
    std::process::exit(3)
}

impl ChampionHoldCommand {
    pub(crate) fn run(self) -> Result<()> {
        let root = super::forge_identity_cmd::workspace();
        match self {
            ChampionHoldCommand::Classify(t) => {
                let labels = t
                    .labels(&root)
                    .unwrap_or_else(|| unreadable("classify", t.pr, "labels"));
                let trusted = if needs_comments(&labels) {
                    t.trusted_bodies(&root)
                        .unwrap_or_else(|| unreadable("classify", t.pr, "trusted comments"))
                } else {
                    Vec::new()
                };
                let adder = stray_operator_candidate(&labels, &trusted)
                    .then(|| fetch_operator_adder(t.repo(), t.pr, &root))
                    .flatten();
                println!("{}", classify_operator(&labels, &trusted, adder).line(t.pr));
                Ok(())
            }
            ChampionHoldCommand::MergeFailedMarker { target, head } => {
                let trusted = target
                    .trusted_bodies(&root)
                    .unwrap_or_else(|| unreadable("merge-failed-marker", target.pr, "comments"));
                let head = head.trim();
                if head.is_empty() || !head.bytes().all(|b| b.is_ascii_hexdigit()) {
                    eprintln!("champion-hold merge-failed-marker: --head {head:?} is not a SHA");
                    std::process::exit(3)
                }
                match merge_failed_notice_due(&trusted, head) {
                    Some(marker) => {
                        println!("{marker}");
                        Ok(())
                    }
                    None => std::process::exit(1),
                }
            }
        }
    }
}
