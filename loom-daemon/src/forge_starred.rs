//! `loom-daemon forge starred --kind issue|pr [--label L] [--without L,..] [--json]` — the
//! shared, star-time-ordered query for `loom:operator-priority` items
//! (#9974 slice 1).
//!
//! Roles used to take starred work in `gh issue list` default order (newest
//! created first), so with two stars the *later-starred* item could win. This
//! command orders starred items exactly as the daemon's work finder does:
//! through [`candidate_cmp`] (the comparator `ready-queue` ranks with), i.e.
//! earliest `labeled` event for the star first, falling back to `createdAt`
//! when the timeline read yields nothing, then issue number.
//!
//! A starred PR inherits its linked issue's star time (`Closes #N` / `Part of
//! #N`-style close keywords in the PR body) when that issue is itself starred
//! and was starred earlier than the PR's own label. Only PRs that carry the
//! label themselves are listed; the star is never added or removed here.
//!
//! Output: one number per line (best first), or `--json` for objects with
//! `number`, `kind`, `starred_at`, `inherited_from`. Exit `0` with possibly
//! empty output = verified answer; [`EX_STARRED_FAILED`] (5) = not answered
//! (fail closed, never an empty queue); `3` = Gitea decline.
//!
//! # Forge cost (#9975 review)
//!
//! One starred listing per call, plus a timeline read only where a star time
//! can change the answer ([`resolve`]): the rows that survive `--kind` /
//! `--label` / `--without`, when two or more do (or `--json` asks for the
//! times), and each kept PR's linked starred issue. Those reads go through
//! the on-disk [`StarTimeCache`], validated by each item's `updated_at`, so a
//! repeat call with no new star activity makes no timeline read at all.
//! Owner/repo comes from the git remote, never a GraphQL `gh repo view`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;

use crate::cmd_out::CmdOutcome;
use crate::forge_cmd::{detect_forge, ForgeType, EX_FORGE_DECLINED, FORGE_CMD_TIMEOUT};
use crate::forge_etag_store as store;
use crate::forge_pr_congestion::link_issue_number;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
use crate::operator_levels;
use crate::work_finder::operator_priority::GhTimelineStarredAt;
use crate::work_finder::{candidate_cmp, PriorityCandidate};
use crate::worktree_ops::gh::resolve_owner_repo;

mod cache;
pub use cache::StarTimeCache;

/// Exit code for "could not answer" — fail CLOSED (same meaning as
/// `forge check-open-pr`'s `5`).
pub const EX_STARRED_FAILED: i32 = 5;

/// Which kind of starred item to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Issue,
    Pr,
}

/// One starred item with the keys the ordering needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StarredRow {
    pub number: u32,
    pub kind: Kind,
    /// Effective operator priority level (#10307): 1 the star, 2 and up the
    /// higher levels, own or daemon-inherited. Orders before star time.
    pub level: u8,
    #[serde(skip)]
    pub created_at: Option<String>,
    /// Effective star time (`None` when the timeline had no event).
    pub starred_at: Option<String>,
    /// The linked issue whose earlier star this PR inherited, if any.
    pub inherited_from: Option<u32>,
}

/// Order `rows` best-first with the daemon's own [`candidate_cmp`]. Pure.
#[must_use]
pub fn order_starred(mut rows: Vec<StarredRow>) -> Vec<StarredRow> {
    rows.sort_by(|a, b| candidate_cmp(&candidate(a), &candidate(b)));
    rows
}

fn candidate(r: &StarredRow) -> PriorityCandidate {
    PriorityCandidate {
        operator_level: r.level,
        operator_priority: true,
        operator_priority_at: r.starred_at.clone(),
        created_at: r.created_at.clone(),
        number: r.number,
        ..PriorityCandidate::default()
    }
}

/// Give each PR in `rows` the star time of its linked issue when that issue
/// is starred (present in `rows`) and starred strictly earlier.
/// `links[i]` is row `i`'s linked issue number, if any.
pub fn inherit_pr_stars(rows: &mut [StarredRow], links: &[Option<u32>]) {
    let issue_at: Vec<(u32, Option<String>)> = rows
        .iter()
        .filter(|r| r.kind == Kind::Issue)
        .map(|r| (r.number, r.starred_at.clone()))
        .collect();
    for (row, link) in rows.iter_mut().zip(links) {
        if row.kind != Kind::Pr {
            continue;
        }
        let Some(n) = link else { continue };
        let Some((_, Some(at))) = issue_at.iter().find(|(i, _)| i == n) else {
            continue;
        };
        if row.starred_at.as_ref().is_none_or(|own| at < own) {
            row.starred_at = Some(at.clone());
            row.inherited_from = Some(*n);
        }
    }
}

/// One listed starred item: its ordering keys plus what filtering, PR
/// inheritance and the cache validator need.
#[derive(Debug, Clone)]
pub struct Listed {
    pub row: StarredRow,
    pub labels: Vec<String>,
    /// A PR's linked issue (close keyword in its body), if any.
    pub link: Option<u32>,
    /// The row's `updated_at`: the [`StarTimeCache`] validator.
    pub updated_at: Option<String>,
}

impl Listed {
    /// Parse a REST `/issues` or `/pulls` object. A `/pulls` object carries no
    /// `pull_request` key, so the caller says which kind it is. Labels may be
    /// names or `{name}` objects.
    #[must_use]
    pub fn from_rest(v: &Value, is_pr: bool) -> Option<Self> {
        let number = u32::try_from(v["number"].as_u64()?).ok()?;
        let text = |k: &str| v[k].as_str().map(str::to_string);
        let labels: Vec<String> = v["labels"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|l| l.as_str().or_else(|| l["name"].as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            row: StarredRow {
                number,
                kind: if is_pr { Kind::Pr } else { Kind::Issue },
                level: operator_levels::level(&labels).max(1),
                created_at: text("created_at"),
                starred_at: None,
                inherited_from: None,
            },
            labels,
            link: if is_pr {
                link_issue_number("", v["body"].as_str().unwrap_or(""))
            } else {
                None
            },
            updated_at: text("updated_at"),
        })
    }
}

/// Every open item carrying any starred label (the star, level 2 and the
/// daemon-written inherited labels, #10307), deduped by number: a listing is
/// one label's, and an item can carry several.
fn list_starred(root: &Path, owner: &str, repo: &str) -> Result<Vec<Listed>> {
    let mut out: Vec<Listed> = Vec::new();
    let mut seen = HashSet::new();
    for label in operator_levels::starred_labels(operator_levels::table()) {
        for l in list_label(root, owner, repo, label)? {
            if seen.insert(l.row.number) {
                out.push(l);
            }
        }
    }
    Ok(out)
}

fn list_label(root: &Path, owner: &str, repo: &str, label: &str) -> Result<Vec<Listed>> {
    let path = format!("repos/{owner}/{repo}/issues?labels={label}&state=open&per_page=100");
    let jq = r#".[] | {number, created_at, updated_at, pr: (.pull_request != null), body: (.body // ""), labels: [.labels[].name]}"#;
    // Spawned through the #9985 facade: it resolves `gh` (LOOM_GH_BIN, else
    // PATH), keys GH_CONFIG_DIR off `root`, and closes stdin.
    let outcome = GhInvocation::new(
        Operation::new("api.rest"),
        AccessIntent::Read,
        GhTarget::Repo {
            owner: owner.to_string(),
            repo: repo.to_string(),
        },
        FORGE_CMD_TIMEOUT,
    )
    .args(["api", path.as_str(), "--paginate", "--jq", jq])
    .current_dir(root)
    .run();
    let stdout = match outcome {
        CmdOutcome::Ran(o) if o.status.success() => o.stdout,
        CmdOutcome::Ran(o) => bail!("gh api failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
        CmdOutcome::Unavailable(u) => bail!("gh could not be run: {u}"),
    };
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(&stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
    {
        let v: Value = serde_json::from_str(line).context("unparseable gh api listing line")?;
        let is_pr = v["pr"].as_bool().unwrap_or(false);
        out.push(Listed::from_rest(&v, is_pr).context("listing line without a number")?);
    }
    Ok(out)
}

/// Filter `listed` to `kind` and the label filters FIRST, then resolve star
/// times only where they can change the answer (#9975 review): for the kept
/// rows when two or more remain (or `always`), and for each kept PR's linked
/// issue when that issue is itself in `listed` (starred). Nothing else is
/// read — not a filtered-out row, not an unlinked issue. `star_at(number,
/// updated_at)` is the lookup (the [`StarTimeCache`] in production). Rows
/// come back best first with PR inheritance applied.
pub fn resolve(
    listed: Vec<Listed>,
    kind: Kind,
    extra: Option<&str>,
    without: &[String],
    always: bool,
    star_at: &mut dyn FnMut(u32, Option<&str>) -> Option<String>,
) -> Vec<StarredRow> {
    let starred_issues: HashMap<u32, Listed> = listed
        .iter()
        .filter(|l| l.row.kind == Kind::Issue)
        .map(|l| (l.row.number, l.clone()))
        .collect();
    let mut kept: Vec<Listed> = listed
        .into_iter()
        .filter(|l| l.row.kind == kind && keep(&l.labels, extra, without))
        .collect();
    if kept.len() < 2 && !always {
        return kept.into_iter().map(|l| l.row).collect();
    }
    for l in &mut kept {
        l.row.starred_at = star_at(l.row.number, l.updated_at.as_deref());
    }
    if kind == Kind::Pr {
        let mut rows: Vec<StarredRow> = kept.iter().map(|l| l.row.clone()).collect();
        let mut links: Vec<Option<u32>> = kept.iter().map(|l| l.link).collect();
        let mut seen = HashSet::new();
        for n in kept.iter().filter_map(|l| l.link) {
            if let (true, Some(issue)) = (seen.insert(n), starred_issues.get(&n)) {
                let mut row = issue.row.clone();
                row.starred_at = star_at(n, issue.updated_at.as_deref());
                rows.push(row);
                links.push(None);
            }
        }
        inherit_pr_stars(&mut rows, &links);
        rows.retain(|r| r.kind == Kind::Pr);
        return order_starred(rows);
    }
    order_starred(kept.into_iter().map(|l| l.row).collect())
}

/// The repo target for `root`: `LOOM_REPO`, else the `origin` remote (no
/// forge call), else — only when neither resolves — `gh repo view`.
fn target_for(root: &Path) -> Result<(store::Target, String, String)> {
    let env_repo = std::env::var("LOOM_REPO").ok().filter(|r| !r.is_empty());
    let mut target = store::resolve_target(Some(root), env_repo.as_deref());
    if target.repo.is_none() {
        let (o, r) = resolve_owner_repo(root)
            .context("could not resolve owner/repo from the git remotes")?;
        target.repo = Some(format!("{o}/{r}"));
    }
    let nwo = target.repo.clone().unwrap_or_default();
    let (owner, repo) = nwo
        .split_once('/')
        .map(|(o, r)| (o.to_string(), r.to_string()))
        .context("owner/repo is not of the form OWNER/REPO")?;
    Ok((target, owner, repo))
}

/// Star-time lookup for one repo: the persisted [`StarTimeCache`] in front
/// of the REST timeline source. Call [`StarTimes::finish`] with the current
/// starred set to prune and save.
pub struct StarTimes {
    cache: StarTimeCache,
    src: GhTimelineStarredAt,
    now: i64,
}

impl StarTimes {
    /// The lookup for `target` reached from `root`. A target without a
    /// resolved repo keeps gh's `{owner}/{repo}` placeholder, as the
    /// conditional listing reads do.
    pub(crate) fn for_target(root: &Path, gh: &Path, target: &store::Target) -> Self {
        Self {
            cache: StarTimeCache::open(root, target),
            src: GhTimelineStarredAt {
                gh_bin: gh.to_path_buf(),
                cwd: Some(root.to_path_buf()),
                repo: target.repo.clone(),
            },
            now: chrono::Utc::now().timestamp(),
        }
    }

    /// `number`'s star time, read only on a cache miss.
    pub fn get(&mut self, number: u32, updated_at: Option<&str>) -> Option<String> {
        self.cache
            .star_at(number, updated_at, &mut self.src, self.now)
    }

    /// Prune entries for items not in `starred`, then persist.
    pub fn finish(mut self, starred: &HashSet<u32>) {
        self.cache.retain_starred(starred);
        self.cache.save();
    }
}

/// Open starred items of `kind` in the repo at `root`, best first, with PR
/// star inheritance applied. `always` resolves star times even for a single
/// row (`--json` reports them); otherwise they are read only when two or
/// more rows need ordering.
///
/// # Errors
/// The owner/repo could not be resolved or the starred listing failed. An
/// unreadable per-item timeline is not an error (createdAt fallback).
pub fn starred_rows(
    root: &Path,
    gh: &Path,
    kind: Kind,
    extra: Option<&str>,
    without: &[String],
    always: bool,
) -> Result<Vec<StarredRow>> {
    let (target, owner, repo) = target_for(root)?;
    let listed = list_starred(root, &owner, &repo)?;
    let starred: HashSet<u32> = listed.iter().map(|l| l.row.number).collect();
    let mut times = StarTimes::for_target(root, gh, &target);
    let rows = resolve(listed, kind, extra, without, always, &mut |n, u| times.get(n, u));
    times.finish(&starred);
    Ok(rows)
}

/// Whether an item with `labels` passes the `--label` / `--without` filter.
#[must_use]
pub fn keep(labels: &[String], extra: Option<&str>, without: &[String]) -> bool {
    extra.is_none_or(|x| labels.iter().any(|l| l == x))
        && !labels.iter().any(|l| without.contains(l))
}

/// Parsed `forge starred` arguments.
#[derive(Debug, Clone)]
pub struct StarredArgs {
    /// `issue` or `pr`.
    pub kind: String,
    /// Also require this label.
    pub label: Option<String>,
    /// Drop items carrying any of these labels.
    pub without: Vec<String>,
    /// Emit JSON objects instead of bare numbers.
    pub json: bool,
}

/// Handle `loom-daemon forge starred`. Exits the process.
pub fn handle(args: StarredArgs) -> Result<()> {
    let StarredArgs {
        kind,
        label,
        without,
        json,
    } = args;
    let (kind, label) = (kind.as_str(), label.as_deref());
    let kind = match kind {
        "issue" => Kind::Issue,
        "pr" => Kind::Pr,
        other => bail!("--kind must be `issue` or `pr`, got `{other}`"),
    };
    let root = std::env::current_dir()
        .context("loom-daemon forge starred: could not resolve the current directory")?;
    if detect_forge(Some(&root)) == ForgeType::Gitea {
        eprintln!("loom-daemon forge starred: GitHub-only; list starred items by hand.");
        std::process::exit(EX_FORGE_DECLINED);
    }
    let gh = PathBuf::from(crate::gh_invocation::gh_bin());
    match starred_rows(&root, &gh, kind, label, &without, json) {
        Ok(rows) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                for r in &rows {
                    println!("{}", r.number);
                }
            }
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("loom-daemon forge starred: {e:#}");
            eprintln!("No answer was produced - this is NOT an empty starred queue.");
            std::process::exit(EX_STARRED_FAILED);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(n: u32, kind: Kind, created: &str, at: Option<&str>) -> StarredRow {
        StarredRow {
            number: n,
            kind,
            level: 1,
            created_at: Some(created.into()),
            starred_at: at.map(str::to_string),
            inherited_from: None,
        }
    }

    /// B is older and lower-numbered, but A was starred first: A wins.
    #[test]
    fn earliest_star_wins_over_creation_and_number() {
        let a = row(20, Kind::Issue, "2026-09-10T00:00:00Z", Some("2026-09-20T00:00:00Z"));
        let b = row(10, Kind::Issue, "2026-09-01T00:00:00Z", Some("2026-09-25T00:00:00Z"));
        let order: Vec<u32> = order_starred(vec![b, a]).iter().map(|r| r.number).collect();
        assert_eq!(order, vec![20, 10]);
    }

    /// An unknown star time falls back to createdAt, as in the daemon.
    #[test]
    fn unknown_star_time_falls_back_to_created_at() {
        let a = row(5, Kind::Issue, "2026-09-02T00:00:00Z", None);
        let b = row(6, Kind::Issue, "2026-09-01T00:00:00Z", Some("2026-09-03T00:00:00Z"));
        let order: Vec<u32> = order_starred(vec![b, a]).iter().map(|r| r.number).collect();
        assert_eq!(order, vec![5, 6]);
    }

    /// #10307: level outranks star time, and an item carrying only a level
    /// label (no `loom:operator-priority`) still counts, own or inherited.
    #[test]
    fn higher_level_outranks_an_earlier_star() {
        let star = row(1, Kind::Issue, "2026-09-01T00:00:00Z", Some("2026-09-02T00:00:00Z"));
        let mut high = row(2, Kind::Issue, "2026-09-05T00:00:00Z", None);
        high.level = 2;
        let order: Vec<u32> = order_starred(vec![star, high])
            .iter()
            .map(|r| r.number)
            .collect();
        assert_eq!(order, vec![2, 1]);
        for labels in [
            ["loom:operator-high-priority"],
            ["loom:high-priority-inherited"],
        ] {
            let v = serde_json::json!({"number": 9, "labels": labels});
            assert_eq!(Listed::from_rest(&v, false).unwrap().row.level, 2, "{labels:?}");
        }
        let v = serde_json::json!({"number": 9, "labels": ["loom:operator-priority"]});
        assert_eq!(Listed::from_rest(&v, false).unwrap().row.level, 1);
    }

    #[test]
    fn label_and_without_filters() {
        let l = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let without = l(&["loom:building", "loom:blocked"]);
        assert!(keep(&l(&["loom:triage"]), None, &without));
        assert!(!keep(&l(&["loom:triage", "loom:building"]), None, &without));
        assert!(!keep(&l(&["loom:triage"]), Some("loom:issue"), &[]));
        assert!(keep(&l(&["loom:issue"]), Some("loom:issue"), &without));
    }

    fn listed(n: u32, kind: Kind, labels: &[&str], link: Option<u32>) -> Listed {
        Listed {
            row: row(n, kind, "2026-09-01T00:00:00Z", None),
            labels: labels.iter().map(|s| (*s).to_string()).collect(),
            link,
            updated_at: Some("2026-10-01T00:00:00Z".into()),
        }
    }

    /// Run [`resolve`] with a lookup that records every read; returns the
    /// order and the numbers read. Star times run backwards from #1.
    fn run(
        items: &[Listed],
        kind: Kind,
        extra: Option<&str>,
        without: &[&str],
        always: bool,
    ) -> (Vec<u32>, Vec<u32>) {
        let without: Vec<String> = without.iter().map(|s| (*s).to_string()).collect();
        let mut reads = Vec::new();
        let rows = resolve(items.to_vec(), kind, extra, &without, always, &mut |n, _| {
            reads.push(n);
            Some(format!("2026-09-{:02}T00:00:00Z", 30 - n % 29))
        });
        (rows.iter().map(|r| r.number).collect(), reads)
    }

    /// #9975 review: filters run BEFORE any timeline read, and a read is made
    /// only where a star time can change the answer.
    #[test]
    fn resolve_reads_only_the_rows_whose_order_it_decides() {
        let items = vec![
            listed(1, Kind::Issue, &["loom:issue"], None),
            listed(2, Kind::Issue, &["loom:issue", "loom:building"], None),
            listed(3, Kind::Issue, &["loom:triage"], None),
            listed(4, Kind::Issue, &["loom:issue"], None),
            listed(28, Kind::Issue, &[], None),
            listed(50, Kind::Pr, &["loom:review-requested"], Some(28)),
            listed(51, Kind::Pr, &["loom:pr"], None),
            listed(52, Kind::Pr, &["loom:review-requested"], Some(77)),
        ];
        // Kind + label + without first: #2 (claimed), #3, #28 and the PRs
        // are never read.
        let (order, reads) =
            run(&items, Kind::Issue, Some("loom:issue"), &["loom:building"], false);
        assert_eq!((order, reads), (vec![4, 1], vec![1, 4]));
        // One surviving row: no order to decide, nothing read unless asked.
        assert_eq!(run(&items, Kind::Issue, Some("loom:triage"), &[], false), (vec![3], vec![]));
        assert_eq!(run(&items, Kind::Issue, Some("loom:triage"), &[], true), (vec![3], vec![3]));
        // PRs: the kept PRs plus a kept PR's linked STARRED issue (#28),
        // never an unstarred link (#77) or an unrelated issue.
        let (order, reads) = run(&items, Kind::Pr, Some("loom:review-requested"), &[], false);
        assert_eq!(reads, vec![50, 52, 28]);
        assert_eq!(order, vec![50, 52], "#50 inherits #28's earlier star");
    }

    #[test]
    fn listed_parses_rest_issue_and_pull_objects() {
        let pull = serde_json::json!({"number": 12, "created_at": "c", "updated_at": "u",
            "body": "Closes #7", "labels": [{"name": "loom:operator-priority"}]});
        let l = Listed::from_rest(&pull, true).unwrap();
        assert_eq!((l.row.kind, l.link, l.updated_at.as_deref()), (Kind::Pr, Some(7), Some("u")));
        assert_eq!(l.labels, vec!["loom:operator-priority".to_string()]);
        let issue = serde_json::json!({"number": 7, "labels": ["loom:issue"], "body": "Closes #3"});
        let l = Listed::from_rest(&issue, false).unwrap();
        assert_eq!((l.row.kind, l.link, l.labels.len()), (Kind::Issue, None, 1));
    }

    #[test]
    fn pr_inherits_an_earlier_linked_issue_star() {
        let mut rows = vec![
            row(100, Kind::Pr, "2026-09-05T00:00:00Z", Some("2026-09-30T00:00:00Z")),
            row(7, Kind::Issue, "2026-09-01T00:00:00Z", Some("2026-09-02T00:00:00Z")),
            row(101, Kind::Pr, "2026-09-05T00:00:00Z", Some("2026-09-10T00:00:00Z")),
        ];
        inherit_pr_stars(&mut rows, &[Some(7), None, None]);
        assert_eq!(rows[0].starred_at.as_deref(), Some("2026-09-02T00:00:00Z"));
        assert_eq!(rows[0].inherited_from, Some(7));
        assert_eq!(rows[2].inherited_from, None);
        let order: Vec<u32> = order_starred(rows).iter().map(|r| r.number).collect();
        // 7 and 100 tie on star time; the older createdAt (7) goes first.
        assert_eq!(order, vec![7, 100, 101]);
    }
}
