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

    /// Read-only calibration of merge-risk self-releases (#12952): lists PRs
    /// carrying a trusted `champion:merge-risk-self-release` record that merged
    /// at least `--min-age-days` ago with whether the merge commit was reverted
    /// within 7 days, then the hit rate per recorded-confidence bucket. Exit 3
    /// (no partial rows) when any read failed.
    SelfReleaseCalibration {
        /// Only PRs merged at least this many days ago are reported.
        #[arg(long, default_value_t = 7, value_name = "DAYS")]
        min_age_days: i64,

        /// How many of the most recently merged PRs to scan.
        #[arg(long, default_value_t = 200, value_name = "N")]
        limit: u32,

        /// Repository, `owner/repo`. Defaults to the current checkout's.
        #[arg(long, value_name = "NWO")]
        repo: Option<String>,
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
            ChampionHoldCommand::SelfReleaseCalibration {
                min_age_days,
                limit,
                repo,
            } => {
                // `gh pr list` does not expand `{owner}/{repo}` (only `gh api`
                // does), so the real `owner/repo` is resolved once here.
                let repo = resolve_repo(repo, || {
                    crate::forge_cmd::repo_nwo_in(&crate::forge_cmd::gh_bin(), Some(&root))
                })
                .unwrap_or_else(|| {
                    eprintln!(
                        "champion-hold self-release-calibration: could not resolve the current repository (pass --repo owner/repo)"
                    );
                    std::process::exit(3)
                });
                match calibration_report(&repo, min_age_days, limit, &root) {
                    Ok(report) => {
                        print!("{report}");
                        Ok(())
                    }
                    Err(what) => {
                        eprintln!("champion-hold self-release-calibration: {what}");
                        std::process::exit(3)
                    }
                }
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

/// The explicit `--repo`, else the resolved current checkout's `owner/repo`.
/// Never the `{owner}/{repo}` placeholder: that only works in `gh api` paths.
fn resolve_repo(
    explicit: Option<String>,
    resolve: impl FnOnce() -> Option<String>,
) -> Option<String> {
    explicit
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .or_else(resolve)
        .filter(|r| r.contains('/') && !r.contains('{'))
}

/// The calibration report, or the first failed read (no partial rows).
fn calibration_report(
    repo: &str,
    min_age_days: i64,
    limit: u32,
    root: &std::path::Path,
) -> Result<String, String> {
    use crate::champion_hold::calibration::{
        is_mature, recorded_confidence, summarize, Row, REVERT_WINDOW_DAYS,
    };
    let now = chrono::Utc::now();
    let list = crate::script_helpers::run_gh(
        &[
            "pr",
            "list",
            "--repo",
            repo,
            "--state",
            "merged",
            "--limit",
            &limit.to_string(),
            "--json",
            "number,mergedAt,mergeCommit",
        ],
        root,
        false,
    );
    let list = list.ok_output().ok_or("merged PR list could not be read")?;
    let prs: Vec<Value> =
        serde_json::from_slice(&list.stdout).map_err(|_| "merged PR list is not JSON")?;
    let mut rows = Vec::new();
    let mut immature = 0usize;
    for p in &prs {
        let pr = p
            .get("number")
            .and_then(Value::as_u64)
            .ok_or("PR without a number")?;
        let merged_at = p
            .get("mergedAt")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok())
            .ok_or_else(|| format!("PR #{pr}: merge time unreadable"))?;
        let trusted = crate::comment_trust::records::fetch_trusted_comments(
            repo,
            &pr.to_string(),
            root,
            false,
        )
        .ok_or_else(|| format!("PR #{pr}: comments unreadable"))?;
        let bodies: Vec<String> = trusted
            .iter()
            .filter_map(|c| c.get("body").and_then(Value::as_str).map(str::to_string))
            .collect();
        let Some(confidence) =
            recorded_confidence(&bodies).map_err(|e| format!("PR #{pr}: {e}"))?
        else {
            continue;
        };
        if !is_mature(merged_at, now, min_age_days) {
            immature += 1;
            continue;
        }
        let sha = p
            .pointer("/mergeCommit/oid")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("PR #{pr}: merge commit unreadable"))?;
        let window_end = merged_at + chrono::Duration::days(REVERT_WINDOW_DAYS);
        let reverted = revert_in_window(repo, pr, sha, window_end, root)?
            || reverts_by_pr(repo, pr, sha, window_end, root)?;
        rows.push(Row {
            pr,
            confidence,
            merged_at,
            reverted,
        });
    }
    let mut out = format!(
        "# merge-risk self-release calibration for {repo} (min age {min_age_days}d, scanned {} merged PRs)\n\
         # revert detection is a heuristic: 'This reverts commit <sha>' on origin/main or a PR body 'Reverts {repo}#N', within {REVERT_WINDOW_DAYS}d of the merge\n\
         # {immature} self-released PR(s) younger than the window omitted\n",
        prs.len()
    );
    for r in &rows {
        out.push_str(&format!(
            "PR #{} confidence={:.2} merged_at={} reverted={}\n",
            r.pr,
            r.confidence,
            r.merged_at.format("%Y-%m-%d"),
            if r.reverted { "yes" } else { "no" }
        ));
    }
    out.push_str("bucket n reverted hit_rate\n");
    for b in summarize(&rows) {
        let rate = b
            .hit_rate
            .map_or("-".to_string(), |h| format!("{:.0}%", h * 100.0));
        out.push_str(&format!("{} {} {} {rate}\n", b.bucket, b.n, b.reverted));
    }
    Ok(out)
}

/// Whether a commit on `origin/main` reverts `sha` within the window.
fn revert_in_window(
    _repo: &str,
    pr: u64,
    sha: &str,
    window_end: chrono::DateTime<chrono::Utc>,
    root: &std::path::Path,
) -> Result<bool, String> {
    let out = std::process::Command::new("git")
        .current_dir(root)
        .args(["log", "origin/main", "-F", "--grep"])
        .arg(format!("This reverts commit {sha}"))
        .arg("--format=%cI")
        .output()
        .map_err(|e| format!("PR #{pr}: git log failed: {e}"))?;
    if !out.status.success() {
        return Err(format!("PR #{pr}: git log of origin/main failed"));
    }
    revert_dates_in_window(&String::from_utf8_lossy(&out.stdout), window_end)
        .map_err(|l| format!("PR #{pr}: unreadable revert date {l:?}"))
}

/// Whether any `%cI` line is at or before `window_end`; `Err(line)` on a bad date.
fn revert_dates_in_window(
    log: &str,
    window_end: chrono::DateTime<chrono::Utc>,
) -> Result<bool, String> {
    let mut any = false;
    for l in log.lines().filter(|l| !l.is_empty()) {
        let at = l
            .parse::<chrono::DateTime<chrono::Utc>>()
            .map_err(|_| l.to_string())?;
        any |= at <= window_end;
    }
    Ok(any)
}

/// Whether a PR created within the window says `Reverts <repo>#<pr>`.
fn reverts_by_pr(
    repo: &str,
    pr: u64,
    sha: &str,
    window_end: chrono::DateTime<chrono::Utc>,
    root: &std::path::Path,
) -> Result<bool, String> {
    let q = format!("\"Reverts {repo}#{pr}\" in:body");
    let out = crate::script_helpers::run_gh(
        &[
            "pr",
            "list",
            "--repo",
            repo,
            "--state",
            "all",
            "--search",
            &q,
            "--limit",
            "20",
            "--json",
            "number,body,createdAt",
        ],
        root,
        false,
    );
    let out = out
        .ok_output()
        .ok_or_else(|| format!("PR #{pr}: revert search failed"))?;
    let found: Vec<Value> = serde_json::from_slice(&out.stdout)
        .map_err(|_| format!("PR #{pr}: revert search is not JSON"))?;
    Ok(search_has_revert(&found, pr, sha, repo, window_end))
}

/// Whether any searched PR (not `pr` itself) created within the window reverts `pr`.
fn search_has_revert(
    found: &[Value],
    pr: u64,
    sha: &str,
    repo: &str,
    window_end: chrono::DateTime<chrono::Utc>,
) -> bool {
    found.iter().any(|f| {
        let body = f.get("body").and_then(Value::as_str).unwrap_or("");
        let created = f
            .get("createdAt")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());
        f.get("number").and_then(Value::as_u64) != Some(pr)
            && created.is_some_and(|c| c <= window_end)
            && crate::champion_hold::calibration::reverts(body, sha, repo, pr)
    })
}

#[cfg(test)]
mod calibration_io_tests {
    use super::*;
    use serde_json::json;

    fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn resolve_repo_never_yields_the_placeholder() {
        assert_eq!(resolve_repo(Some("o/r".into()), || panic!("not needed")), Some("o/r".into()));
        assert_eq!(resolve_repo(None, || Some("a/b".into())), Some("a/b".into()));
        assert_eq!(resolve_repo(None, || None), None);
        assert_eq!(resolve_repo(None, || Some("{owner}/{repo}".into())), None);
        assert_eq!(resolve_repo(Some("{owner}/{repo}".into()), || None), None);
        assert_eq!(resolve_repo(Some(" ".into()), || Some("a/b".into())), Some("a/b".into()));
    }

    #[test]
    fn revert_log_window() {
        let end = t("2026-10-08T00:00:00Z");
        assert_eq!(revert_dates_in_window("", end), Ok(false));
        assert_eq!(revert_dates_in_window("2026-10-07T00:00:00+00:00\n", end), Ok(true));
        assert_eq!(revert_dates_in_window("2026-10-09T00:00:00+00:00\n", end), Ok(false));
        assert!(revert_dates_in_window("garbage\n", end).is_err());
    }

    #[test]
    fn pr_body_revert_decisions() {
        let end = t("2026-10-08T00:00:00Z");
        let found = |n: u64, body: &str, at: &str| {
            vec![json!({"number": n, "body": body, "createdAt": at})]
        };
        // reverted: a different PR in the window naming the real repo
        assert!(search_has_revert(
            &found(9, "Reverts o/r#5", "2026-10-02T00:00:00Z"),
            5,
            "abc",
            "o/r",
            end
        ));
        // the unresolved placeholder never matches a real revert body
        assert!(!search_has_revert(
            &found(9, "Reverts o/r#5", "2026-10-02T00:00:00Z"),
            5,
            "abc",
            "{owner}/{repo}",
            end
        ));
        // clean: no search hits
        assert!(!search_has_revert(&[], 5, "abc", "o/r", end));
        // outside the window
        assert!(!search_has_revert(
            &found(9, "Reverts o/r#5", "2026-10-09T00:00:00Z"),
            5,
            "abc",
            "o/r",
            end
        ));
        // the PR itself, an unrelated PR number
        assert!(!search_has_revert(
            &found(5, "Reverts o/r#5", "2026-10-02T00:00:00Z"),
            5,
            "abc",
            "o/r",
            end
        ));
        assert!(!search_has_revert(
            &found(9, "Reverts o/r#55", "2026-10-02T00:00:00Z"),
            5,
            "abc",
            "o/r",
            end
        ));
    }
}
