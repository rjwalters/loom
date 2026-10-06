//! The fleet store's `repos.yml` history (#10508, #10586): the reader that
//! turns the store's commit log into the [`RosterRevision`]s the `eta-fit/v2`
//! inputs `repo_rank` and `ahead_dispatch_fleet` read
//! ([`super::repo_priority`], [`super::priority_inputs`]).
//!
//! Two halves, so training and serving read one value:
//!
//! - [`sync`] (network): on the ETA authority's fleet refresh cycle, list the
//!   commits that touched `repos.yml` on the store's ref and cache each
//!   revision's content under `<root>/.loom/state/eta/roster-history/`.
//! - [`load`] (disk only): read that cache back as revisions. The daily fit
//!   (`fit::run`) and the tracker's ETA pass both call it, and pass the
//!   result to the one shared input builder.
//!
//! # Wire shape (GitHub REST, through [`Transport`])
//!
//! - `GET repos/{store}/commits?path=repos.yml&sha={ref}&since={S}&per_page=100&page={p}`,
//!   paginated, with `S` = now − [`WINDOW_DAYS`] (the 14-day fit window plus
//!   margin). More than [`MAX_PAGES`] pages is an error, not a silent cut.
//! - `GET repos/{store}/commits?path=repos.yml&sha={ref}&until={S}&per_page=1`:
//!   the **anchor**, the revision in force when the window opens.
//! - `GET repos/{store}/contents/repos.yml?ref={commit}` for each commit not
//!   already cached. Contents are stored content-addressed by blob SHA, and
//!   a commit is immutable, so no revision is fetched twice.
//!
//! # Knowability convention
//!
//! A revision's `committed_at` is the commit's **committer** date (closer to
//! when it landed than the author date). Its `observed_at` is set only from
//! a recorded observation: the cache's index keeps `last_poll_at`, and a
//! commit first listed by a poll **after** an earlier successful poll gets
//! `observed_at` = that poll's time. It cannot have been on the ref before
//! the previous poll (that poll would have listed it), so the observation is
//! a conservative, never-early bound — a backdated or late-pushed commit
//! counts only from when this host first saw it. A commit listed by the
//! **first** poll a cache ever makes has no observation: it falls back to its
//! commit date ([`KnowBasis::CommitDate`]), and coverage says how many did.
//!
//! # When the history is unknown
//!
//! [`load`] returns no history (and its [`HistoryCoverage`] says why) when
//! there is no cache (feature off, or no poll has succeeded yet), when any
//! cached revision is unreadable or not a valid roster (a gap is unknown,
//! never bridged by its neighbour), or when the last successful poll is more
//! than [`MAX_STALE_HOURS`] before the instant asked about (a poll failing
//! for a day leaves the history unable to vouch for the present). It never
//! falls back to today's `repos.yml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use super::repo_priority::{KnowBasis, RosterRevision};
use crate::fleet_store::fetch::{write_atomic, Transport};
use crate::fleet_store::{roster, StoreLocation, ROSTER_PATH};

/// Days of history kept: the 14-day fit window plus a week of margin.
pub const WINDOW_DAYS: i64 = 21;

/// Commits listed per page.
pub const PER_PAGE: usize = 100;

/// Pages listed before the sync gives up rather than cut the history short.
pub const MAX_PAGES: usize = 10;

/// How long after its last successful poll the cache still vouches for the
/// present.
pub const MAX_STALE_HOURS: i64 = 24;

const INDEX: &str = "index.json";
const BLOBS: &str = "blobs";
const INDEX_VERSION: u32 = 1;

/// The cache directory: `<root>/.loom/state/eta/roster-history`, beside the
/// fleet snapshots and the fit files (ignored state, regenerable).
#[must_use]
pub fn dir(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(".loom")
        .join("state")
        .join("eta")
        .join("roster-history")
}

/// One cached revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitEntry {
    /// The commit SHA.
    pub sha: String,
    /// Its committer date.
    pub committed_at: DateTime<Utc>,
    /// The poll that first listed it, when an earlier poll had succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
    /// The blob SHA of its `repos.yml`.
    pub blob: String,
}

/// The cache's index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    /// Format version.
    pub version: u32,
    /// The store, `OWNER/REPO`.
    pub repo: String,
    /// The ref read.
    pub reference: String,
    /// The last successful poll.
    pub last_poll_at: DateTime<Utc>,
    /// Revisions in history order, oldest first.
    pub commits: Vec<CommitEntry>,
}

/// What one [`sync`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncReport {
    /// Revisions in the cache afterwards.
    pub revisions: usize,
    /// Of them, first listed by this poll.
    pub new: usize,
    /// Contents fetched (the rest were cached).
    pub fetched: usize,
}

#[derive(Deserialize)]
struct ListedCommit {
    sha: String,
    commit: ListedCommitBody,
}

#[derive(Deserialize)]
struct ListedCommitBody {
    committer: Option<Signature>,
}

#[derive(Deserialize)]
struct Signature {
    date: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct Contents {
    sha: String,
    content: String,
    encoding: String,
}

fn is_sha(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn list(transport: &dyn Transport, path: &str) -> Result<Vec<(String, DateTime<Utc>)>> {
    let reply = transport.get(path, None, None)?;
    if reply.status != 200 {
        bail!("forge answered HTTP {} listing {path}", reply.status);
    }
    let listed: Vec<ListedCommit> =
        serde_json::from_str(&reply.body).context("malformed commit listing")?;
    listed
        .into_iter()
        .map(|c| {
            if !is_sha(&c.sha) {
                bail!("malformed commit SHA in the listing");
            }
            let at = c
                .commit
                .committer
                .and_then(|s| s.date)
                .with_context(|| format!("commit {} has no committer date", c.sha))?;
            Ok((c.sha, at))
        })
        .collect()
}

fn fetch_contents(
    transport: &dyn Transport,
    location: &StoreLocation,
    commit: &str,
) -> Result<(String, Vec<u8>)> {
    let path = format!("repos/{}/contents/{ROSTER_PATH}?ref={commit}", location.repo);
    let reply = transport.get(&path, None, None)?;
    if reply.status != 200 {
        bail!("forge answered HTTP {} for {ROSTER_PATH} at {commit}", reply.status);
    }
    let c: Contents = serde_json::from_str(&reply.body).context("malformed contents reply")?;
    if c.encoding != "base64" || !is_sha(&c.sha) {
        bail!("unexpected contents reply for {ROSTER_PATH} at {commit}");
    }
    use base64::{engine::general_purpose, Engine as _};
    let compact: String = c.content.chars().filter(|ch| !ch.is_whitespace()).collect();
    let body = general_purpose::STANDARD
        .decode(compact)
        .context("contents are not valid base64")?;
    Ok((c.sha, body))
}

fn read_index(dir: &Path) -> Option<Index> {
    let raw = std::fs::read_to_string(dir.join(INDEX)).ok()?;
    serde_json::from_str::<Index>(&raw)
        .ok()
        .filter(|i| i.version == INDEX_VERSION)
}

/// Poll the store and bring the cache in `dir` up to date (see the module
/// docs). Any failure is an error and leaves the previous cache intact, and
/// `last_poll_at` unmoved, so the next success records observations no
/// earlier than it should.
pub fn sync(
    transport: &dyn Transport,
    dir: &Path,
    location: &StoreLocation,
    now: DateTime<Utc>,
) -> Result<SyncReport> {
    // A cache of another store or ref has no observations to offer.
    let previous =
        read_index(dir).filter(|i| i.repo == location.repo && i.reference == location.reference);
    let since = (now - Duration::days(WINDOW_DAYS)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let base =
        format!("repos/{}/commits?path={ROSTER_PATH}&sha={}", location.repo, location.reference);
    // Newest first, as the forge lists them.
    let mut listed = Vec::new();
    for page in 1..=MAX_PAGES + 1 {
        if page > MAX_PAGES {
            bail!(
                "{ROSTER_PATH} has more than {} commits in {WINDOW_DAYS} days",
                MAX_PAGES * PER_PAGE
            );
        }
        let batch =
            list(transport, &format!("{base}&since={since}&per_page={PER_PAGE}&page={page}"))?;
        let n = batch.len();
        listed.extend(batch);
        if n < PER_PAGE {
            break;
        }
    }
    let anchor = list(transport, &format!("{base}&until={since}&per_page=1"))?;
    for a in anchor {
        if !listed.iter().any(|(s, _)| *s == a.0) {
            listed.push(a);
        }
    }
    listed.reverse();

    let known: BTreeMap<&str, &CommitEntry> = previous
        .as_ref()
        .map(|i| i.commits.iter().map(|c| (c.sha.as_str(), c)).collect())
        .unwrap_or_default();
    let blob_dir = dir.join(BLOBS);
    let mut report = SyncReport::default();
    let mut commits = Vec::with_capacity(listed.len());
    for (sha, committed_at) in listed {
        if let Some(old) = known.get(sha.as_str()) {
            if blob_dir.join(&old.blob).is_file() {
                commits.push((*old).clone());
                continue;
            }
        }
        let (blob, body) = fetch_contents(transport, location, &sha)?;
        write_atomic(&blob_dir.join(&blob), &body)?;
        report.fetched += 1;
        let observed_at = match known.get(sha.as_str()) {
            Some(old) => old.observed_at,
            None => {
                report.new += 1;
                previous.as_ref().map(|_| now)
            }
        };
        commits.push(CommitEntry {
            sha,
            committed_at,
            observed_at,
            blob,
        });
    }
    report.revisions = commits.len();
    let index = Index {
        version: INDEX_VERSION,
        repo: location.repo.clone(),
        reference: location.reference.clone(),
        last_poll_at: now,
        commits,
    };
    let mut body = serde_json::to_vec_pretty(&index)?;
    body.push(b'\n');
    write_atomic(&dir.join(INDEX), &body)?;
    prune_blobs(&blob_dir, &index);
    Ok(report)
}

fn prune_blobs(blob_dir: &Path, index: &Index) {
    let Ok(entries) = std::fs::read_dir(blob_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !index.commits.iter().any(|c| c.blob == name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Why [`load`] did or did not return a history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryStatus {
    /// No cache: the feature is off, or no poll has succeeded.
    #[default]
    Missing,
    /// The last successful poll is too old to vouch for the instant asked.
    Stale,
    /// A cached revision is unreadable or not a valid roster.
    Unreadable,
    /// The history was loaded.
    Loaded,
}

/// What [`load`] found, for the fit report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct HistoryCoverage {
    /// Whether, and if not why not.
    pub status: HistoryStatus,
    /// Revisions loaded.
    pub revisions: usize,
    /// Of them, knowable from a recorded observation.
    pub observed: usize,
    /// Of them, knowable from the commit date alone.
    pub commit_date_only: usize,
}

/// The cached history in `dir` as revisions (oldest first), as of `at`; see
/// the module docs for when it is `None`. Disk only.
#[must_use]
pub fn load(dir: &Path, at: DateTime<Utc>) -> (Option<Vec<RosterRevision>>, HistoryCoverage) {
    let none = |status| {
        (
            None,
            HistoryCoverage {
                status,
                ..HistoryCoverage::default()
            },
        )
    };
    let Some(index) = read_index(dir) else {
        return none(HistoryStatus::Missing);
    };
    if index.commits.is_empty() {
        return none(HistoryStatus::Missing);
    }
    if at - index.last_poll_at > Duration::hours(MAX_STALE_HOURS) {
        return none(HistoryStatus::Stale);
    }
    // `root` in `repos.yml` is irrelevant to priority; any absolute home works.
    let home = Path::new("/");
    let mut revisions = Vec::with_capacity(index.commits.len());
    for c in &index.commits {
        let parsed = std::fs::read(dir.join(BLOBS).join(&c.blob))
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|text| roster::parse(&text, home).ok());
        let Some(parsed) = parsed else {
            log::warn!(
                "eta roster history: revision {} of {ROSTER_PATH} is unreadable; \
                 repo rank is unknown until it is re-fetched",
                c.sha
            );
            return none(HistoryStatus::Unreadable);
        };
        revisions.push(RosterRevision::from_roster(&parsed, c.committed_at, c.observed_at));
    }
    let observed = revisions
        .iter()
        .filter(|r| r.basis() == KnowBasis::Observed)
        .count();
    let coverage = HistoryCoverage {
        status: HistoryStatus::Loaded,
        revisions: revisions.len(),
        observed,
        commit_date_only: revisions.len() - observed,
    };
    (Some(revisions), coverage)
}

/// [`load`] from the workspace's cache.
#[must_use]
pub fn load_for(
    workspace_root: &Path,
    at: DateTime<Utc>,
) -> (Option<Vec<RosterRevision>>, HistoryCoverage) {
    load(&dir(workspace_root), at)
}

/// The fleet refresh cycle's step: [`sync`] the workspace's cache from the
/// configured store. A no-op when `fleet.repo` is unset; a failure is logged
/// and leaves the cache as it was.
pub fn sync_for(workspace_root: &Path, now: DateTime<Utc>) {
    let effective = crate::config_resolver::resolve_effective_config(workspace_root);
    let location =
        match crate::fleet_store::resolve_location(&effective, &|k| std::env::var(k).ok()) {
            Ok(Some(l)) => l,
            Ok(None) => return,
            Err(e) => {
                log::warn!("eta roster history: fleet store misconfigured: {e:#}");
                return;
            }
        };
    let transport = crate::fleet_store::gh::GhTransport::new(workspace_root, &location.repo);
    match sync(&transport, &dir(workspace_root), &location, now) {
        Ok(r) => log::info!(
            "eta roster history: {} revisions of {}:{ROSTER_PATH} ({} new, {} fetched)",
            r.revisions,
            location.repo,
            r.new,
            r.fetched
        ),
        Err(e) => log::warn!("eta roster history: sync failed, keeping the cache: {e:#}"),
    }
}
