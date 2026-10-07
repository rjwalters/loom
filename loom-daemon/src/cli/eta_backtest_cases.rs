//! `eta backtest`'s PR-history `land` case source (#9579).
//!
//! A default `eta backtest` reads local journals only and makes no forge
//! call. These flags add `land` replay cases derived from merged PRs' label
//! timelines ([`loom_daemon::eta::backtest::pr_cases`]), from one of two
//! sources that share one conversion:
//!
//! - `--pr-history PATH` — an offline file of `eta-pr-case/v1` records (a
//!   fixture, or a cache an earlier `--save-pr-history` wrote). No forge call.
//! - `--forge-pr-cases` — an explicit opt-in to fetch them: one bounded
//!   `fetch_histories` pass (`--pr-limit` PRs, the same reads `eta backfill`
//!   makes) plus one `gh pr list` for the closing references. Never a
//!   per-case fetch loop.
//!
//! Forge cases are merged into the sweep-derived set with
//! [`backtest::merge_case_sets`], so a PR merged inside a sweep is scored
//! once, and every PR that yields no case is reported by reason on stderr.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use loom_daemon::cmd_out::Query;
use loom_daemon::eta::backtest::{
    self, cases_from_pr_records_with_roster, parse_pr_records, PrCaseRecord, PrCaseSummary,
    ReplayCase,
};
use loom_daemon::script_helpers::gh_query;

use loom_daemon::eta::history::StageSamples;
use loom_daemon::eta::journal;

use super::eta_cmd::{load_outcome_envelopes, resolve_repo, EtaBacktestArgs};
use super::pr_latency_cmd::fetch_histories;

/// PRs examined per `--forge-pr-cases` run, absent `--pr-limit`.
const DEFAULT_PR_LIMIT: u32 = 100;

/// The PR-history case source flags, flattened into `eta backtest`.
#[derive(clap::Args, Debug, Clone, Default)]
pub(crate) struct PrCaseArgs {
    /// Also replay `land` cases from this offline file of merged-PR label
    /// histories (`eta-pr-case/v1` records, JSON array or JSON Lines). No
    /// forge call.
    #[arg(long, value_name = "PATH")]
    pub pr_history: Option<PathBuf>,

    /// Opt in to fetching merged-PR label histories from the forge for
    /// `land` cases (bounded by `--pr-limit`). Without it, `eta backtest`
    /// never calls the forge.
    #[arg(long)]
    pub forge_pr_cases: bool,

    /// With `--forge-pr-cases`: PRs to examine, most recently created first.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_PR_LIMIT)]
    pub pr_limit: u32,

    /// With `--forge-pr-cases`: also write the fetched records here, for
    /// later offline replay with `--pr-history`.
    #[arg(long, value_name = "PATH", requires = "forge_pr_cases")]
    pub save_pr_history: Option<PathBuf>,

    /// With `--forge-pr-cases`: report fetch progress on stderr.
    #[arg(long)]
    pub progress: bool,
}

/// Where forge PR records come from. A seam so tests never touch the
/// host's live credentials.
pub(crate) trait PrRecordFetcher {
    /// Up to `limit` PR records for `repo`, plus a warning when the
    /// enumeration did not fully answer.
    fn fetch(
        &self,
        root: &Path,
        repo: &str,
        limit: u32,
        progress: bool,
    ) -> (Vec<PrCaseRecord>, Option<String>);
}

/// The real fetcher: `gh`.
pub(crate) struct GhFetcher;

impl PrRecordFetcher for GhFetcher {
    fn fetch(
        &self,
        root: &Path,
        repo: &str,
        limit: u32,
        progress: bool,
    ) -> (Vec<PrCaseRecord>, Option<String>) {
        let (histories, list_error) = fetch_histories(root, Some(repo), limit, false, progress);
        let (closing, refs_error) = fetch_closing_refs(root, repo, limit);
        if let Some(err) = &refs_error {
            eprintln!(
                "[eta backtest] WARNING: closing references did not answer ({err}); \
                 affected PRs are excluded as missing_identity"
            );
        }
        let records = histories
            .iter()
            .map(|h| PrCaseRecord::from_history(repo, h, closing.get(&h.number).cloned()))
            .collect();
        (records, list_error)
    }
}

/// One row of `gh pr list --json number,closingIssuesReferences`.
#[derive(Debug, Deserialize)]
struct ClosingRow {
    number: u32,
    #[serde(default, rename = "closingIssuesReferences")]
    refs: Vec<ClosingRef>,
}

#[derive(Debug, Deserialize)]
struct ClosingRef {
    number: u32,
    #[serde(default)]
    repository: Option<RefRepo>,
}

#[derive(Debug, Deserialize)]
struct RefRepo {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    owner: Option<RefOwner>,
}

#[derive(Debug, Deserialize)]
struct RefOwner {
    #[serde(default)]
    login: Option<String>,
}

/// PR number → the issues it closes **in `repo`**. A cross-repo closing
/// reference is not this repo's issue and is dropped; a reference whose
/// repository the forge did not name is kept (same-repo is the default).
fn closing_map(rows: Vec<ClosingRow>, repo: &str) -> std::collections::BTreeMap<u32, Vec<u32>> {
    rows.into_iter()
        .map(|row| {
            let issues = row
                .refs
                .into_iter()
                .filter(|r| {
                    let Some(rr) = &r.repository else {
                        return true;
                    };
                    match (&rr.name, rr.owner.as_ref().and_then(|o| o.login.as_ref())) {
                        (Some(name), Some(owner)) => {
                            format!("{owner}/{name}").eq_ignore_ascii_case(repo)
                        }
                        _ => true,
                    }
                })
                .map(|r| r.number)
                .collect();
            (row.number, issues)
        })
        .collect()
}

/// One `gh pr list` call for every PR's closing references, over the same
/// `--state all --limit N` enumeration `fetch_histories` uses.
fn fetch_closing_refs(
    root: &Path,
    repo: &str,
    limit: u32,
) -> (std::collections::BTreeMap<u32, Vec<u32>>, Option<String>) {
    let limit = limit.to_string();
    let args = [
        "pr",
        "list",
        "--repo",
        repo,
        "--state",
        "all",
        "--json",
        "number,closingIssuesReferences",
        "--limit",
        &limit,
    ];
    let q: Query<Vec<ClosingRow>> =
        gh_query(&args, root, false, |v: &Vec<ClosingRow>| v.is_empty());
    match q {
        Query::Populated(rows) => (closing_map(rows, repo), None),
        Query::Empty => (Default::default(), None),
        Query::Malformed { error, .. } => {
            (Default::default(), Some(format!("unreadable JSON: {error}")))
        }
        Query::Failed { status, .. } => (Default::default(), Some(format!("exited {status}"))),
        Query::Unavailable(u) => (Default::default(), Some(format!("could not be run: {u:?}"))),
    }
}

impl EtaBacktestArgs {
    /// The history and the replay set every report and `--compare` side is
    /// scored on: local journals, plus the PR-history `land` cases the flags
    /// ask for (deduplicated against the sweep-derived ones), and the line
    /// describing that source when one was used.
    pub(crate) fn replay_inputs(
        &self,
        root: &Path,
        fetcher: &dyn PrRecordFetcher,
    ) -> Result<(StageSamples, Vec<ReplayCase>, Option<String>)> {
        let envelopes = load_outcome_envelopes(root);
        let mut history = StageSamples::default();
        history.push_envelopes(&envelopes);
        let journal_entries = journal::read(&journal::journal_path(root));
        history.push_journal(&journal_entries, "local");
        let mut cases = backtest::cases_from_envelopes(&envelopes);
        cases.extend(backtest::cases_from_journal(&journal_entries));
        let (cases, note) =
            add_pr_cases(root, self.repo.as_deref(), &self.pr_cases, cases, fetcher)?;
        Ok((history, cases, note))
    }
}

/// `base` (the sweep/journal-derived cases) plus every PR-history case the
/// flags ask for, deduplicated, and the line describing what was added.
/// With neither flag set this returns `base` untouched and never calls
/// `fetcher`.
pub(crate) fn add_pr_cases(
    root: &Path,
    repo: Option<&str>,
    args: &PrCaseArgs,
    base: Vec<ReplayCase>,
    fetcher: &dyn PrRecordFetcher,
) -> Result<(Vec<ReplayCase>, Option<String>)> {
    if args.pr_history.is_none() && !args.forge_pr_cases {
        return Ok((base, None));
    }
    let mut records = Vec::new();
    if let Some(path) = &args.pr_history {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading --pr-history {}", path.display()))?;
        records.extend(
            parse_pr_records(&text)
                .map_err(|e| anyhow::anyhow!("--pr-history {}: {e}", path.display()))?,
        );
    }
    if args.forge_pr_cases {
        let repo = match repo {
            Some(r) => r.to_string(),
            None => resolve_repo(root).ok_or_else(|| {
                anyhow::anyhow!("--forge-pr-cases could not resolve owner/repo; pass --repo")
            })?,
        };
        let (fetched, list_error) = fetcher.fetch(root, &repo, args.pr_limit, args.progress);
        if let Some(err) = &list_error {
            if fetched.is_empty() {
                bail!("--forge-pr-cases: enumerating PRs failed: {err}");
            }
            eprintln!("[eta backtest] WARNING: enumerating PRs did not fully answer: {err}");
        }
        if let Some(path) = &args.save_pr_history {
            let mut text = String::new();
            for r in &fetched {
                text.push_str(&serde_json::to_string(r)?);
                text.push('\n');
            }
            std::fs::write(path, text)
                .with_context(|| format!("writing --save-pr-history {}", path.display()))?;
        }
        records.extend(fetched);
    }

    // The `eta-fit/v2` priority inputs (#10508): the cached fleet roster
    // history, or unknown (never today's `repos.yml`) when there is none.
    let history = loom_daemon::eta::roster_history::load_for(root, chrono::Utc::now()).0;
    let (pr_cases, summary) = cases_from_pr_records_with_roster(&records, history.as_deref());
    let (merged, dropped) = backtest::merge_case_sets(base, pr_cases);
    Ok((merged, Some(describe(&summary, dropped))))
}

fn describe(summary: &PrCaseSummary, dropped: usize) -> String {
    let tally = |counts: &std::collections::BTreeMap<String, usize>| {
        if counts.is_empty() {
            "none".to_string()
        } else {
            counts
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ")
        }
    };
    format!(
        "[eta backtest] PR-history land cases: {} PR(s) read, {} contributing, {} case(s), \
         {dropped} already answered by another source; excluded: {}; refused entries: {}",
        summary.prs,
        summary.contributing,
        summary.cases,
        tally(&summary.excluded),
        tally(&summary.refused_entries)
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use loom_daemon::eta::backtest::{Filter, PrCaseRecord};
    use loom_daemon::eta::heuristics::LandV1;
    use loom_daemon::eta::journal::entries_from_pr_history;
    use loom_daemon::eta::{Kind, Provenance};

    const FIXTURE: &str = include_str!("../eta/fixtures/pr-history-land.jsonl");

    /// Fails the test if anything reaches for the forge.
    struct NoForge;
    impl PrRecordFetcher for NoForge {
        fn fetch(&self, _: &Path, _: &str, _: u32, _: bool) -> (Vec<PrCaseRecord>, Option<String>) {
            panic!("the forge must not be called");
        }
    }

    /// Serves the fixture as if fetched, recording the bound it was given.
    struct Stub(std::cell::Cell<Option<u32>>, Option<String>);
    impl PrRecordFetcher for Stub {
        fn fetch(
            &self,
            _: &Path,
            repo: &str,
            limit: u32,
            _: bool,
        ) -> (Vec<PrCaseRecord>, Option<String>) {
            assert_eq!(repo, "rjwalters/loom");
            self.0.set(Some(limit));
            let records = if self.1.is_some() {
                Vec::new()
            } else {
                records()
            };
            (records, self.1.clone())
        }
    }

    fn records() -> Vec<PrCaseRecord> {
        parse_pr_records(FIXTURE).unwrap()
    }

    /// A workspace whose stage journal holds what `eta backfill` would have
    /// written for the fixture PRs, and no sweep-outcome journal at all.
    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let rows: Vec<_> = records()
            .iter()
            .flat_map(|r| entries_from_pr_history(&r.history(), &r.repo, &Provenance::current()))
            .collect();
        journal::append(&journal::journal_path(dir.path()), &rows).unwrap();
        dir
    }

    fn args(root: &Path, cases: PrCaseArgs) -> EtaBacktestArgs {
        EtaBacktestArgs {
            heuristic: "land-v1".to_string(),
            compare: None,
            since: None,
            repo: Some("rjwalters/loom".to_string()),
            repo_root: Some(root.to_path_buf()),
            json: false,
            fit_dir: None,
            adaptation: false,
            pr_cases: cases,
        }
    }

    #[test]
    fn a_default_backtest_stays_offline_and_has_no_land_case() {
        let dir = workspace();
        let (_, cases, note) = args(dir.path(), PrCaseArgs::default())
            .replay_inputs(dir.path(), &NoForge)
            .unwrap();
        assert!(note.is_none());
        assert!(cases.iter().all(|c| c.kind != Kind::Land), "the #9579 symptom, unchanged");
    }

    #[test]
    fn the_offline_pr_history_input_scores_land_cases_without_the_forge() {
        let dir = workspace();
        let file = dir.path().join("prs.jsonl");
        std::fs::write(&file, FIXTURE).unwrap();
        let a = args(
            dir.path(),
            PrCaseArgs {
                pr_history: Some(file),
                ..PrCaseArgs::default()
            },
        );
        let (history, cases, note) = a.replay_inputs(dir.path(), &NoForge).unwrap();
        let note = note.unwrap();
        assert!(note.contains("24 PR(s) read, 20 contributing, 60 case(s)"), "{note}");
        assert!(note.contains("closed_unmerged=1"), "{note}");
        assert!(note.contains("refused entries: none"), "{note}");
        let filter = Filter {
            since: None,
            repo: a.repo.as_deref(),
        };
        let report = backtest::run(&LandV1, &history, &cases, filter, &Provenance::current());
        assert_eq!(report.overall.n, 60);
        assert!(report.overall.scored > 0, "{report:?}");
    }

    #[test]
    fn the_forge_source_is_bounded_deduplicated_and_cacheable() {
        let dir = workspace();
        let file = dir.path().join("prs.jsonl");
        std::fs::write(&file, FIXTURE).unwrap();
        let saved = dir.path().join("saved.jsonl");
        let stub = Stub(std::cell::Cell::new(None), None);
        let a = args(
            dir.path(),
            PrCaseArgs {
                pr_history: Some(file),
                forge_pr_cases: true,
                pr_limit: 30,
                save_pr_history: Some(saved.clone()),
                progress: false,
            },
        );
        let (_, cases, note) = a.replay_inputs(dir.path(), &stub).unwrap();
        assert_eq!(stub.0.get(), Some(30), "the --pr-limit bound reaches the fetch");
        // The same PRs from the file and the forge are one case set.
        assert_eq!(cases.iter().filter(|c| c.kind == Kind::Land).count(), 60);
        assert!(note.unwrap().contains("60 already answered"));
        assert_eq!(parse_pr_records(&std::fs::read_to_string(saved).unwrap()).unwrap(), records());
    }

    #[test]
    fn a_forge_source_that_answered_nothing_is_an_error_not_an_empty_set() {
        let dir = workspace();
        let stub = Stub(std::cell::Cell::new(None), Some("gh pr list exited 1".to_string()));
        let a = args(
            dir.path(),
            PrCaseArgs {
                forge_pr_cases: true,
                pr_limit: 5,
                ..PrCaseArgs::default()
            },
        );
        let err = a.replay_inputs(dir.path(), &stub).unwrap_err();
        assert!(err.to_string().contains("enumerating PRs failed"), "{err}");
    }

    #[test]
    fn closing_references_keep_only_this_repos_issues() {
        let rows: Vec<ClosingRow> = serde_json::from_str(
            r#"[
              {"number":1,"closingIssuesReferences":[
                {"number":10,"repository":{"name":"loom","owner":{"login":"RJWalters"}}},
                {"number":99,"repository":{"name":"other","owner":{"login":"rjwalters"}}}]},
              {"number":2,"closingIssuesReferences":[]},
              {"number":3,"closingIssuesReferences":[{"number":30}]}
            ]"#,
        )
        .unwrap();
        let map = closing_map(rows, "rjwalters/loom");
        assert_eq!(map[&1], vec![10]);
        assert!(map[&2].is_empty());
        assert_eq!(map[&3], vec![30]);
    }
}
