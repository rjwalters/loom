//! `loom-daemon champion-hold classify | merge-failed-marker` (#11932): the
//! I/O half of [`crate::champion_hold::classify`], called by
//! `champion-pr-merge.md`'s Verdict-State Janitor, its merge step, and its
//! "Merge Failed" notice. Reads go through REST (`gh api`), live (uncached):
//! both answers gate a merge or a post.
//!
//! | verb | stdout | exit |
//! |---|---|---|
//! | `classify --pr N` | `LOOM-CHAMPION-HOLD state=<s> pr=<N> detail=<d>` | 0 |
//! | `classify`, a hold with `loom:operator`, events unreadable | `state=champion-hold` (no repair this pass) | 0 |
//! | `classify`, labels or (blocked PR) comments unreadable | (nothing) | 3 |
//! | `classify`, a clear PR's base unreadable | (nothing) | 3 |
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

use crate::champion_hold::classify::{
    classify_operator, fetch_operator_adder, merge_failed_notice_due, needs_comments, stacked_base,
    stray_operator_candidate, with_stacked_base, PrClass,
};
use anyhow::Result;
use serde_json::Value;

#[derive(clap::Subcommand)]
pub(crate) enum ChampionHoldCommand {
    /// Classify a PR for Champion (#11932): `clear` (run the criteria),
    /// `champion-hold` (Champion's own hold: leave it), `foreign-park`
    /// (`loom:blocked` from another actor: skip, no merge, no comment), or
    /// `contradiction` (`loom:pr` beside a contradicting verdict label: the
    /// janitor strips `loom:pr`), or `stray-operator` (a hold the fleet App
    /// labelled `loom:operator`, #11970: the janitor removes `loom:operator`),
    /// or `stacked-on-loom-branch` (based on a Loom stack branch, whether or
    /// not an open PR still heads it, #12761: skip, no merge, no comment).
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

    /// Decide the ratification hold's tick for a PR (#12992): reads the head
    /// contents of every spec/decision file the PR changes and the trusted
    /// comments, and prints `LOOM-CHAMPION-RATIFICATION verdict=<clear|hold|
    /// unreadable> action=<none|hold|stands|release|clear|defer> pr=<N>
    /// head=<sha> files=<a,b|->`. Exit 3 with no stdout when a read failed.
    Ratification(TargetArgs),
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
        let comments = crate::comment_trust::records::fetch_trusted_comments(
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
        let out = crate::script_helpers::run_gh(&["api", &path], root, false);
        let doc = serde_json::from_slice::<Value>(&out.ok_output()?.stdout).ok()?;
        crate::verdict_gate::label_names(&doc)
    }

    /// A live REST read of `path`, decoded. `None` when unreadable.
    fn read_json(path: &str, root: &std::path::Path) -> Option<Value> {
        let out = crate::script_helpers::run_gh(&["api", path], root, false);
        serde_json::from_slice::<Value>(&out.ok_output()?.stdout).ok()
    }

    /// This PR's base when it is a Loom stack branch (#12761), else `None`.
    /// One read of the PR for `base.ref`; the parent is not looked up (an
    /// open, merged or closed parent all mean "do not merge here"). `Err`
    /// when the base could not be read.
    fn loom_stack_base(&self, root: &std::path::Path) -> Result<Option<String>, &'static str> {
        let pull = Self::read_json(&format!("repos/{}/pulls/{}", self.repo(), self.pr), root)
            .ok_or("base")?;
        stacked_base(&pull).ok_or("base")
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
                let class = classify_operator(&labels, &trusted, adder);
                // Only a clear PR can merge, so only it pays for the base read.
                let class = if class == PrClass::Clear {
                    let base = t
                        .loom_stack_base(&root)
                        .unwrap_or_else(|what| unreadable("classify", t.pr, what));
                    with_stacked_base(class, base.as_deref())
                } else {
                    class
                };
                println!("{}", class.line(t.pr));
                Ok(())
            }
            ChampionHoldCommand::Ratification(t) => {
                println!("{}", ratification_line(&t, &root));
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

/// The ratification verb's stdout line; exits 3 on any failed read.
fn ratification_line(t: &TargetArgs, root: &std::path::Path) -> String {
    use crate::champion_hold::ratification::{decide, scan, ChangedFile, Comment, Config, Verdict};
    let repo = t.repo();
    let fail = |what: &str| -> ! { unreadable("ratification", t.pr, what) };
    let pull = TargetArgs::read_json(&format!("repos/{repo}/pulls/{}", t.pr), root)
        .unwrap_or_else(|| fail("head"));
    let head = pull
        .pointer("/head/sha")
        .and_then(Value::as_str)
        .unwrap_or_else(|| fail("head"))
        .to_string();
    let listing = crate::script_helpers::run_gh(
        &[
            "api",
            &format!("repos/{repo}/pulls/{}/files", t.pr),
            "--paginate",
            "--jq",
            ".[]",
        ],
        root,
        false,
    );
    let listing = listing.ok_output().unwrap_or_else(|| fail("file list"));
    let cfg = Config::from_effective(&crate::config_resolver::resolve_effective_config(root));
    let mut files = Vec::new();
    for line in String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter(|l| !l.is_empty())
    {
        let v: Value = serde_json::from_str(line).unwrap_or_else(|_| fail("file list"));
        let name = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let mut f = ChangedFile {
            filename: name("filename").unwrap_or_else(|| fail("file list")),
            previous_filename: name("previous_filename"),
            status: name("status").unwrap_or_default(),
            head_content: None,
        };
        // Contents are read only for files the path rules select.
        if f.status != "removed" && ratification_candidate(&f, &cfg) {
            let path = format!("repos/{repo}/contents/{}?ref={head}", f.filename);
            let out = crate::script_helpers::run_gh(
                &["api", "-H", "Accept: application/vnd.github.raw", &path],
                root,
                false,
            );
            f.head_content = out
                .ok_output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        }
        files.push(f);
    }
    let comments: Vec<Comment> =
        crate::comment_trust::records::fetch_trusted_comments(repo, &t.pr.to_string(), root, false)
            .unwrap_or_else(|| fail("trusted comments"))
            .iter()
            .filter_map(Comment::from_record)
            .collect();
    let verdict = scan(&files, &cfg);
    let action = decide(&verdict, &comments, &head);
    let (name, hit) = match &verdict {
        Verdict::Clear => ("clear", Vec::new()),
        Verdict::Hold(h) => ("hold", h.clone()),
        Verdict::Unreadable(u) => ("unreadable", u.clone()),
    };
    let listed = if hit.is_empty() {
        "-".to_string()
    } else {
        hit.join(",")
    };
    format!(
        "LOOM-CHAMPION-RATIFICATION verdict={name} action={} pr={} head={head} files={listed}",
        action.as_str(),
        t.pr
    )
}

fn ratification_candidate(
    f: &crate::champion_hold::ratification::ChangedFile,
    cfg: &crate::champion_hold::ratification::Config,
) -> bool {
    crate::champion_hold::ratification::is_candidate(f, cfg)
}
