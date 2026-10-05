//! The forge reads behind `forge wait-checks` (#10330).
//!
//! Every read is a REST `GET` through the shared
//! [`crate::forge_etag_store::fetch_conditional`] — reader-App routing, the
//! [`crate::gh_invocation::GhInvocation`] facade, and call accounting under
//! caller [`CALLER`] all come with it — sent with `If-None-Match` whenever an
//! entry is held, so an unchanged poll is a free `304`.
//!
//! Entries live in two places: an in-process map (a wait polls the same URLs
//! many times) seeded from the shared on-disk store, so a fresh `--timeout 0`
//! snapshot process also revalidates instead of re-reading. The PR read uses
//! the exact entry `forge pr view --cached` uses for the same PR
//! ([`crate::forge_cached_view::entry_path`]), so the two share one ETag.
//!
//! The one read that is NOT conditional is the base branch's required-context
//! lookup — [`crate::merge_pr::stale_checks::fetch::required_contexts_with`],
//! shared verbatim with the merge guards so all of them agree about what a
//! branch requires. It runs at most once per wait (and only when a verdict
//! needs it), and is accounted under that implementation's own caller.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

use crate::forge_call_stats::{ops, ForgeOp};
use crate::forge_etag_store::{self as store, ConditionalRead, DiskEntry, Target};

/// The `forge_call_stats` caller every read here is recorded under, so
/// `loom-daemon status` shows this command's `ok` / `not_modified` split.
pub const CALLER: &str = "forge_wait_checks";

/// Filename prefix for this module's check-runs / status entries in the
/// shared store directory.
const CHECKS_PREFIX: &str = "checks-";

/// A check-runs / status entry is keyed by commit SHA, so it is dead once the
/// PR moves on. Entries older than this are pruned on the next write.
const CHECKS_ENTRY_MAX_AGE: Duration = Duration::from_secs(2 * 24 * 60 * 60);

/// Rows per page; also the threshold above which extra pages are read.
const PER_PAGE: usize = 100;

/// Why a read did not produce a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// Will not get better by waiting (auth, not found, unprocessable,
    /// unreadable or truncated payload): end the wait with `ERROR`.
    Fatal(String),
    /// A blip (no HTTP answer, 5xx, timeout): retry on the next poll.
    Transient(String),
}

/// The PR facts one poll needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullHead {
    pub sha: String,
    pub base_ref: String,
}

/// Conditional REST reads for one repository.
pub struct GhReads {
    gh_bin: PathBuf,
    cwd: Option<PathBuf>,
    store_dir: PathBuf,
    target: Target,
    memo: HashMap<String, DiskEntry>,
}

impl GhReads {
    /// Resolve the repository once (`repo`, else `cwd`'s `origin`).
    ///
    /// # Errors
    ///
    /// When no `owner/repo` can be resolved — the URLs must name the repo
    /// explicitly so the cache key and the request can never disagree.
    pub fn new(
        gh_bin: PathBuf,
        cwd: Option<PathBuf>,
        store_dir: PathBuf,
        repo: Option<&str>,
    ) -> Result<Self, String> {
        let target = store::resolve_target(cwd.as_deref(), repo);
        if target.repo.is_none() {
            return Err("cannot resolve owner/repo (pass --repo or run inside a clone)".into());
        }
        Ok(Self {
            gh_bin,
            cwd,
            store_dir,
            target,
            memo: HashMap::new(),
        })
    }

    fn nwo(&self) -> &str {
        self.target.repo.as_deref().unwrap_or_default()
    }

    /// `GET repos/{o}/{r}/pulls/{n}` → head SHA and base branch.
    pub fn pull(&mut self, number: u32) -> Result<PullHead, ReadError> {
        let url = crate::forge_cached_view::build_view_url("pr", Some(self.nwo()), number);
        let key = store::cache_key(self.cwd.as_deref(), &self.target, &url);
        let path = crate::forge_cached_view::entry_path(&self.store_dir, "pr", number, &key);
        let body = self.get(&url, ops::PR_VIEW_STATE, Some(&path))?;
        let v = parse(&body, "pull request")?;
        let field = |a: &str, b: &str| {
            v.get(a)
                .and_then(|x| x.get(b))
                .and_then(Value::as_str)
                .map(String::from)
        };
        match (field("head", "sha"), field("base", "ref")) {
            (Some(sha), Some(base_ref)) => Ok(PullHead { sha, base_ref }),
            _ => Err(ReadError::Fatal("unreadable: pull request has no head.sha/base.ref".into())),
        }
    }

    /// Every check-run for `sha`, folded into one `{total_count, check_runs}`
    /// document. Page 1 is conditional; further pages (only when
    /// `total_count` exceeds one page) are unconditional. A read short of the
    /// forge's own `total_count` is [`ReadError::Fatal`] — never settled on
    /// (#8895: an unread page can hide the only failure).
    pub fn check_runs(&mut self, sha: &str) -> Result<Value, ReadError> {
        let base = format!("repos/{}/commits/{sha}/check-runs?per_page={PER_PAGE}", self.nwo());
        let first = self.get_checks_entry(&base, ops::CI_CHECK_RUNS_FOR_SHA)?;
        let v = parse(&first, "check-runs")?;
        let total = v.get("total_count").and_then(Value::as_u64);
        let Some(Value::Array(rows)) = v.get("check_runs") else {
            // Leave the shape error to the classifier (one refusal story).
            return Ok(v);
        };
        let mut rows = rows.clone();
        let Some(total) = total else { return Ok(v) };
        let mut page = 1;
        while (rows.len() as u64) < total && rows.len() == page * PER_PAGE {
            page += 1;
            let body =
                self.get(&format!("{base}&page={page}"), ops::CI_CHECK_RUNS_FOR_SHA, None)?;
            let more = parse(&body, "check-runs page")?;
            match more.get("check_runs") {
                Some(Value::Array(extra)) if !extra.is_empty() => {
                    rows.extend(extra.iter().cloned())
                }
                _ => break,
            }
        }
        if (rows.len() as u64) < total {
            return Err(ReadError::Fatal(format!(
                "truncated: check-runs read {} of total_count {total}",
                rows.len()
            )));
        }
        Ok(json!({ "total_count": total, "check_runs": rows }))
    }

    /// `GET repos/{o}/{r}/commits/{sha}/status` — legacy commit statuses.
    pub fn statuses(&mut self, sha: &str) -> Result<Value, ReadError> {
        let url = format!("repos/{}/commits/{sha}/status?per_page={PER_PAGE}", self.nwo());
        let body = self.get_checks_entry(&url, ops::CI_CHECK_RUNS_FOR_SHA)?;
        let v = parse(&body, "commit status")?;
        let listed = v
            .get("statuses")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let total = v.get("total_count").and_then(Value::as_u64).unwrap_or(0);
        if (listed as u64) < total {
            return Err(ReadError::Fatal(format!(
                "truncated: commit status read {listed} of total_count {total}"
            )));
        }
        Ok(v)
    }

    /// `GET repos/{o}/{r}` → `default_branch` (SHA mode with no `--base`).
    pub fn default_branch(&mut self) -> Result<String, ReadError> {
        let url = format!("repos/{}", self.nwo());
        let body = self.get_checks_entry(&url, ops::REPO_VIEW)?;
        parse(&body, "repository")?
            .get("default_branch")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| ReadError::Fatal("unreadable: repository has no default_branch".into()))
    }

    /// The base branch's required status-check contexts, plus any notices
    /// (e.g. a plan-gated source) the caller must print.
    pub fn required(&self, base_ref: &str) -> Result<(Vec<String>, Vec<String>), String> {
        crate::merge_pr::stale_checks::fetch::required_contexts_with(
            &self.gh_bin.to_string_lossy(),
            self.nwo(),
            base_ref,
        )
    }

    fn get_checks_entry(&mut self, url: &str, op: ForgeOp) -> Result<String, ReadError> {
        let key = store::cache_key(self.cwd.as_deref(), &self.target, url);
        let path = store::entry_path_with_prefix(&self.store_dir, CHECKS_PREFIX, &key);
        self.get(url, op, Some(&path))
    }

    /// One read. `path` = `Some` makes it conditional on the held entry and
    /// persists a fresh `200`; `None` is an unconditional, unpersisted read.
    fn get(&mut self, url: &str, op: ForgeOp, path: Option<&Path>) -> Result<String, ReadError> {
        let prior = path.and_then(|p| {
            self.memo
                .get(url)
                .cloned()
                .or_else(|| store::read_disk_entry(p))
        });
        let site = ConditionalRead::new(CALLER, op);
        let etag = prior.as_ref().map(|p| p.etag.as_str());
        let (status, response, stderr) = store::fetch_conditional(
            site,
            &self.gh_bin,
            self.cwd.as_deref(),
            &self.target,
            url,
            etag,
        )
        .map_err(|e| ReadError::Transient(format!("{e:#}")))?;
        match response {
            Some(r) if r.status == 304 => {
                if let Some(p) = prior {
                    self.memo.insert(url.to_string(), p.clone());
                    return Ok(p.body);
                }
                Err(ReadError::Transient(format!("{url}: 304 with no held entry")))
            }
            Some(r) if r.status == 200 && status.success() => {
                if let (Some(p), Some(etag)) = (path, r.etag.clone()) {
                    let entry = DiskEntry {
                        etag,
                        body: r.body.clone(),
                    };
                    store::write_disk_entry(p, &entry);
                    self.memo.insert(url.to_string(), entry);
                    prune_stale(&self.store_dir);
                }
                Ok(r.body)
            }
            Some(r) if matches!(r.status, 401 | 403 | 404 | 410 | 422) => {
                Err(ReadError::Fatal(format!("HTTP {} for {url}", r.status)))
            }
            Some(r) => Err(ReadError::Transient(format!("HTTP {} for {url}", r.status))),
            None => {
                let why = stderr
                    .lines()
                    .next()
                    .unwrap_or("no HTTP response")
                    .to_string();
                Err(ReadError::Transient(format!("{url}: {why}")))
            }
        }
    }
}

fn parse(body: &str, what: &str) -> Result<Value, ReadError> {
    serde_json::from_str(body.trim())
        .map_err(|e| ReadError::Fatal(format!("unreadable: {what} is not JSON ({e})")))
}

/// Remove this module's entries last written more than
/// [`CHECKS_ENTRY_MAX_AGE`] ago (one pair per commit SHA ever waited on).
fn prune_stale(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for e in rd.filter_map(Result::ok) {
        let ours = e
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(CHECKS_PREFIX) && n.ends_with(".json"));
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > CHECKS_ENTRY_MAX_AGE);
        if ours && stale {
            let _ = std::fs::remove_file(e.path());
        }
    }
}
