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

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::cmd_out::CmdOutcome;
use crate::forge_cmd::{detect_forge, ForgeType, EX_FORGE_DECLINED, FORGE_CMD_TIMEOUT};
use crate::forge_pr_congestion::link_issue_number;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
use crate::work_finder::operator_priority::{GhTimelineStarredAt, StarredAtSource};
use crate::work_finder::{candidate_cmp, PriorityCandidate, OPERATOR_PRIORITY_LABEL};
use crate::worktree_ops::gh::resolve_owner_repo;

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

struct Listed {
    row: StarredRow,
    labels: Vec<String>,
    link: Option<u32>,
}

fn list_starred(root: &Path, owner: &str, repo: &str) -> Result<Vec<Listed>> {
    let path = format!(
        "repos/{owner}/{repo}/issues?labels={OPERATOR_PRIORITY_LABEL}&state=open&per_page=100"
    );
    let jq = r#".[] | {number, created_at, pr: (.pull_request != null), body: (.body // ""), labels: [.labels[].name]}"#;
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
        let v: serde_json::Value =
            serde_json::from_str(line).context("unparseable gh api listing line")?;
        let number = u32::try_from(v["number"].as_u64().context("missing number")?)?;
        let is_pr = v["pr"].as_bool().unwrap_or(false);
        out.push(Listed {
            row: StarredRow {
                number,
                kind: if is_pr { Kind::Pr } else { Kind::Issue },
                created_at: v["created_at"].as_str().map(str::to_string),
                starred_at: None,
                inherited_from: None,
            },
            labels: v["labels"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|l| l.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            link: if is_pr {
                link_issue_number("", v["body"].as_str().unwrap_or(""))
            } else {
                None
            },
        });
    }
    Ok(out)
}

/// Open starred items of `kind` in the repo at `root`, best first, with PR
/// star inheritance applied. Shared by this command and `pr-queue`
/// ([`crate::pr_planning`]), so every consumer orders stars identically.
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
) -> Result<Vec<StarredRow>> {
    let (owner, repo) =
        resolve_owner_repo(root).context("could not resolve owner/repo from the git remotes")?;
    let listed = list_starred(root, &owner, &repo)?;
    let mut src = GhTimelineStarredAt {
        gh_bin: gh.to_path_buf(),
        cwd: Some(root.to_path_buf()),
        repo: Some(format!("{owner}/{repo}")),
    };
    let mut rows = Vec::new();
    let mut links = Vec::new();
    for mut l in listed {
        // An unreadable timeline degrades to the createdAt fallback, exactly
        // like the daemon's own cache does; it never drops the item.
        l.row.starred_at = src.starred_at(l.row.number).unwrap_or(None);
        rows.push((l.row, l.labels, l.link));
    }
    let mut only_rows: Vec<StarredRow> = rows.iter().map(|r| r.0.clone()).collect();
    links.extend(rows.iter().map(|r| r.2));
    inherit_pr_stars(&mut only_rows, &links);
    let filtered: Vec<StarredRow> = only_rows
        .into_iter()
        .zip(rows.iter().map(|r| &r.1))
        .filter(|(r, labels)| r.kind == kind && keep(labels, extra, without))
        .map(|(r, _)| r)
        .collect();
    Ok(order_starred(filtered))
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
    match starred_rows(&root, &gh, kind, label, &without) {
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

    #[test]
    fn label_and_without_filters() {
        let l = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let without = l(&["loom:building", "loom:blocked"]);
        assert!(keep(&l(&["loom:triage"]), None, &without));
        assert!(!keep(&l(&["loom:triage", "loom:building"]), None, &without));
        assert!(!keep(&l(&["loom:triage"]), Some("loom:issue"), &[]));
        assert!(keep(&l(&["loom:issue"]), Some("loom:issue"), &without));
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
